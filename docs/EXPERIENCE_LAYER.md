# Muse Experience Layer (MUSEX-01 .. MUSEX-18)

This is the reference doc for the S118 "MUSEX" build: personas, the channel director, group
watch-together, the two-timescale adaptation loop, a conversational assistant, a bespoke Discord
bot, a what's-hot / cultural-relevance layer, a privacy-scoped knowledge-graph + visualizations,
and the Constellation GUI control panel that ties them together.

**Grounding.** Every claim below was checked against the actual source at `origin/main` (through
the MUSEX-18 merge, `446cd6b`) — module doc comments, function signatures, and (where wiring status
is asserted) `grep` for real callers. Where a module is fully implemented and tested but not
actually reachable in a running deployment, that is stated plainly, not glossed over. See
[`docs/MUSEX-experience-layer.md`](MUSEX-experience-layer.md) for the original MUSEX-01
audit/scaffold document (accurate through MUSEX-03; superseded by this doc for everything after).

> **This document follows the same accuracy convention as the rest of `docs/`** (see the README's
> "Accuracy note" and ["Wiring status"](../README.md#wiring-status-what-actually-runs) section):
> implemented-and-tested is not the same claim as wired-and-running, and this doc says which is
> which for every subsystem.

---

## 0. Privacy posture (read this first — it is load-bearing everywhere below)

The experience layer's consent model is **opt-in-only, by construction, not by convention**, and it
recurs identically across five different subsystems:

- **`src/discord/identity.rs`** — `FriendIdentity`'s `taste_opt_in` flag and linked
  `muse_account_id` are **private fields**. `FriendIdentity::new`/`Default` both produce
  not-opted-in. The only production mutator that grants consent is `FriendIdentity::opt_in(self,
  muse_account_id) -> Self`, which sets the flag and links the account atomically — there is no
  code path outside the module that can construct an already-opted-in identity (a
  `#[cfg(test)]`-only `from_parts_for_test` escape hatch exists for one defensive test and does not
  exist in a production build). `TrustedFriends` (a `HashMap<String, FriendIdentity>`) exposes only
  `opted_in_friends()` as its enumeration — every downstream consumer is forced through the
  filtered view.
- **`src/promotion/targeting.rs`** — `promote_new_title` only ever scores/targets friends drawn
  from `opted_in_friends()`; a non-opted-in friend gets nothing, silently, "same as if the title
  never landed for them."
- **`src/premiere/schedule.rs`** — `PremiereEvent.invited` is private, populated only from the
  opt-in-filtered intersection of `friends.opted_in_friends()`; `rsvp()` errors for anyone not in
  that set, and `discussion::post_message` checks both allowlist membership *and*
  `friend.is_opted_in()` before any DB write.
- **`src/kg/assemble.rs`** — `assemble_shared_graph(friends, data, taste_neighbor_threshold)`
  builds a `HashSet` of opted-in friend IDs up front and checks membership before including any
  node or edge; a co-view edge requires **both** ends to be opted in.
- **`src/web/graph.rs`** — a codex review of the first cut found a real bypass: handlers accepted
  an already-assembled `KgGraph` straight off the wire, which let a client skip the opt-in filter
  entirely. The fix (now in place): **no handler accepts a pre-assembled graph** — every
  `/api/graph/*` handler accepts only raw source inputs and always routes them through
  `assemble_shared_graph` as a single choke point before any visualization builder runs, proven by
  a test literally named `privacy_is_enforced_by_the_real_async_handlers`.
- **`src/cultural/`** ("the-talk", §6 below) uses a *different* privacy mechanism worth naming
  precisely: it's not opt-in/allowlist, it's **no-PII-egress-by-construction** — its `TrendQuery`/
  `TalkQuery` types are structurally incapable of carrying an account ID at all.

**Net effect for an operator:** a non-opted-in friend or account contributes zero taste data,
zero graph nodes/edges, zero promotion targeting, and zero premiere visibility, anywhere in this
layer. This isn't a filter applied late — in most of these modules the consent-bearing struct
cannot be constructed in an already-granted state from outside the module that owns it.

---

## 1. Multi-persona engine — `src/persona/`, `src/taste_model/`

*Grounded in: `src/persona/mod.rs`, `src/persona/derive.rs`, `src/persona/blend.rs`,
`src/taste_model/profile.rs`.*

A `Persona` is a pgvector taste centroid + a `defining_signals` JSON blob, built by reusing
`taste_model::profile`'s existing embedding/averaging code (`mean_embedding`,
`context_key_for`) — it is a *view* over the per-account `taste_profile` machinery documented in
`docs/architecture.md`/`docs/behavior-spec.md`, never a second taste-storage path, and it respects
the crate-wide "taste is never blended across accounts" invariant at the storage layer.

- **`derive.rs`** — `derive_context_cluster_personas(pool, account_id)` buckets an account's watch
  signals by weekend/weekday × time-of-day (via `taste_model::profile::context_key_for`) into
  distinct *derived* personas (e.g. a "weeknight" persona vs. a "Saturday morning" persona for the
  same account). `derive_explicit(pool, name, media_item_ids)` builds an operator-declared persona
  from a curated title list, returning `None` (never an error) on empty input.
  `Persona::explain()` produces a human-readable rationale.
- **`blend.rs`** — `blend_personas(&[Persona]) -> BlendResult` combines N personas into one session
  taste vector for group contexts. It is deliberately **not** a naive average: each embedding
  dimension is weighted by `1 / (1 + variance_d / (mean_d² + ε))`, so dimensions the personas
  *agree* on dominate and dimensions they diverge on are suppressed. `BlendStatus` distinguishes
  `Blended`, `SinglePersona` (degrade for one input), and `NoOverlap` — a cosine-similarity check on
  the weakest pairwise persona-centroid similarity (at/below 0.0) that surfaces a genuinely
  divergent group instead of silently blending it into taste soup. The explanation is built from
  the personas' shared `defining_signals.top_genres`.

**Wiring status:** `derive_context_cluster_personas`/`derive_explicit` have no HTTP route or
worker caller — persona derivation must currently be invoked directly (test/ops path only).
`blend_personas` *is* exercised in production code, but only as an internal step of
`watch_together` (§3), which is itself not wired to an HTTP entry point yet (see that section) —
so today, in a running deployment, no persona or blend result is currently reachable end-to-end
from outside the process.

---

## 2. Channel director — `src/channels/director.rs`

*Grounded in: `src/channels/director.rs`, `src/channels/compose.rs`, `src/channels/routes.rs`.*

The director is a **second, additive** channel-building path alongside the pre-existing MUSE-24
`compose::compose_channel_run` — the module doc is explicit about the distinction: `compose`
round-robins a caller-*chosen* set of shows against a time budget ("keep these shows going, in
this order, for about this long"); the director *decides* what to watch from the real candidate
pool (`curation::candidates`/`curation::recommend` — the same pool `/recommend` ranks), given a
taste target (a persona or blended session vector) and a time budget ("given this taste and this
time, decide"). Its own doc states plainly: **"this module changes no compose behavior and is not
wired into `compose_channel_run` in any way."**

- `program_channel(pool, constraints) -> ChannelSchedule` is a pure, DB-free function: it splits
  candidates into "safe" and "exploration" deques, fits them to a runtime budget, and assigns each
  slot a `SlotIntent` (`WarmUp` / `Main` / `WindDown`) following an energy arc across the session,
  with deterministic exploration placement seeded by `DirectorConstraints::seed` (reproducible for
  testing, not randomized per call).
- Named presets (`DirectorPresetName::{MusePrime, ComfortRewatch, DeepCutSundays,
  BackgroundCooking}`) each carry a `persona_name` lookup key the director itself never resolves —
  resolving persona-by-name to an actual `Persona` is the caller's responsibility.

**Wiring status:** only `channels::compose_channel_run` is reachable over HTTP (via
`channels/routes.rs`, one route also listed in the README's HTTP API surface). `director::
program_channel` has no route of its own — its only production caller is `watch_together` (§3),
which is itself unwired. `settings::ExperienceSettings.channel_director` (§8) exists as a toggle
name but is not read by `director.rs` anywhere.

---

## 3. Server-agnostic watch-together — `src/watch_together/`

*Grounded in: `src/watch_together/mod.rs`, `src/watch_together/sync.rs`.*

Watch-together answers "who's on the couch right now, and what should **we** watch" by composing
three already-shipped pieces in sequence rather than reimplementing any of them: **blend**
(`persona::blend::blend_personas`, §1) folds present members' personas into one session taste
vector (or a `NoOverlap` compromise), **program** (`channels::director::program_channel`, §2) turns
that into 2-3 distinct `LobbyOption`s (one call per named lobby preset, so the group sees a genuine
spread rather than one take-it-or-leave-it schedule), and **lobby** presents those options, each
explained with the blend's own rationale plus the schedule's real per-slot `because_line` (reused
verbatim, never re-derived) — `GroupSession::lock_pick` records the chosen option.

**Server-agnostic architecture, enforced by a compiled test, not just a docstring.** `mod.rs`
contains a real test — `orchestration_module_has_zero_server_specific_dependencies` — that
source-scans the module for forbidden strings like `PlexClient` and `Jellyfin`. The orchestration
logic (blend → program → lobby → lock) only ever touches abstract persona/blend/director/candidate
types; it has no idea what media server, if any, is playing anything.

**Sync delegation (`sync.rs`) is the deliberately separate piece that DOES know about servers** —
its own module doc calls out that "turning a locked pick into an actual play command against a
real media server is a SEPARATE, later concern," and frames itself as *only the decision half*:

- `trait ServerSyncPrimitive { fn kind(); async fn delegate(...) }` is the seam a real per-server
  adapter implements.
- `decide_sync_mode(clients) -> SyncMode` delegates to a native sync primitive only if **every**
  present client shares one `ServerSyncPrimitiveKind` (`JellyfinSyncPlay` or `PlexWatchTogether`
  — both are the *kind* enum values only, not shipped adapters, see below); otherwise it falls back
  to `CoordinatedStart` — a synchronized countdown + "press play now" + presence-ping scheduler
  that Muse fully owns and drives itself. A second compiled test proves the module builds
  **no low-level sync protocol** of its own (no frame-timing loop, no seek-drift correction, no
  custom wire format) — `CoordinatedStart` is the load-bearing default path, not a fallback that
  rarely fires.
- `JellyfinSyncPlay::delegate` is a real type but its `delegate` method **unconditionally returns
  `Err(NotImplemented)`**, explicitly flagged in-source as an
  **[EXTERNAL-API ASSUMPTION — UNVERIFIED]** stub. There is no shipped Plex or Jellyfin sync
  adapter — the architecture is proven server-agnostic, but no server is actually plugged in yet.

**Jellyfin, generally:** confirmed via a full-crate grep — the only two references anywhere in
`src/` are this `JellyfinSyncPlay` stub and an unrelated webhook-notification-type mapper in
`tracker/interpret.rs` that has no registered HTTP route. There is no working Jellyfin client, and
`plex_control::cast::CastController` (the pre-existing MUSE-22 trait `watch_together`'s design
doc pointed at as the extension seam) still has exactly one real implementation
(`PlexControlClient`) plus the unimplemented `GoogleCastController` stub from before MUSEX — the
watch-together `sync.rs` module does not actually import or use `CastController` at all; the
relationship is design-doc-level, not code-level.

**Wiring status:** `create_group_session`/`decide_sync_mode` have zero production callers outside
tests — `main.rs` only declares `pub mod watch_together;`. `settings::ExperienceSettings.
watch_together` (§8) exists as a toggle but is not read anywhere in this module (and, per the
settings module's own doc comment, defaults to **enabled**, which given the module is unwired is
moot in practice but is worth flagging as inconsistent with the "opt-in by default" framing used
elsewhere in this doc).

---

## 4. Two-timescale adaptation loop — `src/adaptation/`

*Grounded in: `src/adaptation/mod.rs` (single file).*

Two functions, two timescales, both confidence-gated against the same threshold constant
(`HIGH_CONFIDENCE_THRESHOLD = 0.65`, also referenced by the assistant, §5):

- **Fast** — `fast_adapt()` reacts within a single session, per incoming signal, but only above the
  confidence threshold; below it, it deliberately does nothing
  (`FastAdaptationKind::NoAdaptation`) rather than whipsaw the next pick on an ambiguous signal.
  It only ever touches the *next* schedule slot, never durable taste state. Signal → action
  mapping: `Fatigue` → wind-down + shrink remaining runtime; `Negative` → avoid that source;
  `Engagement` → favor more-like-the-last-pick; `Interruption` → never adapts (an interruption
  alone is not a taste signal).
- **Slow** — `slow_consolidate()` runs at a multi-session, "sleep-time" cadence and only moves
  *durable* taste state (the `taste_profile` data §1 and `docs/behavior-spec.md` describe) when a
  pattern is sustained across **≥3 distinct sessions** (`SUSTAINED_PATTERN_MIN_SESSIONS`) at ≥0.5
  confidence. Only `Negative` and `Engagement` patterns can move durable weight, and the move is
  capped at `MAX_DURABLE_WEIGHT_DELTA = 0.8` — deliberately below what an explicit user action
  (e.g. a rating) would weigh, so implicit multi-session inference never outweighs an explicit
  signal.

**Wiring status:** fully implemented and unit-tested, but **zero non-test callers anywhere in the
crate** — no worker, no route, no other module invokes `fast_adapt` or `slow_consolidate` today.
`settings::ExperienceSettings.adaptation_loop` (§8) exists as a toggle/aggressiveness tunable but,
per the settings module's own documented seam (§8), is not read by this module.

---

## 5. Conversational assistant — `src/assistant/`, `src/conversational/`

*Grounded in: `src/assistant/mod.rs`, `src/conversational/mod.rs`.*

These are two distinct, complementary modules — not a legacy/successor pair — and the naming is a
little confusing, so it's worth stating precisely:

- **`src/assistant/` (MUSEX-12) is the loop's *active* sense.** `tracker::interpret` (existing,
  pre-MUSEX) is the *passive* sense — it turns play-state telemetry into an `InterpretedSignal`
  with a confidence score. `adaptation::fast_adapt` (§4) is what happens once a signal is
  confident enough; below `HIGH_CONFIDENCE_THRESHOLD` it just drops the signal. `assistant::
  decide_ask()` is the third option for exactly that gap: convert a low-confidence-but-material
  signal into ground truth by asking, through Lumina's voice, instead of guessing or silently
  discarding it. `AskFrequency::Never` is a real per-account opt-out
  (`settings::QuestionFrequency`, §8) — a genuine "don't ever ask me" switch, not just a UI label.
- **`src/conversational/` (MUSEX-14, part B) is "library-first" natural-language requests** —
  e.g. "something like Sicario but lighter." `handle_conversational_request()` is built directly on
  `recall::run_ladder` — the same vector → trigram → TMDb tiered search `POST /query/resolve`
  already uses — specifically because that ladder never reaches the TMDb (beyond-library) tier
  while an in-library hit exists. That ordering *is* "reason against the library first," not a
  separate rule this module re-implements. Only a genuinely-missing title is ever routed onward to
  `arr::request`. The module's own doc is candid about a real limitation: **"every missing-title
  request built here currently classifies as `NeedsReview` or `Blocked` — never
  `AutoApprovable` in practice"** — i.e. the safety-gate gradient exists in the type system but the
  conversational path hasn't (yet) produced a request confident enough to auto-approve.

**Wiring status:** both `decide_ask()` and `handle_conversational_request()` have **zero
production/HTTP callers** — test-only today. There is no chat surface (Discord or otherwise) that
actually invokes either function in a running deployment; the Discord bot (§6) is the closest thing
to a live conversational surface and does not call into `assistant`/`conversational`.

---

## 6. Bespoke Discord bot — `src/discord/`

*Grounded in: `src/discord/mod.rs`, `src/discord/identity.rs`, `src/discord/client.rs`,
`src/discord/bot.rs`.*

The module doc is explicit about intent: this is "a genuinely bespoke social surface for Muse, NOT
a Requestrr/Notifiarr command-table reskin," reusing the real brain
(`curation::recommend`, `persona`, `taste_review::trace`) rather than a second,
Discord-specific rationale path.

- **Identity/consent** — `identity.rs`, see §0 above for the full detail. `FriendIdentity` is
  default-private, `TrustedFriends` is the allowlist wrapper exposing only
  `opted_in_friends()`.
- **`DiscordClient` trait** (`client.rs`) — `post_embed`/`reply`, implemented by
  `RealDiscordClient` (a real REST client, config-gated on a Discord bot token, but per its own
  doc comment "never exercised against a live endpoint") and `MockDiscordClient` (a test
  recorder).
- **Taste-aware, not command-dumb** — `bot::respond`'s `TasteAware` command arm calls
  `curation::candidates::gather_on_deck_candidates` and
  `curation::recommend::{rank_candidates, build_rationale}` — the *same* pipeline `POST /recommend`
  uses — so a Discord recommendation is genuinely taste-driven, not a static reply table. By
  contrast, the `Generic` command arm takes zero arguments, so it is structurally incapable of
  leaking any taste data — proven by a DB-gated negative test with real seeded data, following the
  same "construction proves the invariant" style used for `FriendIdentity` and `cultural`'s
  no-PII-egress types.

**Wiring status:** **no live Discord integration exists.** There is no gateway/webhook receiver
mounted anywhere in `src/http/`; `RealDiscordClient::from_config` is never called from production
code; `bot::respond`, `promotion::run_promotion_dispatch` (§7), and `premiere::discussion::
announce_thread` (§7) all have test-only callers. `main.rs` only declares `pub mod discord;`.
`settings::ExperienceSettings.discord_bot` (§8) is the one settings toggle in this whole layer with
a *proven* real enforcement point: `promotion::run_promotion_dispatch` checks
`settings.is_discord_bot_enabled()` and short-circuits to an empty result when disabled, verified
by a lazy-connect-pool test — but since `run_promotion_dispatch` itself has no production caller,
the enforcement is real but currently inert end-to-end.

---

## 7. What's-hot / the-talk, promotion, and premiere engagement

*Grounded in: `src/trending/mod.rs`, `src/cultural/mod.rs`, `src/promotion/mod.rs`,
`src/premiere/mod.rs`.*

**What's-hot base layer — `src/trending/`** (pre-MUSEX, MUSE-19) is TMDb trending/popular ingest
only: `snapshot_trending()` pulls TMDb, writes `trending_snapshots`, resolves streaming providers.
No social or group concept lives here.

**"The-talk" — `src/cultural/`** (MUSEX-07) is the actual social layer, a sibling module, not part
of `trending/`. `build_cultural_picks()` intersects three legs, all hard-gated together: TRENDING
(via `trending::TmdbClient`), "the TALK" (comment/rating volume, optional Trakt integration), and
the account's own library-ownership + taste centroid. A sparse taste profile falls back to
`cold_start_recommendations` rather than an empty or generic result. Privacy here is the
no-PII-egress mechanism from §0 — `TrendQuery`/`TalkQuery` structurally cannot carry an account ID,
tested end-to-end. `TmdbTrendSource::talk()` always returns `NotImplemented` (TMDb has no
talk-volume endpoint); `TraktTrendSource` is explicitly flagged in-source as a "documented
best-effort guess, not verified against a live Trakt endpoint."

**Taste-targeted promotion — `src/promotion/`** (MUSEX-14, part A) refuses to be a broadcast
firehose: `targeting::promote_new_title` scores a newly-available library title against each
*opted-in* friend's own taste centroid (cosine similarity) and only messages friends who clear
`Config::promotion_match_threshold` — a non-matching friend gets nothing, silently. It reuses the
real recommendation brain (`curation::candidates::Candidate` with
`CandidateSource::Taste`, then `curation::recommend::build_rationale`) rather than a second
rationale path. Its DB-gated privacy negative test is explicitly commented
"LOAD-BEARING PRIVACY NEGATIVE TEST" in source.

**Premiere events + engagement — `src/premiere/`** (MUSEX-15) is three opt-in capabilities built
on already-shipped pieces, inventing no new consent model:
- `schedule` — a title + time + RSVP + a grounded "why this pairing" rationale, announced via the
  Discord `RichEmbed` shape; only opted-in/allowlisted friends can be invited or RSVP (§0).
- `discussion` — async, book-club-style per-title threads, gated by the same `TrustedFriends`
  allowlist/opt-in check `schedule::PremiereEvent::rsvp` uses.
- `engagement` — `EngagementTier::{Starter, Trusted, Curator}`, computed from real
  watch-through-rate and household-love-rate signals, that **modulates, never bypasses**
  `arr::request`'s existing tiered safety gate. `submit_with_engagement_budget` can only ever
  move a request from `AutoApprovable` down to `NeedsReview` when over budget — the module's own
  doc states the budget is "strictly a brake, never an accelerator," so it can never turn a
  `Blocked` or `NeedsReview` request into `AutoApprovable`.

The premiere module's own doc is candid about a real follow-on: wiring RSVP'd premiere attendees
into an actual `watch_together::GroupSession` (§3) "is a natural, separately-reviewable follow-up,
not done in this pass" — the two features are not yet connected.

**Wiring status:** `trending::snapshot_trending` is called from the maintenance worker (see
README's wiring-status section) — it is live. `cultural::build_cultural_picks` is channel-callable
and should be re-verified for its exact HTTP reachability before an operator relies on it (worth a
follow-on wiring pass, not verified end-to-end in this doc). `promotion::run_promotion_dispatch`
and `premiere::discussion::announce_thread` are fully implemented and tested but, per §6, have no
production caller today because they terminate in the unwired Discord surface.

---

## 8. KG coupling + visualizations — `src/kg/`, `src/web/graph.rs`

*Grounded in: `src/kg/assemble.rs`, `src/kg/model.rs`, `src/kg/query.rs`, `src/kg/viz.rs`,
`src/web/graph.rs`.*

**This is a watch-history / group-dynamics graph *internal* to Muse** — it is not the Atlas KG
(the `kg_*` Terminus tools used by the moosenet-spec build pipeline). MUSEX-01's original audit
flagged this exact ambiguity as something to scope carefully; MUSEX-16/17 resolved it by building
a self-contained, Muse-owned graph over friends/watch-history/personas, not an Atlas integration.

- **`assemble_shared_graph(friends: &TrustedFriends, data: &GraphSourceData,
  taste_neighbor_threshold: f32) -> KgGraph`** (`kg/assemble.rs`) is a pure, DB-free function — see
  §0 for its privacy filter. Its module doc is explicit that it "doesn't prescribe the source, only
  the shape" of `GraphSourceData { watches, co_views, personas }`; the caller resolves real records
  from `repo::watch_stats`, `repo::persona`, `premiere::schedule` RSVP data, etc. In production the
  only caller is `web/graph.rs`, and — worth flagging precisely — that handler builds
  `GraphSourceData` straight from the **HTTP request body**, not from the database. The module's
  own doc says plainly: *"nothing in this crate persists a live `TrustedFriends` allowlist or a
  discord-user-id↔account mapping in the database... wiring a DB-backed assembly path is real
  future work."* So today, the caller of `/api/graph/*` supplies the friend roster and watch data
  itself on every request; there is no server-side persisted source of truth for "who is opted in"
  that these endpoints read automatically.
- **`kg/query.rs`** — `bridge_between()` (BFS shortest path between two graph nodes),
  `taste_neighbor_clusters()` (union-find clustering by taste-neighbor threshold).
- **`kg/viz.rs`** — four visualization builders: `build_taste_map`, `build_group_dynamics`
  (includes a Tarjan articulation-point pass — i.e. it can surface which friend is the "bridge"
  holding a group's shared taste together), `build_watch_history`, `build_taste_clusters`.
- **`src/web/graph.rs` (MUSEX-17)** registers four real routes in `src/web/mod.rs`:
  `POST /api/graph/taste-map`, `POST /api/graph/group-dynamics`,
  `POST /api/graph/watch-history`, `POST /api/graph/taste-clusters`. As described in §0, a codex
  review caught and fixed a privacy bypass here — handlers now accept only raw source inputs and
  always route through `assemble_shared_graph` first, proven by
  `tests::privacy_is_enforced_by_the_real_async_handlers`.

**Wiring status:** these four routes are genuinely live and reachable — this is one of only a
handful of experience-layer subsystems that is (unlike most of §1-§6) actually callable in a
running deployment. What is *not* wired is the data-source side: there's no worker or DB-backed
step that keeps a `TrustedFriends` roster or watch-history snapshot current server-side, so a
caller must supply that data on each request (see above).

---

## 9. The GUI control surface — `src/settings/`, `src/web/settings.rs`

*Grounded in: `src/settings/mod.rs`, `src/web/settings.rs`, `migrations/0102_experience_settings.sql`.*

`ExperienceSettings` is the single persisted settings document (one JSONB row via
`repo::settings`), served over `GET`/`PUT /api/settings`. It's genuinely comprehensive:

- **Master switch** — `master_enabled`.
- **Per-subsystem toggles** — `channel_director`, `watch_together`, `adaptation_loop`,
  `discord_bot` (sensitive, see below), `whats_hot`, `kg_viz`.
- **Tunables** — adaptation aggressiveness, serendipity percentage, question frequency (including a
  silent mode, backing `AskFrequency::Never`, §5), persona definitions, Discord promotion cadence +
  the trusted-friends roster, trend-source weighting, per-user sharing granularity, KG-viz opt-in.
- **Secret-masking** — the module's own doc states plainly: nothing in `ExperienceSettings` ever
  holds a raw secret. The Discord bot token stays exactly where `discord::client::
  RealDiscordClient` already reads it — `Config::discord_bot_token`, <secret-manager>-materialized env at
  runtime — and is never accepted by the `PUT` DTO, never written into the settings document, never
  returned by `GET`. `mask_discord_token` turns "is a token configured at all" into a display-only
  `***configured***` placeholder for `GET` responses.
- **Confirmation-gated sensitive toggles** — `web/settings.rs`'s `evaluate_update` requires
  `confirm_sensitive: true` (defaulting to `false`, fail-closed) to enable `discord_bot` or to
  *widen* `sharing.granularity` (ranked `Private < HouseholdOnly < TrustedFriendsOnly < Public` via
  `SharingGranularity::widens`) — narrowing sharing or toggling a non-sensitive switch needs no
  confirmation.

**The module's own documented seam, stated verbatim because it matters for every section above:**
several of these tunables already exist as independently-typed, independently-tested domain values
elsewhere (`adaptation::Aggressiveness`, `assistant::AskFrequency`,
`channels::serendipity::SerendipityRange`), none of which derive `serde`/persist today. Rather than
retrofit `serde` onto those production types, `settings/` defines its **own** GUI-facing mirror
types with `From` conversions into the real ones where one exists. The doc is explicit: *"Wiring
every subsystem's internals to READ this panel on every call is real follow-on work; what this
item guarantees is that the panel is the authoritative PERSISTED surface, and that the
master/per-subsystem gate is REAL and enforced at at least one concrete entry point end-to-end"* —
that one concrete entry point is `promotion::run_promotion_dispatch`'s `discord_bot` check (§6/§7).
In other words: **the settings panel is a real, persisted, secret-safe, confirmation-gated control
surface, but most of the toggles it exposes are not yet read by the subsystems they're named
after** — this is the single most important accuracy caveat in this whole document, and it applies
directly to the "unwired" findings in §1-§6 above.

**Wiring status:** `GET`/`PUT /api/settings` is a live, real HTTP surface. The settings it persists
are, with the one proven exception above, not yet consulted by the subsystems they configure.

---

## 10. Operator section

**What's actually live today**, per the wiring-status notes above:
- Channel building via `POST /channels/{id}/compose` (pre-MUSEX `compose.rs`, unaffected by the
  director).
- `POST /api/graph/{taste-map,group-dynamics,watch-history,clusters}` — but the caller must supply
  the friend roster and watch data on every request; there's no server-side persisted source yet.
- `GET`/`PUT /api/settings` — a real, working control panel, but most toggles are not yet consulted
  by their named subsystem (§9).
- `trending::snapshot_trending` runs from the maintenance worker.

**What is fully built and tested, but will do nothing in a running deployment today** because
nothing calls it: persona derivation/blending outside `watch_together`, the channel director,
watch-together session creation and sync delegation, the fast/slow adaptation loop, the
conversational assistant (both the "active sense" and the "library-first request" halves), the
entire Discord bot (no gateway is mounted — configuring a bot token today would not make the bot
respond to anything), taste-targeted promotion dispatch, and premiere discussion announcements.

**Practical consequence:** don't point an operator at this layer expecting personas, watch-together
lobbies, adaptive scheduling, a chatty assistant, a live Discord bot, promotion pings, or premiere
announcements to happen automatically — none of those paths are triggered by any worker or route
today. The graph-viz endpoints and the settings panel are the two pieces of this layer an operator
can actually exercise right now. Closing the remaining wiring gaps (mounting a Discord gateway,
scheduling adaptation/derivation passes, having settings actually gate their named subsystems, and
connecting watch-together to a real playback client) is real, scoped follow-on work, not something
this document should imply is already done.

**Privacy defaults an operator should know:** every friend/account starts **not opted in**
everywhere this layer touches them (§0). No taste data, graph presence, promotion targeting, or
premiere visibility exists for anyone until they are explicitly, atomically opted in via
`FriendIdentity::opt_in`. Enabling the Discord bot toggle itself requires `confirm_sensitive: true`
in the settings `PUT` (§9).

---

## 11. Contributor section

- **Reuse over reinvention is the house style.** Nearly every module in this layer explicitly cites
  what it reuses rather than reimplements: personas reuse `taste_model::profile`'s embedding code;
  the director reuses `curation::candidates`/`curation::recommend`; watch-together reuses persona
  blend + the director + `because_line` rationale text verbatim; the conversational assistant
  reuses `recall::run_ladder`; the Discord bot reuses `curation::recommend` for its taste-aware
  replies; promotion reuses the same candidate/rationale pipeline; premiere reuses the Discord
  `RichEmbed` shape and `arr::request`'s existing safety gate. When extending this layer, look for
  the existing primitive before adding a new one — the codebase's own doc comments usually name it.
- **"Construction proves the invariant" is the recurring privacy pattern.** `FriendIdentity`,
  `assistant::AskFrequency::Never`, and `cultural`'s `TrendQuery`/`TalkQuery` all make an invalid
  state structurally unrepresentable (private fields + a single atomic mutator, or a type that
  simply has no field to carry the sensitive data) rather than relying on a runtime check
  somewhere. Follow this pattern for any new consent-bearing type in this layer rather than adding
  another `if opted_in` check at a call site.
- **Server-agnosticism is enforced by a compiled test, not a lint rule or a comment** —
  `watch_together::mod.rs`'s `orchestration_module_has_zero_server_specific_dependencies` literally
  source-scans for forbidden strings. If you add a new watch-together capability, keep
  server-specific code inside `sync.rs` (or a new adapter file) — putting a `PlexClient` reference
  into `mod.rs` will fail that test.
- **The wiring gap is the main thing to fix next, not a footnote.** Per §10, the bulk of MUSEX-04
  through MUSEX-18 is implemented-and-tested-but-unreachable. Before adding a *new* experience-layer
  feature, consider whether wiring an *existing* one (a Discord gateway, an adaptation-loop
  scheduler tick, a settings-panel read in `director.rs`/`watch_together`) is higher-value — several
  of the module doc comments cited above explicitly flag their own wiring as "real follow-on work,"
  meaning the original authors intended it to be picked up as its own scoped item, not silently
  assumed complete.
- **Settings mirror types, not the production types themselves** (§9) — if you add a new tunable to
  an existing subsystem (e.g. a new `adaptation::Aggressiveness` variant), remember to also update
  its `settings::` mirror and `From` conversion; the two are not the same type and don't sync
  automatically.
- **Tests are DB-gated, not DB-required** — like the rest of the crate (see
  [`docs/TESTING.md`](TESTING.md)), privacy-sensitive tests in this layer (the "LOAD-BEARING
  PRIVACY NEGATIVE TEST" in `promotion/targeting.rs`, the handler-level privacy test in
  `web/graph.rs`) skip cleanly without `MUSE_TEST_DATABASE_URL` rather than failing — don't assume a
  green CI run without a live DB configured has actually exercised them; check whether they were
  skipped.

---

## 12. Three example flows

These trace what *would* happen if the relevant wiring existed, so a reader can reason about the
design — each flow explicitly notes where it currently stops in a real deployment (per §10).

### (a) A solo evening

An account with a populated `taste_profile` and no watch-together session in play. `channels::
director::program_channel` would take that account's own persona (or its raw `taste_profile` if no
persona exists yet) plus a runtime budget and produce a `WarmUp → Main → WindDown` schedule pulled
from the real recommend candidate pool — the same one `/recommend` ranks. Mid-session, a `Fatigue`
or `Negative` signal above the confidence threshold would trigger `adaptation::fast_adapt` to
wind down early or steer away from the disliked source; a low-confidence signal would be a
candidate for `assistant::decide_ask` to surface a clarifying question through Lumina instead of
guessing. **Today:** none of `director`, `fast_adapt`, or `decide_ask` has a caller, so a solo
evening in the live deployment still runs through the pre-existing `compose_channel_run` +
`proactive` generators (docs/behavior-spec.md), not this layer.

### (b) A group watch-together

Two or more household members with existing personas open a lobby. `watch_together::
create_group_session` would call `persona::blend::blend_personas` on the present members' personas
(landing on `Blended`, `SinglePersona`, or a flagged `NoOverlap`), then call `channels::director::
program_channel` once per configured lobby preset to produce 2-3 genuinely distinct schedule
options, each carrying the blend's explanation plus real per-slot `because_line` rationale.
`GroupSession::lock_pick` records the group's choice. `watch_together::sync::decide_sync_mode`
would then decide whether every present client shares one native sync primitive (today: none do,
since `JellyfinSyncPlay::delegate` is an unconditional `NotImplemented` stub and no Plex adapter
exists) and fall back to `CoordinatedStart` — a countdown + presence-ping Muse drives itself, no
frame-accurate protocol required. **Today:** there is no HTTP entry point that creates a
`GroupSession` in production, so this flow is fully implemented and tested in isolation but not
triggerable by an actual user session.

### (c) A Discord premiere

An operator schedules a premiere (`premiere::schedule`) for a title, inviting only friends who are
both on the `TrustedFriends` allowlist and individually opted in (§0) — anyone else simply cannot
be invited, by construction. The invite would announce via `discord::client::RichEmbed`, and
invited friends could `rsvp()` (rejected if they're not in `invited`). During or after the
premiere, `discussion::post_message` lets RSVP'd, opted-in friends post to an async discussion
thread. Separately, a friend messaging the bot with a taste-aware command would route through
`bot::respond`'s `TasteAware` arm into the *same* `curation::recommend` pipeline `/recommend` uses,
so the reply is a genuine, personalized recommendation, not a canned response — while a `Generic`
command is structurally incapable of touching any taste data at all. **Today:** no Discord gateway
or webhook receiver is mounted anywhere in `src/http/`, so nothing in this flow — the invite, the
RSVP, the discussion post, or the bot reply — is reachable from an actual Discord server; every
piece is implemented and unit/DB-gated tested, but the bot has never sent or received a real
Discord message in a running deployment.
