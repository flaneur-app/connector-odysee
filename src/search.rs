// SPDX-License-Identifier: AGPL-3.0-or-later

//! Search: Lighthouse finds claim ids, `claim_search` hydrates them.

use async_trait::async_trait;
use flaneur_connector::{Result, Search, SearchFilters, SearchResult};
use flaneur_model::{Cursor, ItemKind, Page, Timestamp};
use serde::Deserialize;

use crate::claim::{SHORTS_MAX_ASPECT_RATIO, SHORTS_MAX_DURATION};
use crate::{Odysee, offset};

const SEARCH_PAGE_SIZE: usize = 20;

/// One Lighthouse hit: ids only.
#[derive(Debug, Deserialize)]
pub(crate) struct Hit {
    #[serde(rename = "claimId")]
    pub claim_id: String,
}

/// Lighthouse's `time_filter`: the narrowest window that still contains `after`.
fn time_filter(after: Timestamp, now: Timestamp) -> Option<&'static str> {
    let age = now - after;
    [
        (3_600, "lasthour"),
        (86_400, "today"),
        (7 * 86_400, "thisweek"),
        (31 * 86_400, "thismonth"),
        (366 * 86_400, "thisyear"),
    ]
    .into_iter()
    .find(|(window, _)| age <= *window)
    .map(|(_, name)| name)
}

/// Query parameters of a Lighthouse search.
pub(crate) fn search_params(
    query: &str,
    filters: &SearchFilters,
    from: usize,
    now: Timestamp,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("s", query.to_owned()),
        ("size", SEARCH_PAGE_SIZE.to_string()),
        ("from", from.to_string()),
        ("nsfw", (filters.safe == Some(false)).to_string()),
        // Paid content cannot be played from Flâneur.
        ("free_only", "true".to_owned()),
    ];
    match filters.kind {
        // Without a kind, channels come too; `mediaType` would drop them.
        None => params.push(("claimType", "file,channel".to_owned())),
        Some(kind) => {
            params.push(("claimType", "file".to_owned()));
            params.push(("mediaType", "video".to_owned()));
            match kind {
                ItemKind::Short => {
                    params.push(("max_duration", SHORTS_MAX_DURATION.to_string()));
                }
                ItemKind::Video => {
                    params.push(("exclude_shorts", "true".to_owned()));
                    params.push((
                        "exclude_shorts_aspect_ratio_lte",
                        SHORTS_MAX_ASPECT_RATIO.to_string(),
                    ));
                    params.push((
                        "exclude_shorts_duration_lte",
                        SHORTS_MAX_DURATION.to_string(),
                    ));
                }
                ItemKind::Live | ItemKind::Premiere | ItemKind::Upcoming => {}
            }
        }
    }
    if let Some(window) = filters.published_after.and_then(|t| time_filter(t, now)) {
        params.push(("time_filter", window.to_owned()));
    }
    params
}

impl Odysee {
    /// Lighthouse hits, then their claims, in Lighthouse's order.
    pub(crate) async fn search_hits(
        &self,
        url: url::Url,
        params: &[(&str, String)],
    ) -> Result<(usize, Vec<crate::claim::Claim>)> {
        let mut url = url;
        url.query_pairs_mut().extend_pairs(params);
        let hits: Vec<Hit> = self.api.get_json(url).await?;
        let ids: Vec<String> = hits.into_iter().map(|h| h.claim_id).collect();
        let count = ids.len();
        if ids.is_empty() {
            return Ok((0, Vec::new()));
        }
        Ok((count, self.claims_by_ids(&ids).await?))
    }
}

#[async_trait]
impl Search for Odysee {
    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SearchResult>> {
        let from = offset(cursor)?;
        let now = Self::now();
        let params = search_params(query, filters, from, now);
        let (hits, claims) = self
            .search_hits(self.api.endpoints.search.clone(), &params)
            .await?;
        let service = self.service();
        let results = claims
            .into_iter()
            .filter_map(|claim| {
                if claim.is_channel() {
                    Some(SearchResult::Channel(claim.channel_summary(service)))
                } else if claim.is_collection() {
                    Some(SearchResult::Playlist(claim.to_playlist(service)))
                } else {
                    let item = claim.to_item(service, now)?;
                    let kind_ok = filters.kind.is_none_or(|k| k == item.kind);
                    let date_ok = filters
                        .published_after
                        .is_none_or(|after| item.published.is_some_and(|p| p.at >= after));
                    (kind_ok && date_ok).then(|| SearchResult::Item(Box::new(item)))
                }
            })
            .collect();
        Ok(Page {
            items: results,
            next: (hits >= SEARCH_PAGE_SIZE).then(|| Cursor((from + hits).to_string())),
            total: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_narrowest_time_filter() {
        let now = 1_000_000_000;
        assert_eq!(time_filter(now - 60, now), Some("lasthour"));
        assert_eq!(time_filter(now - 3 * 86_400, now), Some("thisweek"));
        assert_eq!(time_filter(now - 400 * 86_400, now), None);
    }

    #[test]
    fn short_searches_ask_for_files_only() {
        let filters = SearchFilters {
            kind: Some(ItemKind::Short),
            ..SearchFilters::default()
        };
        let params = search_params("cats", &filters, 20, 0);
        assert!(params.contains(&("claimType", "file".to_owned())));
        assert!(params.contains(&("max_duration", "180".to_owned())));
        assert!(params.contains(&("from", "20".to_owned())));
        assert!(params.contains(&("nsfw", "false".to_owned())));
    }
}
