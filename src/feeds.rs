// SPDX-License-Identifier: AGPL-3.0-or-later

//! Home (Odysee's curated categories), trending, shorts, and the feed of the followed channels.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use flaneur_connector::{Error, FeedKind, Feeds, Result};
use flaneur_model::{Cursor, Item, Page};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::claim::{MATURE_TAGS, SHORTS_MAX_ASPECT_RATIO, SHORTS_MAX_DURATION};
use crate::preference::SharedPreference;
use crate::{Odysee, next_page, page_number};

/// The curated categories change a few times a day at most.
const HOMEPAGE_TTL: Duration = Duration::from_secs(3600);
const FEED_PAGE_SIZE: usize = 30;
const DAY: i64 = 86_400;

/// `ui/constants/claim_search.ts`.
const ORDER_TRENDING: &[&str] = &["trending_group", "trending_mixed"];
const ORDER_TOP: &[&str] = &["effective_amount"];
const ORDER_NEW: &[&str] = &["release_time"];

/// One curated category of the homepage (`custom/homepages/v2` of odysee-frontend).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Category {
    #[serde(default)]
    pub channel_ids: Vec<String>,
    /// A number as a string, or `auto`.
    #[serde(default)]
    pub channel_limit: Option<String>,
    #[serde(default)]
    pub days_of_content: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub claim_type: Option<Value>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, rename = "exclude_shorts")]
    pub exclude_shorts: Option<bool>,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i64>,
    #[serde(default)]
    pub hide_by_default: Option<bool>,
}

impl Category {
    fn claim_search(&self, now: i64) -> Value {
        let order: &[&str] = match self.order.as_deref() {
            Some("top") => ORDER_TOP,
            Some("new") => ORDER_NEW,
            _ => ORDER_TRENDING,
        };
        let reposts = self.claim_type.as_ref().is_some_and(|t| {
            t.as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("repost")))
        });
        let claim_type: &[&str] = if reposts {
            &["stream", "repost"]
        } else {
            &["stream"]
        };
        let days = i64::from(self.days_of_content.unwrap_or(30));
        // Hour boundary, as the web app does: the proxy caches identical queries.
        let since = (now - days * DAY) / 3600 * 3600;
        let mut params = json!({
            "page": 1,
            "page_size": self.page_size.unwrap_or(12),
            "claim_type": claim_type,
            "stream_types": ["video"],
            "has_source": true,
            "channel_ids": self.channel_ids,
            "order_by": order,
            "release_time": format!(">{since}"),
            "not_tags": MATURE_TAGS,
            "no_totals": true,
            "remove_duplicates": true,
        });
        if let Some(limit) = self.limit_per_channel() {
            params["limit_claims_per_channel"] = json!(limit);
        }
        if self.exclude_shorts == Some(true) {
            params["exclude_shorts"] = json!(true);
        }
        if let Some(duration) = &self.duration {
            params["duration"] = json!(duration);
        }
        if !self.tags.is_empty() {
            params["any_tags"] = json!(self.tags);
        }
        params
    }

    /// `ui/util/buildHomepage.ts#getLimitPerChannel`.
    fn limit_per_channel(&self) -> Option<u32> {
        match self.channel_limit.as_deref()? {
            "auto" => Some(match self.channel_ids.len() {
                n if n < 150 => 3,
                n if n < 250 => 2,
                _ => 1,
            }),
            n => n.parse().ok().filter(|n| *n > 0),
        }
    }
}

pub(crate) struct CachedHomepage {
    fetched: Instant,
    categories: Arc<Vec<Category>>,
}

#[derive(Deserialize)]
struct HomepageEnvelope {
    #[serde(default)]
    data: BTreeMap<String, Option<Homepage>>,
}

#[derive(Deserialize)]
struct Homepage {
    #[serde(default)]
    categories: BTreeMap<String, Category>,
}

/// The categories shown on the homepage, in their order.
pub(crate) fn homepage_categories(envelope: Value, language: &str) -> Result<Vec<Category>> {
    let envelope: HomepageEnvelope =
        serde_json::from_value(envelope).map_err(|e| Error::Parse(e.to_string()))?;
    let homepage = envelope
        .data
        .into_iter()
        .find(|(lang, page)| lang == language && page.is_some())
        .and_then(|(_, page)| page)
        .ok_or(Error::NotFound)?;
    let mut categories: Vec<Category> = homepage
        .categories
        .into_values()
        .filter(|c| !c.channel_ids.is_empty() && c.hide_by_default != Some(true))
        .collect();
    categories.sort_by_key(|c| c.sort_order.unwrap_or(i64::MAX));
    Ok(categories)
}

impl Odysee {
    async fn categories(&self) -> Result<Arc<Vec<Category>>> {
        {
            let cache = self.homepage.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = cache.as_ref()
                && cached.fetched.elapsed() < HOMEPAGE_TTL
            {
                return Ok(cached.categories.clone());
            }
        }
        let categories = match self.fetch_categories(&self.language).await {
            Err(Error::NotFound) if self.language != "en" => self.fetch_categories("en").await,
            other => other,
        }?;
        let categories = Arc::new(categories);
        *self.homepage.lock().unwrap_or_else(PoisonError::into_inner) = Some(CachedHomepage {
            fetched: Instant::now(),
            categories: categories.clone(),
        });
        Ok(categories)
    }

    async fn fetch_categories(&self, language: &str) -> Result<Vec<Category>> {
        let mut url = self.api.endpoints.homepage.clone();
        url.query_pairs_mut().append_pair("hp", language);
        homepage_categories(self.api.get_json(url).await?, language)
    }

    /// One category per page, in the homepage's order.
    async fn home(&self, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        let categories = self.categories().await?;
        let index = cursor.map_or(Ok(0), |c| {
            c.0.parse::<usize>()
                .map_err(|_| Error::Parse(format!("invalid cursor {:?}", c.0)))
        })?;
        let Some(category) = categories.get(index) else {
            return Ok(Page::last(Vec::new()));
        };
        let page = self
            .claim_search(category.claim_search(Self::now()))
            .await?;
        Ok(Page {
            items: self.items(page.items),
            next: (index + 1 < categories.len()).then(|| Cursor((index + 1).to_string())),
            total: None,
        })
    }

    async fn discover(&self, mut params: Value, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        let page = page_number(cursor)?;
        params["page"] = json!(page);
        params["page_size"] = json!(FEED_PAGE_SIZE);
        let found = self.claim_search(params).await?;
        let count = found.items.len();
        Ok(Page {
            items: self.items(found.items),
            next: next_page(page, count, FEED_PAGE_SIZE, found.total_pages),
            total: found.total_items,
        })
    }

    async fn trending(&self, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        self.discover(
            json!({
                "claim_type": ["stream"],
                "stream_types": ["video"],
                "has_source": true,
                "order_by": ORDER_TRENDING,
                "not_tags": MATURE_TAGS,
                "exclude_shorts": true,
                "no_totals": true,
                "remove_duplicates": true,
            }),
            cursor,
        )
        .await
    }

    /// Trending vertical videos of the last month, with the web app's shorts rule.
    async fn shorts(&self, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        let since = (Self::now() - 30 * DAY) / 3600 * 3600;
        self.discover(
            json!({
                "claim_type": ["stream"],
                "stream_types": ["video"],
                "has_source": true,
                "duration": format!("<={SHORTS_MAX_DURATION}"),
                "content_aspect_ratio": format!("<={SHORTS_MAX_ASPECT_RATIO}"),
                "order_by": ORDER_TRENDING,
                "release_time": format!(">{since}"),
                "not_tags": MATURE_TAGS,
                "limit_claims_per_channel": 2,
                "no_totals": true,
                "remove_duplicates": true,
            }),
            cursor,
        )
        .await
    }

    /// Latest items of the followed channels, from the shared preference.
    async fn subscription_feed(&self, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        let token = self.require_token()?;
        let preference = SharedPreference::fetch(&self.api, &token).await?;
        let ids = preference.map_or_default(|p| p.followed_channel_ids());
        if ids.is_empty() {
            return Ok(Page::last(Vec::new()));
        }
        self.discover(
            json!({
                "channel_ids": ids,
                "claim_type": ["stream", "repost"],
                "stream_types": ["video"],
                "has_source": true,
                "order_by": ORDER_NEW,
                "release_time": format!("<{}", Self::now()),
                "no_totals": true,
            }),
            cursor,
        )
        .await
    }
}

#[async_trait]
impl Feeds for Odysee {
    async fn feed(&self, kind: FeedKind, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        match kind {
            FeedKind::Home => self.home(cursor).await,
            FeedKind::Trending => self.trending(cursor).await,
            FeedKind::Shorts => self.shorts(cursor).await,
            FeedKind::Subscriptions => self.subscription_feed(cursor).await,
        }
    }
}
