# AGENTS.md — Flâneur `connector-odysee`

This repository inherits the [canonical workspace rules](https://github.com/flaneur-app/.dev/blob/main/AGENTS.md). Only project-specific details are documented here.

## Scope

The Odysee connector of [Flâneur](https://github.com/flaneur-app): the public Odysee backend for content and accounts, and Commentron for comments. It implements the connector traits of [`flaneur-app/core`](https://github.com/flaneur-app/core).

**Status:** first version: anonymous reads verified live; account writes (sign-in, reactions,
comments) only verified against a mock server, no Odysee test account yet.

## Structure

```text
src/
├── lib.rs          Odysee, its builder, the Connector impl and Capabilities
├── http.rs         reqwest client, Endpoints, the three wire formats, error mapping
├── claim.rs        LBRY claims → Item, Channel, Playlist (kinds, shorts rule, mature tags)
├── links.rs        odysee.com and lbry:// URLs, ref ids
├── auth.rs         user/new, password and email-link sign-in, session secret
├── preference.rs   the wallet's `shared` preference (read-only: follows, collections)
├── feeds.rs        home categories, trending, shorts, subscription feed
├── search.rs       Lighthouse search
├── channels.rs     channel header, tabs, collections
├── player.rs       CDN streams, HLS probe, reactions, related (Recsys)
├── comments.rs     Commentron read, post through the proxy's channel_sign
└── library.rs      history, watch later, follows, reactions
tests/
├── mock.rs         exact requests and fixture parsing, against wiremock
├── live.rs         #[ignore] tests against the real Odysee backend
└── fixtures/       trimmed real responses
examples/odysee.rs  home, a search and a resolve, live
```

Ref ids are full claim ids, or an LBRY URL without its scheme (`@Odysee#8/missinfo#d`) when they
come from a shared link with short claim ids.

The research behind the endpoints is in
[`flaneur-app/spikes`](https://github.com/flaneur-app/spikes) (`docs/odysee-research.md`).

## Rules specific to Odysee

- Never write the `shared` preference (`preference_set`, `sync_apply`, `sync/set`): it holds the
  follows and collections of every Odysee client of the user. Lifting this needs a read-modify-write
  that keeps unknown keys, the `old_hash` conflict check, and a test account.
- Never send a `Referer` or `Origin` of `odysee.com`.
- Reads go to the proxy without `X-Lbry-Auth-Token`: anonymous calls are cached better.

## License

**AGPL-3.0-or-later**. Every source file starts with an SPDX header:

```rust
// SPDX-License-Identifier: AGPL-3.0-or-later
```

Dependencies must have AGPL-compatible licenses; `cargo deny check` enforces the list in `deny.toml`.

## Naming and legal positioning

- Never use "YouTube" or "tube" in the name of the app, a crate meant for end users, or the org.
  Service names appear only where they describe what a connector talks to.
- No ad blocking and no downloading as advertised features.
- Cookies and tokens stay on the user's machine (OS keyring). No code path sends them anywhere but
  to the service they belong to.
- Every README keeps the non-affiliation notice.

## Commit Scopes

Defined in `scopes.json`:

| Scope | Covers |
| --- | --- |
| `connector` | Connector implementation: claims, search, following, reactions, comments |
| `auth` | Account sign-in and session handling |
| `player` | Stream resolution |
| `build`, `test`, `docs`, `deps`, `ci`, `ai` | As usual |

## Commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --no-tests=pass
cargo deny check
cargo nextest run --run-ignored ignored-only   # live tests, network needed
cargo run --example odysee [query]             # live example
```
