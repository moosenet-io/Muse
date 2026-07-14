//! MUSEX-18 (Plane TERM #394): load/save for the single
//! [`crate::settings::ExperienceSettings`] document backing the
//! Constellation GUI control + tuning panel. `migrations/0102_experience_settings.sql`
//! is the schema — one singleton row (`id = 1`) holding the whole document
//! as JSONB, per this crate's "thin repo layer, runtime sqlx only" doc in
//! `repo::mod`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::settings::ExperienceSettings;

/// Load the persisted settings document, or [`ExperienceSettings::default`]
/// when no row has been saved yet — a fresh deployment with no panel writes
/// yet must still answer `GET /api/settings` with sane defaults, never a
/// 404/500.
pub async fn load(pool: &PgPool) -> MuseResult<ExperienceSettings> {
    let row: Option<(serde_json::Value,)> =
        sqlx::query_as("SELECT data FROM experience_settings WHERE id = 1")
            .fetch_optional(pool)
            .await
            .map_err(MuseError::Database)?;

    match row {
        Some((data,)) => serde_json::from_value(data).map_err(|e| {
            MuseError::Internal(anyhow::anyhow!(
                "stored settings document did not deserialize into ExperienceSettings: {e}"
            ))
        }),
        None => Ok(ExperienceSettings::default()),
    }
}

/// Upsert the singleton settings row. Returns the same document back (the
/// caller-supplied value is authoritative once written — there is no
/// server-side mutation of the document on save), mirroring
/// `repo::persona::upsert_for_account`'s `RETURNING`-flavored ergonomics
/// even though this is a plain document write, not a generated row.
pub async fn save(pool: &PgPool, settings: &ExperienceSettings) -> MuseResult<ExperienceSettings> {
    let data = serde_json::to_value(settings).map_err(|e| {
        MuseError::Internal(anyhow::anyhow!(
            "failed to serialize ExperienceSettings: {e}"
        ))
    })?;

    sqlx::query(
        r#"
        INSERT INTO experience_settings (id, data, updated_at)
        VALUES (1, $1, now())
        ON CONFLICT (id) DO UPDATE SET
            data = EXCLUDED.data,
            updated_at = now()
        "#,
    )
    .bind(&data)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(settings.clone())
}

/// DB-backed coverage: real load/save round trip against Postgres,
/// `MUSE_TEST_DATABASE_URL`-gated per this crate's standing convention
/// (`crate::promotion::targeting::db_gated`, `crate::endpoint_tests::db_gated`,
/// …) — skips cleanly, never fails, when no test database is configured.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::settings::{
        DiscordBotSettings, QuestionFrequency, QuestionFrequencySettings, SharingGranularity,
        SharingSettings, TrustedFriendEntry,
    };

    async fn test_pool_or_skip(test_name: &str) -> Option<PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                 (expected in the default test run; this harness does not \
                 require a live DB)"
            );
            return None;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");
        Some(pool)
    }

    /// Round-trip persistence: save a non-default, deliberately varied
    /// document -> reload -> field-by-field equal (`ExperienceSettings`
    /// derives `PartialEq`, so a single `assert_eq!` covers every field).
    #[tokio::test]
    async fn save_then_load_round_trips_field_by_field() {
        let Some(pool) = test_pool_or_skip("save_then_load_round_trips_field_by_field").await
        else {
            return;
        };

        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.channel_director.serendipity_percent = 42.5;
        settings.adaptation_loop.aggressiveness = 0.9;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            promotion_cadence_secs: 3_600,
            promotion_match_threshold: 0.8,
            trusted_friends: vec![TrustedFriendEntry {
                discord_user_id: "discord-round-trip".to_string(),
                display_name: "Round Trip Probe".to_string(),
            }],
        };
        settings.question_frequency = QuestionFrequencySettings {
            frequency: QuestionFrequency::Reduced,
            silent_mode: false,
        };
        settings.sharing = SharingSettings {
            granularity: SharingGranularity::HouseholdOnly,
        };

        let saved = save(&pool, &settings).await.expect("save settings");
        assert_eq!(saved, settings);

        let reloaded = load(&pool).await.expect("load settings");
        assert_eq!(reloaded, settings);
    }

    /// A save followed by a SECOND save (different values) must overwrite,
    /// not accumulate a duplicate row — proves the singleton `ON CONFLICT`
    /// upsert actually upserts.
    #[tokio::test]
    async fn a_second_save_overwrites_rather_than_duplicates() {
        let Some(pool) = test_pool_or_skip("a_second_save_overwrites_rather_than_duplicates").await
        else {
            return;
        };

        let mut first = ExperienceSettings::default();
        first.channel_director.serendipity_percent = 10.0;
        save(&pool, &first).await.expect("first save");

        let mut second = ExperienceSettings::default();
        second.channel_director.serendipity_percent = 90.0;
        save(&pool, &second).await.expect("second save");

        let reloaded = load(&pool).await.expect("load settings");
        assert_eq!(reloaded.channel_director.serendipity_percent, 90.0);

        let row_count: i64 = sqlx::query_scalar("SELECT count(*) FROM experience_settings")
            .fetch_one(&pool)
            .await
            .expect("count rows");
        assert_eq!(row_count, 1);
    }

    /// `load` on a database with no settings row yet answers defaults, not
    /// an error — the "fresh deployment" path `load`'s own doc promises.
    #[tokio::test]
    async fn load_with_no_saved_row_returns_defaults() {
        let Some(pool) = test_pool_or_skip("load_with_no_saved_row_returns_defaults").await else {
            return;
        };

        // This test runs against whatever state the shared test DB happens
        // to be in (other tests in this module may have already saved a
        // row) -- so it only asserts the NO-ROW case when the table is
        // genuinely empty, rather than assuming ordering across tests.
        let row_count: i64 = sqlx::query_scalar("SELECT count(*) FROM experience_settings")
            .fetch_one(&pool)
            .await
            .expect("count rows");
        if row_count != 0 {
            eprintln!(
                "load_with_no_saved_row_returns_defaults: table already has a row \
                 (shared test DB, another test wrote it first) — skipping the \
                 no-row assertion rather than asserting on a state this test \
                 didn't create"
            );
            return;
        }

        let loaded = load(&pool).await.expect("load settings");
        assert_eq!(loaded, ExperienceSettings::default());
    }
}
