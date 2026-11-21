// SPDX-License-Identifier: AGPL-3.0-or-later

//! The signed-in user's own things: history, watch later and favourites, follows, reactions.
//!
//! Follows and collections come from the shared preference, read-only (see
//! [`crate::preference`]); history and reactions from the internal API.

use std::collections::HashMap;

use async_trait::async_trait;
use flaneur_connector::{Error, Library, PlaybackReport, Reactions, Result, Subscriptions};
use flaneur_model::{
    ChannelRef, ChannelSummary, Cursor, Item, ItemRef, Page, Playlist, PlaylistRef, Progress,
    Reaction, Timestamp, WatchState,
};
use serde::Deserialize;

use crate::links::{claim_id_from_uri, is_claim_id};
use crate::preference::{FAVORITES, SharedPreference, WATCH_LATER};
use crate::{MAX_PAGE_SIZE, Odysee, offset, page_number};

const HISTORY_PAGE_SIZE: u32 = 30;
const PLAYLIST_PAGE_SIZE: usize = 30;

#[derive(Debug, Deserialize)]
pub(crate) struct ViewHistory {
    #[serde(default)]
    pub items: Vec<ViewHistoryItem>,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ViewHistoryItem {
    pub claim_id: String,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction]Z` to Unix seconds; the internal API's timestamps are UTC.
pub(crate) fn parse_utc(s: &str) -> Option<Timestamp> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

fn builtin_title(id: &str) -> &'static str {
    if id == WATCH_LATER {
        "Watch Later"
    } else {
        "Favorites"
    }
}

impl Odysee {
    async fn preference(&self) -> Result<SharedPreference> {
        let token = self.require_token()?;
        Ok(SharedPreference::fetch(&self.api, &token)
            .await?
            .unwrap_or_default())
    }

    /// Resume positions, in seconds, by claim id.
    async fn last_positions(&self, token: &str, claim_ids: &[String]) -> HashMap<String, f64> {
        if claim_ids.is_empty() {
            return HashMap::new();
        }
        self.api
            .internal(
                "file/last_positions",
                &[("claim_ids", &claim_ids.join(","))],
                Some(token),
            )
            .await
            .unwrap_or_default()
    }

    /// One page of items from a list of claim ids.
    async fn items_page(&self, ids: &[String], cursor: Option<&Cursor>) -> Result<Page<Item>> {
        let from = offset(cursor)?;
        let end = (from + PLAYLIST_PAGE_SIZE).min(ids.len());
        let slice = ids.get(from..end).unwrap_or_default();
        let claims = self.claims_by_ids(slice).await?;
        Ok(Page {
            items: self.items(claims),
            next: (end < ids.len()).then(|| Cursor(end.to_string())),
            total: Some(ids.len() as u64),
        })
    }
}

#[async_trait]
impl Library for Odysee {
    async fn history(&self, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        let token = self.require_token()?;
        let page = page_number(cursor)?;
        let history: ViewHistory = self
            .api
            .internal(
                "user/view_history",
                &[
                    ("page", &page.to_string()),
                    ("page_size", &HISTORY_PAGE_SIZE.to_string()),
                ],
                Some(&token),
            )
            .await?;
        let ids: Vec<String> = history.items.iter().map(|i| i.claim_id.clone()).collect();
        let (claims, positions) =
            tokio::join!(self.claims_by_ids(&ids), self.last_positions(&token, &ids));
        let viewed: HashMap<&str, Option<Timestamp>> = history
            .items
            .iter()
            .map(|i| {
                (
                    i.claim_id.as_str(),
                    i.updated_at.as_deref().and_then(parse_utc),
                )
            })
            .collect();
        let now = Self::now();
        let items = self
            .items(claims?)
            .into_iter()
            .map(|mut item| {
                let id = item.r#ref.id.as_str();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let position = positions
                    .get(id)
                    .filter(|p| p.is_finite() && **p > 0.0)
                    .map(|p| *p as u32);
                item.watch = Some(WatchState {
                    progress: position.map(|position| Progress {
                        position,
                        duration: item.duration,
                        updated_at: viewed.get(id).copied().flatten().unwrap_or(now),
                    }),
                    watched: false,
                    reaction: None,
                });
                item
            })
            .collect();
        Ok(Page {
            items,
            next: history.has_more.then(|| Cursor((page + 1).to_string())),
            total: None,
        })
    }

    async fn report_playback(&self, _report: &PlaybackReport) -> Result<()> {
        Err(Error::Unsupported)
    }

    async fn playlists(&self, _cursor: Option<&Cursor>) -> Result<Page<Playlist>> {
        let preference = self.preference().await?;
        let playlists = [WATCH_LATER, FAVORITES]
            .into_iter()
            .map(|id| {
                let builtin = preference.builtin(id);
                Playlist {
                    r#ref: PlaylistRef::new(self.service().clone(), id),
                    title: builtin
                        .and_then(|b| b.name.clone())
                        .unwrap_or_else(|| builtin_title(id).to_owned()),
                    channel: None,
                    item_count: Some(builtin.map_or(0, |b| b.items.len() as u64)),
                    thumbnails: Vec::new(),
                }
            })
            .collect();
        Ok(Page::last(playlists))
    }

    fn watch_later(&self) -> Option<PlaylistRef> {
        Some(PlaylistRef::new(self.service().clone(), WATCH_LATER))
    }

    async fn playlist_items(
        &self,
        playlist: &PlaylistRef,
        cursor: Option<&Cursor>,
    ) -> Result<Page<Item>> {
        let ids: Vec<String> = if playlist.id == WATCH_LATER || playlist.id == FAVORITES {
            let preference = self.preference().await?;
            preference.builtin(&playlist.id).map_or_default(|b| {
                b.items
                    .iter()
                    .filter_map(|uri| claim_id_from_uri(uri))
                    .map(str::to_owned)
                    .collect()
            })
        } else if is_claim_id(&playlist.id) {
            // A published collection: anyone can read it.
            let claim = self.lookup(&playlist.id).await?;
            if !claim.is_collection() {
                return Err(Error::NotFound);
            }
            claim.value.claims
        } else {
            return Err(Error::NotFound);
        };
        self.items_page(&ids, cursor).await
    }

    async fn add_to_playlist(&self, _playlist: &PlaylistRef, _item: &ItemRef) -> Result<()> {
        Err(Error::Unsupported)
    }

    async fn remove_from_playlist(&self, _playlist: &PlaylistRef, _item: &ItemRef) -> Result<()> {
        Err(Error::Unsupported)
    }
}

#[async_trait]
impl Subscriptions for Odysee {
    async fn list(&self, cursor: Option<&Cursor>) -> Result<Page<ChannelSummary>> {
        let ids = self.preference().await?.followed_channel_ids();
        let from = offset(cursor)?;
        let end = (from + MAX_PAGE_SIZE).min(ids.len());
        let slice = ids.get(from..end).unwrap_or_default();
        let claims = self.claims_by_ids(slice).await?;
        Ok(Page {
            items: claims
                .iter()
                .filter(|c| c.is_channel())
                .map(|c| c.channel_summary(self.service()))
                .collect(),
            next: (end < ids.len()).then(|| Cursor(end.to_string())),
            total: Some(ids.len() as u64),
        })
    }

    async fn set_subscribed(&self, _channel: &ChannelRef, _subscribed: bool) -> Result<()> {
        // Follows live in the shared preference, which this connector never writes.
        Err(Error::Unsupported)
    }
}

/// Form fields of `reaction/react`, as the web app sends them.
pub(crate) fn react_form(
    claim_id: &str,
    kind: &'static str,
    remove: bool,
) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("claim_ids", claim_id.to_owned()),
        ("type", kind.to_owned()),
    ];
    if remove {
        form.push(("remove", "true".to_owned()));
    } else {
        let other = if kind == "like" { "dislike" } else { "like" };
        form.push(("clear_types", other.to_owned()));
    }
    form
}

#[async_trait]
impl Reactions for Odysee {
    async fn react(&self, item: &ItemRef, reaction: Reaction) -> Result<()> {
        let token = self.require_token()?;
        let claim_id = self.claim_id(&item.id).await?;
        let form = match reaction {
            Reaction::Like => react_form(&claim_id, "like", false),
            Reaction::Dislike => react_form(&claim_id, "dislike", false),
            Reaction::None => {
                // `remove` takes one type: find out which one is set.
                let (_, mine) = self
                    .reaction_list(&claim_id, Some(&token))
                    .await?
                    .for_claim(&claim_id);
                match mine {
                    Some(Reaction::Like) => react_form(&claim_id, "like", true),
                    Some(Reaction::Dislike) => react_form(&claim_id, "dislike", true),
                    _ => return Ok(()),
                }
            }
        };
        let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let _: serde_json::Value = self
            .api
            .internal("reaction/react", &form, Some(&token))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_internal_api_timestamps() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-11-21T00:34:01Z"), Some(1_795_221_241));
        assert_eq!(parse_utc("2026-11-21T00:34:01.123Z"), Some(1_795_221_241));
        assert_eq!(parse_utc("2026-11-21 00:34:01"), None);
    }
}
