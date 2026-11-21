// SPDX-License-Identifier: AGPL-3.0-or-later

//! Comments, from Commentron.
//!
//! Posting signs the text with the user's channel key. That key lives in the user's hosted
//! wallet: the proxy's `channel_sign` signs on Odysee's side, so no key ever reaches Flâneur.

use std::collections::HashMap;
use std::fmt::Write as _;

use async_trait::async_trait;
use flaneur_connector::{CommentSort, Comments, Error, Result};
use flaneur_model::{
    ChannelRef, Comment, CommentAuthor, Cursor, ItemRef, Page, Precision, Published, ServiceId,
    Thumbnail,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Odysee, page_number};

const COMMENTS_PAGE_SIZE: u32 = 30;
/// Commentron's `reaction.List` takes at most 50 ids.
const MAX_REACTION_IDS: usize = 50;

/// `commentapi.CommentItem`. Commentron omits `replies`, `is_creator` and friends when they are
/// zero or false (`omitempty`), so absent means zero there.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommentItem {
    #[serde(default)]
    pub comment: String,
    pub comment_id: String,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub replies: u64,
    #[serde(default)]
    pub is_creator: bool,
    #[serde(default)]
    pub is_pinned: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CommentList {
    #[serde(default)]
    pub items: Vec<CommentItem>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub total_pages: Option<u32>,
    #[serde(default)]
    pub total_items: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct CommentReactions {
    #[serde(default)]
    others_reactions: HashMap<String, HashMap<String, i64>>,
}

#[derive(Deserialize)]
struct Signature {
    signature: String,
    signing_ts: String,
}

impl CommentItem {
    pub fn to_comment(
        &self,
        service: &ServiceId,
        reactions: Option<&HashMap<String, i64>>,
        avatars: Vec<Thumbnail>,
    ) -> Comment {
        let count = |key: &str| {
            reactions
                .and_then(|r| r.get(key))
                .and_then(|n| u64::try_from(*n).ok())
        };
        Comment {
            id: self.comment_id.clone(),
            author: CommentAuthor {
                name: self
                    .channel_name
                    .clone()
                    .unwrap_or_else(|| "Anonymous".to_owned()),
                channel: self
                    .channel_id
                    .as_ref()
                    .map(|id| ChannelRef::new(service.clone(), id)),
                avatars,
                is_publisher: self.is_creator,
            },
            text: self.comment.clone(),
            published: self.timestamp.filter(|t| *t > 0).map(|at| Published {
                at,
                precision: Precision::Exact,
            }),
            likes: count("like"),
            reply_count: Some(self.replies),
            replies_token: (self.replies > 0).then(|| self.comment_id.clone()),
            // A reply is posted with the comment's id as `parent_id`.
            reply_token: Some(self.comment_id.clone()),
            pinned: self.is_pinned,
            hearted: count("creator_like").is_some_and(|n| n > 0),
            my_reaction: None,
        }
    }
}

fn hex(data: &[u8]) -> String {
    data.iter()
        .fold(String::with_capacity(data.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// `comment.List` parameters; Commentron's `sort_by`: 0 newest, 1 oldest, 3 popularity.
pub(crate) fn list_params(
    claim_id: &str,
    parent: Option<&str>,
    sort: CommentSort,
    page: u32,
) -> Value {
    let mut params = json!({
        "claim_id": claim_id,
        "page": page,
        "page_size": COMMENTS_PAGE_SIZE,
    });
    if let Some(parent) = parent {
        params["parent_id"] = json!(parent);
        params["sort_by"] = json!(1);
    } else {
        params["top_level"] = json!(true);
        params["sort_by"] = json!(match sort {
            CommentSort::Top => 3,
            CommentSort::Newest => 0,
        });
    }
    params
}

impl Odysee {
    /// One page of `comment.List`, with like counts and author avatars.
    async fn comment_page(&self, params: Value, page: u32) -> Result<Page<Comment>> {
        let list: CommentList = self.api.comments("comment.List", params).await?;
        let ids: Vec<&str> = list
            .items
            .iter()
            .take(MAX_REACTION_IDS)
            .map(|c| c.comment_id.as_str())
            .collect();
        let mut channel_ids: Vec<String> = Vec::new();
        for id in list.items.iter().filter_map(|c| c.channel_id.as_ref()) {
            if !channel_ids.contains(id) {
                channel_ids.push(id.clone());
            }
        }
        let reactions = async {
            if ids.is_empty() {
                return CommentReactions::default();
            }
            self.api
                .comments("reaction.List", json!({"comment_ids": ids.join(",")}))
                .await
                .unwrap_or_default()
        };
        let channels = async {
            if channel_ids.is_empty() {
                return Vec::new();
            }
            self.claims_by_ids(&channel_ids).await.unwrap_or_default()
        };
        let (reactions, channels) = tokio::join!(reactions, channels);
        let service = self.service();
        let avatars: HashMap<&str, Vec<Thumbnail>> = channels
            .iter()
            .map(|c| (c.claim_id.as_str(), c.channel_summary(service).avatars))
            .collect();
        let items = list
            .items
            .iter()
            .map(|c| {
                c.to_comment(
                    service,
                    reactions.others_reactions.get(&c.comment_id),
                    c.channel_id
                        .as_deref()
                        .and_then(|id| avatars.get(id))
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();
        let current = list.page.unwrap_or(page);
        Ok(Page {
            items,
            next: list
                .total_pages
                .filter(|total| current < *total)
                .map(|_| Cursor((current + 1).to_string())),
            total: list.total_items,
        })
    }
}

#[async_trait]
impl Comments for Odysee {
    async fn comments(
        &self,
        item: &ItemRef,
        sort: CommentSort,
        cursor: Option<&Cursor>,
    ) -> Result<Page<Comment>> {
        let page = page_number(cursor)?;
        let claim_id = self.claim_id(&item.id).await?;
        self.comment_page(list_params(&claim_id, None, sort, page), page)
            .await
    }

    async fn replies(
        &self,
        item: &ItemRef,
        replies_token: &str,
        cursor: Option<&Cursor>,
    ) -> Result<Page<Comment>> {
        let page = page_number(cursor)?;
        let claim_id = self.claim_id(&item.id).await?;
        self.comment_page(
            list_params(&claim_id, Some(replies_token), CommentSort::Newest, page),
            page,
        )
        .await
    }

    async fn post(&self, item: &ItemRef, text: &str, reply_to: Option<&str>) -> Result<Comment> {
        let token = self.require_token()?;
        let channel = self.default_channel().ok_or_else(|| {
            Error::Unavailable("comments need a channel: create one on odysee.com".into())
        })?;
        let claim_id = self.claim_id(&item.id).await?;
        let signed: Signature = self
            .api
            .proxy(
                "channel_sign",
                json!({"channel_id": channel.claim_id, "hexdata": hex(text.as_bytes())}),
                Some(&token),
            )
            .await?;
        let mut params = json!({
            "comment": text,
            "claim_id": claim_id,
            "channel_id": channel.claim_id,
            "channel_name": channel.name,
            "signature": signed.signature,
            "signing_ts": signed.signing_ts,
        });
        if let Some(parent) = reply_to {
            params["parent_id"] = json!(parent);
        }
        let created: CommentItem = self.api.comments("comment.Create", params).await?;
        Ok(created.to_comment(self.service(), None, Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes_utf8() {
        assert_eq!(hex("hé".as_bytes()), "68c3a9");
    }

    #[test]
    fn replies_are_oldest_first() {
        let p = list_params("c", Some("p"), CommentSort::Top, 2);
        assert_eq!(p["parent_id"], "p");
        assert_eq!(p["sort_by"], 1);
        assert!(p.get("top_level").is_none());
        let p = list_params("c", None, CommentSort::Top, 1);
        assert_eq!(p["sort_by"], 3);
        assert_eq!(p["top_level"], true);
    }
}
