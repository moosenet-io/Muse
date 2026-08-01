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
/// Review finding (MACT-02, codex, confirmed): `name` is neither unique nor
/// stable (`plex_clients` rows are never deleted or freshness-checked), so
/// a `LIMIT 1 ORDER BY last_seen_at DESC` tiebreak can silently relay a stop
/// to an unrelated device — a renamed client, or two devices that happen to
/// share a display name. For a mutation that interrupts a person mid-film,
/// that ambiguity must be a refusal. This fetches at most 2 matching rows
/// and returns [`crate::repo::AtMostOne::Ambiguous`] when there's more than
/// one — the caller reports that distinctly from "no match at all"
/// ([`crate::repo::AtMostOne::None`], "no resolvable target", never a false
/// success either way).
///
/// TODO(S130-J): the real fix is for `play_sessions` to stamp the stable
/// Plex client id directly at ingest (spec J's territory), so resolution
/// never needs a name match — and this ambiguity can't arise — at all.
/// This function is a temporary bridge, not the intended long-term design.
pub async fn find_machine_identifier_by_name(
    pool: &PgPool,
    name: &str,
) -> MuseResult<crate::repo::AtMostOne<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT machine_identifier FROM plex_clients WHERE name = $1 \
         ORDER BY last_seen_at DESC LIMIT 2",
    )
    .bind(name)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(crate::repo::at_most_one(
        rows.into_iter().map(|(id,)| id).collect(),
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
