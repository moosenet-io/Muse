//! MUSET-04 (Plane TERM #369): reusable, real-snapshot-SHAPED fixtures +
//! documented golden expectations for the taste/recommend pipeline.
//!
//! ## Why this exists
//! Phase 3 (TASTE regression) and Phase 4 (shadow-parity) both need
//! representative library subsets, viewing-history profiles, and known-shape
//! edge cases to run against — data shaped like a real Plex/Tautulli
//! snapshot (see `crate::snapshot`), not one-off inline literals scattered
//! across each phase's own tests. This module is that shared source: a
//! small set of named fixtures, each with a [`ProfileExpectation`] that
//! documents what a CORRECT taste/recommend computation should produce for
//! it, plus a loader ([`loader::load`]) that seeds a fixture into the
//! isolated snapshot/test Postgres through the SAME guarded connection path
//! MUSET-03 established (`crate::snapshot::load::connect_snapshot_db` /
//! `MUSE_SNAPSHOT_DATABASE_URL` — never a raw `PgPool` that bypasses the
//! guard).
//!
//! ## The four fixtures
//! - [`heavy_rewatcher`] — one title rewatched many times should dominate
//!   the recency-weighted genre affinity over a single-finish title in a
//!   different genre.
//! - [`cold_start_empty`] — a freshly-onboarded account with a library but
//!   NO watch/rating/watchlist history at all. A correct recompute finds
//!   nothing to aggregate: empty affinity maps, no centroid.
//! - [`multi_genre`] — several titles across genuinely distinct genres,
//!   finished with equal weight and the same recency, so no single genre
//!   should dominate the resulting affinity map.
//! - [`sparse_metadata`] — a title with almost no descriptive metadata (no
//!   genres, no overview, no runtime, no year). Watch/rating signals still
//!   record correctly and a genre-affinity computation degrades cleanly
//!   (empty, not an error) rather than crashing on the missing dimensions.
//!
//! ## Real-shaped, never real data
//! Every title/genre/name below is synthetic (`"MUSET-04 ..."`-prefixed,
//! same idiom as `crate::snapshot`'s and `crate::endpoint_tests`' fixtures)
//! — no real library title, no real account, no real path. The loader
//! additionally suffixes every unique key (`tmdb_id`, `plex_account_id`,
//! library `root_folder`) with a fresh UUID so repeated test runs never
//! collide (matches `crate::snapshot::db_gated` and `endpoint_tests::db_gated`).
//!
//! ## No live-system contact (S9)
//! Nothing here ever opens its own `PgPool` — [`loader::load`] takes an
//! already-connected, already guard-validated pool (the caller obtains one
//! via `crate::snapshot::load::connect_snapshot_db_from_env` /
//! `connect_snapshot_db`, exactly like every other DB-gated test in this
//! crate). This module never reads `MUSE_DATABASE_URL` or any live-fleet
//! secret, and it is `#[cfg(test)]`-only — it never ships in the release
//! binary.

pub mod loader;

use crate::models::library::LibraryKind;
use crate::models::media_metadata::MediaKind;

/// One media title to seed into a fixture's library. Real-shaped (the same
/// fields `snapshot::normalize::normalize_plex_media_item` maps into
/// `NewMediaMetadata`), but every value here is synthetic.
#[derive(Debug, Clone)]
pub struct MediaSeed {
    pub kind: MediaKind,
    pub title: &'static str,
    pub year: Option<i32>,
    /// Base tmdb id — the loader appends a per-run UUID suffix so repeated
    /// loads never collide on the `(kind, tmdb_id)` upsert key.
    pub tmdb_id: &'static str,
    pub genres: &'static [&'static str],
    pub overview: Option<&'static str>,
    pub runtime_minutes: Option<i32>,
}

/// One (account, media) behavioral fact to seed — mirrors exactly the atoms
/// `taste_model::signals::derive_signals_for_account` turns into
/// `taste_signals` rows (finish / rewatch / abandon / rating / watchlist).
#[derive(Debug, Clone, Default)]
pub struct WatchSignal {
    /// Index into the fixture's `library.items`.
    pub media_index: usize,
    pub finished_count: i32,
    pub rewatch_count: i32,
    pub abandoned: bool,
    pub rating: Option<f32>,
    pub watchlisted: bool,
    pub watchlist_fulfilled: bool,
    /// `last_watched_at` (and `first_watched_at`) are seeded as
    /// `now - days_ago`, so fixtures can express "recent" vs "stale"
    /// viewing without hardcoding a timestamp.
    pub days_ago: i64,
}

/// A small library subset to seed for a fixture.
#[derive(Debug, Clone)]
pub struct LibrarySeed {
    pub name: &'static str,
    pub kind: LibraryKind,
    pub items: &'static [MediaSeed],
}

/// The documented "known-good" expectation for a fixture — what a CORRECT
/// taste/recommend computation should produce given this profile's seeded
/// data. Phase 3 (TASTE regression) and Phase 4 (shadow-parity) assert
/// against these fields instead of each independently re-deriving what
/// "correct" means for a given fixture.
#[derive(Debug, Clone)]
pub struct ProfileExpectation {
    /// Human-readable description of the scenario this fixture models.
    pub description: &'static str,
    /// True iff a correct taste recompute should find NOTHING to
    /// aggregate for this account: empty genre/person/keyword affinity
    /// maps and no `overall_centroid` — the cold-start case.
    pub expect_empty_profile: bool,
    /// If `Some`, the genre expected to carry the single largest
    /// recency-weighted affinity weight after a taste recompute
    /// (`taste_model::profile::compute_genre_affinity`).
    pub expected_top_genre: Option<&'static str>,
    /// If `Some`, an upper bound in `[0.0, 1.0]` on any ONE genre's share
    /// of the total positive affinity mass — used to assert a
    /// multi-genre profile stays genuinely spread out rather than
    /// dominated by a single genre.
    pub max_single_genre_share: Option<f64>,
    /// Number of `taste_signals` rows a correct derive
    /// (`taste_model::signals::derive_signals_for_account`) should
    /// produce for this account, given the fixture's seeded
    /// watch_stats/ratings/watchlist rows.
    pub expected_signal_count: usize,
}

/// A complete fixture: a name, a library subset, the behavioral signals to
/// seed against it, and the documented expectation of what a correct
/// taste/recommend computation over that data should look like.
#[derive(Debug, Clone)]
pub struct AccountProfileFixture {
    /// Short, stable identifier — used to build unique DB keys (never
    /// itself written as a raw title/username).
    pub name: &'static str,
    pub library: LibrarySeed,
    pub signals: &'static [WatchSignal],
    pub expectation: ProfileExpectation,
}

// ---------------------------------------------------------------------
// The four fixtures.
// ---------------------------------------------------------------------

/// A heavy-rewatcher profile: one title rewatched five times beyond its
/// first finish should dominate the recency-weighted genre affinity over a
/// single-finish title in a different genre (spec: rewatch is a "VERY
/// strong +" signal — see `taste_model::signals::WEIGHT_REWATCH_PER`).
pub fn heavy_rewatcher() -> AccountProfileFixture {
    const ITEMS: &[MediaSeed] = &[
        MediaSeed {
            kind: MediaKind::Movie,
            title: "MUSET-04 Comfort Rewatch Movie",
            year: Some(2015),
            tmdb_id: "muset04-heavy-comfort",
            genres: &["comfort-drama"],
            overview: Some("A synthetic MUSET-04 fixture title, rewatched often."),
            runtime_minutes: Some(105),
        },
        MediaSeed {
            kind: MediaKind::Movie,
            title: "MUSET-04 One-Off Documentary",
            year: Some(2019),
            tmdb_id: "muset04-heavy-doc",
            genres: &["documentary"],
            overview: Some("A synthetic MUSET-04 fixture title, watched once."),
            runtime_minutes: Some(90),
        },
    ];
    const SIGNALS: &[WatchSignal] = &[
        WatchSignal {
            media_index: 0,
            finished_count: 6,
            rewatch_count: 5,
            abandoned: false,
            rating: None,
            watchlisted: false,
            watchlist_fulfilled: false,
            days_ago: 5,
        },
        WatchSignal {
            media_index: 1,
            finished_count: 1,
            rewatch_count: 0,
            abandoned: false,
            rating: None,
            watchlisted: false,
            watchlist_fulfilled: false,
            days_ago: 5,
        },
    ];
    AccountProfileFixture {
        name: "heavy_rewatcher",
        library: LibrarySeed {
            name: "MUSET-04 Heavy Rewatcher Library",
            kind: LibraryKind::Movie,
            items: ITEMS,
        },
        signals: SIGNALS,
        expectation: ProfileExpectation {
            description: "one title rewatched 5x beyond its first finish (comfort-drama) \
                           vs. a single-finish title in a different genre (documentary); \
                           the rewatched title's genre must dominate the affinity map",
            expect_empty_profile: false,
            expected_top_genre: Some("comfort-drama"),
            max_single_genre_share: None,
            // finished + rewatched for item0 (2 rows), finished for item1 (1 row).
            expected_signal_count: 3,
        },
    }
}

/// A cold-start / freshly-onboarded profile: the account and its library
/// exist, but there is NO watch/rating/watchlist history at all. A correct
/// recompute must find nothing to aggregate, never error.
pub fn cold_start_empty() -> AccountProfileFixture {
    const ITEMS: &[MediaSeed] = &[MediaSeed {
        kind: MediaKind::Movie,
        title: "MUSET-04 Cold Start Unwatched Movie",
        year: Some(2021),
        tmdb_id: "muset04-cold-start",
        genres: &["adventure"],
        overview: Some("A synthetic MUSET-04 fixture title with no viewing history."),
        runtime_minutes: Some(100),
    }];
    const SIGNALS: &[WatchSignal] = &[];
    AccountProfileFixture {
        name: "cold_start_empty",
        library: LibrarySeed {
            name: "MUSET-04 Cold Start Library",
            kind: LibraryKind::Movie,
            items: ITEMS,
        },
        signals: SIGNALS,
        expectation: ProfileExpectation {
            description: "freshly-onboarded account: a library exists but there is no \
                           watch_stats/ratings/watchlist row at all — recompute must \
                           degrade to an empty profile, never error",
            expect_empty_profile: true,
            expected_top_genre: None,
            max_single_genre_share: None,
            expected_signal_count: 0,
        },
    }
}

/// A multi-genre profile: four titles across genuinely distinct genres,
/// each finished once with the same recency, so no single genre should
/// dominate the resulting affinity map.
pub fn multi_genre() -> AccountProfileFixture {
    const ITEMS: &[MediaSeed] = &[
        MediaSeed {
            kind: MediaKind::Movie,
            title: "MUSET-04 Multi-Genre Scifi Title",
            year: Some(2018),
            tmdb_id: "muset04-multi-scifi",
            genres: &["scifi"],
            overview: Some("A synthetic MUSET-04 fixture title."),
            runtime_minutes: Some(115),
        },
        MediaSeed {
            kind: MediaKind::Movie,
            title: "MUSET-04 Multi-Genre Comedy Title",
            year: Some(2018),
            tmdb_id: "muset04-multi-comedy",
            genres: &["comedy"],
            overview: Some("A synthetic MUSET-04 fixture title."),
            runtime_minutes: Some(95),
        },
        MediaSeed {
            kind: MediaKind::Movie,
            title: "MUSET-04 Multi-Genre Horror Title",
            year: Some(2018),
            tmdb_id: "muset04-multi-horror",
            genres: &["horror"],
            overview: Some("A synthetic MUSET-04 fixture title."),
            runtime_minutes: Some(98),
        },
        MediaSeed {
            kind: MediaKind::Movie,
            title: "MUSET-04 Multi-Genre Romance Title",
            year: Some(2018),
            tmdb_id: "muset04-multi-romance",
            genres: &["romance"],
            overview: Some("A synthetic MUSET-04 fixture title."),
            runtime_minutes: Some(102),
        },
    ];
    const SIGNALS: &[WatchSignal] = &[
        WatchSignal {
            media_index: 0,
            finished_count: 1,
            rewatch_count: 0,
            abandoned: false,
            rating: None,
            watchlisted: false,
            watchlist_fulfilled: false,
            days_ago: 10,
        },
        WatchSignal {
            media_index: 1,
            finished_count: 1,
            rewatch_count: 0,
            abandoned: false,
            rating: None,
            watchlisted: false,
            watchlist_fulfilled: false,
            days_ago: 10,
        },
        WatchSignal {
            media_index: 2,
            finished_count: 1,
            rewatch_count: 0,
            abandoned: false,
            rating: None,
            watchlisted: false,
            watchlist_fulfilled: false,
            days_ago: 10,
        },
        WatchSignal {
            media_index: 3,
            finished_count: 1,
            rewatch_count: 0,
            abandoned: false,
            rating: None,
            watchlisted: false,
            watchlist_fulfilled: false,
            days_ago: 10,
        },
    ];
    AccountProfileFixture {
        name: "multi_genre",
        library: LibrarySeed {
            name: "MUSET-04 Multi-Genre Library",
            kind: LibraryKind::Movie,
            items: ITEMS,
        },
        signals: SIGNALS,
        expectation: ProfileExpectation {
            description: "four titles across distinct genres, each finished once with the \
                           same recency — no single genre should dominate the affinity map",
            expect_empty_profile: false,
            expected_top_genre: None,
            // 4 equally-weighted genres -> ~25% each; allow generous headroom
            // (0.4) so this stays a "no genre dominates" check, not a
            // brittle exact-quarter assertion.
            max_single_genre_share: Some(0.4),
            expected_signal_count: 4,
        },
    }
}

/// A sparse-metadata edge case: a title with almost no descriptive
/// metadata (no genres, no overview, no runtime, no year). Watch signals
/// still record correctly, and genre-affinity computation must degrade
/// cleanly (an empty result, never an error) on the missing dimension.
pub fn sparse_metadata() -> AccountProfileFixture {
    const ITEMS: &[MediaSeed] = &[MediaSeed {
        kind: MediaKind::Movie,
        title: "MUSET-04 Sparse Metadata Title",
        year: None,
        tmdb_id: "muset04-sparse",
        genres: &[],
        overview: None,
        runtime_minutes: None,
    }];
    const SIGNALS: &[WatchSignal] = &[WatchSignal {
        media_index: 0,
        finished_count: 1,
        rewatch_count: 0,
        abandoned: false,
        rating: Some(7.0),
        watchlisted: false,
        watchlist_fulfilled: false,
        days_ago: 2,
    }];
    AccountProfileFixture {
        name: "sparse_metadata",
        library: LibrarySeed {
            name: "MUSET-04 Sparse Metadata Library",
            kind: LibraryKind::Movie,
            items: ITEMS,
        },
        signals: SIGNALS,
        expectation: ProfileExpectation {
            description: "a title with no genres/overview/runtime/year; a finish signal \
                           records correctly but genre-affinity computation degrades to \
                           empty (no genres to aggregate), never errors",
            expect_empty_profile: false,
            expected_top_genre: None,
            max_single_genre_share: None,
            expected_signal_count: 1,
        },
    }
}

/// Every fixture this module ships, for callers (Phase 3/4) that want to
/// iterate the whole set rather than naming one.
pub fn all_fixtures() -> Vec<AccountProfileFixture> {
    vec![
        heavy_rewatcher(),
        cold_start_empty(),
        multi_genre(),
        sparse_metadata(),
    ]
}
