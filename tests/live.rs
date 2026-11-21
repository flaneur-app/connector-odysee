// SPDX-License-Identifier: AGPL-3.0-or-later

//! Live tests against the public Odysee backend, anonymous. Ignored by default:
//!
//! ```text
//! cargo nextest run --run-ignored ignored-only
//! ```

// Pages of items are too long to print on failure.
#![allow(clippy::assert_is_empty)]

use flaneur_connector::{
    ChannelTab, CommentSort, Connector, Error, FeedKind, SearchFilters, SearchResult, Target,
};
use flaneur_connector_odysee::Odysee;
use flaneur_model::{ChannelRef, ItemKind, ItemRef, ServiceId};

/// `@Odysee`.
const ODYSEE_CHANNEL: &str = "80d2590ad04e36fb1d077a9b9e3a8bba76defdf8";
/// "Miss Information" on `@Odysee`: a video with many comments.
const VIDEO: &str = "d2bd744f12b05ed0bc1fee5030a55dbd73258e38";
/// "Channel Roundup: May 2026" on `@Odysee`: transcoded, so with an HLS ladder.
const TRANSCODED: &str = "ee58d95e7e42ec9479e13b28d57556f9ffb373dc";

fn odysee() -> Odysee {
    Odysee::new().unwrap()
}

fn service() -> ServiceId {
    ServiceId::new("odysee")
}

#[tokio::test]
#[ignore = "live"]
async fn feeds() {
    let odysee = odysee();
    let feeds = odysee.feeds().unwrap();
    let home = feeds.feed(FeedKind::Home, None).await.unwrap();
    assert!(!home.items.is_empty());
    assert!(home.next.is_some());
    let second = feeds
        .feed(FeedKind::Home, home.next.as_ref())
        .await
        .unwrap();
    assert!(!second.items.is_empty());

    let trending = feeds.feed(FeedKind::Trending, None).await.unwrap();
    assert!(trending.items.len() >= 10);
    assert!(
        trending
            .items
            .iter()
            .all(|i| i.flags.sensitive == Some(false))
    );

    let shorts = feeds.feed(FeedKind::Shorts, None).await.unwrap();
    assert!(!shorts.items.is_empty());
    assert!(shorts.items.iter().all(|i| i.kind == ItemKind::Short));

    assert!(matches!(
        feeds.feed(FeedKind::Subscriptions, None).await,
        Err(Error::SignInRequired)
    ));
}

#[tokio::test]
#[ignore = "live"]
async fn search() {
    let odysee = odysee();
    let search = odysee.search().unwrap();
    let page = search
        .search("odysee", &SearchFilters::default(), None)
        .await
        .unwrap();
    assert!(
        page.items
            .iter()
            .any(|r| matches!(r, SearchResult::Channel(_)))
    );
    assert!(
        page.items
            .iter()
            .any(|r| matches!(r, SearchResult::Item(_)))
    );
    let next = search
        .search("odysee", &SearchFilters::default(), page.next.as_ref())
        .await
        .unwrap();
    assert!(!next.items.is_empty());
}

#[tokio::test]
#[ignore = "live"]
async fn channel_page() {
    let odysee = odysee();
    let channels = odysee.channels().unwrap();
    let channel = ChannelRef::new(service(), ODYSEE_CHANNEL);
    let header = channels.channel(&channel).await.unwrap();
    assert_eq!(header.handle.as_deref(), Some("@Odysee"));
    assert!(!header.summary.avatars.is_empty());

    let videos = channels
        .items(&channel, ChannelTab::Videos, None)
        .await
        .unwrap();
    assert!(!videos.items.is_empty());
    assert!(videos.total.is_some());
    let more = channels
        .items(&channel, ChannelTab::Videos, videos.next.as_ref())
        .await
        .unwrap();
    assert!(!more.items.is_empty());

    channels
        .items(&channel, ChannelTab::Lives, None)
        .await
        .unwrap();
    let playlists = channels.playlists(&channel, None).await.unwrap();
    assert!(!playlists.items.is_empty());

    // A short link resolves too.
    let by_name = ChannelRef::new(service(), "@Odysee#8");
    assert_eq!(
        channels.channel(&by_name).await.unwrap().summary.r#ref.id,
        ODYSEE_CHANNEL
    );
}

#[tokio::test]
#[ignore = "live"]
async fn resolve_and_related() {
    let odysee = odysee();
    let player = odysee.player();
    let item = ItemRef::new(service(), VIDEO);
    let playable = player.resolve(&item).await.unwrap();
    assert_eq!(playable.streams.len(), 1);
    assert_eq!(playable.streams[0].container, flaneur_model::Container::Mp4);
    assert!(playable.item.reactions.likes.is_some());
    assert!(playable.item.channel.is_some());

    let transcoded = player
        .resolve(&ItemRef::new(service(), TRANSCODED))
        .await
        .unwrap();
    let hls = transcoded.hls_manifest.unwrap();
    assert!(hls.as_str().ends_with("/master.m3u8"));

    let related = player.related(&item, None).await.unwrap();
    assert!(!related.items.is_empty());
    assert!(related.items.iter().all(|i| i.r#ref.id != VIDEO));

    // From a shared link with short claim ids.
    let Some(Target::Item(shared)) = odysee.parse_url("https://odysee.com/@Odysee:8/missinfo:d")
    else {
        panic!("not an item");
    };
    let playable = player.resolve(&shared).await.unwrap();
    assert_eq!(playable.item.r#ref.id, VIDEO);
}

#[tokio::test]
#[ignore = "live"]
async fn livestreams_are_not_played_yet() {
    let odysee = odysee();
    // `@RT`'s permanent livestream claim.
    let live = ItemRef::new(service(), "d826937ad9bf3b7991eada5034c4612389583bc1");
    match odysee.player().resolve(&live).await {
        Err(Error::Unavailable(reason)) => assert!(reason.contains("live")),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
#[ignore = "live"]
async fn comments_and_replies() {
    let odysee = odysee();
    let comments = odysee.comments().unwrap();
    let item = ItemRef::new(service(), VIDEO);
    let page = comments
        .comments(&item, CommentSort::Top, None)
        .await
        .unwrap();
    assert!(!page.items.is_empty());
    assert!(page.next.is_some());
    assert!(page.items.iter().any(|c| c.likes.is_some()));
    assert!(page.items.iter().any(|c| !c.author.avatars.is_empty()));
    let with_replies = page
        .items
        .iter()
        .find(|c| c.replies_token.is_some())
        .unwrap();
    let replies = comments
        .replies(&item, with_replies.replies_token.as_deref().unwrap(), None)
        .await
        .unwrap();
    assert!(!replies.items.is_empty());

    let newest = comments
        .comments(&item, CommentSort::Newest, None)
        .await
        .unwrap();
    assert!(!newest.items.is_empty());
}
