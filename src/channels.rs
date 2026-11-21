// SPDX-License-Identifier: AGPL-3.0-or-later

//! Channel pages: the channel claim, its streams by kind, its collections.

use async_trait::async_trait;
use flaneur_connector::{ChannelTab, Channels, Error, Result};
use flaneur_model::{Channel, ChannelRef, Cursor, Item, Page, Playlist};
use serde_json::{Value, json};

use crate::claim::{SHORTS_MAX_ASPECT_RATIO, SHORTS_MAX_DURATION};
use crate::preference::SharedPreference;
use crate::{Odysee, next_page, page_number};

const CHANNEL_PAGE_SIZE: usize = 30;

/// `claim_search` parameters of a tab, without the page.
pub(crate) fn tab_params(channel_id: &str, tab: ChannelTab) -> Value {
    let mut params = json!({
        "channel_ids": [channel_id],
        "claim_type": ["stream"],
        "order_by": ["release_time"],
        "page_size": CHANNEL_PAGE_SIZE,
    });
    match tab {
        ChannelTab::Videos => {
            params["stream_types"] = json!(["video"]);
            params["has_source"] = json!(true);
            params["exclude_shorts"] = json!(true);
        }
        ChannelTab::Shorts => {
            params["stream_types"] = json!(["video"]);
            params["has_source"] = json!(true);
            params["duration"] = json!(format!("<={SHORTS_MAX_DURATION}"));
            params["content_aspect_ratio"] = json!(format!("<={SHORTS_MAX_ASPECT_RATIO}"));
        }
        ChannelTab::Lives => {
            params["has_no_source"] = json!(true);
        }
    }
    params
}

#[async_trait]
impl Channels for Odysee {
    async fn channel(&self, channel: &ChannelRef) -> Result<Channel> {
        let claim = self.lookup(&channel.id).await?;
        if !claim.is_channel() {
            return Err(Error::NotFound);
        }
        let mut page = claim.to_channel(self.service());
        if let Some(token) = self.token() {
            // Both need some auth token; neither is worth failing the page for.
            let form = [("claim_id", claim.claim_id.as_str())];
            let count =
                self.api
                    .internal::<Vec<u64>>("subscription/sub_count", &form, Some(&token));
            let preference = SharedPreference::fetch(&self.api, &token);
            let (count, preference) = tokio::join!(count, preference);
            page.subscribers = count.ok().and_then(|c| c.first().copied());
            page.subscribed = preference
                .ok()
                .map(|p| p.is_some_and(|p| p.followed_channel_ids().contains(&claim.claim_id)));
        }
        Ok(page)
    }

    async fn items(
        &self,
        channel: &ChannelRef,
        tab: ChannelTab,
        cursor: Option<&Cursor>,
    ) -> Result<Page<Item>> {
        let channel_id = self.claim_id(&channel.id).await?;
        let mut params = tab_params(&channel_id, tab);
        let page = page_number(cursor)?;
        params["page"] = json!(page);
        let found = self.claim_search(params).await?;
        let count = found.items.len();
        Ok(Page {
            items: self.items(found.items),
            next: next_page(page, count, CHANNEL_PAGE_SIZE, found.total_pages),
            total: found.total_items,
        })
    }

    async fn playlists(
        &self,
        channel: &ChannelRef,
        cursor: Option<&Cursor>,
    ) -> Result<Page<Playlist>> {
        let channel_id = self.claim_id(&channel.id).await?;
        let page = page_number(cursor)?;
        let found = self
            .claim_search(json!({
                "channel_ids": [channel_id],
                "claim_type": ["collection"],
                "order_by": ["height"],
                "page": page,
                "page_size": CHANNEL_PAGE_SIZE,
            }))
            .await?;
        let count = found.items.len();
        Ok(Page {
            items: found
                .items
                .iter()
                .filter(|c| c.is_collection())
                .map(|c| c.to_playlist(self.service()))
                .collect(),
            next: next_page(page, count, CHANNEL_PAGE_SIZE, found.total_pages),
            total: found.total_items,
        })
    }
}
