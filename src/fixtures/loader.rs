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
            let genre_id: i64 = sqlx::query_scalar(
                r#"
                INSERT INTO genres (name) VALUES ($1)
                ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
                RETURNING id
                "#,
            )
            .bind(genre_name)
            .fetch_one(pool)
            .await
            .map_err(MuseError::Database)?;

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
    })
}

/// Remove everything [`load`] wrote for `loaded`. Deleting the library
/// cascades its `media_items`, which in turn cascades their
/// `watch_stats`/`ratings`/`watchlist` rows (all `ON DELETE CASCADE` on
/// `media_item_id` — see `migrations/0017_watch_stats_ratings_watchlist.sql`).
/// Deleting the account cascades `taste_signals`/`taste_profile`/
/// `taste_context_centroids` (`migrations/0019_taste_profile.sql`,
/// `migrations/0020_taste_signals.sql`). Deleting each `media_metadata` row
/// cascades its `media_metadata_genres` join rows; the shared `genres` rows
/// themselves are left in place (harmless, reusable, not per-fixture-owned)
/// — matching the cleanup posture of `crate::snapshot::db_gated`'s tests.
/// Best-effort (`.ok()` on each statement, same as every other db_gated
/// cleanup in this crate) so a partial failure never panics a test's
/// teardown.
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
        assert_eq!(
            top.as_deref(),
            fixture.expectation.expected_top_genre,
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
