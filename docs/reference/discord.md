# discord

The Discord bot core (126 KG nodes, MUSEX-13) — a bespoke social surface for Muse, not a
command-table reskin of existing request bots. It is the first surface that lets a
*friend* (a trusted human outside the Plex-account model) talk to Muse's brain from
Discord, and it deliberately reuses the real brain (`curation::recommend`, `persona`,
`taste_review::trace`) rather than inventing a second, Discord-specific
recommendation/rationale path.

Privacy is the load-bearing design: identity is **allowlisted** (`TrustedFriends`) and
consent is **private, default-false** — taste features activate only after an explicit,
atomic opt-in that links a Muse account.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `discord::identity::FriendIdentity` | struct | `src/discord/identity.rs` | Per-Discord-user record: `opt_in(muse_account_id)` grants consent atomically (flag + linked account together); `is_opted_in`/`linked_account` read it |
| `discord::identity::TrustedFriends` | struct | `src/discord/identity.rs` | The allowlist scoping who is served at all (`from_friends`, `remove`, `len`) — the subsystem's top-ranked KG symbols live here |
| `discord::client::DiscordClient` | trait | `src/discord/client.rs` | The client seam (send embed/reply); mirrors the crate's trait-plus-mock pattern |
| `discord::client::RichEmbed` | struct | `src/discord/client.rs` | Server-agnostic embed (title + poster art URL + synopsis) |
| `discord::client::MockDiscordClient` | struct | `src/discord/client.rs` | Test double; the real client is config-gated and documented best-effort (no live call in the test suite) |
| `discord::bot::decide_response_mode` | fn | `src/discord/bot.rs` | The default-private gate deciding what may be said where |
| `discord::bot::build_generic_reply` | fn | `src/discord/bot.rs` | Brain-driven reply construction for non-opted-in callers |

## How it connects

Consent state persists via `repo::friend_opt_in`; the opt-in/opt-out HTTP routes
(`discord::opt_in_route`) are mounted by `http::router` and are inert when the bot is
disabled or the caller isn't allowlisted (tested). Downstream consumers build on the
same identity/consent model instead of inventing their own: `premiere` invites and RSVPs
only `TrustedFriends`-allowlisted, opted-in friends; `promotion::targeting` targets only
opted-in friends above the match threshold; `watch_together` and the `kg` shared graph
apply the same opt-in filter.

## Configuration

- `DISCORD_BOT_TOKEN` — the bot credential; unset means the bot is **inert**: no live
  Discord API call is ever made and `RealDiscordClient::from_config` returns `None`.

## Notes and gaps

- There is no live Discord integration exercised in this crate's own test suite — the
  real client is a documented best-effort implementation behind the trait.
- The four-piece shape (identity / client / embed / bot) mirrors the seam pattern
  `cultural::source::TrendSource` and `watch_together::sync::ServerSyncPrimitive`
  established.
- Not covered here: the premiere announce/discussion flows built on top — see
  [premiere](premiere.md).
