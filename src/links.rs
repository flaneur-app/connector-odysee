// SPDX-License-Identifier: AGPL-3.0-or-later

//! Odysee web URLs, `lbry://` URLs, and the ids the connector puts in refs.
//!
//! A ref id is either a full claim id (40 hex characters), which is what every listing returns,
//! or an LBRY URL without its scheme (`@Odysee#8/channel-roundup#e`), which is what a shared link
//! carries: short claim ids can only be expanded by the service, and `parse_url` cannot call it.

use flaneur_connector::Target;
use flaneur_model::{ChannelRef, ItemRef, PlaylistRef, ServiceId};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use url::Url;

const WEB_BASE: &str = "https://odysee.com/";
const HOSTS: &[&str] = &["odysee.com", "www.odysee.com"];

/// Characters escaped in a path segment of a web URL.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// A full claim id: 40 lowercase hexadecimal characters.
pub(crate) fn is_claim_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The full claim id at the end of a permanent URI (`lbry://@name#<id>`, `lbry://name#<id>`).
pub(crate) fn claim_id_from_uri(uri: &str) -> Option<&str> {
    let last = uri.rsplit('/').next()?;
    let (_, id) = last.split_once(['#', ':'])?;
    is_claim_id(id).then_some(id)
}

/// `lbry://@Odysee#8/stream#5` → `https://odysee.com/@Odysee:8/stream:5`.
pub(crate) fn web_url_from_lbry(lbry: &str) -> Option<Url> {
    let path = lbry.strip_prefix("lbry://")?;
    let segments: Vec<String> = path
        .split('/')
        .map(|segment| utf8_percent_encode(&segment.replace('#', ":"), SEGMENT).to_string())
        .collect();
    Url::parse(&format!("{WEB_BASE}{}", segments.join("/"))).ok()
}

/// One `name[:modifier]` or `name[#modifier]` segment.
struct Segment {
    name: String,
    modifier: Option<String>,
}

impl Segment {
    fn parse(raw: &str) -> Option<Self> {
        let decoded = percent_decode_str(raw).decode_utf8().ok()?;
        let (name, modifier) = match decoded.split_once([':', '#']) {
            Some((name, modifier)) => (name, Some(modifier.to_owned())),
            None => (decoded.as_ref(), None),
        };
        if name.is_empty() || name == "@" {
            return None;
        }
        Some(Self {
            name: name.to_owned(),
            modifier: modifier.filter(|m| !m.is_empty()),
        })
    }

    fn is_channel(&self) -> bool {
        self.name.starts_with('@')
    }

    fn full_id(&self) -> Option<&str> {
        self.modifier.as_deref().filter(|m| is_claim_id(m))
    }

    fn lbry(&self) -> String {
        match &self.modifier {
            Some(m) => format!("{}#{m}", self.name),
            None => self.name.clone(),
        }
    }
}

fn target(service: &ServiceId, segments: &[&str]) -> Option<Target> {
    let parsed: Vec<Segment> = segments
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| Segment::parse(s))
        .collect::<Option<_>>()?;
    match parsed.as_slice() {
        [channel] if channel.is_channel() => Some(Target::Channel(ChannelRef::new(
            service.clone(),
            channel
                .full_id()
                .map_or_else(|| channel.lbry(), str::to_owned),
        ))),
        [stream] => Some(Target::Item(ItemRef::new(
            service.clone(),
            stream
                .full_id()
                .map_or_else(|| stream.lbry(), str::to_owned),
        ))),
        [channel, stream] if channel.is_channel() && !stream.is_channel() => {
            let id = stream.full_id().map_or_else(
                || format!("{}/{}", channel.lbry(), stream.lbry()),
                str::to_owned,
            );
            Some(Target::Item(ItemRef::new(service.clone(), id)))
        }
        _ => None,
    }
}

/// Recognizes an `lbry://` URL from its raw text, `@` included.
pub(crate) fn parse_lbry(service: &ServiceId, uri: &str) -> Option<Target> {
    let rest = uri.strip_prefix("lbry://")?;
    let rest = rest.split(['?', '&']).next().unwrap_or(rest);
    let segments: Vec<&str> = rest.split('/').collect();
    target(service, &segments)
}

/// Recognizes `https://odysee.com/...` and `lbry://...` URLs.
pub(crate) fn parse(service: &ServiceId, url: &Url) -> Option<Target> {
    if url.scheme() == "lbry" {
        // WHATWG parsing reads the `@` of `lbry://@channel` as an empty user name and drops it,
        // so a `Url` cannot tell `lbry://@name` from `lbry://name`. Two segments are always a
        // channel and a stream; one segment is taken as a stream. `parse_lbry` reads the raw text.
        let rest = url.as_str().strip_prefix("lbry://")?;
        let rest = rest.split(['?', '&']).next().unwrap_or(rest);
        let mut segments: Vec<String> = rest.split('/').map(str::to_owned).collect();
        if segments.len() == 2 && !segments[0].starts_with('@') {
            segments[0].insert(0, '@');
        }
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        return target(service, &segments);
    }
    if !matches!(url.scheme(), "http" | "https") || !HOSTS.contains(&url.host_str()?) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["$", "playlist", id, ..]
            if is_claim_id(id) || *id == "watchlater" || *id == "favorites" =>
        {
            Some(Target::Playlist(PlaylistRef::new(service.clone(), *id)))
        }
        // `$/embed/{name}/{claim_id}`, the old embed form.
        ["$", "embed", _name, id] if is_claim_id(id) => {
            Some(Target::Item(ItemRef::new(service.clone(), *id)))
        }
        ["$", "embed", rest @ ..] => target(service, rest),
        ["$", ..] => None,
        _ => target(service, &segments),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> ServiceId {
        ServiceId::new("odysee")
    }

    fn parse_str(s: &str) -> Option<Target> {
        parse(&service(), &Url::parse(s).unwrap())
    }

    #[allow(clippy::unnecessary_wraps)]
    fn item(id: &str) -> Option<Target> {
        Some(Target::Item(ItemRef::new(service(), id)))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn channel(id: &str) -> Option<Target> {
        Some(Target::Channel(ChannelRef::new(service(), id)))
    }

    const ID: &str = "ee58d95e7e42ec9479e13b28d57556f9ffb373dc";

    #[test]
    fn parses_web_urls() {
        assert_eq!(
            parse_str("https://odysee.com/@Odysee:8/channel-roundup-may-2026:e"),
            item("@Odysee#8/channel-roundup-may-2026#e")
        );
        assert_eq!(
            parse_str(&format!(
                "https://odysee.com/@Odysee:8/channel-roundup:{ID}?t=30"
            )),
            item(ID)
        );
        assert_eq!(
            parse_str("https://odysee.com/@Odysee:8"),
            channel("@Odysee#8")
        );
        assert_eq!(
            parse_str("https://www.odysee.com/@Odysee"),
            channel("@Odysee")
        );
        assert_eq!(
            parse_str("https://odysee.com/@Odysee:80d2590ad04e36fb1d077a9b9e3a8bba76defdf8"),
            channel("80d2590ad04e36fb1d077a9b9e3a8bba76defdf8")
        );
        assert_eq!(parse_str("https://odysee.com/missinfo"), item("missinfo"));
        assert_eq!(
            parse_str("https://odysee.com/missinfo:d"),
            item("missinfo#d")
        );
        assert_eq!(
            parse_str("https://odysee.com/@Caf%C3%A9:1/cr%C3%AApe:2"),
            item("@Café#1/crêpe#2")
        );
    }

    #[test]
    fn parses_special_pages() {
        assert_eq!(
            parse_str(&format!("https://odysee.com/$/embed/channel-roundup/{ID}")),
            item(ID)
        );
        assert_eq!(
            parse_str("https://odysee.com/$/embed/@Odysee:8/missinfo:d"),
            item("@Odysee#8/missinfo#d")
        );
        assert_eq!(
            parse_str(&format!("https://odysee.com/$/playlist/{ID}")),
            Some(Target::Playlist(PlaylistRef::new(service(), ID)))
        );
        assert_eq!(parse_str("https://odysee.com/$/settings"), None);
        assert_eq!(parse_str("https://odysee.com/"), None);
        assert_eq!(parse_str("https://example.com/@Odysee:8"), None);
    }

    #[test]
    fn parses_lbry_urls() {
        assert_eq!(
            parse_str("lbry://@Odysee#8/channel-roundup-may-2026#e"),
            item("@Odysee#8/channel-roundup-may-2026#e")
        );
        assert_eq!(parse_str(&format!("lbry://channel-roundup#{ID}")), item(ID));
        assert_eq!(parse_str("lbry://@Odysee/@Other"), None);
        // A `Url` drops the `@` of a lone channel: only the raw text keeps it.
        assert_eq!(parse_str("lbry://@Odysee#8"), item("Odysee#8"));
        assert_eq!(
            parse_lbry(&service(), "lbry://@Odysee#8"),
            channel("@Odysee#8")
        );
        assert_eq!(
            parse_lbry(&service(), "lbry://@Odysee#8/missinfo#d"),
            item("@Odysee#8/missinfo#d")
        );
    }

    #[test]
    fn builds_web_urls() {
        assert_eq!(
            web_url_from_lbry("lbry://@Odysee#8/channel-roundup-may-2026#e")
                .unwrap()
                .as_str(),
            "https://odysee.com/@Odysee:8/channel-roundup-may-2026:e"
        );
        assert_eq!(
            web_url_from_lbry("lbry://@Café#1/a b#2").unwrap().as_str(),
            "https://odysee.com/@Caf%C3%A9:1/a%20b:2"
        );
    }

    #[test]
    fn extracts_claim_ids() {
        assert_eq!(
            claim_id_from_uri("lbry://@Odysee#80d2590ad04e36fb1d077a9b9e3a8bba76defdf8"),
            Some("80d2590ad04e36fb1d077a9b9e3a8bba76defdf8")
        );
        assert_eq!(claim_id_from_uri(&format!("lbry://name#{ID}")), Some(ID));
        assert_eq!(claim_id_from_uri("lbry://@Odysee#8"), None);
    }
}
