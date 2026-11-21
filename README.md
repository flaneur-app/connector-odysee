# Flâneur connector: Odysee

The Odysee connector of [Flâneur](https://github.com/flaneur-app): the public Odysee backend for content and accounts, and Commentron for comments. It implements the connector traits of [`flaneur-app/core`](https://github.com/flaneur-app/core).

> Flâneur is an independent, open-source project. It is not affiliated with, endorsed by or
> sponsored by Google, YouTube, Odysee, LBRY or any other service it can connect to. All
> trademarks belong to their owners.

## What works

Everything goes from the user's machine to Odysee: there is no Flâneur server in between.

| Feature | Without an account | Signed in |
| --- | --- | --- |
| Home (Odysee's curated categories), trending, shorts | ✅ | ✅ |
| Feed of the followed channels | — | ✅ (read from the account) |
| Search (videos, channels, playlists) | ✅ | ✅ |
| Channel pages: videos, shorts, lives, playlists | ✅ | ✅ (+ follower count) |
| Playback: original file + HLS when transcoded | ✅ | ✅ (+ resume position) |
| Related items | ✅ | ✅ |
| Comments and replies | ✅ read | ✅ read, post with the account's channel |
| Likes and dislikes | counts | ✅ react |
| History, watch later, favourites | — | ✅ read |
| Follow, unfollow, add to watch later | — | ❌ on purpose, see below |
| Livestreams | listed | ❌ not played yet, see below |

Playback offers the original file and, once Odysee has transcoded it, the HLS ladder. A 4 KiB
read of the file's head decides: when the CDN refuses the file, when its MP4 index comes after the
media (the player would have to fetch the end of the file before the first frame), or when it is
over 4 GiB, only HLS is offered if it exists. The rule is documented in `src/player.rs`.

Sign-in: email and password, or a link sent by email (`Connector::send_sign_in_link`, then
`poll_sign_in_link`). The session token is handed to the app, which keeps it in the OS keyring.

**Not done on purpose:**

- **Follows and watch later are read, never written.** Odysee keeps them in one JSON object of the
  user's hosted wallet that every Odysee client syncs, and a write replaces the whole object: a bug
  would wipe the user's follows everywhere.
- **Livestreams are not played.** Their HLS only answers requests that carry an `odysee.com`
  `Referer`, and Flâneur does not pretend to be the Odysee web app.
- **No paid, rented or members-only content**, and no downloads.

## Usage

```rust,no_run
use flaneur_connector::{Connector, FeedKind};
use flaneur_connector_odysee::Odysee;

# async fn run() -> Result<(), flaneur_connector::Error> {
let odysee = Odysee::new()?;
let home = odysee.feeds().unwrap().feed(FeedKind::Home, None).await?;
let playable = odysee.player().resolve(&home.items[0].r#ref).await?;
# Ok(())
# }
```

`cargo run --example odysee [query]` prints the home feed, a search, and the streams of one video.

## Development

See [AGENTS.md](AGENTS.md) for the conventions and the commands. The shared dev container lives in
[`flaneur-app/.dev`](https://github.com/flaneur-app/.dev).

## License

[AGPL-3.0-or-later](LICENSE).
