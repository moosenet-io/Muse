# MUSEX — Experience Layer: Build Map, Server-Abstraction Audit, Scaffold Plan

MUSEX-01 (Plane `TERM #377`). This is a documentation/audit deliverable, not feature code: it
grounds the S118 MUSEX build (personas, channel director, watch-together, adaptation, assistant,
Discord, KG/graph, GUI) in the *actual* Muse source, audits whether Plex + Jellyfin can sit behind
one server-abstraction interface, and lays out the module scaffold + phase order later MUSEX items
build against.

**Grounding method (read this before trusting anything below).** The Atlas KG tools (`kg_query`,
`kg_semantic_search`, etc.) were not reachable from this session — no MCP tool of that shape was
available to the agent producing this document. Everything below is grounded directly in the real
`moosenet/Muse` source tree (`worktrees/MUSEX-01` off `origin/main`, commit range through
MUSET-11/#376) instead: real file paths, real symbols, real doc comments. Anywhere this document
describes something *not* verifiable from the repo (chiefly: external API behavior of Jellyfin,
which Muse does not talk to at all today), it is explicitly flagged **[EXTERNAL-API ASSUMPTION —
UNVERIFIED]** rather than stated as fact.

---

## 1. KG-grounded build map — what Muse's "brain" is today

### 1.1 The three-layer brain, as it actually exists

Muse's recommendation/curation "brain" is not one module — it's three layered subsystems, each
with a real, shipped implementation:

| Layer | Module | Entry point | What it does |
|---|---|---|---|
| **Signal capture** | `src/taste_model/signals.rs` | `recency_weight`, `rating_weight` | Turns watch/rating/watchlist facts into `taste_signals` rows with fixed base weights defined as named consts in `signals.rs` (`WEIGHT_FINISH`, `WEIGHT_REWATCH_PER`, `WEIGHT_ABANDON`, `RATING_MIDPOINT` + `RATING_WEIGHT_SCALE` for the rating-around-midpoint scaling via `rating_weight`, `WEIGHT_WATCHLIST_ADD`, `WEIGHT_WATCHLIST_FULFILLED_BONUS`) — cite those consts rather than copying the numbers, which can drift. Stored **undecayed**; exponential half-life decay (`DEFAULT_HALF_LIFE_DAYS`, ~6 months) is applied at read time via `recency_weight`, never baked into storage. |
| **Profile aggregation** | `src/taste_model/profile.rs` | `aggregate_weighted`, called from `src/taste_model/recompute.rs::recompute_taste` | Recency-weighted sums folded into `taste_profile`'s `genre_affinity`/`person_affinity`/`keyword_affinity` (decade affinity is nested under `keyword_affinity.decades` — a documented schema divergence, see the module doc comment) plus `overall_centroid` (embedding mean of finished titles) and `taste_context_centroids` (weekend/weekday × time-of-day embedding buckets). Strictly per-`account_id` — taste is **never blended across accounts** (this is stated three times in the source: `taste_model/mod.rs`, `profile.rs`, and the MUSE-03 schema comment — treat it as a hard invariant any persona/blending work must respect explicitly, not silently break). |
| **Curation / recommend** | `src/curation/candidates.rs` + `src/curation/recommend.rs` | `recommend_handler`, `on_deck_handler`, `gaps_handler` (mounted via `src/curation/mod.rs`) | Four candidate sources (`CandidateSource::OnDeck`/`Gap`/`Taste`/`AvailableNow`) merged and de-duped, then `score_candidate` blends a fixed per-source tier weight from `recommend.rs::source_weight` (ordering, highest first: `OnDeck` > `Gap` > `Taste` > `AvailableNow` — read the exact factors in that function rather than trusting a copied number here, they can drift) with the candidate's own `taste_fit` and an availability adjustment (`AVAILABILITY_GRABBABLE_BONUS` added for a grabbable not-in-library pick, `AVAILABILITY_UNAVAILABLE_PENALTY` subtracted for one checked-and-unavailable — both consts in the same file). Rationale generation (`build_rationale`) **always** starts from a deterministic `template_rationale` built only from `Candidate::facts`, then optionally asks Chord's LLM to rephrase — never invents beyond the facts, and any Chord failure degrades to the template, never to a hard error. |

Supporting the brain: `src/taste_model/chord_client.rs` (`ChordClient`, `DEFAULT_MODEL`) is the
one LLM call surface for `model_notes` generation, reused by `curation::recommend`'s rationale
rephrasing and by `channels::compose`'s optional show-priority permutation. `src/taste_review/`
(referenced by `curation::recommend` as `build_reasoning_trace`/`ReasoningTrace`) is the
adversarial-reasoning audit trail from MUSET-07 — it records *why* a recommendation fired, which
is the natural anchor point for a future persona/assistant layer that needs to explain itself.

Downstream of the brain, already shipped:

- **Channel composition** (`src/channels/compose.rs`, MUSE-24) — `compose_channel_run` builds an
  ordered lineup from a `channels` row + show list, fully deterministic by default
  (`ComposeOptions::use_llm = false`), with an optional LLM re-ordering pass that degrades to
  deterministic order on any failure. `src/channels/presets.rs` holds six named presets (Saturday
  Morning, Prestige Drama Night, 90s Chaos, Comfort Rewatch, Discover, Household Movie Night) as
  data resolving to `ComposeOptions` overlays.
- **Proactive generation** (`src/proactive/generators.rs`, MUSE-12) — five event-driven generators
  (new-season/gap, Friday-evening, abandonment insight, grab-window/freeleech, zeitgeist) already
  write natural-language copy through the same Chord LLM surface, with a system prompt literally
  framing the voice as *"You are Lumina, a warm, concise personal assistant relaying one proactive
  media [item]…"* (`generators.rs:136`) — this is the closest existing thing to a "persona," and
  any MUSEX-02..04 persona work should treat this prompt as the seed voice, not a green field.
- **Linear tuner + streaming** (`src/tuner/`, `src/streaming/`, MUSE-28/29) — exposes `channels`
  as an HDHomeRun-emulated tuner (`tuner::hdhr`) plus an M3U/XMLTV surface (`tuner::m3u`,
  `tuner::xmltv`) and a real ffmpeg concat streaming engine (`streaming::ffmpeg`, `streaming::onnow`)
  that joins mid-stream at "what's on now."
- **Playback control** (`src/plex_control/`, MUSE-22) — see §2 below; this is the mechanism the
  channel director and any future watch-together / adaptation work drives playback through.

### 1.2 Where the S118 experience-layer items fit on top

**Update (MUSEX-02, Plane TERM #378 — landed).** The `persona/` module now EXISTS: `src/persona/`
(`mod.rs` + `derive.rs`), `src/repo/persona.rs`, `src/models/persona.rs`, and
`migrations/0100_personas.sql` (a `personas` table with a `vector(768)` taste centroid + a
`persona_members` table for shared/household personas). It implements the MUSEX-02 slice — derived
(context-cluster) and explicit personas, the addressable list/get-by-id/get-by-name seam MUSEX-03
blending consumes, deterministic derivation, and per-persona explainability — reusing
`taste_model::profile`'s embedding/centroid code (a new shared `mean_embedding` helper) rather than
a second taste store. The description of the 02/03/04 row below remains the design intent; the voice
layer (`ChordClient`-backed) and the blend/select surface are the parts MUSEX-03/04 still add on top
of this foundation. The rest of the list — channel-director-as-agent (beyond MUSE-24's composer),
watch-together, real-time adaptation, a conversational assistant, Discord integration, and a KG/graph
surface — still does not exist in the repo (verified at MUSEX-01 time: no hits for `WatchTogether`,
`SyncPlay`, or `discord`/`Discord` anywhere under `src/`; and, before MUSEX-02, no `persona`/`Persona`
taste concept either). The build map:

| MUSEX item(s) | Builds on | Why that dependency |
|---|---|---|
| **02/03/04 — Personas** | `taste_model::profile` (per-account affinities/centroids), `taste_model::chord_client` (voice), `proactive::generators`'s existing "Lumina" system-prompt seed | A persona is a *view* over an account's already-computed `taste_profile` plus a consistent voice layer on top of `ChordClient` calls — it must not create a second taste-storage path. The per-account-never-blended invariant (§1.1) means a "household" or "blended" persona is a NEW aggregation rule on top of N `taste_profile` rows, not a mutation of the existing per-account one. |
| **05/06/07 — Channel director** | `channels::compose` (MUSE-24), `channels::presets`, `taste_model::profile` | MUSE-24's composer is already "the agentic director" per its own module doc — 05/06/07 is deepening it (more directive types, richer LLM reasoning, persona-aware show selection) not building a new composer. Depends on personas (02-04) landing first if directives are meant to be persona-aware. |
| **08/09/10 — Watch-together** | `plex_control::cast::CastController` trait, `plex_control::client::PlexControlClient`, `streaming::onnow` | This is the item most constrained by what the media servers can actually do — see §2/§3. Depends on the adapter contract in §2 existing (or at minimum the Plex-only `CastController` seam being extended) before real multi-server sync work starts. |
| **11 — Adaptation** | `taste_model::signals` (recency weighting is already the "adapts over time" mechanism at the taste layer), `curation::recommend` scoring | "Adaptation" at the recommendation layer already exists as recency decay; new work here is likely real-time within-session adaptation (e.g. reacting to a skip/abandon signal mid-channel-run), which has no current entry point — `channels::compose`'s `channel_runs` are generated once, not adjusted live except via the existing `regenerate_channel_run`/`adjust_channel_run` helpers noted in `channels/compose.rs`'s module doc. |
| **12 — Assistant** | `curation::recommend` (rationale/reasoning trace), `taste_review::trace::ReasoningTrace`, `proactive` (existing conversational surface), personas (02-04) | A conversational assistant is the natural consumer of `ReasoningTrace` (already built for MUSET-07's audit purposes) — reuse it as the assistant's explanation substrate rather than inventing a second one. |
| **13/14/15 — Discord** | `src/http/` (axum router, `AppState`), `proactive` (the existing "deliver a proactive item" surface) | No Discord code exists; this is a new outbound (and likely inbound slash-command) integration analogous to how `taste_model::chord_client` wraps Chord — same "typed client, gracefully degrades if unconfigured" shape used everywhere else in the crate (`PlexClient::from_config`, `ChordClient`, TMDb/Searxng/News clients per `taste_model/mod.rs`'s doc comment) should be followed for a `DiscordClient`. |
| **16/17 — KG/graph** | Nothing existing in Muse itself — this is Muse becoming a *consumer* (or producer) of the Atlas KG (`kg_*` Terminus tools) referenced throughout the moosenet-spec pipeline, not a taste-graph inside Muse. Scope carefully against that distinction before building. |
| **18 — GUI** | `src/web/` (`artwork.rs` proxy, `guide.rs`), `src/http/` router | `src/web/` already exists as a thin web-facing layer (artwork proxying, EPG guide rendering) — a fuller GUI extends this, it doesn't start from zero. |

**Phase-order implication carried into §4:** personas (02-04) should land before channel-director
deepening (05-07) and before the assistant (12), since both consume persona state. Watch-together
(08-10) is gated on the adapter contract in §2, independent of the persona work. Discord (13-15)
and GUI (18) are presentation-layer and can proceed in parallel once their upstream data surfaces
(personas, assistant, channels) exist. KG/graph (16-17) is the most likely to need its own scoping
spec before implementation — its dependency is external (Atlas), not internal to Muse.

---

## 2. Media server-abstraction audit

### 2.1 Current state: Plex only, no abstraction, two separate clients

Grep across `src/` for `plex`/`jellyfin`/`media server`/`adapter`/`MediaServer` confirms:

- **Jellyfin: zero references anywhere in the codebase.** Not a stub, not a config field, not a
  TODO. Muse today is Plex-only.
- **Plex itself is split into two independent clients with no shared trait:**
  - `src/plex/mod.rs` — `PlexClient`, a **read-only** typed HTTP client (library/metadata,
    sessions, history, on-deck, recently-added, ratings, watchlist via Plex Discover cloud,
    accounts, image fetch). Explicitly documented as making "no writes to Plex" (`plex/mod.rs:3`).
  - `src/plex_control/` — `PlexControlClient` (in `client.rs`), a **write/control** client for the
    Plex Companion protocol (play/pause/stop/skip/timeline poll, player discovery, play-queue
    building). Explicitly documented as never mutating the Plex library (`plex_control/mod.rs:7`).
  - Both are constructed independently from `Config`, but with **different missing-config
    postures** (verified in source — this matters for the adapter contract in §2.2):
    `PlexClient::from_config` returns `Option<Self>` and degrades to `None` when
    `PLEX_URL`/`PLEX_TOKEN` are unset (read path is optional-by-default), whereas
    `PlexControlClient::from_config` returns `MuseResult<Self>` and **errors**
    (`MuseError::Config("PLEX_URL is not set")` / `"PLEX_TOKEN is not set"`) on missing config —
    there is no call site wrapping it in a graceful-degrade path. So the control/playback seam is
    **not** optional-by-default the way the read seam is; a `MediaServerClient`+`CastController`
    adapter must decide deliberately how to treat an unconfigured control path (see §2.2), not
    assume it silently no-ops. Neither client implements a trait shared with the other or with
    anything hypothetical for another server.
- **One seam already exists, and it's the right shape to build on:** `src/plex_control/cast.rs`
  defines `pub trait CastController: Send + Sync` — an async trait with `play_media`, `play`,
  `pause`, `stop`, `skip_next`, `poll_timeline` — implemented today only by `PlexControlClient`.
  Its own module doc is explicit about the intent: *"Today there's exactly one implementation:
  `PlexControlClient`… this trait exists so a later `GoogleCastController` can be dropped in
  without touching callers"* (`cast.rs:4-9`). `cast.rs` also ships a documented placeholder,
  `GoogleCastController`, for bare-Chromecast DIAL/Cast-v2 fallback, explicitly deferred
  ("TODO(muse): implement raw Cast v2… Deliberately not implemented in MUSE-22").

**Conclusion: Plex and Jellyfin are NOT currently peers behind one interface — there is no
interface. But the codebase already contains the exact seam pattern (`CastController`) that a
Plex+Jellyfin adapter contract should extend, for the control/playback side.** The read side
(`PlexClient`) has no analogous trait yet; §2.2 proposes one modeled on the same pattern.

### 2.2 The proposed ADAPTER CONTRACT

Two traits, mirroring the existing read/write split (`PlexClient` vs. `PlexControlClient` /
`CastController`) rather than collapsing them — that split is already a deliberate design decision
in the shipped code (see the "no writes" / "never mutates the library" doc comments above), and a
unified read+write trait would violate it.

```rust
// Read/metadata surface — the MediaServerClient analogue to today's PlexClient.
// Return types would reuse/generalize the existing MediaItem/Library/Account
// shapes in src/plex/models.rs rather than inventing parallel ones.
#[async_trait]
pub trait MediaServerClient: Send + Sync {
    async fn libraries(&self) -> MuseResult<Vec<Library>>;
    async fn library_items(&self, section_key: &str) -> MuseResult<Vec<MediaItem>>;
    async fn metadata(&self, item_id: &str) -> MuseResult<Option<MediaItem>>;
    async fn sessions(&self) -> MuseResult<Vec<MediaItem>>;
    async fn history(&self, account_id: Option<&str>) -> MuseResult<Vec<MediaItem>>;
    async fn on_deck(&self) -> MuseResult<Vec<MediaItem>>;
    async fn recently_added(&self) -> MuseResult<Vec<MediaItem>>;
    async fn accounts(&self) -> MuseResult<Vec<Account>>;
    async fn fetch_image(&self, path_or_url: &str) -> MuseResult<(Vec<u8>, String)>;
    // watchlist() is deliberately NOT on this trait — it's Plex-cloud-specific
    // (Plex Discover, not the local server); a Jellyfin adapter would either
    // no-op it or the trait would need a capability-flag / Option<T> pattern,
    // matching this doc's own "capability introspection" recommendation below.
}

// Playback-control surface — this ALREADY EXISTS as `CastController`
// (src/plex_control/cast.rs) and should be extended in place, not replaced:
// a JellyfinControlClient implementing the existing CastController trait
// is the minimal-diff path, reusing play_media/play/pause/stop/skip_next/
// poll_timeline exactly as PlexControlClient does today.
```

Why two traits and not one `MediaServer` god-trait: it follows the codebase's own existing
precedent (`PlexClient` / `PlexControlClient` split) instead of introducing a new shape, and it
lets a server that only supports one side (e.g. a metadata-only integration) implement just that
trait — relevant given the capability gaps in §2.3.

**Capability introspection recommendation.** Given how uneven playback-sync support is per §2.3,
the adapter layer should expose a `capabilities()`-style map (the same pattern the moosenet-spec
skill documents for the git-private/git-public forge adapters, `CapabilityMap` → `supported` /
`unsupported` / `experimental`) rather than let callers discover gaps via runtime errors. This is a
recommendation for MUSEX-08/09 to adopt, not something already implemented anywhere in Muse today.

### 2.3 SYNC-CAPABILITY MAP

| Server / client | Frame-accurate sync support | Coordinated-start fallback needed? | Source |
|---|---|---|---|
| **Plex — Plex Companion clients** (registered app/TV clients Plex can target directly) | `plex_control::client::PlexControlClient` issues `play`/`pause`/`stop`/`skip_next` + `poll_timeline` per target today (`cast.rs`, `client.rs`). No group/party-sync primitive exists in this client — every command targets one `machineIdentifier` at a time. **Not verified against a live server** — `client.rs`'s own header states *"this crate has never been exercised against a live Plex Media Server or a real registered client… Treat the exact header/query-param behavior… as best-effort until it's live-verified"* (`client.rs:5-10`). | Yes — building a multi-client "watch together" on top of per-target commands means Muse itself must issue synchronized `play_media` calls with matched offsets to each target's `poll_timeline`; there is no native Plex "watch party" primitive being used here. |
| **Plex — official Watch Together** | Not integrated by Muse at all. **[EXTERNAL-API ASSUMPTION — UNVERIFIED]** Publicly, Plex's own Watch Together feature has historically been limited in scope and has seen reduced investment/availability — treat any claim about its current capability as unverified until checked against Plex's live product docs; do not build MUSEX-08/09 assuming it's usable as a foundation. | N/A — assumption only, not code in this repo. |
| **Plex — bare Chromecast (no Plex receiver)** | Not controllable via `PlexControlClient`/Companion at all — this is exactly the gap `cast.rs`'s `GoogleCastController` placeholder exists for (DIAL discovery + Cast v2 launching the Plex receiver app), and it is explicitly unimplemented (`cast.rs`'s own TODO). | Yes, unconditionally — no sync path exists for this target class until `GoogleCastController` is built. |
| **Jellyfin — SyncPlay API** | Muse has zero Jellyfin integration today (§2.1). **[EXTERNAL-API ASSUMPTION — UNVERIFIED]** Jellyfin publicly documents a "SyncPlay" feature (group playback sync across Jellyfin clients) — this document does not verify its exact API shape, transport (it's commonly described as WebSocket-based group-sync), or client-support matrix against Jellyfin's live docs; MUSEX-08/09 must verify this directly against Jellyfin's API reference before relying on it, not against this document. | Presumed no (SyncPlay is *designed* for this), but unverified — confirm before depending on it. |
| **Android TV clients (either server)** | No Android-TV-specific code anywhere in `src/`. **[EXTERNAL-API ASSUMPTION — UNVERIFIED]** Android TV apps for both Plex and Jellyfin are frequently the most capability-constrained client class for remote-control/sync protocols (background-execution and casting-API restrictions are common on the platform) — flagged here as a known likely per-client gap the SYNC-CAPABILITY audit should re-check per concrete client version, not assumed a blanket "supports frame-sync." | Yes — treat Android TV as coordinated-start-fallback-required by default until a specific client is verified otherwise. |

**What this table is for:** MUSEX-08/09 (watch-together) should treat "frame-accurate sync" as
available only where verified (currently: nowhere, in this codebase, today) and build the
coordinated-start fallback (synchronized `play_media` + timeline-poll-based drift correction via
the adapter contract's `CastController`-shaped trait) as the *default* path, upgrading to a native
sync primitive (Jellyfin SyncPlay, if verified suitable) only where the capability map confirms it.

---

## 3. Experience-layer module SCAFFOLD plan

### 3.1 Proposed module layout

Following the existing top-level `src/<feature>/` convention (`src/curation/`, `src/channels/`,
`src/proactive/`, `src/plex_control/`, etc. — there is no existing `src/experience/` umbrella, and
this document does not recommend inventing one; it would fight the grain of the current flat
feature-module layout):

```
src/
  persona/            # NEW — MUSEX-02/03/04
    mod.rs             # persona definition, resolution (which persona for which request)
    voice.rs           # Chord prompt-shaping layered on taste_model::chord_client::ChordClient
    blend.rs           # household/blended-persona aggregation OVER N taste_profile rows —
                        # never a mutation of the per-account taste_profile itself (see §1.1)

  media_server/        # NEW — MUSEX-08/09/10 adapter contract home
    mod.rs              # MediaServerClient + capability map traits (§2.2)
    plex.rs             # thin adapter: PlexClient/PlexControlClient -> MediaServerClient/CastController
    jellyfin.rs          # NEW client, implements the same traits (only once §2's contract lands)
    sync.rs              # watch-together orchestration: coordinated-start + drift correction,
                          # built against CastController — not server-specific

  assistant/            # NEW — MUSEX-12
    mod.rs               # conversational surface, reuses taste_review::trace::ReasoningTrace
                          # and curation::recommend rather than a second explanation path

  discord/               # NEW — MUSEX-13/14/15
    mod.rs                 # DiscordClient, same "typed client, graceful degrade if unconfigured"
                            # shape as ChordClient/PlexClient::from_config
    commands.rs             # inbound slash-command handling
    delivery.rs              # outbound: reuses proactive::generators' existing delivery pattern

  # channels/ (existing, MUSE-24) — MUSEX-05/06/07 extends compose.rs + presets.rs in place,
  #   does not fork a parallel director module.
  # web/ (existing) — MUSEX-18 extends artwork.rs/guide.rs + adds new routes, not a rewrite.
  # No new module for MUSEX-11 (adaptation) proposed yet — see §3.3, it likely belongs inside
  #   taste_model/ (extending signals.rs's real-time path) and channels/compose.rs (extending
  #   adjust_channel_run), not a new top-level module.
```

### 3.2 How it hooks into existing recommend/curation/taste code

- `persona/` reads from `taste_model::profile` (existing `taste_profile` rows) — it must not add a
  second per-account affinity store. Persona "voice" wraps `taste_model::chord_client::ChordClient`
  the same way `curation::recommend::build_rationale` and `channels::compose`'s optional LLM pass
  already do (construct once from `Config`, degrade to a deterministic fallback on any failure).
- `media_server/` replaces direct `PlexClient`/`PlexControlClient` construction in `AppState`
  (`src/http/`) with trait objects (`Arc<dyn MediaServerClient>`, `Arc<dyn CastController>`),
  mirroring how `AppState` already holds `Option<PlexClient>` gracefully-degrading today.
  `channels::compose` and `streaming::` would consume the trait, not the concrete Plex type,
  once this lands — that's the actual point of the abstraction, not just adding a Jellyfin client
  next to Plex.
- `assistant/` is a thin new HTTP surface over existing read paths (`curation::recommend`,
  `taste_review::trace`) — no new taste computation.
- `discord/` is purely a new delivery/command surface; it should call into the *existing*
  `proactive` HTTP handlers / generator outputs rather than duplicating generation logic.

### 3.3 Phase order + dependencies

1. **Phase 0 — Adapter contract (MUSEX-08 groundwork only, not full watch-together).** Land
   `media_server::MediaServerClient` + extend `CastController` usage, with `plex.rs` as a
   pass-through adapter over the *existing* `PlexClient`/`PlexControlClient` (zero behavior
   change, pure refactor-behind-a-trait). This unblocks everything else that wants
   server-abstracted playback state without waiting on Jellyfin support to exist.
2. **Phase 1 — Personas (MUSEX-02/03/04).** Depends only on existing `taste_model`. No dependency
   on Phase 0.
3. **Phase 2 — Channel director deepening (MUSEX-05/06/07).** Depends on Phase 1 if directives are
   persona-aware (per the S118 item ordering); otherwise depends on nothing new.
4. **Phase 3 — Watch-together (MUSEX-08/09/10).** Depends on Phase 0 (adapter contract) being in
   place; Jellyfin's actual `jellyfin.rs` adapter can be built in parallel with `sync.rs` once the
   trait shape is stable, gated on the external-API verification flagged in §2.3.
5. **Phase 4 — Adaptation (MUSEX-11).** Depends on Phase 2/3 existing (there needs to be a live
   channel-run or watch-together session to adapt) — likely the last brain-layer item, extending
   `channels::compose`'s existing `adjust_channel_run` and `taste_model::signals` real-time path
   rather than inventing new state.
6. **Phase 5 — Assistant (MUSEX-12).** Depends on Phase 1 (personas) for voice, and benefits from
   Phase 4 (adaptation) existing so it can explain live adjustments, but its baseline (explaining
   existing recommendations via `ReasoningTrace`) can start as soon as Phase 1 lands.
7. **Phase 6 — Discord (MUSEX-13/14/15) and GUI (MUSEX-18).** Presentation-layer; can proceed in
   parallel with each other once Phase 5 (assistant) and Phase 2 (channels) respectively have
   something to present. Not blocking, and not blocked by Phase 3/4.
8. **KG/graph (MUSEX-16/17)** sits outside this dependency chain — it depends on the *external*
   Atlas KG surface (Terminus `kg_*` tools), not on any Muse-internal phase above. Scope it as its
   own mini-spec once the Atlas KG's Muse-specific integration points are defined; this document
   does not attempt to design that integration (out of grounded-evidence scope — no existing Muse
   code touches Atlas today).

### 3.4 Scaffold: not added *by MUSEX-01* (persona/ since landed in MUSEX-02)

No Rust scaffold files were added alongside *this MUSEX-01 document*. Rationale: every proposed new
module in §3.1 (`persona/`, `media_server/`, `assistant/`, `discord/`) had zero existing code to
hang an empty stub off safely without guessing at a `mod.rs` shape that a later MUSEX item would
just delete and redo — an empty `pub mod persona;` with no types would not have saved the
implementer real work and risked becoming stale/misleading before MUSEX-02 actually landed. The
module layout above is the scaffold plan; MUSEX-02 (first persona item) and the MUSEX-08
adapter-contract groundwork item create their own `mod.rs` files as part of real implementation,
following this document's layout.

**Status:** MUSEX-02 has since landed the real `persona/` module (see the §1.2 update above) — the
`persona/` row of the §3.1 layout is now built (`mod.rs` = persona definition + `explain()`,
`derive.rs` = context-cluster + explicit derivation). MUSEX-03 has since landed `blend.rs`
(`persona::blend::blend_personas`): an agreement-weighted intersection of N personas' centroids
into one session taste vector for group watching (up-weights embedding dimensions the personas
agree on, suppresses ones they diverge on — deliberately not a naive average), with an
explanation built from the personas' shared `defining_signals.top_genres`, a `SinglePersona`
degrade for one input, and a cosine-similarity-based `NoOverlap` detector (weakest pairwise
persona-centroid similarity at/below 0.0) that surfaces a genuinely-divergent group instead of
silently blending it. The `persona/` row of §3.1 is now fully built. The `media_server/`,
`assistant/`, and `discord/` modules remain unbuilt scaffold plan.

---

## 4. Summary for later MUSEX items

- **Ground truth, not KG:** this document is sourced entirely from `moosenet/Muse` (`worktrees/MUSEX-01`,
  off `origin/main`) because Atlas KG tools were unreachable this session. Re-verify against the KG
  once reachable, particularly for anything that may have changed between this audit and a later
  item's actual implementation.
- **The brain is real and layered** (§1.1): `taste_model` (signals → profile) → `curation`
  (candidates → recommend) → `channels`/`proactive` (consumers). New experience-layer work should
  be additive on top of this, never a parallel taste-storage path, and must respect the
  never-blend-taste-across-accounts invariant explicitly when doing persona/household work.
- **No server abstraction exists yet; `CastController` is the seam to extend** (§2). Jellyfin has
  zero footprint in the repo. The proposed `MediaServerClient` trait (§2.2) mirrors the existing
  Plex read/write client split rather than inventing a new shape.
- **Sync capability is unverified almost everywhere** (§2.3) — build the coordinated-start
  fallback as the default, not the exception, and treat every "native sync" claim (Plex Watch
  Together, Jellyfin SyncPlay) as an external-API assumption requiring verification before MUSEX-08/09
  depends on it.
- **Scaffold plan is a layout + phase order** (§3), not committed stub code — Phase 0 (adapter
  contract) and Phase 1 (personas) are the two items with no upstream MUSEX dependency and can
  start immediately; everything else chains off them as mapped in §3.3.
