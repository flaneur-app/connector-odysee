// SPDX-License-Identifier: AGPL-3.0-or-later

//! The HTTP side of the connector: one client, the Odysee hosts, and the three wire formats
//! (the SDK proxy's JSON-RPC, the internal API's forms, Commentron's JSON-RPC).
//!
//! Tokens only ever travel in a header (`X-Lbry-Auth-Token`) or a form body (`auth_token`), never
//! in a URL, and nothing here logs a request body or a header.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use flaneur_connector::{Error, Result};
use reqwest::{RequestBuilder, Response, StatusCode, header};
use rustls_platform_verifier::BuilderVerifierExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

/// User agent of every request: who we are and where to read about it.
pub(crate) const USER_AGENT: &str = concat!(
    "Flaneur/",
    env!("CARGO_PKG_VERSION"),
    " (+https://flaneur.baxyz.dev)"
);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The Odysee hosts the connector talks to.
///
/// [`Endpoints::odysee`] is the public Odysee backend; tests point every host at a mock server
/// with [`Endpoints::with_base`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// LBRY SDK JSON-RPC proxy (claims, streams, the hosted wallet).
    pub proxy: Url,
    /// Internal API base (users, reactions, history), ending with `/`.
    pub internal: Url,
    /// Commentron JSON-RPC endpoint.
    pub comments: Url,
    /// Lighthouse search.
    pub search: Url,
    /// Recsys search, for related items.
    pub related: Url,
    /// Curated homepage categories.
    pub homepage: Url,
    /// Video CDN base, ending with `/`.
    pub cdn: Url,
}

impl Endpoints {
    /// The public Odysee backend.
    ///
    /// # Panics
    ///
    /// Never: the URLs are constants.
    #[must_use]
    pub fn odysee() -> Self {
        let parse = |s: &str| Url::parse(s).expect("constant URL");
        Self {
            proxy: parse("https://api.na-backend.odysee.com/api/v1/proxy"),
            internal: parse("https://api.odysee.com/"),
            comments: parse("https://comments.odysee.tv/api/v2"),
            search: parse("https://lighthouse.odysee.tv/search"),
            related: parse("https://recsys.odysee.tv/search"),
            homepage: parse("https://odysee.com/$/api/content/v2/get"),
            cdn: parse("https://player.odycdn.com/"),
        }
    }

    /// Every host under one base URL, each on its own path: `proxy`, `internal/`, `comments`,
    /// `search`, `related`, `homepage`, `cdn/`. Meant for tests against a mock server.
    ///
    /// # Panics
    ///
    /// When `base` cannot be a base URL (`data:` URLs, for instance).
    #[must_use]
    pub fn with_base(base: &Url) -> Self {
        let join = |path: &str| base.join(path).expect("base URL");
        Self {
            proxy: join("proxy"),
            internal: join("internal/"),
            comments: join("comments"),
            search: join("search"),
            related: join("related"),
            homepage: join("homepage"),
            cdn: join("cdn/"),
        }
    }
}

impl Default for Endpoints {
    fn default() -> Self {
        Self::odysee()
    }
}

/// Status and message of a failed internal API call, kept apart so that sign-in can tell
/// "confirm by email" (409, 417) from other failures.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: Option<StatusCode>,
    pub error: Error,
}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        e.error
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self {
            status: None,
            error,
        }
    }
}

/// The HTTP client and the hosts.
#[derive(Clone)]
pub(crate) struct Api {
    http: reqwest::Client,
    pub endpoints: Arc<Endpoints>,
    next_id: Arc<AtomicU64>,
}

impl Api {
    pub fn new(endpoints: Endpoints) -> Result<Self> {
        Ok(Self {
            http: build_client()?,
            endpoints: Arc::new(endpoints),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    fn rpc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Calls a method of the SDK proxy. `token` is sent only when given: anonymous calls are
    /// cached better by the proxy, so reads leave it out.
    pub async fn proxy<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        token: Option<&str>,
    ) -> Result<T> {
        tracing::debug!(method, "odysee proxy call");
        let mut url = self.endpoints.proxy.clone();
        url.query_pairs_mut().append_pair("m", method);
        let body =
            json!({"jsonrpc": "2.0", "method": method, "params": params, "id": self.rpc_id()});
        let mut request = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json-rpc")
            .body(body.to_string());
        if let Some(token) = token {
            request = request.header("X-Lbry-Auth-Token", token);
        }
        let response = send(request).await?;
        let envelope: RpcEnvelope<T> = read_json(response).await?;
        envelope.into_result(proxy_error)
    }

    /// Calls a method of Commentron.
    pub async fn comments<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        tracing::debug!(method, "commentron call");
        let mut url = self.endpoints.comments.clone();
        url.query_pairs_mut().append_pair("m", method);
        let body =
            json!({"jsonrpc": "2.0", "method": method, "params": params, "id": self.rpc_id()});
        let request = self.http.post(url).json(&body);
        let response = send(request).await?;
        let envelope: RpcEnvelope<T> = read_json(response).await?;
        envelope.into_result(|e| Error::Service(e.message))
    }

    /// Calls the internal API (`resource/action`), as a form. `auth_token` goes first when
    /// given, as the web app sends it.
    pub async fn internal<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, &str)],
        token: Option<&str>,
    ) -> Result<T, ApiError> {
        tracing::debug!(path, "odysee internal api call");
        let url = self
            .endpoints
            .internal
            .join(path)
            .map_err(|e| Error::Parse(e.to_string()))?;
        let mut fields: Vec<(&str, &str)> = Vec::with_capacity(form.len() + 1);
        if let Some(token) = token {
            fields.push(("auth_token", token));
        }
        fields.extend_from_slice(form);
        let request = self.http.post(url).form(&fields);
        let response = request
            .send()
            .await
            .map_err(|e| ApiError::from(network(&e)))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ApiError {
                status: Some(status),
                error: rate_limited(&response),
            });
        }
        let text = response
            .text()
            .await
            .map_err(|e| ApiError::from(network(&e)))?;
        let envelope: InternalEnvelope<T> = match serde_json::from_str(&text) {
            Ok(envelope) => envelope,
            Err(e) if status.is_success() => {
                return Err(Error::Parse(format!("{path}: {e}")).into());
            }
            Err(_) => {
                return Err(ApiError {
                    status: Some(status),
                    error: status_error(status, format!("{path}: HTTP {status}")),
                });
            }
        };
        if status.is_success() && envelope.success {
            // `data: null` is a valid answer for writes; types that cannot be null reject it.
            return match envelope.data {
                Some(data) => Ok(data),
                None => T::deserialize(Value::Null)
                    .map_err(|_| Error::Parse(format!("{path}: no data")).into()),
            };
        }
        let message = envelope
            .error
            .unwrap_or_else(|| format!("{path}: HTTP {status}"));
        Err(ApiError {
            status: Some(status),
            error: status_error(status, message),
        })
    }

    /// GETs a JSON document.
    pub async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        tracing::debug!(host = url.host_str(), path = url.path(), "odysee get");
        let response = send(self.http.get(url)).await?;
        read_json(response).await
    }

    /// The first `len` bytes of a file, and the status: no error mapping, the caller reads it.
    pub async fn head_bytes(&self, url: Url, len: u64) -> Result<(StatusCode, Vec<u8>)> {
        let response = self
            .http
            .get(url)
            .header(header::RANGE, format!("bytes=0-{}", len.saturating_sub(1)))
            .send()
            .await
            .map_err(|e| network(&e))?;
        let status = response.status();
        if !status.is_success() {
            return Ok((status, Vec::new()));
        }
        let bytes = response.bytes().await.map_err(|e| network(&e))?;
        // A server that ignores `Range` sends the whole file: keep only the head.
        let len = usize::try_from(len).unwrap_or(usize::MAX).min(bytes.len());
        Ok((status, bytes[..len].to_vec()))
    }

    /// GETs a small text document; `None` on 404.
    pub async fn get_text(&self, url: Url) -> Result<Option<String>> {
        let response = self.http.get(url).send().await.map_err(|e| network(&e))?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(status_error(status, format!("HTTP {status}")));
        }
        response.text().await.map(Some).map_err(|e| network(&e))
    }
}

fn build_client() -> Result<reqwest::Client> {
    // ring, not aws-lc-rs: aws-lc carries the OpenSSL license, which the AGPL cannot take.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(BuilderVerifierExt::with_platform_verifier)
        .map_err(|e| Error::Network(format!("TLS setup: {e}")))?
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| Error::Network(e.without_url().to_string()))
}

async fn send(request: RequestBuilder) -> Result<Response> {
    let response = request.send().await.map_err(|e| network(&e))?;
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        Err(rate_limited(&response))
    } else {
        Err(status_error(status, format!("HTTP {status}")))
    }
}

async fn read_json<T: DeserializeOwned>(response: Response) -> Result<T> {
    let bytes = response.bytes().await.map_err(|e| network(&e))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Parse(e.to_string()))
}

/// reqwest errors carry the URL; ours never hold a token, but drop it anyway.
fn network(e: &reqwest::Error) -> Error {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    Error::Network(kind.to_owned())
}

fn rate_limited(response: &Response) -> Error {
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    Error::RateLimited { retry_after }
}

fn status_error(status: StatusCode, message: String) -> Error {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::SignInRequired,
        StatusCode::NOT_FOUND => Error::NotFound,
        StatusCode::TOO_MANY_REQUESTS => Error::RateLimited { retry_after: None },
        _ => Error::Service(message),
    }
}

/// The proxy's "authentication token missing" (-32080) and "authentication required" (-32084).
fn proxy_error(e: RpcError) -> Error {
    match e.code {
        -32080 | -32084 => Error::SignInRequired,
        _ => Error::Service(e.message),
    }
}

#[derive(Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

impl<T> RpcEnvelope<T> {
    fn into_result(self, map_error: impl FnOnce(RpcError) -> Error) -> Result<T> {
        match (self.result, self.error) {
            (_, Some(error)) => Err(map_error(error)),
            (Some(result), None) => Ok(result),
            (None, None) => Err(Error::Parse("JSON-RPC response without a result".into())),
        }
    }
}

#[derive(Deserialize)]
struct InternalEnvelope<T> {
    #[serde(default)]
    success: bool,
    error: Option<String>,
    data: Option<T>,
}
