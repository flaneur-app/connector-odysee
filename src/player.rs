// SPDX-License-Identifier: AGPL-3.0-or-later

//! Stream resolution and related items.
//!
//! An on-demand claim plays from the CDN in two forms: the original file, progressive and
//! seekable with `Range` (`/v6/streams/{claim_id}/{sd_hash[..6]}.{ext}`), and, once Odysee has
//! transcoded it, an HLS ladder (`/v6/streams/{claim_id}/{sd_hash}/master.m3u8`). The web player
//! finds the HLS by probing the file URL with an `odysee.com` `Referer`; Flâneur builds the master
//! URL directly instead and treats a 404 as "no HLS yet".
//!
//! Which one to offer is decided from a 4 KiB ranged read of the file's head, done alongside the
//! HLS probe (see [`choose`]):
//!
//! - The CDN refuses some files outright (`401 this content cannot be accessed at the moment`):
//!   HLS only, or [`Error::Unavailable`] when there is no HLS either.
//! - An MP4 whose index (`moov`) comes after the media (`mdat`) cannot start until the player has
//!   fetched the end of the file; `GStreamer` over HTTP may read the whole file first. When HLS
//!   exists, only HLS is offered.
//! - Originals over 4 GiB are offered only as HLS when HLS exists: their bitrate is that of the
//!   upload, and the HLS ladder adapts to the connection.
//! - Otherwise both are offered and the view-model's selector picks.

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

/// Bytes read from the head of the file to find its index.
const HEAD_PROBE_LEN: u64 = 4096;
/// Originals larger than this are left out when HLS exists.
const MAX_ORIGINAL_WITH_HLS: u64 = 4 << 30;

/// What the head of the original file says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileHead {
    /// MP4 with its index first: plays at once over HTTP.
    FastStart,
    /// MP4 with its index after the media.
    IndexAtEnd,
    /// Not an MP4, or the boxes did not say within the probe.
    Unknown,
    /// The CDN refused the file (401, 403).
    Refused,
}

/// Reads the top-level MP4 boxes of the head of a file.
pub(crate) fn mp4_head(data: &[u8]) -> FileHead {
    let mut offset = 0usize;
    while let Some(header) = data.get(offset..offset + 8) {
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let size = match size32 {
            // Extends to the end of the file: nothing comes after it.
            0 => u64::MAX,
            1 => match data.get(offset + 8..offset + 16) {
                Some(b) => u64::from_be_bytes(b.try_into().unwrap_or([0; 8])),
                None => return FileHead::Unknown,
            },
            n => u64::from(n),
        };
        match &header[4..8] {
            b"moov" => return FileHead::FastStart,
            b"mdat" => return FileHead::IndexAtEnd,
            _ => {}
        }
        if size < 8 {
            return FileHead::Unknown;
        }
        match usize::try_from(size)
            .ok()
            .and_then(|s| offset.checked_add(s))
        {
            Some(next) => offset = next,
            None => return FileHead::Unknown,
        }
    }
    FileHead::Unknown
}

/// The streams to offer, given what the probes found (see the module docs).
pub(crate) fn choose(file: Stream, head: FileHead, hls: Option<&Url>) -> Result<Vec<Stream>> {
    let huge = file.size.is_some_and(|s| s > MAX_ORIGINAL_WITH_HLS);
    match (head, hls.is_some()) {
        (FileHead::Refused, false) => Err(Error::Unavailable(
            "Odysee's CDN refuses this file at the moment".into(),
        )),
        (FileHead::Refused | FileHead::IndexAtEnd, true) => Ok(Vec::new()),
        (_, true) if huge => Ok(Vec::new()),
        (FileHead::IndexAtEnd, false) => Ok(vec![Stream {
            label: Some("Original file (index at the end: slow to start)".to_owned()),
            ..file
        }]),
        _ => Ok(vec![file]),
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
        label: Some("Original file".to_owned()),
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

    /// The original file's URL and what its head says.
    async fn probed_file(
        &self,
        claim: &Claim,
        media_type: &str,
        sd_hash: &str,
    ) -> Result<(Url, FileHead)> {
        let url = self.file_url(claim, media_type, sd_hash).await?;
        let head = match self.api.head_bytes(url.clone(), HEAD_PROBE_LEN).await {
            Ok((status, _)) if matches!(status.as_u16(), 401 | 403) => FileHead::Refused,
            Ok((status, data)) if status.is_success() && media_type.ends_with("/mp4") => {
                mp4_head(&data)
            }
            // A 429 or a network error says nothing about the file.
            _ => FileHead::Unknown,
        };
        Ok((url, head))
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
            self.probed_file(&claim, &media_type, &sd_hash),
            self.hls_master(claim_id, &sd_hash),
            self.reaction_list(claim_id, token),
            views,
            position,
        );
        let (file, head) = file?;

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
                watched: None,
                reaction: my_reaction,
            });
        }

        let streams = choose(
            file_stream(file, &claim, &media_type, source.size),
            head,
            hls.as_ref(),
        )?;
        Ok(Playable {
            item: details,
            streams,
            hls_manifest: hls,
            dash_manifest: None,
            expires_at: None,
            start_at,
            tracking: None,
            // The VOD CDN needs no header; the live one needs a Referer (see the crate docs).
            headers: Vec::new(),
            handoff: None,
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

    fn head(boxes: &[(&[u8; 4], u32)]) -> Vec<u8> {
        // Only the headers matter: each box is its header padded to its declared size.
        let mut data = Vec::new();
        for (kind, size) in boxes {
            let start = data.len();
            data.extend_from_slice(&size.to_be_bytes());
            data.extend_from_slice(&kind[..]);
            data.resize(start + *size as usize, 0);
        }
        data.truncate(4096);
        data
    }

    #[test]
    fn reads_the_mp4_box_order() {
        assert_eq!(
            mp4_head(&head(&[(b"ftyp", 32), (b"moov", 9000)])),
            FileHead::FastStart
        );
        assert_eq!(
            mp4_head(&head(&[(b"ftyp", 32), (b"mdat", 90_000)])),
            FileHead::IndexAtEnd
        );
        assert_eq!(
            mp4_head(&head(&[(b"ftyp", 32), (b"free", 8), (b"mdat", 90_000)])),
            FileHead::IndexAtEnd
        );
        // A box larger than the probe hides what follows.
        assert_eq!(
            mp4_head(&head(&[(b"ftyp", 32), (b"uuid", 9000)])),
            FileHead::Unknown
        );
        assert_eq!(mp4_head(b"not an mp4 at all"), FileHead::Unknown);
        assert_eq!(mp4_head(&[]), FileHead::Unknown);
    }

    fn file(size: u64) -> Stream {
        Stream {
            url: Url::parse("https://cdn.example/f.mp4").unwrap(),
            kind: StreamKind::Muxed,
            container: Container::Mp4,
            bitrate: None,
            size: Some(size),
            width: None,
            height: None,
            fps: None,
            video_codec: None,
            hdr: None,
            audio: None,
            label: None,
        }
    }

    #[test]
    fn prefers_hls_over_files_that_cannot_stream() {
        let hls = Url::parse("https://cdn.example/master.m3u8").unwrap();
        let mb = 1 << 20;
        // Fast-start and reasonably sized: both, the selector picks.
        assert_eq!(
            choose(file(500 * mb), FileHead::FastStart, Some(&hls))
                .unwrap()
                .len(),
            1
        );
        // Index at the end, refused, or huge: HLS only.
        assert_eq!(
            choose(file(500 * mb), FileHead::IndexAtEnd, Some(&hls))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            choose(file(500 * mb), FileHead::Refused, Some(&hls))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            choose(file(7400 * mb), FileHead::FastStart, Some(&hls))
                .unwrap()
                .len(),
            0
        );
        // No HLS: the file is all there is, flagged when it will be slow to start.
        let only = choose(file(7400 * mb), FileHead::IndexAtEnd, None).unwrap();
        assert!(only[0].label.as_deref().unwrap().contains("slow to start"));
        assert_eq!(
            choose(file(7400 * mb), FileHead::Unknown, None)
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            choose(file(mb), FileHead::Refused, None),
            Err(Error::Unavailable(_))
        ));
    }

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
