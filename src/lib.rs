// SPDX-License-Identifier: AGPL-3.0-or-later

//! The Odysee connector of Flâneur.
//!
//! [`Odysee`] implements [`Connector`] over the public Odysee backend: the LBRY SDK proxy for
//! claims and the hosted wallet, the internal API for accounts, reactions and history, Lighthouse
//! for search, Recsys for related items, and Commentron for comments. Every request goes from the
//! user's machine to Odysee; there is no Flâneur server in between.
//!
//! What it does not do, on purpose:
//!
//! - **Write the shared preference** (follow, unfollow, watch later). Odysee keeps follows and
//!   collections in one JSON object of the hosted wallet that every Odysee client syncs, and a
//!   write replaces the whole object: a mistake would wipe the user's follows everywhere. Reading
//!   it is safe and supported.
//! - **Play livestreams.** Their HLS only answers requests that carry an `odysee.com` `Referer`,
//!   and sending it would mean presenting Flâneur as the Odysee web app.
//! - **Pay** for paid, rented or members-only content.

mod auth;
mod channels;
mod claim;
mod comments;
mod feeds;
mod http;
mod library;
mod links;
mod player;
mod preference;
mod search;

use std::sync::{Mutex, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use flaneur_connector::{
    AuthMethod, Capabilities, Channels, Comments, Connector, Credentials, Error, Feeds, Library,
    PendingSignIn, Player, Reactions, Result, Search, ServiceInfo, Session, Subscriptions, Support,
    Target,
};
use flaneur_model::{Cursor, Item, ServiceId, Timestamp};
use serde_json::{Value, json};

pub use auth::DefaultChannel;
pub use http::Endpoints;

use auth::SessionState;
use claim::{Claim, ClaimSearch};

/// Stable id of the connector.
pub const SERVICE_ID: &str = "odysee";

/// Most claims `claim_search` returns in one page.
const MAX_PAGE_SIZE: usize = 50;

/// The Odysee connector.
pub struct Odysee {
    api: http::Api,
    info: ServiceInfo,
    language: String,
    session: RwLock<Option<SessionState>>,
    homepage: Mutex<Option<feeds::CachedHomepage>>,
}

/// Options of an [`Odysee`] connector.
#[derive(Debug, Clone)]
pub struct OdyseeBuilder {
    endpoints: Endpoints,
    language: String,
}

impl Default for OdyseeBuilder {
    fn default() -> Self {
        Self {
            endpoints: Endpoints::odysee(),
            language: "en".to_owned(),
        }
    }
}

impl OdyseeBuilder {
    /// Hosts to talk to; the public Odysee backend by default.
    #[must_use]
    pub fn endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Language of the curated homepage and of new users (`en`, `fr`, `es`, `de`, `it`, `zh`,
    /// `ru`, `hi`, `pt-BR`); `en` by default. Homepage languages Odysee does not curate fall back
    /// to `en`.
    #[must_use]
    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    /// Builds the connector.
    pub fn build(self) -> Result<Odysee> {
        Ok(Odysee {
            api: http::Api::new(self.endpoints)?,
            info: ServiceInfo {
                id: ServiceId::new(SERVICE_ID),
                name: "Odysee".to_owned(),
                auth_methods: vec![AuthMethod::Password, AuthMethod::EmailLink],
            },
            language: self.language,
            session: RwLock::new(None),
            homepage: Mutex::new(None),
        })
    }
}

impl Odysee {
    /// A connector to the public Odysee backend, in English.
    pub fn new() -> Result<Self> {
        OdyseeBuilder::default().build()
    }

    /// Options.
    #[must_use]
    pub fn builder() -> OdyseeBuilder {
        OdyseeBuilder::default()
    }

    /// The channel comments are posted with, when the signed-in account has one.
    pub fn default_channel(&self) -> Option<DefaultChannel> {
        self.state().and_then(|s| s.channel)
    }

    fn service(&self) -> &ServiceId {
        &self.info.id
    }

    fn state(&self) -> Option<SessionState> {
        self.session
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_state(&self, state: Option<SessionState>) {
        *self.session.write().unwrap_or_else(PoisonError::into_inner) = state;
    }

    fn token(&self) -> Option<String> {
        self.state().map(|s| s.auth_token)
    }

    fn require_token(&self) -> Result<String> {
        self.token().ok_or(Error::SignInRequired)
    }

    fn now() -> Timestamp {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }

    /// One `claim_search`, always anonymous: the proxy caches those better.
    async fn claim_search(&self, params: Value) -> Result<ClaimSearch> {
        self.api.proxy("claim_search", params, None).await
    }

    /// Claims by full id, in the order asked, skipping the ones that are gone.
    async fn claims_by_ids(&self, ids: &[String]) -> Result<Vec<Claim>> {
        let mut found: Vec<Claim> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_PAGE_SIZE) {
            let page = self
                .claim_search(json!({
                    "claim_ids": chunk,
                    "page_size": chunk.len(),
                    "no_totals": true,
                }))
                .await?;
            found.extend(page.items);
        }
        Ok(ids
            .iter()
            .filter_map(|id| found.iter().position(|c| &c.claim_id == id))
            .map(|i| found[i].clone().resolved())
            .collect())
    }

    /// One claim from a ref id: a full claim id, or an LBRY URL from a shared link.
    async fn lookup(&self, id: &str) -> Result<Claim> {
        if links::is_claim_id(id) {
            return self
                .claims_by_ids(&[id.to_owned()])
                .await?
                .into_iter()
                .next()
                .ok_or(Error::NotFound);
        }
        let url = format!("lbry://{}", id.trim_start_matches("lbry://"));
        let mut resolved: serde_json::Map<String, Value> = self
            .api
            .proxy("resolve", json!({"urls": [url]}), None)
            .await?;
        let value = resolved.remove(&url).ok_or(Error::NotFound)?;
        if value.get("error").is_some() || value.get("claim_id").is_none() {
            return Err(Error::NotFound);
        }
        let claim: Claim =
            serde_json::from_value(value).map_err(|e| Error::Parse(e.to_string()))?;
        Ok(claim.resolved())
    }

    /// The full claim id of a ref id.
    async fn claim_id(&self, id: &str) -> Result<String> {
        if links::is_claim_id(id) {
            Ok(id.to_owned())
        } else {
            Ok(self.lookup(id).await?.claim_id)
        }
    }

    fn items(&self, claims: impl IntoIterator<Item = Claim>) -> Vec<Item> {
        let now = Self::now();
        claims
            .into_iter()
            .filter_map(|c| c.resolved().to_item(self.service(), now))
            .collect()
    }
}

/// Reads a page-number cursor; the first page is 1.
fn page_number(cursor: Option<&Cursor>) -> Result<u32> {
    cursor.map_or(Ok(1), |c| {
        c.0.parse::<u32>()
            .ok()
            .filter(|p| *p >= 1)
            .ok_or_else(|| Error::Parse(format!("invalid cursor {:?}", c.0)))
    })
}

/// Reads an offset cursor; the first page is 0.
fn offset(cursor: Option<&Cursor>) -> Result<usize> {
    cursor.map_or(Ok(0), |c| {
        c.0.parse::<usize>()
            .map_err(|_| Error::Parse(format!("invalid cursor {:?}", c.0)))
    })
}

/// The cursor of the next `claim_search` page, from the totals when the page has them, else
/// from whether the page came back full.
fn next_page(
    page: u32,
    found: usize,
    page_size: usize,
    total_pages: Option<u32>,
) -> Option<Cursor> {
    let more = total_pages.map_or(found >= page_size, |total| page < total);
    more.then(|| Cursor((page + 1).to_string()))
}

#[async_trait]
impl Connector for Odysee {
    fn info(&self) -> &ServiceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            signed_in: self.state().is_some(),
            home: Support::Anonymous,
            subscription_feed: Support::SignedIn,
            trending: Support::Anonymous,
            shorts: Support::Anonymous,
            search: Support::Anonymous,
            channels: Support::Anonymous,
            related: Support::Anonymous,
            history_read: Support::SignedIn,
            // Positions reach Odysee only through its playback analytics (watchman).
            history_write: Support::Unsupported,
            // Published collections, channel playlists included.
            public_playlists: Support::Anonymous,
            // Watch later and favourites, from the shared preference.
            playlists_read: Support::SignedIn,
            // Watch later lives in the shared preference: see the crate docs.
            playlists_write: Support::Unsupported,
            subscriptions_read: Support::SignedIn,
            // Follows live in the shared preference: see the crate docs.
            subscriptions_write: Support::Unsupported,
            reactions_write: Support::SignedIn,
            comments_read: Support::Anonymous,
            comments_write: Support::SignedIn,
            comment_replies_write: Support::SignedIn,
        }
    }

    async fn sign_in(&self, credentials: Credentials) -> Result<Session> {
        let Credentials::Password { login, password } = credentials else {
            return Err(Error::Unsupported);
        };
        let state = auth::sign_in_password(&self.api, &self.language, &login, &password).await?;
        let session = state.to_session(self.service())?;
        self.set_state(Some(state));
        Ok(session)
    }

    async fn restore(&self, session: &Session) -> Result<()> {
        if session.service != *self.service() {
            return Err(Error::SignInRequired);
        }
        let mut state = SessionState::from_secret(&session.secret)?;
        let me = auth::me(&self.api, &state.auth_token).await?;
        if me.has_verified_email != Some(true) {
            return Err(Error::SignInRequired);
        }
        state.user_id = me.id.or(state.user_id);
        state.email = me.primary_email.or(state.email);
        if state.channel.is_none() {
            state.channel = auth::default_channel(&self.api, &state.auth_token).await;
        }
        self.set_state(Some(state));
        Ok(())
    }

    async fn send_sign_in_link(&self, email: &str) -> Result<PendingSignIn> {
        auth::send_link(&self.api, &self.language, email).await
    }

    async fn poll_sign_in_link(&self, pending: &PendingSignIn) -> Result<Option<Session>> {
        let Some(state) = auth::poll_link(&self.api, pending).await? else {
            return Ok(None);
        };
        let session = state.to_session(self.service())?;
        self.set_state(Some(state));
        Ok(Some(session))
    }

    async fn sign_out(&self) {
        self.set_state(None);
    }

    fn parse_url(&self, url: &str) -> Option<Target> {
        links::parse(self.service(), url)
    }

    fn player(&self) -> &dyn Player {
        self
    }

    fn feeds(&self) -> Option<&dyn Feeds> {
        Some(self)
    }

    fn search(&self) -> Option<&dyn Search> {
        Some(self)
    }

    fn channels(&self) -> Option<&dyn Channels> {
        Some(self)
    }

    fn library(&self) -> Option<&dyn Library> {
        Some(self)
    }

    fn subscriptions(&self) -> Option<&dyn Subscriptions> {
        Some(self)
    }

    fn reactions(&self) -> Option<&dyn Reactions> {
        Some(self)
    }

    fn comments(&self) -> Option<&dyn Comments> {
        Some(self)
    }
}
