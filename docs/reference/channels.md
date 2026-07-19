# channels

The channel composer — "the agentic director" — plus named presets (188 KG nodes,
MUSE-24 + MUSEX-05/07). Given a `channels` row and a set of shows, it composes an
ordered lineup: round-robin each show's next-unwatched (or taste-ranked-priority)
episode, interleaved with cadence-matched interstitials, bounded by a target session
length — persisted as a fresh `channel_runs` row plus ordered `channel_programs` forming
a contiguous timeline (`end_at[i] == start_at[i+1]`).

Two properties are load-bearing:

- **Deterministic core, LLM-optional.** The algorithm works with no LLM at all; an
  optional pass via Chord's `/v1/chat/completions` may re-order round-robin priority and
  write a human rationale, and on ANY failure (unconfigured, network, bad response) it
  falls back to the deterministic order plus a templated rationale. Composition never
  fails because the LLM is unavailable.
- **Append-only history.** Re-composing always inserts a **new** `channel_runs` row —
  prior runs are never mutated.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `channels::director::program_channel` | fn | `src/channels/director.rs` | The MUSEX-05 director: an already-scored candidate pool → a timed, intent-tagged `ChannelSchedule` (it deliberately does not invent a second scoring formula) |
| `channels::director::TimeOfDay::from_hour` | fn | `src/channels/director.rs` | Time-of-day bucketing for director constraints |
| `channels::director::list_director_presets` | fn | `src/channels/director.rs` | The named director presets |
| `channels::compose::ComposeOptions` | struct | `src/channels/compose.rs` | Composer knobs (`use_llm`, ordering, session length) for `compose_channel_run`/`regenerate_channel_run`/`adjust_channel_run` |
| `channels::serendipity::SerendipityRange::from_fraction` | fn | `src/channels/serendipity.rs` | Clamped serendipity dial (rejects non-finite input) controlling how adventurous a lineup gets |
| `channels::presets::list_presets` | fn | `src/channels/presets.rs` | Named lineup presets (Saturday Morning, Prestige Drama Night, 90s Chaos, Comfort Rewatch, Discover, Household Movie …) |
| `channels::director_route::friends_with_one_opted_in` | fn | `src/channels/director_route.rs` | Consent filter for group-aware programming over the director HTTP route |

## How it connects

Reads candidates and episode state through `repo` and persists runs/programs through
`repo::channel`; the optional LLM pass goes through the Chord client. Consumers:
`tuner::scheduler` fills linear channels' grids (its own deterministic round-robin — the
compose director serves `mode='on_demand'`), `watch_together` calls `program_channel`
once per lobby preset to give a group genuinely distinct options, and `web`'s guide API
renders the resulting `channel_programs`. `streaming` turns the persisted grid into the
actual MPEG-TS stream.

## Configuration

- `CHORD_URL` — enables the optional LLM re-ordering/rationale pass.
- `MUSE_CHANNEL_GUIDE_WINDOW_HOURS`, `MUSE_CHANNEL_SCHEDULER_TICK_SECS` — the linear
  grid window and scheduler cadence (read by `tuner::scheduler`, which feeds the same
  `channel_programs` table).

## Notes and gaps

- `program_channel` takes an already-scored `Vec<DirectorCandidate>` and has no taste-
  vector parameter — blend-weighted ordering is the *caller's* job (see
  `watch_together`'s module doc for the reasoning).
- Determinism is tested (`program_channel_is_deterministic_for_identical_inputs`).
- Not covered here: how the composed grid becomes video — see
  [tuner](tuner.md) and the streaming section of [architecture](../architecture.md).
