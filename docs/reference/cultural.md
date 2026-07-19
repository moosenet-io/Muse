# cultural

The "what's hot / the talk" cultural layer (90 KG nodes, MUSEX-07). Pulls **trending**
(TMDb — reusing the existing `trending::TmdbClient` — plus, config-gated, Trakt
watcher counts) and **the talk** (comment/rating volume via Trakt) through the
`TrendSource` seam, cached behind `TrendCache` to respect API rate limits, then
**intersects** that with the account's actual library ownership (`media_items`) and
taste signal (`taste_profile` cosine similarity) to produce `CulturalPick`s.

All three legs are HARD gates: a trending title you own but whose taste relevance is
below `TASTE_RELEVANCE_MIN` (or has no computable taste signal) is **dropped**, not
merely ranked lower — the surface is a genuine three-way intersection, not "everything
you own that's trending, sorted by taste."

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `cultural::recommend_cultural` | fn | `src/cultural/mod.rs` | The single entry point a channel/GUI calls; branches on profile sparsity |
| `cultural::build_cultural_picks` | fn | `src/cultural/mod.rs` | The three-way intersection (trending ∩ owned ∩ taste ≥ `TASTE_RELEVANCE_MIN`), ranked taste-fit-descending |
| `cultural::is_profile_sparse` | fn | `src/cultural/mod.rs` | The cold-start trigger |
| `cultural::select_cold_start_picks` | fn | `src/cultural/mod.rs` | The sparse-profile fallback: genre-count-profiled picks instead of taste-fit ranking |
| `cultural::profile_with_genre_count` | fn | `src/cultural/mod.rs` | Genre-distribution profiling used by cold start |
| `cultural::source::TraktTrendSource::get` | fn | `src/cultural/source.rs` | The Trakt half of the `TrendSource` seam (public endpoints; inert unless `TRAKT_CLIENT_ID` is set) |
| `cultural::cache::TrendCache` | struct | `src/cultural/cache.rs` | TTL cache in front of any `TrendSource` (keys: `TrendCacheKey`/`TalkCacheKey`) |
| `cultural::source::MockTrendSource` | struct | `src/cultural/source.rs` | Test double for the seam |

## How it connects

Reads `taste_profile` and `media_items` through `repo`, and computes taste fit with the
same `persona::blend::cosine_similarity` the rest of the taste stack uses (no second
similarity formula). The TMDb leg reuses `trending`'s client rather than adding another
TMDb integration. Output `CulturalPick`s (with their `headline` "the talk" line) feed
curation surfaces and channel/GUI callers.

## Configuration

- `TRAKT_CLIENT_ID` — enables the Trakt trend source (required header credential).
- `TRAKT_API_KEY` — optional OAuth bearer for user-level endpoints (the public
  trending/talk pulls don't need it).
- `MUSE_TRAKT_BASE_URL` — override seam for tests/proxies; unset means Trakt's public
  host.
- `MUSE_TREND_CACHE_TTL_SECS` — cache freshness window (default 3600).
- `TMDB_API_KEY` — the TMDb leg (via `trending`).

## Notes and gaps

- `TASTE_RELEVANCE_MIN` is a code constant (0.2 in `src/cultural/mod.rs`), not an env
  tunable.
- Both sources are independently inert when unconfigured; with neither configured the
  layer produces nothing rather than erroring.
- Not covered here: how population-level divergence ("you vs the masses") is computed —
  that is `radar`, a separate subsystem.
