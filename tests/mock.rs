// SPDX-License-Identifier: AGPL-3.0-or-later

//! The requests the connector builds, against a mock server: every write (sign-in, reactions,
//! comments) and the parsing of trimmed real responses (`tests/fixtures`).
//!
//! The writes were never run against a real Odysee account: these tests pin the exact requests
//! the web app sends, as read in its source.

// Pages of items are too long to print on failure.
#![allow(clippy::assert_is_empty)]

use flaneur_connector::{
    ChannelTab, CommentSort, Connector, Credentials, Error, FeedKind, SearchFilters, SearchResult,
    Session,
};
use flaneur_connector_odysee::{DefaultChannel, Endpoints, Odysee};
use flaneur_model::{ChannelRef, ItemKind, ItemRef, PlaylistRef, Reaction, ServiceId};
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{body_partial_json, body_string, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const VIDEO: &str = "d2bd744f12b05ed0bc1fee5030a55dbd73258e38";
const VIDEO_SD_HASH: &str = "c2d5d142f296a782fab44239a6ec2f0e7ff62832cb7d438c4458849a6a66845b44125c32d0fad8692ac091dc61ce4fa6";
const LIVE: &str = "d826937ad9bf3b7991eada5034c4612389583bc1";
const ODYSEE_CHANNEL: &str = "80d2590ad04e36fb1d077a9b9e3a8bba76defdf8";
const MY_CHANNEL: &str = "1234567890abcdef1234567890abcdef12345678";
const TOKEN: &str = "test-token-0123456789abcdef012345";

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn service() -> ServiceId {
    ServiceId::new("odysee")
}

async fn setup() -> (MockServer, Odysee) {
    let server = MockServer::start().await;
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    let odysee = Odysee::builder()
        .endpoints(Endpoints::with_base(&base))
        .build()
        .unwrap();
    (server, odysee)
}

fn ok(data: Value) -> ResponseTemplate {
    let mut body = json!({"success": true, "error": null});
    body["data"] = data;
    ResponseTemplate::new(200).set_body_json(body)
}

fn rpc(result: Value) -> ResponseTemplate {
    let mut body = json!({"jsonrpc": "2.0", "id": 1});
    body["result"] = result;
    ResponseTemplate::new(200).set_body_json(body)
}

fn internal(action: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST")).and(path(format!("/internal/{action}")))
}

fn proxy(rpc_method: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/proxy"))
        .and(query_param("m", rpc_method))
}

fn commentron(rpc_method: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/comments"))
        .and(query_param("m", rpc_method))
}

fn verified_me() -> ResponseTemplate {
    ok(json!({"id": 42, "primary_email": "me@example.org", "has_verified_email": true}))
}

fn session(channel: Option<&str>) -> Session {
    let channel = channel.map(|id| json!({"claim_id": id, "name": "@me"}));
    Session {
        service: service(),
        account: None,
        secret:
            json!({"v": 1, "auth_token": TOKEN, "app_id": "app", "user_id": 42, "channel": channel})
                .to_string(),
    }
}

/// A connector signed in with [`TOKEN`] and, when given, a default channel.
async fn signed_in(channel: Option<&str>) -> (MockServer, Odysee) {
    let (server, odysee) = setup().await;
    internal("user/me")
        .and(body_string(format!("auth_token={TOKEN}")))
        .respond_with(verified_me())
        .mount(&server)
        .await;
    if channel.is_none() {
        proxy("channel_list")
            .respond_with(rpc(json!({"items": []})))
            .mount(&server)
            .await;
        proxy("preference_get")
            .respond_with(rpc(json!({"shared": null})))
            .mount(&server)
            .await;
    }
    odysee.restore(&session(channel)).await.unwrap();
    server.reset().await;
    (server, odysee)
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).and_then(|v| v.to_str().ok())
}

fn form(request: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

// --- Accounts -------------------------------------------------------------------------------

#[tokio::test]
async fn password_sign_in_upgrades_a_fresh_anonymous_token() {
    let (server, odysee) = setup().await;
    internal("user/new")
        .respond_with(ok(json!({"id": 42, "auth_token": TOKEN})))
        .expect(1)
        .mount(&server)
        .await;
    internal("user/signin")
        .and(body_string(format!(
            "auth_token={TOKEN}&email=me%40example.org&password=hunter2"
        )))
        .respond_with(ok(json!({"id": 42})))
        .expect(1)
        .mount(&server)
        .await;
    internal("user/me")
        .respond_with(verified_me())
        .mount(&server)
        .await;
    proxy("channel_list")
        .and(wiremock::matchers::header("X-Lbry-Auth-Token", TOKEN))
        .respond_with(rpc(json!({"items": [
            {"claim_id": "a".repeat(40), "name": "@rich", "meta": {"effective_amount": "50.0"}},
            {"claim_id": MY_CHANNEL, "name": "@me", "meta": {"effective_amount": "1.0"}},
        ]})))
        .mount(&server)
        .await;
    proxy("preference_get")
        .and(body_partial_json(json!({"params": {"key": "shared"}})))
        .respond_with(rpc(
            json!({"shared": {"type": "object", "version": "0.1", "value": {
                "settings": {"active_channel_claim": MY_CHANNEL}
            }}}),
        ))
        .mount(&server)
        .await;

    assert!(!odysee.capabilities().signed_in);
    let session = odysee
        .sign_in(Credentials::Password {
            login: "me@example.org".into(),
            password: "hunter2".into(),
        })
        .await
        .unwrap();
    assert!(odysee.capabilities().signed_in);
    assert_eq!(session.account.as_ref().unwrap().name, "@me");
    assert!(!session.secret.contains("hunter2"));
    assert!(session.secret.contains(TOKEN));
    // The channel the user picked on odysee.com wins over the one with the most credits.
    assert_eq!(
        odysee.default_channel(),
        Some(DefaultChannel {
            claim_id: MY_CHANNEL.into(),
            name: "@me".into()
        })
    );

    let requests = server.received_requests().await.unwrap();
    let new_user = form(&requests[0]);
    assert_eq!(new_user[0], ("auth_token".into(), String::new()));
    assert_eq!(new_user[1], ("language".into(), "en".into()));
    assert_eq!(new_user[2].0, "app_id");
    assert_eq!(new_user[2].1.len(), 66);
}

#[tokio::test]
async fn sign_in_that_odysee_wants_confirmed_by_email() {
    let (server, odysee) = setup().await;
    internal("user/new")
        .respond_with(ok(json!({"id": 42, "auth_token": TOKEN})))
        .mount(&server)
        .await;
    internal("user/signin")
        .respond_with(ResponseTemplate::new(417).set_body_json(
            json!({"success": false, "error": "email verification required", "data": null}),
        ))
        .mount(&server)
        .await;
    let result = odysee
        .sign_in(Credentials::Password {
            login: "me@example.org".into(),
            password: "hunter2".into(),
        })
        .await;
    assert!(matches!(result, Err(Error::Unavailable(_))));
    assert!(!odysee.capabilities().signed_in);
}

#[tokio::test]
async fn email_link_sign_in_polls_until_the_link_is_opened() {
    let (server, odysee) = setup().await;
    internal("user/new")
        .respond_with(ok(json!({"id": 42, "auth_token": TOKEN})))
        .mount(&server)
        .await;
    internal("user/exists")
        .and(body_string(format!(
            "auth_token={TOKEN}&email=me%40example.org"
        )))
        .respond_with(ok(json!({"has_password": false})))
        .mount(&server)
        .await;
    internal("user_email/resend_token")
        .and(body_string(format!(
            "auth_token={TOKEN}&email=me%40example.org&only_if_expired=true"
        )))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    internal("user/me")
        .respond_with(ok(json!({"id": 42, "has_verified_email": false})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    internal("user/me")
        .respond_with(verified_me())
        .mount(&server)
        .await;
    proxy("channel_list")
        .respond_with(rpc(json!({"items": []})))
        .mount(&server)
        .await;
    proxy("preference_get")
        .respond_with(rpc(json!({})))
        .mount(&server)
        .await;

    let pending = odysee.send_sign_in_link("me@example.org").await.unwrap();
    assert!(!format!("{pending:?}").contains(TOKEN));
    assert!(pending.message.is_some());
    assert!(odysee.poll_sign_in_link(&pending).await.unwrap().is_none());
    let session = odysee.poll_sign_in_link(&pending).await.unwrap().unwrap();
    assert_eq!(
        session.account.unwrap().handle.as_deref(),
        Some("me@example.org")
    );
    assert!(odysee.capabilities().signed_in);
    assert_eq!(odysee.default_channel(), None);
}

#[tokio::test]
async fn restore_rejects_an_expired_token() {
    let (server, odysee) = setup().await;
    internal("user/me")
        .respond_with(ResponseTemplate::new(403).set_body_json(
            json!({"success": false, "error": "could not authenticate user", "data": null}),
        ))
        .mount(&server)
        .await;
    assert!(matches!(
        odysee.restore(&session(Some(MY_CHANNEL))).await,
        Err(Error::SignInRequired)
    ));
    assert!(!odysee.capabilities().signed_in);
}

#[tokio::test]
async fn capabilities_follow_the_session() {
    use flaneur_connector::Support;
    let (_server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    let caps = odysee.capabilities();
    assert!(caps.signed_in);
    assert!(caps.comments_write.available(true));
    assert!(caps.comment_replies_write.available(true));
    assert_eq!(caps.public_playlists, Support::Anonymous);
    assert_eq!(caps.playlists_read, Support::SignedIn);
    assert!(
        odysee
            .info()
            .auth_methods
            .contains(&flaneur_connector::AuthMethod::EmailLink)
    );
    assert_eq!(caps.subscriptions_read, Support::SignedIn);
    assert_eq!(caps.subscriptions_write, Support::Unsupported);
    assert_eq!(caps.playlists_write, Support::Unsupported);
    assert_eq!(caps.history_write, Support::Unsupported);
    odysee.sign_out().await;
    assert!(!odysee.capabilities().signed_in);
    assert_eq!(odysee.default_channel(), None);
}

#[tokio::test]
async fn shared_preference_writes_are_never_attempted() {
    let (server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    let channel = ChannelRef::new(service(), ODYSEE_CHANNEL);
    let item = ItemRef::new(service(), VIDEO);
    let watch_later = odysee.library().unwrap().watch_later().unwrap();
    assert!(matches!(
        odysee
            .subscriptions()
            .unwrap()
            .set_subscribed(&channel, true)
            .await,
        Err(Error::Unsupported)
    ));
    assert!(matches!(
        odysee
            .library()
            .unwrap()
            .add_to_playlist(&watch_later, &item)
            .await,
        Err(Error::Unsupported)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// --- Reactions ------------------------------------------------------------------------------

#[tokio::test]
async fn like_sends_the_web_app_form() {
    let (server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    internal("reaction/react")
        .and(body_string(format!(
            "auth_token={TOKEN}&claim_ids={VIDEO}&type=like&clear_types=dislike"
        )))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    internal("reaction/react")
        .and(body_string(format!(
            "auth_token={TOKEN}&claim_ids={VIDEO}&type=dislike&clear_types=like"
        )))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    let reactions = odysee.reactions().unwrap();
    let item = ItemRef::new(service(), VIDEO);
    reactions.react(&item, Reaction::Like).await.unwrap();
    reactions.react(&item, Reaction::Dislike).await.unwrap();
}

#[tokio::test]
async fn clearing_a_reaction_removes_the_one_set() {
    let (server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    internal("reaction/list")
        .and(body_string(format!("auth_token={TOKEN}&claim_ids={VIDEO}")))
        .respond_with(ok(json!({
            "my_reactions": {VIDEO: {"like": 1, "dislike": 0}},
            "others_reactions": {VIDEO: {"like": 10, "dislike": 2}},
        })))
        .mount(&server)
        .await;
    internal("reaction/react")
        .and(body_string(format!(
            "auth_token={TOKEN}&claim_ids={VIDEO}&type=like&remove=true"
        )))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    odysee
        .reactions()
        .unwrap()
        .react(&ItemRef::new(service(), VIDEO), Reaction::None)
        .await
        .unwrap();
}

#[tokio::test]
async fn reactions_need_a_session() {
    let (server, odysee) = setup().await;
    assert!(matches!(
        odysee
            .reactions()
            .unwrap()
            .react(&ItemRef::new(service(), VIDEO), Reaction::Like)
            .await,
        Err(Error::SignInRequired)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// --- Comments -------------------------------------------------------------------------------

#[tokio::test]
async fn posting_a_reply_signs_through_the_proxy() {
    let (server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    proxy("channel_sign")
        .and(wiremock::matchers::header("X-Lbry-Auth-Token", TOKEN))
        .and(body_partial_json(json!({
            "jsonrpc": "2.0",
            "method": "channel_sign",
            "params": {"channel_id": MY_CHANNEL, "hexdata": "68c3a9"},
        })))
        .respond_with(rpc(
            json!({"signature": "ab".repeat(64), "signing_ts": "1795223000"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    commentron("comment.Create")
        .and(body_partial_json(json!({
            "method": "comment.Create",
            "params": {
                "comment": "hé",
                "claim_id": VIDEO,
                "parent_id": "parent",
                "channel_id": MY_CHANNEL,
                "channel_name": "@me",
                "signature": "ab".repeat(64),
                "signing_ts": "1795223000",
            },
        })))
        .respond_with(rpc(json!({
            "comment": "hé", "comment_id": "new", "claim_id": VIDEO, "parent_id": "parent",
            "timestamp": 1_795_223_001, "channel_id": MY_CHANNEL, "channel_name": "@me",
            "currency": "", "support_amount": 0, "is_hidden": false, "is_pinned": false,
            "is_fiat": false, "is_protected": false,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let comment = odysee
        .comments()
        .unwrap()
        .post(&ItemRef::new(service(), VIDEO), "hé", Some("parent"))
        .await
        .unwrap();
    assert_eq!(comment.id, "new");
    assert_eq!(comment.text, "hé");
    assert_eq!(comment.author.name, "@me");
    assert_eq!(comment.reply_count, Some(0));
    assert_eq!(comment.reply_token.as_deref(), Some("new"));

    // The token goes to the proxy only, never to Commentron.
    for request in server.received_requests().await.unwrap() {
        if request.url.path() == "/comments" {
            assert!(header(&request, "X-Lbry-Auth-Token").is_none());
            assert!(!String::from_utf8_lossy(&request.body).contains(TOKEN));
        }
    }
}

#[tokio::test]
async fn posting_without_a_channel_explains_why() {
    let (server, odysee) = signed_in(None).await;
    let result = odysee
        .comments()
        .unwrap()
        .post(&ItemRef::new(service(), VIDEO), "hi", None)
        .await;
    assert!(matches!(result, Err(Error::Unavailable(_))));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn parses_a_comment_page() {
    let (server, odysee) = setup().await;
    commentron("comment.List")
        .and(body_partial_json(json!({"params": {
            "claim_id": VIDEO, "page": 1, "page_size": 30, "top_level": true, "sort_by": 3
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("comment_list.json")))
        .mount(&server)
        .await;
    commentron("reaction.List")
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("comment_reactions.json")))
        .mount(&server)
        .await;
    proxy("claim_search")
        .respond_with(rpc(json!({"items": []})))
        .mount(&server)
        .await;
    let page = odysee
        .comments()
        .unwrap()
        .comments(&ItemRef::new(service(), VIDEO), CommentSort::Top, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.total, Some(511));
    assert_eq!(page.next.as_ref().unwrap().0, "2");
    let first = &page.items[0];
    assert_eq!(first.author.name, "@YeRudITea");
    assert_eq!(first.likes, Some(244));
    assert_eq!(first.reply_count, Some(11));
    assert_eq!(first.replies_token.as_deref(), Some(first.id.as_str()));
    assert_eq!(first.reply_token.as_deref(), Some(first.id.as_str()));
    assert_eq!(first.published.unwrap().at, 1_670_435_179);
    assert!(!first.hearted && !first.pinned);
    // Not in the reaction fixture: unknown, not zero.
    assert_eq!(page.items[2].likes, None);
}

// --- Library --------------------------------------------------------------------------------

#[tokio::test]
async fn history_reads_view_history_and_resume_positions() {
    let (server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    internal("user/view_history")
        .and(body_string(format!(
            "auth_token={TOKEN}&page=1&page_size=30"
        )))
        .respond_with(ok(json!({
            "items": [{"claim_id": VIDEO, "updated_at": "2026-11-20T10:00:00Z"}],
            "page": 1,
            "has_more": true,
        })))
        .mount(&server)
        .await;
    proxy("claim_search")
        .and(body_partial_json(json!({"params": {"claim_ids": [VIDEO]}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;
    internal("file/last_positions")
        .and(body_string(format!("auth_token={TOKEN}&claim_ids={VIDEO}")))
        .respond_with(ok(json!({VIDEO: 42.5})))
        .mount(&server)
        .await;
    let page = odysee.library().unwrap().history(None).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next.as_ref().unwrap().0, "2");
    let watch = page.items[0].watch.unwrap();
    assert_eq!(watch.watched, None);
    let progress = watch.progress.unwrap();
    assert_eq!(progress.position, 42);
    assert_eq!(progress.updated_at, 1_795_168_800);
}

#[tokio::test]
async fn subscriptions_and_watch_later_come_from_the_shared_preference() {
    let (server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    proxy("preference_get")
        .and(wiremock::matchers::header("X-Lbry-Auth-Token", TOKEN))
        .respond_with(rpc(json!({"shared": {"type": "object", "version": "0.1", "value": {
            "following": [{"uri": format!("lbry://@Odysee#{ODYSEE_CHANNEL}"), "notificationsDisabled": true}],
            "subscriptions": [format!("lbry://@Odysee#{ODYSEE_CHANNEL}")],
            "builtinCollections": {"watchlater": {"id": "watchlater", "name": "Watch Later",
                "items": [format!("lbry://missinfo#{VIDEO}")], "type": "playlist"}},
        }}})))
        .mount(&server)
        .await;
    proxy("claim_search")
        .and(body_partial_json(json!({"params": {
            "channel_ids": [ODYSEE_CHANNEL], "order_by": ["release_time"], "has_source": true
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;
    proxy("claim_search")
        .and(body_partial_json(
            json!({"params": {"claim_ids": [ODYSEE_CHANNEL]}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;
    proxy("claim_search")
        .and(body_partial_json(json!({"params": {"claim_ids": [VIDEO]}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;

    let feed = odysee
        .feeds()
        .unwrap()
        .feed(FeedKind::Subscriptions, None)
        .await
        .unwrap();
    assert!(feed.items.iter().any(|i| i.r#ref.id == VIDEO));

    let follows = odysee.subscriptions().unwrap().list(None).await.unwrap();
    assert_eq!(follows.items.len(), 1);
    assert_eq!(follows.items[0].name, "Odysee");
    assert_eq!(follows.total, Some(1));

    let library = odysee.library().unwrap();
    let playlists = library.playlists(None).await.unwrap();
    assert_eq!(playlists.items[0].item_count, Some(1));
    assert_eq!(
        playlists.items[0].kind,
        flaneur_model::PlaylistKind::WatchLater
    );
    assert_eq!(
        playlists.items[1].kind,
        flaneur_model::PlaylistKind::Regular
    );
    let watch_later = library
        .playlist_items(&PlaylistRef::new(service(), "watchlater"), None)
        .await
        .unwrap();
    assert_eq!(watch_later.items.len(), 1);
    assert_eq!(watch_later.items[0].r#ref.id, VIDEO);
}

// --- Anonymous reads ------------------------------------------------------------------------

#[tokio::test]
async fn home_turns_categories_into_claim_searches() {
    let (server, odysee) = setup().await;
    Mock::given(method("GET"))
        .and(path("/homepage"))
        .and(query_param("hp", "en"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("homepage.json")))
        .expect(1)
        .mount(&server)
        .await;
    proxy("claim_search")
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;

    let feeds = odysee.feeds().unwrap();
    let first = feeds.feed(FeedKind::Home, None).await.unwrap();
    assert!(!first.items.is_empty());
    assert_eq!(first.next.as_ref().unwrap().0, "1");
    let second = feeds
        .feed(FeedKind::Home, first.next.as_ref())
        .await
        .unwrap();
    assert!(second.next.is_none());

    let requests = server.received_requests().await.unwrap();
    let searches: Vec<Value> = requests
        .iter()
        .filter(|r| r.url.path() == "/proxy")
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["params"].clone())
        .collect();
    // "Featured" first: "Discover" has no channels and "Spirituality" is hidden by default.
    let categories = &fixture("homepage.json")["data"]["en"]["categories"];
    let featured = &searches[0];
    assert_eq!(
        featured["channel_ids"],
        categories["PRIMARY_CONTENT"]["channelIds"]
    );
    assert_eq!(
        searches[1]["channel_ids"],
        categories["TECHNOLOGY"]["channelIds"]
    );
    assert_eq!(
        featured["order_by"],
        json!(["trending_group", "trending_mixed"])
    );
    assert_eq!(featured["claim_type"], json!(["stream", "repost"]));
    assert_eq!(featured["limit_claims_per_channel"], json!(1));
    assert_eq!(featured["exclude_shorts"], json!(true));
    assert_eq!(featured["page_size"], json!(24));
    assert!(featured["release_time"].as_str().unwrap().starts_with('>'));
    assert_eq!(searches[1]["limit_claims_per_channel"], json!(2));
    assert_eq!(searches[1]["claim_type"], json!(["stream"]));
}

#[tokio::test]
async fn anonymous_reads_never_send_the_token() {
    let (server, odysee) = signed_in(Some(MY_CHANNEL)).await;
    proxy("claim_search")
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;
    let page = odysee
        .feeds()
        .unwrap()
        .feed(FeedKind::Trending, None)
        .await
        .unwrap();
    // The markdown post, the channel and the collection of the fixture are not items.
    assert_eq!(page.items.len(), 6);
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| header(r, "X-Lbry-Auth-Token").is_none())
    );
    let ua = header(&requests[0], "user-agent").unwrap();
    assert!(ua.starts_with("Flaneur/") && ua.ends_with("(+https://flaneur.baxyz.dev)"));
}

#[tokio::test]
async fn search_hydrates_lighthouse_hits_in_order() {
    let (server, odysee) = setup().await;
    // The real fixture parses; its claims are not in the claim fixture, so it hydrates to nothing.
    let hits = fixture("lighthouse_search.json");
    assert_eq!(hits.as_array().unwrap().len(), 4);
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("s", "odysee"))
        .and(query_param("claimType", "file,channel"))
        .and(query_param("nsfw", "false"))
        .and(query_param("free_only", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"claimId": "fcac33d8c03197f239274717824356433bb8aad1", "name": "our-channels"},
            {"claimId": VIDEO, "name": "missinfo", "channel_claim_id": ODYSEE_CHANNEL},
            {"claimId": "50b203a2d44cf71482dcad17b7b1adeb3e1de97b", "name": "channel-roundup-october-2026"},
            {"claimId": ODYSEE_CHANNEL, "name": "@Odysee", "channel_claim_id": null},
        ])))
        .mount(&server)
        .await;
    proxy("claim_search")
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;
    let page = odysee
        .search()
        .unwrap()
        .search("odysee", &SearchFilters::default(), None)
        .await
        .unwrap();
    let kinds: Vec<&str> = page
        .items
        .iter()
        .map(|r| match r {
            SearchResult::Item(_) => "item",
            SearchResult::Channel(_) => "channel",
            SearchResult::Playlist(_) => "playlist",
        })
        .collect();
    assert_eq!(kinds, ["playlist", "item", "channel"]);
    assert!(page.next.is_none());
}

#[tokio::test]
async fn resolve_builds_the_cdn_urls() {
    let (server, odysee) = setup().await;
    proxy("claim_search")
        .and(body_partial_json(json!({"params": {"claim_ids": [VIDEO]}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/cdn/v6/streams/{VIDEO}/{VIDEO_SD_HASH}/master.m3u8")))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-STREAM-INF:BANDWIDTH=4026000,RESOLUTION=1920x1080\nv0.m3u8\n",
        ))
        .mount(&server)
        .await;
    internal("reaction/list")
        .and(body_string(format!("claim_ids={VIDEO}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("reaction_list.json")))
        .mount(&server)
        .await;

    let playable = odysee
        .player()
        .resolve(&ItemRef::new(service(), VIDEO))
        .await
        .unwrap();
    let base = server.uri();
    assert_eq!(
        playable.streams[0].url.as_str(),
        format!("{base}/cdn/v6/streams/{VIDEO}/c2d5d1.mp4")
    );
    assert_eq!(playable.streams[0].size, Some(87_603_020));
    assert_eq!(
        playable.hls_manifest.unwrap().as_str(),
        format!("{base}/cdn/v6/streams/{VIDEO}/{VIDEO_SD_HASH}/master.m3u8")
    );
    assert_eq!(playable.item.reactions.likes, Some(2269));
    assert_eq!(playable.item.reactions.dislikes, Some(207));
    assert_eq!(playable.item.kind, ItemKind::Video);
    // No Referer, no Origin: Flâneur does not pose as odysee.com.
    for request in server.received_requests().await.unwrap() {
        assert!(header(&request, "referer").is_none());
        assert!(header(&request, "origin").is_none());
    }
}

#[tokio::test]
async fn resolve_without_hls_and_of_a_livestream() {
    let (server, odysee) = setup().await;
    proxy("claim_search")
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("claim_search_mixed.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/cdn/v6/streams/{VIDEO}/{VIDEO_SD_HASH}/master.m3u8"
        )))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_string("unable to serve fragment: transcoding is in progress"),
        )
        .mount(&server)
        .await;
    internal("reaction/list")
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("reaction_list.json")))
        .mount(&server)
        .await;
    let player = odysee.player();
    let playable = player
        .resolve(&ItemRef::new(service(), VIDEO))
        .await
        .unwrap();
    assert!(playable.hls_manifest.is_none());
    assert_eq!(playable.streams.len(), 1);
    match player.resolve(&ItemRef::new(service(), LIVE)).await {
        Err(Error::Unavailable(reason)) => assert_eq!(reason, "live playback not supported yet"),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn channel_tabs_and_short_links() {
    let (server, odysee) = setup().await;
    proxy("resolve")
        .and(body_partial_json(
            json!({"params": {"urls": ["lbry://@Odysee#8"]}}),
        ))
        .respond_with(rpc(json!({"lbry://@Odysee#8": {
            "claim_id": ODYSEE_CHANNEL, "name": "@Odysee", "value_type": "channel",
            "canonical_url": "lbry://@Odysee#8", "value": {"title": "Odysee"}
        }})))
        .mount(&server)
        .await;
    proxy("claim_search")
        .and(body_partial_json(json!({"params": {
            "channel_ids": [ODYSEE_CHANNEL], "has_no_source": true, "page": 2
        }})))
        .respond_with(rpc(
            json!({"items": [], "page": 2, "total_pages": 2, "total_items": 31}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let channels = odysee.channels().unwrap();
    let channel = ChannelRef::new(service(), "@Odysee#8");
    assert_eq!(
        channels.channel(&channel).await.unwrap().summary.r#ref.id,
        ODYSEE_CHANNEL
    );
    let page = channels
        .items(
            &channel,
            ChannelTab::Lives,
            Some(&flaneur_model::Cursor("2".into())),
        )
        .await
        .unwrap();
    assert_eq!(page.total, Some(31));
    assert!(page.next.is_none());
}
