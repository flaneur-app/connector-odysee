// SPDX-License-Identifier: AGPL-3.0-or-later

//! Stream resolution and related items.
//!
//! An on-demand claim plays from the CDN in two forms: the original file, progressive and
//! seekable with `Range` (`/v6/streams/{claim_id}/{sd_hash[..6]}.{ext}`), and, once Odysee has
//! transcoded it, an HLS ladder (`/v6/streams/{claim_id}/{sd_hash}/master.m3u8`). The web player
//! finds the HLS by probing the file URL with an `odysee.com` `Referer`; Flâneur builds the master
//! URL directly instead and treats a 404 as "no HLS yet".

use std::collections::HashMap;

use async_trait::async_trait;
use flaneur_connector::{Error, Player, Result};
use flaneur_model::{
    Container, Cursor, Item, ItemKind, ItemRef, Page, Playable, Progress, Reaction, Stream,
    StreamKind, WatchState,
};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::claim::Claim;
use crate::{Odysee, offset};

const RELATED_PAGE_SIZE: usize = 20;

/// Reaction counts of `reaction/list`, by claim id.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReactionList {
    #[serde(default)]
    pub my_reactions: HashMap<String, Counts>,
    #[serde(default)]
    pub others_reactions: HashMap<String, Counts>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct Counts {
    #[serde(default)]
    pub like: u64,
    #[serde(default)]
    pub dislike: u64,
}

impl ReactionList {
    /// Public counts (the user's own reaction is listed apart, so it is added back) and the
    /// user's reaction when the list was asked with a token.
    pub fn for_claim(&self, claim_id: &str) -> (Option<Counts>, Option<Reaction>) {
        let mine = self.my_reactions.get(claim_id).copied();
        let others = self.others_reactions.get(claim_id).copied();
        let total = others.map(|o| {
            let m = mine.unwrap_or_default();
            Counts {
                like: o.like + m.like,
                dislike: o.dislike + m.dislike,
            }
        });
        let reaction = mine.map(|m| {
            if m.like > 0 {
                Reaction::Like
            } else if m.dislike > 0 {
                Reaction::Dislike
            } else {
                Reaction::None
            }
        });
        (total, reaction)
    }
}

/// Extension of the CDN file for the media types whose extension the web app's MIME table gives
/// unambiguously. Other types ask the proxy's `get` for the URL.
fn extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "audio/mp4" => Some("m4a"),
        _ => None,
    }
}

fn container(media_type: &str) -> Container {
    match media_type {
        "video/mp4" | "audio/mp4" => Container::Mp4,
        "video/webm" | "audio/webm" => Container::WebM,
        other => Container::Other(other.to_owned()),
    }
}

/// The original file as a stream: muxed for videos, audio-only for audio files.
fn file_stream(url: Url, claim: &Claim, media_type: &str, size: Option<u64>) -> Stream {
    let is_audio = media_type.starts_with("audio/");
    let video = claim.value.video.filter(|_| !is_audio);
    Stream {
        url,
        kind: if is_audio {
            StreamKind::Audio
        } else {
            StreamKind::Muxed
        },
        container: container(media_type),
        bitrate: None,
        size,
        width: video.and_then(|v| v.width),
        height: video.and_then(|v| v.height),
        fps: None,
        video_codec: None,
        hdr: None,
        audio: None,
    }
}

/// Why a claim cannot be played here, if it cannot.
pub(crate) fn unplayable(claim: &Claim, item: &Item) -> Option<Error> {
    if claim.is_livestream() {
        // The live HLS needs an odysee.com Referer; see the crate docs.
        return Some(Error::Unavailable("live playback not supported yet".into()));
    }
    if item.kind == ItemKind::Upcoming {
        return Some(Error::Unavailable("not released yet".into()));
    }
    if claim.is_members_only() {
        return Some(Error::Unavailable("members-only content".into()));
    }
    if claim.is_paid() {
        return Some(Error::Unavailable("paid content".into()));
    }
    None
}

#[derive(Deserialize)]
struct Get {
    streaming_url: String,
}

impl Odysee {
    /// The original file's URL.
    async fn file_url(&self, claim: &Claim, media_type: &str, sd_hash: &str) -> Result<Url> {
        if let (Some(ext), Some(sd6)) = (extension(media_type), sd_hash.get(..6)) {
            return self
                .api
                .endpoints
                .cdn
                .join(&format!("v6/streams/{}/{sd6}.{ext}", claim.claim_id))
                .map_err(|e| Error::Parse(e.to_string()));
        }
        let uri = claim
            .permanent_url
            .clone()
            .unwrap_or_else(|| format!("lbry://{}#{}", claim.name, claim.claim_id));
        let get: Get = self
            .api
            .proxy("get", json!({"uri": uri, "save_file": false}), None)
            .await?;
        Url::parse(&get.streaming_url).map_err(|e| Error::Parse(e.to_string()))
    }

    /// The HLS master playlist, when Odysee has transcoded the claim.
    async fn hls_master(&self, claim_id: &str, sd_hash: &str) -> Option<Url> {
        let url = self
            .api
            .endpoints
            .cdn
            .join(&format!("v6/streams/{claim_id}/{sd_hash}/master.m3u8"))
            .ok()?;
        let body = self.api.get_text(url.clone()).await.ok()??;
        body.trim_start().starts_with("#EXTM3U").then_some(url)
    }

    pub(crate) async fn reaction_list(
        &self,
        claim_ids: &str,
        token: Option<&str>,
    ) -> Result<ReactionList> {
        Ok(self
            .api
            .internal("reaction/list", &[("claim_ids", claim_ids)], token)
            .await?)
    }
}

#[async_trait]
impl Player for Odysee {
    async fn resolve(&self, item: &ItemRef) -> Result<Playable> {
        let claim = self.lookup(&item.id).await?;
        let mut details = claim
            .to_item(self.service(), Self::now())
            .ok_or_else(|| Error::Unavailable("not a video or audio file".into()))?;
        if let Some(error) = unplayable(&claim, &details) {
            return Err(error);
        }
        let source = claim
            .value
            .source
            .clone()
            .ok_or_else(|| Error::Parse("stream without a source".into()))?;
        let media_type = source.media_type.clone().unwrap_or_default();
        let sd_hash = source
            .sd_hash
            .clone()
            .ok_or_else(|| Error::Parse("source without an sd_hash".into()))?;

        let token = self.token();
        let token = token.as_deref();
        let claim_id = claim.claim_id.as_str();
        let views = async {
            match token {
                Some(token) => self
                    .api
                    .internal::<Vec<u64>>("file/view_count", &[("claim_id", claim_id)], Some(token))
                    .await
                    .ok()
                    .and_then(|v| v.first().copied()),
                None => None,
            }
        };
        let position = async {
            match token {
                Some(token) => self
                    .api
                    .internal::<HashMap<String, f64>>(
                        "file/last_positions",
                        &[("claim_ids", claim_id)],
                        Some(token),
                    )
                    .await
                    .ok()
                    .and_then(|p| p.get(claim_id).copied()),
                None => None,
            }
        };
        let (file, hls, reactions, views, position) = tokio::join!(
            self.file_url(&claim, &media_type, &sd_hash),
            self.hls_master(claim_id, &sd_hash),
            self.reaction_list(claim_id, token),
            views,
            position,
        );
        let file = file?;

        let (counts, my_reaction) = reactions.map_or_default(|r| r.for_claim(claim_id));
        if let Some(counts) = counts {
            details.reactions.likes = Some(counts.like);
            details.reactions.dislikes = Some(counts.dislike);
        }
        details.views = views;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let start_at = position
            .filter(|p| p.is_finite() && *p > 0.0)
            .map(|p| p as u32);
        if my_reaction.is_some() || start_at.is_some() {
            details.watch = Some(WatchState {
                progress: start_at.map(|position| Progress {
                    position,
                    duration: details.duration,
                    updated_at: Self::now(),
                }),
                watched: false,
                reaction: my_reaction,
            });
        }

        let stream = file_stream(file, &claim, &media_type, source.size);
        Ok(Playable {
            item: details,
            streams: vec![stream],
            hls_manifest: hls,
            dash_manifest: None,
            expires_at: None,
            start_at,
            tracking: None,
        })
    }

    async fn related(&self, item: &ItemRef, cursor: Option<&Cursor>) -> Result<Page<Item>> {
        let from = offset(cursor)?;
        let claim = self.lookup(&item.id).await?;
        let params = [
            ("s", claim.title()),
            ("size", RELATED_PAGE_SIZE.to_string()),
            ("from", from.to_string()),
            ("related_to", claim.claim_id.clone()),
            ("nsfw", "false".to_owned()),
            ("free_only", "true".to_owned()),
        ];
        let (hits, claims) = self
            .search_hits(self.api.endpoints.related.clone(), &params)
            .await?;
        let items = self
            .items(claims)
            .into_iter()
            .filter(|i| i.r#ref.id != claim.claim_id)
            .collect();
        Ok(Page {
            items,
            next: (hits >= RELATED_PAGE_SIZE).then(|| Cursor((from + hits).to_string())),
            total: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_my_reaction_back_to_the_counts() {
        let list: ReactionList = serde_json::from_str(
            r#"{"my_reactions":{"a":{"like":1,"dislike":0}},"others_reactions":{"a":{"like":213,"dislike":37}}}"#,
        )
        .unwrap();
        let (counts, mine) = list.for_claim("a");
        let counts = counts.unwrap();
        assert_eq!((counts.like, counts.dislike), (214, 37));
        assert_eq!(mine, Some(Reaction::Like));
        let (counts, mine) = list.for_claim("b");
        assert!(counts.is_none() && mine.is_none());
    }
}
