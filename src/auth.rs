// SPDX-License-Identifier: AGPL-3.0-or-later

//! Odysee accounts.
//!
//! Every Odysee session is an `auth_token` of the internal API. A client first mints an anonymous
//! user (`user/new`, with a random `app_id` of its own), then upgrades that same token to the
//! account: by password (`user/signin`), or by an email link the user opens in any browser while
//! the client polls `user/me`. No new token is issued on success, so the token minted at the start
//! is the session.

use std::fmt;

use flaneur_connector::{Account, Error, PendingSignIn, Result, Session};
use flaneur_model::ServiceId;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::http::{Api, ApiError};
use crate::preference::SharedPreference;

const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const APP_ID_LEN: usize = 66;
/// Version of the session secret's JSON format.
const SECRET_VERSION: u32 = 1;

/// The channel the account comments and reacts with: the one picked on odysee.com
/// (`active_channel_claim`), else the one with the most credits staked, as the web app does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultChannel {
    /// Claim id of the channel.
    pub claim_id: String,
    /// Name, with its `@`.
    pub name: String,
}

/// Everything a signed-in session holds. Serialized as the [`Session::secret`].
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SessionState {
    #[serde(rename = "v")]
    pub version: u32,
    /// Token of the internal API; also the proxy's `X-Lbry-Auth-Token`.
    pub auth_token: String,
    /// The installation id `user/new` was called with.
    pub app_id: String,
    #[serde(default)]
    pub user_id: Option<u64>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub channel: Option<DefaultChannel>,
}

impl fmt::Debug for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionState")
            .field("auth_token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

impl SessionState {
    pub fn to_session(&self, service: &ServiceId) -> Result<Session> {
        let name = self
            .channel
            .as_ref()
            .map(|c| c.name.clone())
            .or_else(|| self.email.clone())
            .unwrap_or_else(|| "Odysee".to_owned());
        Ok(Session {
            service: service.clone(),
            account: Some(Account {
                name,
                handle: self.email.clone(),
                avatars: Vec::new(),
            }),
            secret: serde_json::to_string(self).map_err(|e| Error::Parse(e.to_string()))?,
        })
    }

    pub fn from_secret(secret: &str) -> Result<Self> {
        let state: Self = serde_json::from_str(secret)
            .map_err(|_| Error::Parse("unreadable Odysee session".into()))?;
        if state.version != SECRET_VERSION || state.auth_token.is_empty() {
            return Err(Error::SignInRequired);
        }
        Ok(state)
    }
}

/// A random installation id: 66 base58 characters, as `user/new` requires.
pub(crate) fn new_app_id() -> Result<String> {
    let mut id = String::with_capacity(APP_ID_LEN);
    let mut buf = [0u8; 128];
    while id.len() < APP_ID_LEN {
        getrandom::getrandom(&mut buf)
            .map_err(|e| Error::Service(format!("no randomness: {e}")))?;
        // 58 * 4 = 232: rejecting bytes from 232 up keeps the draw uniform.
        for b in buf.iter().filter(|b| **b < 232) {
            if id.len() == APP_ID_LEN {
                break;
            }
            id.push(char::from(BASE58[usize::from(*b % 58)]));
        }
    }
    Ok(id)
}

#[derive(Deserialize)]
struct NewUser {
    auth_token: String,
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct Me {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub primary_email: Option<String>,
    #[serde(default)]
    pub has_verified_email: Option<bool>,
}

#[derive(Deserialize)]
struct ChannelList {
    #[serde(default)]
    items: Vec<OwnChannel>,
}

#[derive(Deserialize)]
struct OwnChannel {
    claim_id: String,
    name: String,
    #[serde(default)]
    meta: OwnChannelMeta,
}

#[derive(Default, Deserialize)]
struct OwnChannelMeta {
    #[serde(default)]
    effective_amount: Option<String>,
}

/// Mints an anonymous user: the token every later step upgrades.
pub(crate) async fn new_user(api: &Api, language: &str) -> Result<SessionState> {
    let app_id = new_app_id()?;
    let user: NewUser = api
        .internal(
            "user/new",
            &[
                ("auth_token", ""),
                ("language", language),
                ("app_id", &app_id),
            ],
            None,
        )
        .await?;
    Ok(SessionState {
        version: SECRET_VERSION,
        auth_token: user.auth_token,
        app_id,
        user_id: user.id,
        email: None,
        channel: None,
    })
}

pub(crate) async fn me(api: &Api, token: &str) -> Result<Me> {
    Ok(api.internal("user/me", &[], Some(token)).await?)
}

/// Password sign-in on a fresh anonymous token.
pub(crate) async fn sign_in_password(
    api: &Api,
    language: &str,
    email: &str,
    password: &str,
) -> Result<SessionState> {
    let mut state = new_user(api, language).await?;
    let signed: Result<serde_json::Value, ApiError> = api
        .internal(
            "user/signin",
            &[("email", email), ("password", password)],
            Some(&state.auth_token),
        )
        .await;
    match signed {
        Ok(_) => {}
        // 409: the email is not verified; 417: Odysee wants this sign-in confirmed by email.
        Err(ApiError {
            status: Some(StatusCode::CONFLICT | StatusCode::EXPECTATION_FAILED),
            ..
        }) => {
            return Err(Error::Unavailable(
                "Odysee asks to confirm this sign-in by email: use the email link sign-in".into(),
            ));
        }
        Err(ApiError {
            status: Some(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
            ..
        }) => return Err(Error::Service("wrong email or password".into())),
        Err(e) => return Err(e.into()),
    }
    complete(api, &mut state).await?;
    Ok(state)
}

/// Sends a sign-in link to `email`, on a fresh anonymous token.
pub(crate) async fn send_link(api: &Api, language: &str, email: &str) -> Result<PendingSignIn> {
    let state = new_user(api, language).await?;
    let exists: Result<serde_json::Value, ApiError> = api
        .internal("user/exists", &[("email", email)], Some(&state.auth_token))
        .await;
    match exists {
        // 412: the account exists without a password, which is fine for a link.
        Ok(_)
        | Err(ApiError {
            status: Some(StatusCode::PRECONDITION_FAILED),
            ..
        }) => {}
        Err(ApiError {
            status: Some(StatusCode::NOT_FOUND),
            ..
        }) => {
            return Err(Error::Unavailable(
                "no Odysee account uses this email: create one on odysee.com".into(),
            ));
        }
        Err(e) => return Err(e.into()),
    }
    let _: serde_json::Value = api
        .internal(
            "user_email/resend_token",
            &[("email", email), ("only_if_expired", "true")],
            Some(&state.auth_token),
        )
        .await?;
    Ok(PendingSignIn {
        // The not-yet-verified session, in the same format as a session secret.
        state: serde_json::to_string(&state).map_err(|e| Error::Parse(e.to_string()))?,
        message: Some("Open the link Odysee sent to your email, in any browser.".to_owned()),
    })
}

/// Checks whether the link was opened; `None` while it was not.
pub(crate) async fn poll_link(api: &Api, pending: &PendingSignIn) -> Result<Option<SessionState>> {
    let mut state = SessionState::from_secret(&pending.state)?;
    let me = me(api, &state.auth_token).await?;
    if me.has_verified_email != Some(true) {
        return Ok(None);
    }
    complete(api, &mut state).await?;
    Ok(Some(state))
}

/// Fills the account details once the token belongs to a verified account.
pub(crate) async fn complete(api: &Api, state: &mut SessionState) -> Result<()> {
    let me = me(api, &state.auth_token).await?;
    if me.has_verified_email != Some(true) {
        return Err(Error::Unavailable(
            "the Odysee account has no verified email yet".into(),
        ));
    }
    state.user_id = me.id.or(state.user_id);
    state.email = me.primary_email;
    state.channel = default_channel(api, &state.auth_token).await;
    Ok(())
}

/// The account's default channel, `None` when it has none or the wallet is unreachable.
pub(crate) async fn default_channel(api: &Api, token: &str) -> Option<DefaultChannel> {
    let list = api.proxy::<ChannelList>(
        "channel_list",
        json!({"page": 1, "page_size": 50, "resolve": false}),
        Some(token),
    );
    let preference = SharedPreference::fetch(api, token);
    let (list, preference) = tokio::join!(list, preference);
    let mut channels = list.ok()?.items;
    let active = preference
        .ok()
        .flatten()
        .and_then(|p| p.active_channel_claim());
    if let Some(active) = active
        && let Some(pos) = channels.iter().position(|c| c.claim_id == active)
    {
        let c = channels.swap_remove(pos);
        return Some(DefaultChannel {
            claim_id: c.claim_id,
            name: c.name,
        });
    }
    let staked = |c: &OwnChannel| {
        c.meta
            .effective_amount
            .as_deref()
            .and_then(|a| a.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    channels
        .into_iter()
        .max_by(|a, b| staked(a).total_cmp(&staked(b)))
        .map(|c| DefaultChannel {
            claim_id: c.claim_id,
            name: c.name,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_66_base58_characters() {
        let a = new_app_id().unwrap();
        let b = new_app_id().unwrap();
        assert_eq!(a.len(), APP_ID_LEN);
        assert!(a.bytes().all(|c| BASE58.contains(&c)));
        assert_ne!(a, b);
    }

    #[test]
    fn secret_round_trips_and_never_shows_in_debug() {
        let state = SessionState {
            version: SECRET_VERSION,
            auth_token: "tok-very-secret".into(),
            app_id: "app".into(),
            user_id: Some(7),
            email: Some("me@example.org".into()),
            channel: Some(DefaultChannel {
                claim_id: "c".repeat(40),
                name: "@me".into(),
            }),
        };
        let session = state.to_session(&ServiceId::new("odysee")).unwrap();
        assert!(!format!("{session:?}").contains("very-secret"));
        assert!(!format!("{state:?}").contains("very-secret"));
        assert_eq!(session.account.as_ref().unwrap().name, "@me");
        let back = SessionState::from_secret(&session.secret).unwrap();
        assert_eq!(back.auth_token, "tok-very-secret");
        assert_eq!(back.channel, state.channel);
    }

    #[test]
    fn rejects_unknown_secrets() {
        assert!(matches!(
            SessionState::from_secret("{}"),
            Err(Error::Parse(_))
        ));
        assert!(matches!(
            SessionState::from_secret(r#"{"v":99,"auth_token":"x","app_id":"y"}"#),
            Err(Error::SignInRequired)
        ));
    }
}
