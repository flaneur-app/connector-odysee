// SPDX-License-Identifier: AGPL-3.0-or-later

//! LBRY claims as the SDK proxy returns them (`resolve`, `claim_search`), and their mapping to
//! the Flâneur model.

use flaneur_model::{
    Channel, ChannelRef, ChannelSummary, ContentFlags, Dimensions as Frame, Item, ItemKind,
    ItemRef, Playlist, PlaylistKind, PlaylistRef, Precision, Published, Seconds, ServiceId,
    Thumbnail, Timestamp,
};
use serde::{Deserialize, Deserializer};
use url::Url;

use crate::links;

/// Tags the web app treats as mature (`ui/constants/tags.ts`, `MATURE_TAGS`).
pub(crate) const MATURE_TAGS: &[&str] = &[
    "porn",
    "porno",
    "nsfw",
    "mature",
    "xxx",
    "sex",
    "creampie",
    "blowjob",
    "handjob",
    "boobs",
    "big boobs",
    "big dick",
    "pussy",
    "cumshot",
    "anal",
    "hard fucking",
    "ass",
    "fuck",
    "hentai",
];

/// The web app's shorts rule (`ui/constants/settings.ts`): short and vertical enough.
pub(crate) const SHORTS_MAX_DURATION: Seconds = 180;
/// Width / height at or below which a short video counts as a short.
pub(crate) const SHORTS_MAX_ASPECT_RATIO: f64 = 0.8;

const TAG_MEMBERS_ONLY: &str = "c:members-only";
const TAG_PAYWALLS: &[&str] = &["c:purchase", "c:rental"];
const TAG_SCHEDULED_PREFIX: &str = "c:scheduled";

/// Release times past this (the year 3000) are placeholders some scheduled claims carry.
const MAX_PLAUSIBLE_TIME: Timestamp = 32_503_680_000;

/// Thumbnail resizer of the Odysee CDN.
const THUMBNAIL_RESIZER: &str = "https://thumbnails.odycdn.com/optimize/";

/// A claim: a stream (video, audio, document, livestream), a channel, a collection or a repost.
/// Field names are the proxy's.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Claim {
    pub claim_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub canonical_url: Option<String>,
    #[serde(default)]
    pub permanent_url: Option<String>,
    #[serde(default)]
    pub value_type: Option<String>,
    /// Block time, Unix seconds.
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub value: ClaimValue,
    /// `None` too when the proxy could not resolve the channel: it then sends only
    /// `{"channel_id": …}` (an invalid or abandoned channel signature).
    #[serde(default, deserialize_with = "lenient_claim")]
    pub signing_channel: Option<Box<Claim>>,
    #[serde(default, deserialize_with = "lenient_claim")]
    pub reposted_claim: Option<Box<Claim>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Meta {
    #[serde(default)]
    pub creation_timestamp: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ClaimValue {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<Media>,
    #[serde(default)]
    pub cover: Option<Media>,
    /// Unix seconds, sent as a string.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub release_time: Option<i64>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub stream_type: Option<String>,
    #[serde(default)]
    pub video: Option<Dimensions>,
    #[serde(default)]
    pub audio: Option<Dimensions>,
    #[serde(default)]
    pub fee: Option<Fee>,
    /// Claim ids of a collection.
    #[serde(default)]
    pub claims: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Media {
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Source {
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub sd_hash: Option<String>,
    /// Bytes, sent as a string.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct Dimensions {
    /// Whole seconds.
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Fee {
    #[serde(default, deserialize_with = "lenient_f64")]
    pub amount: Option<f64>,
}

/// One page of `claim_search`.
#[derive(Debug, Deserialize)]
pub(crate) struct ClaimSearch {
    /// Entries that are not claims are skipped: one odd entry must not lose the page.
    #[serde(default, deserialize_with = "lenient_claims")]
    pub items: Vec<Claim>,
    #[serde(default)]
    pub total_items: Option<u64>,
    #[serde(default)]
    pub total_pages: Option<u32>,
}

impl Claim {
    /// The claim a repost points to, or the claim itself.
    #[must_use]
    pub fn resolved(self) -> Self {
        match self.reposted_claim {
            Some(target) if self.value_type.as_deref() == Some("repost") => *target,
            _ => self,
        }
    }

    pub fn is_channel(&self) -> bool {
        self.value_type.as_deref() == Some("channel")
    }

    pub fn is_collection(&self) -> bool {
        self.value_type.as_deref() == Some("collection")
    }

    pub fn is_stream(&self) -> bool {
        self.value_type.as_deref() == Some("stream")
    }

    fn has_tag(&self, tag: &str) -> bool {
        self.value.tags.iter().any(|t| t.eq_ignore_ascii_case(tag))
    }

    /// A livestream claim is a stream with no source file.
    pub fn is_livestream(&self) -> bool {
        self.is_stream() && self.value.source.is_none()
    }

    /// Video or audio, as opposed to documents, images and other files.
    pub fn is_media(&self) -> bool {
        if self.is_livestream() {
            return true;
        }
        match self.value.stream_type.as_deref() {
            Some("video" | "audio") => true,
            Some(_) => false,
            None => self
                .media_type()
                .is_some_and(|m| m.starts_with("video/") || m.starts_with("audio/")),
        }
    }

    pub fn media_type(&self) -> Option<&str> {
        self.value.source.as_ref()?.media_type.as_deref()
    }

    pub fn is_mature(&self) -> bool {
        MATURE_TAGS.iter().any(|t| self.has_tag(t))
    }

    pub fn is_members_only(&self) -> bool {
        self.has_tag(TAG_MEMBERS_ONLY)
    }

    /// Bought, rented, or with a fee: Flâneur cannot pay for it.
    pub fn is_paid(&self) -> bool {
        TAG_PAYWALLS.iter().any(|t| self.has_tag(t))
            || self
                .value
                .fee
                .as_ref()
                .and_then(|f| f.amount)
                .is_some_and(|a| a > 0.0)
    }

    pub fn is_scheduled(&self) -> bool {
        self.value
            .tags
            .iter()
            .any(|t| t.starts_with(TAG_SCHEDULED_PREFIX))
    }

    fn dimensions(&self) -> Option<Dimensions> {
        self.value.video.or(self.value.audio)
    }

    pub fn duration(&self) -> Option<Seconds> {
        self.dimensions()?.duration.filter(|d| *d > 0)
    }

    /// The web app's rule: at most 180 s, and at most 0.8 wide for 1 high.
    pub fn is_short(&self) -> bool {
        let (Some(duration), Some(frame)) =
            (self.value.video.and_then(|v| v.duration), self.frame())
        else {
            return false;
        };
        duration > 0
            && duration <= SHORTS_MAX_DURATION
            && f64::from(frame.width) / f64::from(frame.height) <= SHORTS_MAX_ASPECT_RATIO
    }

    /// Frame size of a video, when the claim gives both sides.
    pub fn frame(&self) -> Option<Frame> {
        let video = self.value.video?;
        let (width, height) = (video.width?, video.height?);
        (width > 0 && height > 0).then_some(Frame { width, height })
    }

    /// Release time, falling back to the block time, then the creation time.
    pub fn release_time(&self) -> Option<Timestamp> {
        self.value
            .release_time
            .or(self.timestamp)
            .or(self.meta.creation_timestamp)
            .filter(|t| *t > 0)
    }

    pub fn kind(&self, now: Timestamp) -> ItemKind {
        let future = self.release_time().is_some_and(|t| t > now);
        if self.is_livestream() {
            if future {
                ItemKind::Upcoming
            } else {
                ItemKind::Live
            }
        } else if future && self.is_scheduled() {
            // A scheduled upload: Odysee plays it as a regular video once released.
            ItemKind::Upcoming
        } else if self.is_short() {
            ItemKind::Short
        } else {
            ItemKind::Video
        }
    }

    pub fn title(&self) -> String {
        self.value
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| self.name.clone())
    }

    pub fn web_url(&self) -> Option<Url> {
        let lbry = self
            .canonical_url
            .as_deref()
            .or(self.permanent_url.as_deref())?;
        links::web_url_from_lbry(lbry)
    }

    pub fn channel_summary(&self, service: &ServiceId) -> ChannelSummary {
        ChannelSummary {
            r#ref: ChannelRef::new(service.clone(), &self.claim_id),
            name: self.title(),
            avatars: thumbnail_url(self.value.thumbnail.as_ref()).map_or_default(|u| avatars(&u)),
            verified: None,
        }
    }

    pub fn to_channel(&self, service: &ServiceId) -> Channel {
        Channel {
            summary: self.channel_summary(service),
            handle: Some(self.name.clone()).filter(|n| !n.is_empty()),
            description: self.value.description.clone().filter(|d| !d.is_empty()),
            subscribers: None,
            banners: thumbnail_url(self.value.cover.as_ref()).map_or_default(|u| vec![original(u)]),
            subscribed: None,
            web_url: self.web_url(),
        }
    }

    pub fn to_playlist(&self, service: &ServiceId) -> Playlist {
        Playlist {
            r#ref: PlaylistRef::new(service.clone(), &self.claim_id),
            kind: PlaylistKind::Regular,
            title: self.title(),
            channel: self
                .signing_channel
                .as_deref()
                .map(|c| c.channel_summary(service)),
            item_count: Some(self.value.claims.len() as u64),
            thumbnails: thumbnail_url(self.value.thumbnail.as_ref())
                .map_or_default(|u| card_thumbnails(&u)),
        }
    }

    /// The item of a stream claim; `None` for channels, collections and non-media files.
    pub fn to_item(&self, service: &ServiceId, now: Timestamp) -> Option<Item> {
        if !self.is_stream() || !self.is_media() {
            return None;
        }
        let kind = self.kind(now);
        let release = self.release_time().filter(|t| *t < MAX_PLAUSIBLE_TIME);
        let (published, scheduled_start) = if kind == ItemKind::Upcoming {
            (None, release)
        } else {
            (
                release.map(|at| Published {
                    at,
                    precision: Precision::Exact,
                }),
                None,
            )
        };
        Some(Item {
            r#ref: ItemRef::new(service.clone(), &self.claim_id),
            kind,
            title: self.title(),
            description: self.value.description.clone().filter(|d| !d.is_empty()),
            duration: if self.is_livestream() {
                None
            } else {
                self.duration()
            },
            published,
            scheduled_start,
            channel: self
                .signing_channel
                .as_deref()
                .map(|c| c.channel_summary(service)),
            thumbnails: thumbnail_url(self.value.thumbnail.as_ref())
                .map_or_default(|u| card_thumbnails(&u)),
            dimensions: self.frame(),
            views: None,
            reactions: flaneur_model::ReactionCounts::default(),
            flags: ContentFlags {
                // Odysee marks mature content with tags only: no tag is the creator saying "not
                // mature", which is all the service ever says.
                sensitive: Some(self.is_mature()),
                age_restricted: None,
                members_only: Some(self.is_members_only()),
            },
            web_url: self.web_url(),
            watch: None,
        })
    }
}

fn thumbnail_url(media: Option<&Media>) -> Option<Url> {
    let url = Url::parse(media?.url.as_deref()?.trim()).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn original(url: Url) -> Thumbnail {
    Thumbnail {
        url,
        width: None,
        height: None,
    }
}

/// A resized copy from the CDN's resizer (`s:{w}:{h}/quality:85/plain/{original}`).
fn resized(url: &Url, width: u32, height: u32) -> Option<Thumbnail> {
    let resized = Url::parse(&format!(
        "{THUMBNAIL_RESIZER}s:{width}:{height}/quality:85/plain/{url}"
    ))
    .ok()?;
    Some(Thumbnail {
        url: resized,
        width: Some(width),
        height: Some(height),
    })
}

fn card_thumbnails(url: &Url) -> Vec<Thumbnail> {
    let mut all: Vec<Thumbnail> = [(390, 220), (1280, 720)]
        .into_iter()
        .filter_map(|(w, h)| resized(url, w, h))
        .collect();
    all.push(original(url.clone()));
    all
}

fn avatars(url: &Url) -> Vec<Thumbnail> {
    let mut all: Vec<Thumbnail> = [96, 256]
        .into_iter()
        .filter_map(|s| resized(url, s, s))
        .collect();
    all.push(original(url.clone()));
    all
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient {
    Int(i64),
    Float(f64),
    Text(String),
}

fn lenient_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[allow(clippy::cast_possible_truncation)]
    Ok(match Option::<Lenient>::deserialize(d)? {
        Some(Lenient::Int(i)) => Some(i),
        Some(Lenient::Float(f)) if f.is_finite() => Some(f as i64),
        Some(Lenient::Text(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

/// A nested claim, or `None` when it is not a full claim.
fn lenient_claim<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Box<Claim>>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value
        .and_then(|v| serde_json::from_value::<Claim>(v).ok())
        .map(Box::new))
}

fn lenient_claims<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Claim>, D::Error> {
    let values = Option::<Vec<serde_json::Value>>::deserialize(d)?.unwrap_or_default();
    let total = values.len();
    let claims: Vec<Claim> = values
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    if claims.len() < total {
        tracing::debug!(skipped = total - claims.len(), "malformed claims skipped");
    }
    Ok(claims)
}

fn lenient_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(lenient_i64(d)?.and_then(|i| u64::try_from(i).ok()))
}

fn lenient_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    #[allow(clippy::cast_precision_loss)]
    Ok(match Option::<Lenient>::deserialize(d)? {
        Some(Lenient::Int(i)) => Some(i as f64),
        Some(Lenient::Float(f)) => Some(f),
        Some(Lenient::Text(s)) => s.trim().parse().ok(),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When the fixture was recorded: two claims are scheduled after it.
    const RECORDED: Timestamp = 1_795_223_000;

    fn fixture() -> Vec<Claim> {
        #[derive(Deserialize)]
        struct Envelope {
            result: ClaimSearch,
        }
        let json = include_str!("../tests/fixtures/claim_search_mixed.json");
        serde_json::from_str::<Envelope>(json).unwrap().result.items
    }

    fn claim(id_prefix: &str) -> Claim {
        fixture()
            .into_iter()
            .find(|c| c.claim_id.starts_with(id_prefix))
            .unwrap()
    }

    fn service() -> ServiceId {
        ServiceId::new("odysee")
    }

    #[test]
    fn maps_a_video() {
        let item = claim("d2bd744f").to_item(&service(), RECORDED).unwrap();
        assert_eq!(item.kind, ItemKind::Video);
        assert_eq!(item.r#ref.id, "d2bd744f12b05ed0bc1fee5030a55dbd73258e38");
        assert_eq!(
            item.title,
            "Introducing: Miss Information, Your Fairy Government Godmother"
        );
        assert_eq!(item.duration, Some(145));
        assert_eq!(
            item.published,
            Some(Published {
                at: 1_670_434_627,
                precision: Precision::Exact
            })
        );
        assert_eq!(item.scheduled_start, None);
        let channel = item.channel.unwrap();
        assert_eq!(channel.name, "Odysee");
        assert_eq!(channel.r#ref.id, "80d2590ad04e36fb1d077a9b9e3a8bba76defdf8");
        assert_eq!(channel.avatars.len(), 3);
        assert_eq!(
            item.web_url.unwrap().as_str(),
            "https://odysee.com/@Odysee:8/missinfo:d"
        );
        assert_eq!(item.flags.sensitive, Some(false));
        assert_eq!(item.flags.members_only, Some(false));
        assert_eq!(item.views, None);
        assert_eq!(
            item.dimensions,
            Some(Frame {
                width: 1920,
                height: 1080
            })
        );
        let widest = Thumbnail::best_for_width(&item.thumbnails, 1000).unwrap();
        assert_eq!(widest.width, Some(1280));
        assert!(
            widest
                .url
                .as_str()
                .starts_with("https://thumbnails.odycdn.com/optimize/s:1280:720/")
        );
    }

    #[test]
    fn maps_kinds() {
        let kind = |p: &str| claim(p).to_item(&service(), RECORDED).map(|i| i.kind);
        // 83 s, 1080x1920.
        assert_eq!(kind("cf87118e"), Some(ItemKind::Short));
        // A livestream claim (no source) released in the past.
        assert_eq!(kind("d826937a"), Some(ItemKind::Live));
        // A scheduled livestream and a scheduled upload.
        assert_eq!(kind("4742d0c5"), Some(ItemKind::Upcoming));
        assert_eq!(kind("952ad1ac"), Some(ItemKind::Upcoming));
        // A markdown post, a channel, a collection: not items.
        assert_eq!(kind("50b203a2"), None);
        assert_eq!(kind("80d2590a"), None);
        assert_eq!(kind("fcac33d8"), None);
    }

    #[test]
    fn upcoming_items_carry_their_start() {
        let live = claim("4742d0c5").to_item(&service(), RECORDED).unwrap();
        assert_eq!(live.scheduled_start, Some(1_795_224_600));
        assert_eq!(live.published, None);
        assert_eq!(live.duration, None);
        // A placeholder release time in the year 5 billion.
        let placeholder = claim("919f002e").to_item(&service(), RECORDED).unwrap();
        assert_eq!(placeholder.kind, ItemKind::Upcoming);
        assert_eq!(placeholder.scheduled_start, None);
        // No title: the claim name stands in.
        assert_eq!(placeholder.title, "live83224");
    }

    #[test]
    fn unresolved_signing_channels_do_not_lose_the_page() {
        #[derive(Deserialize)]
        struct Envelope {
            result: ClaimSearch,
        }
        // A real page where one claim's channel signature is invalid: the proxy then sends
        // `signing_channel: {"channel_id": …}` without a `claim_id`.
        let json = include_str!("../tests/fixtures/claim_search_unresolved_channel.json");
        let items = serde_json::from_str::<Envelope>(json).unwrap().result.items;
        assert_eq!(items.len(), 2);
        let unsigned = items
            .iter()
            .find(|c| c.claim_id.starts_with("8b494893"))
            .unwrap()
            .to_item(&service(), RECORDED)
            .unwrap();
        assert_eq!(unsigned.title, "Cats and kittens - 2011");
        assert!(unsigned.channel.is_none());
        assert!(items[0].signing_channel.is_some());

        let broken =
            r#"{"items": [{"name": "no-id"}, 7, {"claim_id": "ok", "value_type": "stream"}]}"#;
        let page: ClaimSearch = serde_json::from_str(broken).unwrap();
        assert_eq!(page.items.len(), 1);
    }

    #[test]
    fn mature_tags_mark_items_sensitive() {
        let mut c = claim("d2bd744f");
        c.value.tags.push("NSFW".into());
        assert_eq!(
            c.to_item(&service(), RECORDED).unwrap().flags.sensitive,
            Some(true)
        );
    }

    #[test]
    fn maps_channels_and_collections() {
        let channel = claim("80d2590a").to_channel(&service());
        assert_eq!(channel.handle.as_deref(), Some("@Odysee"));
        assert_eq!(channel.summary.name, "Odysee");
        assert_eq!(channel.banners.len(), 1);
        assert_eq!(
            channel.web_url.unwrap().as_str(),
            "https://odysee.com/@Odysee:8"
        );
        let playlist = claim("fcac33d8").to_playlist(&service());
        assert_eq!(playlist.title, "Our Channels");
        assert_eq!(playlist.item_count, Some(4));
    }

    #[test]
    fn paywalls_and_shorts_rules() {
        let mut c = claim("d2bd744f");
        assert!(!c.is_paid());
        c.value.fee = Some(Fee { amount: Some(1.5) });
        assert!(c.is_paid());
        let mut c = claim("cf87118e");
        c.value.video.as_mut().unwrap().duration = Some(181);
        assert!(!c.is_short());
    }
}
