//! MUSET-04: the shared loader that seeds an [`super::AccountProfileFixture`]
//! into the isolated snapshot/test Postgres.
//!
//! This is the ONE place fixtures get inserted — Phase 3/4 call [`load`]
//! rather than each hand-rolling their own insert sequence, so every
//! consumer seeds data the same (real-shaped, guard-path-only) way. Callers
//! MUST pass a pool already obtained via
//! `crate::snapshot::load::connect_snapshot_db`/`connect_snapshot_db_from_env`
//! (the guarded path MUSET-03 established) — this module never opens its
//! own connection, exactly like `crate::snapshot::normalize`'s loaders trust
//! their caller on that.

use chrono::Utc;
use serde_json::Value as Json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{MuseError, MuseResult};
use crate::models::account::{Account, NewAccount};
use crate::models::library::{Library, NewLibrary};
use crate::models::media_item::{MediaItem, NewMediaItem};
use crate::models::media_metadata::{MediaMetadata, NewMediaMetadata};
use crate::models::watch_stats::NewWatchStats;
use crate::repo;

use super::AccountProfileFixture;

/// The result of loading one fixture: the created account/library/items, so
/// a caller (Phase 3/4 test, or this module's own round-trip test) can run
/// real taste/recommend code against them and assert against the fixture's
/// `expectation`.
#[derive(Debug)]
pub struct LoadedFixture {
    pub account: Account,
    pub library: Library,
    /// Index-aligned with the fixture's `library.items`.
    pub items: Vec<(MediaMetadata, MediaItem)>,
    /// Number of `watch_stats` rows written (one per `WatchSignal`).
    pub signals_inserted: usize,
    /// The per-run UUID suffix applied to every unique key this load
    /// created (library name/root, tmdb ids, plex account id, and the
    /// genre names) — so a caller can scope assertions to exactly this
    /// load's rows.
    pub suffix: String,
    /// The `genres` row ids [`load`] created for this fixture (each genre
    /// name is UUID-suffixed, so these rows are private to this load and
    /// never a shared/pre-existing reference genre). [`cleanup`] deletes
    /// exactly these, leaving shared genre reference data untouched.
    pub genre_ids: Vec<i64>,
    /// The genre names this fixture uses, mapped to the actual
    /// UUID-suffixed name written to `genres` — so a caller can relate a
    /// computed `genre_affinity` key (which is the suffixed name) back to
    /// the fixture's base genre name.
    pub genre_names: std::collections::BTreeMap<String, String>,
}

impl LoadedFixture {
    /// The suffixed genre name actually stored in `genres` for a fixture's
    /// base genre name (e.g. `"comfort-drama"` -> `"comfort-drama-<uuid>"`),
    /// which is also the key a taste `genre_affinity` map uses. Returns
    /// `None` if this load never seeded that base genre.
    pub fn suffixed_genre(&self, base: &str) -> Option<&str> {
        self.genre_names.get(base).map(|s| s.as_str())
    }
}

fn path_safe(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Seed `fixture` into `pool` (already guard-validated by the caller).
/// Every unique key (tmdb id, plex account id, library root folder) is
/// suffixed with a fresh UUID so repeated loads across test runs never
/// collide — same idiom as `crate::snapshot::db_gated` and
/// `crate::endpoint_tests::db_gated`.
pub async fn load(pool: &PgPool, fixture: &AccountProfileFixture) -> MuseResult<LoadedFixture> {
    let suffix = Uuid::new_v4().simple().to_string();

    let new_library = NewLibrary {
        name: format!("{}-{suffix}", fixture.library.name),
        kind: fixture.library.kind,
        root_folder: format!("/fixtures/muset04-{}-{suffix}", fixture.name),
        source_arr_name: None,
        source_arr_url: None,
    };
    let library = repo::library::create(pool, &new_library).await?;

    let mut genre_ids: Vec<i64> = Vec::new();
    let mut genre_names: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut items = Vec::with_capacity(fixture.library.items.len());
    for (idx, seed) in fixture.library.items.iter().enumerate() {
        let new_metadata = NewMediaMetadata {
            kind: seed.kind,
            tmdb_id: Some(format!("{}-{suffix}", seed.tmdb_id)),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: Json::Object(Default::default()),
            title: seed.title.to_string(),
            sort_title: None,
            original_title: None,
            original_language: None,
            status: None,
            overview: seed.overview.map(|s| s.to_string()),
            studio: None,
            network: None,
            runtime_minutes: seed.runtime_minutes,
            year: seed.year,
            images: Json::Array(Vec::new()),
        };
        let metadata = repo::media_metadata::upsert_by_tmdb(pool, &new_metadata).await?;

        for genre_name in seed.genres {
            // Suffix the genre name with the per-run UUID so the `genres`
            // row is PRIVATE to this load — never a shared/pre-existing
            // reference genre. This is what makes `cleanup` able to delete
            // exactly the rows this load created without touching any
            // reference data (and avoids cross-run contention on a shared
            // `(name)` row). The suffixed name is also the key the taste
            // `genre_affinity` map will use (the taste query returns
            // `genres.name`), so `genre_names` records the base->suffixed
            // mapping for callers to relate the two.
            let suffixed = format!("{genre_name}-{suffix}");
            let genre_id: i64 = sqlx::query_scalar(
                r#"
                INSERT INTO genres (name) VALUES ($1)
                ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
                RETURNING id
                "#,
            )
            .bind(&suffixed)
            .fetch_one(pool)
            .await
            .map_err(MuseError::Database)?;

            if !genre_ids.contains(&genre_id) {
                genre_ids.push(genre_id);
            }
            genre_names.insert((*genre_name).to_string(), suffixed);

            sqlx::query(
                "INSERT INTO media_metadata_genres (media_metadata_id, genre_id) \
                 VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(metadata.id)
            .bind(genre_id)
            .execute(pool)
            .await
            .map_err(MuseError::Database)?;
        }

        let new_item = NewMediaItem {
            library_id: library.id,
            media_metadata_id: metadata.id,
            path: format!(
                "/fixtures/muset04-{}-{suffix}/{}-{idx}.mkv",
                fixture.name,
                path_safe(seed.title)
            ),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: Some(format!("muset04-{}-{suffix}-{idx}", fixture.name)),
            added_at: None,
        };
        let item = repo::media_item::upsert(pool, &new_item).await?;
        items.push((metadata, item));
    }

    let new_account = NewAccount {
        plex_account_id: Some(format!("muset04-{}-{suffix}", fixture.name)),
        username: Some(format!("muset04_{}_{suffix}", fixture.name)),
        friendly_name: Some(fixture.name.to_string()),
        is_home_user: false,
        is_primary: false,
    };
    let account = repo::account::upsert_by_plex_account_id(pool, &new_account).await?;

    let now = Utc::now();
    let mut signals_inserted = 0usize;
    for signal in fixture.signals {
        let Some((_, item)) = items.get(signal.media_index) else {
            return Err(MuseError::Config(format!(
                "fixture {:?}: signal references media_index {} but the library only has {} items",
                fixture.name,
                signal.media_index,
                items.len()
            )));
        };
        let observed_at = now - chrono::Duration::days(signal.days_ago);

        let new_stats = NewWatchStats {
            account_id: account.id,
            media_item_id: item.id,
            play_count: signal
                .finished_count
                .max(if signal.abandoned { 1 } else { 0 }),
            finished_count: signal.finished_count,
            rewatch_count: signal.rewatch_count,
            total_watched_ms: 0,
            avg_percent: None,
            last_watched_at: Some(observed_at),
            abandoned: signal.abandoned,
            first_watched_at: Some(observed_at),
        };
        repo::watch_stats::upsert_watch_stats(pool, &new_stats).await?;
        signals_inserted += 1;

        if let Some(rating) = signal.rating {
            repo::watch_stats::upsert_rating(pool, account.id, item.id, rating, observed_at)
                .await?;
        }
        if signal.watchlisted {
            repo::watch_stats::add_to_watchlist(pool, account.id, item.id, observed_at).await?;
            if signal.watchlist_fulfilled {
                repo::watch_stats::mark_fulfilled(pool, account.id, item.id).await?;
            }
        }
    }

    Ok(LoadedFixture {
        account,
        library,
        items,
        signals_inserted,
        suffix,
        genre_ids,
        genre_names,
    })
}

/// Remove everything [`load`] wrote for `loaded`, leaving the DB exactly as
/// it was found (a load->cleanup round-trip nets zero rows in every table
/// the loader touches). Deletions run in FK-safe order:
/// 1. **library** — cascades its `media_items`, which cascade their
///    `watch_stats`/`ratings`/`watchlist` rows (all `ON DELETE CASCADE` on
///    `media_item_id` — see `migrations/0017_watch_stats_ratings_watchlist.sql`).
/// 2. **media_metadata** (each row this load created) — cascades its
///    `media_metadata_genres` join rows (`migrations/0011`), so the join
///    rows are gone before the `genres` rows they reference are deleted.
/// 3. **genres** (`loaded.genre_ids`) — each genre name was UUID-suffixed at
///    load time, so these rows are PRIVATE to this load; deleting exactly
///    them removes the fixture's genre state without touching any shared or
///    pre-existing reference genre. Runs AFTER (2) so no join row still
///    references them.
/// 4. **account** — cascades `taste_signals`/`taste_profile`/
///    `taste_context_centroids` (`migrations/0019`, `migrations/0020`).
///
/// Best-effort (`.ok()` on each statement, same as every other db_gated
/// cleanup in this crate) so a partial failure never panics a test's
/// teardown — the reversal test below asserts the round-trip actually nets
/// zero rows, including in `genres`/`media_metadata_genres`.
pub async fn cleanup(pool: &PgPool, loaded: &LoadedFixture) -> MuseResult<()> {
    sqlx::query("DELETE FROM libraries WHERE id = $1")
        .bind(loaded.library.id)
        .execute(pool)
        .await
        .ok();
    for (metadata, _) in &loaded.items {
        sqlx::query("DELETE FROM media_metadata WHERE id = $1")
            .bind(metadata.id)
            .execute(pool)
            .await
            .ok();
    }
    // Delete exactly this load's (UUID-suffixed, hence private) genre rows,
    // now that their `media_metadata_genres` join rows are gone with the
    // media_metadata above. Never touches shared/pre-existing genre rows.
    if !loaded.genre_ids.is_empty() {
        sqlx::query("DELETE FROM genres WHERE id = ANY($1)")
            .bind(&loaded.genre_ids)
            .execute(pool)
            .await
            .ok();
    }
    sqlx::query("DELETE FROM accounts WHERE id = $1")
        .bind(loaded.account.id)
        .execute(pool)
        .await
        .ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::load as snapshot_load;
    use crate::taste_model::profile;
    use crate::taste_model::signals::{replace_derived_signals, DEFAULT_HALF_LIFE_DAYS};

    /// Same skip-cleanly-without-a-DB idiom as
    /// `crate::snapshot::db_gated::snapshot_pool_or_skip` — reused directly
    /// rather than re-implemented, so this module is gated identically to
    /// the rest of the snapshot pipeline (MUSE_SNAPSHOT_DATABASE_URL /
    /// MUSE_TEST_DATABASE_URL, guard-checked, never a live system).
    async fn fixture_pool_or_skip(test_name: &str) -> Option<PgPool> {
        let Some(database_url) = snapshot_load::snapshot_database_url_from_env() else {
            eprintln!(
                "{} / {} not set -- skipping {test_name} (expected in the default test \
                 run; MUSET-04 fixtures do not require a live DB)",
                snapshot_load::SNAPSHOT_DATABASE_URL_VAR,
                snapshot_load::TEST_DATABASE_URL_VAR,
            );
            return None;
        };
        let pool = snapshot_load::connect_snapshot_db(&database_url)
            .await
            .expect("connect to the configured snapshot/test DSN (guard-checked)");
        snapshot_load::migrate_snapshot_db(&pool)
            .await
            .expect("migrations should apply cleanly to the isolated snapshot DB");
        Some(pool)
    }

    /// Find the genre with the largest weight in a `compute_genre_affinity`
    /// result, and the total positive mass, for the top-genre /
    /// max-single-genre-share assertions below.
    fn top_genre_and_shares(
        affinity: &Json,
    ) -> (Option<String>, f64, std::collections::BTreeMap<String, f64>) {
        let Json::Object(map) = affinity else {
            return (None, 0.0, Default::default());
        };
        let mut shares = std::collections::BTreeMap::new();
        let mut total = 0.0;
        for (k, v) in map {
            let w = v.as_f64().unwrap_or(0.0).max(0.0);
            shares.insert(k.clone(), w);
            total += w;
        }
        let top = shares
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(k, _)| k.clone());
        (top, total, shares)
    }

    #[tokio::test]
    async fn heavy_rewatcher_fixture_loads_and_the_rewatched_genre_dominates() {
        let Some(pool) =
            fixture_pool_or_skip("heavy_rewatcher_fixture_loads_and_the_rewatched_genre_dominates")
                .await
        else {
            return;
        };
        let fixture = super::super::heavy_rewatcher();
        let loaded = load(&pool, &fixture).await.expect("fixture should load");
        assert_eq!(loaded.signals_inserted, fixture.signals.len());

        let derived = replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed");
        assert_eq!(
            derived.len(),
            fixture.expectation.expected_signal_count,
            "derived taste_signals row count should match the fixture's documented expectation"
        );

        let now = Utc::now();
        let genre_affinity =
            profile::compute_genre_affinity(&pool, loaded.account.id, now, DEFAULT_HALF_LIFE_DAYS)
                .await
                .expect("computing genre affinity should succeed");
        let (top, _, _) = top_genre_and_shares(&genre_affinity);
        // The affinity map's key is the actual (UUID-suffixed) genre name
        // stored in `genres`, so relate the fixture's base expectation to
        // this load's suffixed name before comparing.
        let expected_top = fixture
            .expectation
            .expected_top_genre
            .and_then(|base| loaded.suffixed_genre(base))
            .map(|s| s.to_string());
        assert_eq!(
            top, expected_top,
            "the rewatched title's genre must dominate the affinity map"
        );

        cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn cold_start_empty_fixture_loads_and_produces_no_affinity() {
        let Some(pool) =
            fixture_pool_or_skip("cold_start_empty_fixture_loads_and_produces_no_affinity").await
        else {
            return;
        };
        let fixture = super::super::cold_start_empty();
        let loaded = load(&pool, &fixture).await.expect("fixture should load");
        assert_eq!(
            loaded.signals_inserted, 0,
            "cold-start fixture seeds no watch_stats rows"
        );

        let derived = replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed even with no history");
        assert!(
            derived.is_empty(),
            "a cold-start account should derive zero taste_signals"
        );
        assert_eq!(derived.len(), fixture.expectation.expected_signal_count);
        assert!(fixture.expectation.expect_empty_profile);

        let now = Utc::now();
        let genre_affinity = profile::compute_genre_affinity(
            &pool,
            loaded.account.id,
            now,
            DEFAULT_HALF_LIFE_DAYS,
        )
        .await
        .expect("computing genre affinity should succeed (and be empty) for a cold-start account");
        assert_eq!(genre_affinity, Json::Object(Default::default()));

        let centroid = profile::compute_overall_centroid(
            &pool,
            loaded.account.id,
            now,
            DEFAULT_HALF_LIFE_DAYS,
        )
        .await
        .expect("computing the overall centroid should succeed (and be None)");
        assert!(
            centroid.is_none(),
            "a cold-start account should have no overall_centroid"
        );

        cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn multi_genre_fixture_loads_and_no_single_genre_dominates() {
        let Some(pool) =
            fixture_pool_or_skip("multi_genre_fixture_loads_and_no_single_genre_dominates").await
        else {
            return;
        };
        let fixture = super::super::multi_genre();
        let loaded = load(&pool, &fixture).await.expect("fixture should load");

        let derived = replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed");
        assert_eq!(derived.len(), fixture.expectation.expected_signal_count);

        let now = Utc::now();
        let genre_affinity =
            profile::compute_genre_affinity(&pool, loaded.account.id, now, DEFAULT_HALF_LIFE_DAYS)
                .await
                .expect("computing genre affinity should succeed");
        let (_, total, shares) = top_genre_and_shares(&genre_affinity);
        assert!(
            total > 0.0,
            "a multi-genre profile should have positive affinity mass"
        );
        let max_share = fixture
            .expectation
            .max_single_genre_share
            .expect("fixture declares a bound");
        for (genre, w) in &shares {
            let share = w / total;
            assert!(
                share <= max_share,
                "genre {genre:?} carries {share:.3} of the affinity mass, exceeding the \
                 documented max_single_genre_share {max_share:.3} — the profile should stay spread out"
            );
        }

        cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn sparse_metadata_fixture_loads_and_degrades_cleanly_with_no_genres() {
        let Some(pool) = fixture_pool_or_skip(
            "sparse_metadata_fixture_loads_and_degrades_cleanly_with_no_genres",
        )
        .await
        else {
            return;
        };
        let fixture = super::super::sparse_metadata();
        let loaded = load(&pool, &fixture).await.expect("fixture should load");
        assert_eq!(loaded.signals_inserted, fixture.signals.len());

        let derived = replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed for a sparse-metadata title");
        assert_eq!(derived.len(), fixture.expectation.expected_signal_count);

        let now = Utc::now();
        // The load-bearing assertion: no genres attached to this title must
        // never error the affinity computation, just produce an empty map.
        let genre_affinity =
            profile::compute_genre_affinity(&pool, loaded.account.id, now, DEFAULT_HALF_LIFE_DAYS)
                .await
                .expect("genre affinity must degrade cleanly (not error) with no genres");
        assert_eq!(genre_affinity, Json::Object(Default::default()));

        cleanup(&pool, &loaded).await.ok();
    }

    /// Count rows matching a `name/tmdb`-style `LIKE '%<suffix>'` predicate.
    async fn count_like(pool: &PgPool, sql: &str, like: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(like)
            .fetch_one(pool)
            .await
            .expect("scoped count query should succeed")
    }

    /// Count rows matching a single `= <id>` predicate.
    async fn count_by_id(pool: &PgPool, sql: &str, id: i64) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("scoped count query should succeed")
    }

    /// The blocking finding from review (codex): `cleanup` must fully
    /// reverse `load` — a load->cleanup round-trip must net ZERO rows in
    /// EVERY table the loader touches, INCLUDING `genres` and the
    /// `media_metadata_genres` join. Uses `heavy_rewatcher` (the fixture
    /// that exercises genres + multiple signal types), derives taste_signals
    /// too, then asserts each scoped row exists after load and is gone after
    /// cleanup. Scoping every count to this load's UUID suffix / account id
    /// keeps the assertion correct even under cargo's parallel test
    /// execution (a concurrent fixture load has a different suffix, so it
    /// never perturbs these counts).
    #[tokio::test]
    async fn cleanup_fully_reverses_load_including_genres() {
        let Some(pool) = fixture_pool_or_skip("cleanup_fully_reverses_load_including_genres").await
        else {
            return;
        };
        let fixture = super::super::heavy_rewatcher();
        let loaded = load(&pool, &fixture).await.expect("fixture should load");
        // Also derive taste_signals, so the round-trip must reverse those.
        replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed");

        // Suffix-at-end predicate (genre names, tmdb ids, library name,
        // plex account id all END with the suffix); `like_contains` is for
        // `media_items.path`, where the suffix sits mid-string.
        let like = format!("%{}", loaded.suffix);
        let like_contains = format!("%{}%", loaded.suffix);

        // --- After load: the fixture's scoped rows all exist. ---
        assert!(
            count_like(
                &pool,
                "SELECT count(*) FROM genres WHERE name LIKE $1",
                &like
            )
            .await
                > 0,
            "load should have created this fixture's (suffixed) genre rows"
        );
        assert!(
            count_like(
                &pool,
                "SELECT count(*) FROM media_metadata_genres mmg \
                 JOIN media_metadata mm ON mm.id = mmg.media_metadata_id \
                 WHERE mm.tmdb_id LIKE $1",
                &like,
            )
            .await
                > 0,
            "load should have created media_metadata_genres join rows"
        );
        assert!(
            count_like(
                &pool,
                "SELECT count(*) FROM media_metadata WHERE tmdb_id LIKE $1",
                &like
            )
            .await
                > 0
        );
        assert!(
            count_like(
                &pool,
                "SELECT count(*) FROM libraries WHERE name LIKE $1",
                &like
            )
            .await
                > 0
        );
        assert!(
            count_like(
                &pool,
                "SELECT count(*) FROM accounts WHERE plex_account_id LIKE $1",
                &like
            )
            .await
                > 0
        );
        assert!(
            count_by_id(
                &pool,
                "SELECT count(*) FROM taste_signals WHERE account_id = $1",
                loaded.account.id
            )
            .await
                > 0,
            "deriving signals should have created taste_signals rows"
        );
        assert!(
            count_by_id(
                &pool,
                "SELECT count(*) FROM watch_stats WHERE account_id = $1",
                loaded.account.id
            )
            .await
                > 0
        );

        // --- After cleanup: every scoped row is gone (DB as we found it). ---
        cleanup(&pool, &loaded)
            .await
            .expect("cleanup should succeed");

        assert_eq!(
            count_like(
                &pool,
                "SELECT count(*) FROM genres WHERE name LIKE $1",
                &like
            )
            .await,
            0,
            "cleanup must delete the fixture's genre rows (the review finding)"
        );
        assert_eq!(
            count_like(
                &pool,
                "SELECT count(*) FROM media_metadata_genres mmg \
                 JOIN media_metadata mm ON mm.id = mmg.media_metadata_id \
                 WHERE mm.tmdb_id LIKE $1",
                &like,
            )
            .await,
            0,
            "cleanup must leave no media_metadata_genres join rows"
        );
        assert_eq!(
            count_like(
                &pool,
                "SELECT count(*) FROM media_metadata WHERE tmdb_id LIKE $1",
                &like
            )
            .await,
            0
        );
        assert_eq!(
            count_like(
                &pool,
                "SELECT count(*) FROM media_items WHERE path LIKE $1",
                &like_contains
            )
            .await,
            0
        );
        assert_eq!(
            count_like(
                &pool,
                "SELECT count(*) FROM libraries WHERE name LIKE $1",
                &like
            )
            .await,
            0
        );
        assert_eq!(
            count_like(
                &pool,
                "SELECT count(*) FROM accounts WHERE plex_account_id LIKE $1",
                &like
            )
            .await,
            0
        );
        assert_eq!(
            count_by_id(
                &pool,
                "SELECT count(*) FROM watch_stats WHERE account_id = $1",
                loaded.account.id
            )
            .await,
            0
        );
        assert_eq!(
            count_by_id(
                &pool,
                "SELECT count(*) FROM taste_signals WHERE account_id = $1",
                loaded.account.id
            )
            .await,
            0,
            "cleanup must leave no taste_signals rows"
        );
    }

    #[test]
    fn all_fixtures_reference_only_in_bounds_media_indices() {
        // A pure, DB-free sanity check that every fixture's declared
        // signals index into its own library — runs unconditionally (no
        // MUSE_TEST_DATABASE_URL needed), so a fixture authoring mistake
        // (a signal pointing past the end of its library) is caught even
        // when no scratch Postgres is configured.
        for fixture in super::super::all_fixtures() {
            for signal in fixture.signals {
                assert!(
                    signal.media_index < fixture.library.items.len(),
                    "fixture {:?}: signal media_index {} out of bounds for a {}-item library",
                    fixture.name,
                    signal.media_index,
                    fixture.library.items.len()
                );
            }
        }
    }
}
