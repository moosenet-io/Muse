//! Persistence for discovered `PlexPlayer`s into the `plex_clients` table
//! (migration `0090_plex_clients.sql`).
//!
//! Uses runtime `sqlx::query` (not the `query!` compile-time macro) since
//! this crate is built without a live `DATABASE_URL` available to
//! `sqlx-cli`/the build script in this environment.

use sqlx::postgres::PgPool;

use crate::error::{MuseError, MuseResult};

use super::models::PlexPlayer;

/// Upsert a batch of discovered players, keyed on `machine_identifier`.
/// Re-discovering an already-known player refreshes its metadata and
/// `last_seen_at`; discovery never deletes rows (a player that's briefly
/// offline should stay in the table).
pub async fn upsert_players(pool: &PgPool, players: &[PlexPlayer]) -> MuseResult<()> {
    for player in players {
        upsert_player(pool, player).await?;
    }
    Ok(())
}

async fn upsert_player(pool: &PgPool, player: &PlexPlayer) -> MuseResult<()> {
    sqlx::query(
        r#"
        INSERT INTO plex_clients
            (machine_identifier, name, product, device, platform, address, port, protocol_caps, is_cast_target, last_seen_at)
        VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
        ON CONFLICT (machine_identifier) DO UPDATE SET
            name           = EXCLUDED.name,
            product        = EXCLUDED.product,
            device         = EXCLUDED.device,
            platform       = EXCLUDED.platform,
            address        = EXCLUDED.address,
            port           = EXCLUDED.port,
            protocol_caps  = EXCLUDED.protocol_caps,
            is_cast_target = EXCLUDED.is_cast_target,
            last_seen_at   = now()
        "#,
    )
    .bind(&player.machine_identifier)
    .bind(&player.name)
    .bind(&player.product)
    .bind(&player.device)
    .bind(&player.platform)
    .bind(&player.address)
    .bind(player.port.map(i32::from))
    .bind(&player.protocol_caps)
    .bind(player.is_cast_target)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(())
}

/// Best-effort resolution from a `play_sessions.player` display name (e.g.
/// "Living Room") to the `plex_clients.machine_identifier` MACT-02 needs to
/// address a Companion stop command — Muse doesn't (yet) stamp the exact
/// client id on `play_sessions` at ingest time, only the name Plex reported.
///
/// Review finding, cycle 1 (MACT-02, codex, confirmed): `name` is neither
/// unique nor stable, so a `LIMIT 1 ORDER BY last_seen_at DESC` tiebreak can
/// silently relay a stop to an unrelated device — a renamed client, or two
/// devices that happen to share a display name. Fixed by fetching at most 2
/// matching rows and refusing (returning ambiguous) when there's more than
/// one, rather than picking.
///
/// Review finding, cycle 2 (codex, confirmed): uniqueness alone isn't
/// enough — `plex_clients` rows are never pruned, so *exactly one* match
/// can be an OBSOLETE row while the session actually belongs to a
/// newly-connected client sharing that name. `fresh_within_secs` bounds how
/// stale a match's `last_seen_at` may be before it's trusted
/// (`Config::terminate_target_fresh_within_secs`,
/// `MUSE_TERMINATE_TARGET_FRESH_SECS`). A name that matches only STALE rows
/// is [`crate::repo::FreshnessLookup::StaleOnly`] — a refusal distinct from
/// both "no match at all" and "ambiguous", never a silent promotion to
/// "the only one, so it must be right".
///
/// TODO(S130-J): the real fix is for `play_sessions` to stamp the stable
/// Plex client id directly at ingest (spec J's territory), so resolution
/// never needs a name match — and neither the ambiguity nor the staleness
/// hazard can arise — at all. This function is a temporary bridge, not the
/// intended long-term design; the freshness window above is the defensible
/// mitigation until then, not a substitute for it.
pub async fn find_machine_identifier_by_name(
    pool: &PgPool,
    name: &str,
    fresh_within_secs: u64,
) -> MuseResult<crate::repo::FreshnessLookup<String>> {
    let fresh_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT machine_identifier FROM plex_clients \
         WHERE name = $1 AND last_seen_at >= now() - make_interval(secs => $2) \
         ORDER BY last_seen_at DESC LIMIT 2",
    )
    .bind(name)
    .bind(fresh_within_secs as f64)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;

    let fresh_rows: Vec<String> = fresh_rows.into_iter().map(|(id,)| id).collect();

    // Only pay for the "does ANY match exist, fresh or not" check when
    // there were zero fresh rows -- a fresh match (one, or ambiguously
    // many) never needs it.
    let any_match_exists = if fresh_rows.is_empty() {
        let (exists,): (bool,) =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM plex_clients WHERE name = $1)")
                .bind(name)
                .fetch_one(pool)
                .await
                .map_err(MuseError::Database)?;
        exists
    } else {
        false
    };

    Ok(crate::repo::classify_with_freshness(
        fresh_rows,
        any_match_exists,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    /// Exercises the upsert against a real Postgres if `MUSE_TEST_DATABASE_URL`
    /// is set (pointed at the `plex_clients` migration having been applied);
    /// skips cleanly otherwise so the suite never requires a live DB.
    #[tokio::test]
    async fn upsert_players_inserts_then_updates() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("skipping upsert_players_inserts_then_updates: MUSE_TEST_DATABASE_URL not set");
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        let player = PlexPlayer {
            machine_identifier: "test-upsert-client".to_string(),
            name: Some("Test Chromecast".to_string()),
            product: Some("Chromecast".to_string()),
            device: Some("stb".to_string()),
            platform: None,
            address: Some("<internal-ip>".to_string()),
            port: Some(8009),
            protocol_caps: vec!["playback".to_string(), "timeline".to_string()],
            is_cast_target: true,
        };

        upsert_players(&pool, std::slice::from_ref(&player))
            .await
            .expect("first upsert");

        let name: Option<String> = sqlx::query_scalar(
            "SELECT name FROM plex_clients WHERE machine_identifier = $1",
        )
        .bind(&player.machine_identifier)
        .fetch_one(&pool)
        .await
        .expect("fetch name");
        assert_eq!(name.as_deref(), Some("Test Chromecast"));

        let mut updated = player.clone();
        updated.name = Some("Living Room Chromecast".to_string());
        upsert_players(&pool, std::slice::from_ref(&updated))
            .await
            .expect("second upsert (update path)");

        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM plex_clients WHERE machine_identifier = $1",
        )
        .bind(&player.machine_identifier)
        .fetch_one(&pool)
        .await
        .expect("count rows");
        assert_eq!(count, 1, "upsert must not create a duplicate row");

        let name: Option<String> = sqlx::query_scalar(
            "SELECT name FROM plex_clients WHERE machine_identifier = $1",
        )
        .bind(&player.machine_identifier)
        .fetch_one(&pool)
        .await
        .expect("fetch updated name");
        assert_eq!(name.as_deref(), Some("Living Room Chromecast"));

        sqlx::query("DELETE FROM plex_clients WHERE machine_identifier = $1")
            .bind(&player.machine_identifier)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
}
