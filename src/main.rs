//! Muse — AI-native media curation & taste companion.
//!
//! Phase 0 scaffold (MUSE-01): boots tracing, config, a lazy Postgres pool,
//! the axum HTTP server, and the (currently empty) background-worker
//! harness. No domain logic lives here yet — see the founding spec
//! `specs/S96-muse-foundation.md`.

pub mod arr;
pub mod channels;
mod config;
pub mod curation;
mod db;
pub mod embed;
#[cfg(test)]
mod endpoint_tests;
pub mod enrichment;
mod error;
#[cfg(test)]
mod fixtures;
mod http;
#[cfg(test)]
mod integration_tests;
pub mod maintenance;
pub mod models;
mod plex;
mod plex_control;
pub mod proactive;
mod prowlarr;
mod radar;
mod recall;
pub mod repo;
mod snapshot;
mod streaming;
#[cfg(test)]
mod taste_mechanics_tests;
pub mod taste_model;
pub mod tautulli;
mod tracker;
mod trending;
mod tuner;
mod web;
mod workers;

use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::http::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MUSET-03: the snapshot-acquisition operator CLI. Gated behind an
    // explicit subcommand argument -- `muse snapshot-acquire` -- so it can
    // NEVER run as part of normal service startup (systemd/the container
    // entrypoint invoke `muse` with no arguments). This is the only path in
    // this binary that touches `snapshot::acquisition`'s process-spawning /
    // live-source-reading code; it returns before any of the server
    // bootstrap below runs.
    if std::env::args().nth(1).as_deref() == Some("snapshot-acquire") {
        return run_snapshot_acquire_cli().await;
    }

    let config = Config::from_env();

    init_tracing(&config.log_level);

    tracing::info!(bind_addr = %config.bind_addr, "starting muse");

    let pool = db::build_pool(&config)
        .map_err(|e| anyhow::anyhow!("failed to construct database pool: {e}"))?;

    let plex_client = crate::plex::PlexClient::from_config(&config);
    tracing::info!(
        plex_configured = plex_client.is_some(),
        "plex client initialized"
    );

    let prowlarr_client = crate::prowlarr::ProwlarrClient::from_config(&config);
    tracing::info!(
        prowlarr_configured = prowlarr_client.is_some(),
        "prowlarr client initialized"
    );

    // MUSE-05: parse the configured *arr fleet. A malformed MUSE_ARR_INSTANCES
    // degrades to zero instances (logged, not fatal) — same posture as an
    // unconfigured Plex client above.
    let arr_instances = match config.arr_instances() {
        Ok(instances) => {
            tracing::info!(count = instances.len(), "arr fleet configured");
            instances
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse MUSE_ARR_INSTANCES; arr ingest will have no instances");
            Vec::new()
        }
    };

    // MUSE-19/MUSE-09: `TmdbClient` is shared between the trending ingest
    // (MUSE-19) and `/query/resolve`'s beyond-the-library tier (MUSE-09).
    // Constructed once here (parity with the Plex client above, and so
    // TMDB_API_KEY misconfiguration is visible at boot); the scheduled
    // trending-ingest worker that calls `trending::snapshot_trending` on a
    // cadence is a follow-on wiring item — see `src/trending/mod.rs`.
    let tmdb_client = crate::trending::TmdbClient::from_config(&config);
    tracing::info!(
        tmdb_configured = tmdb_client.is_some(),
        "tmdb client initialized"
    );

    // MUSE-09: the query-embedding side of the MUSE-08 embed client. Reuses
    // the same `OllamaEmbedClient` type the embedder pipeline uses to embed
    // `media_item`s, just pointed at a caller's free-text query instead of
    // a title's composed source text.
    let embed_client = crate::embed::OllamaEmbedClient::from_config(&config);
    tracing::info!(
        embed_configured = embed_client.is_some(),
        "embed client initialized"
    );

    // MUSE-14: forum/critic sentiment + "does it get good" + renewal/
    // trailer news, cached into `external_enrichment`. Both sub-clients
    // degrade independently and gracefully — see `EnrichmentService`.
    let enrichment = crate::enrichment::EnrichmentService::from_config(&config);

    let state = Arc::new(AppState {
        pool,
        config: config.clone(),
        plex: plex_client,
        prowlarr: prowlarr_client,
        arr_instances,
        enrichment,
        tmdb: tmdb_client,
        embed: embed_client,
    });

    // Best-effort migration attempt at startup. This is a scaffold: if the DB
    // isn't reachable yet, log and continue — /health will report db:down
    // and MUSE-02+ will make this a harder gate once real schema exists.
    if let Err(e) = db::migrate(&state.pool).await {
        tracing::warn!(error = %e, "startup migration did not complete; continuing (db may be unavailable)");
    }

    workers::spawn_workers(state.clone());

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", config.bind_addr))?;

    let app = http::router(state);

    tracing::info!(bind_addr = %config.bind_addr, "muse listening");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("http server error: {e}"))?;

    Ok(())
}

/// MUSET-03 operator CLI: `muse snapshot-acquire`.
///
/// Runs the out-of-band, read-only acquisition step (AC1) -- copies any
/// configured SQLite sources, and/or runs `pg_dump` against a configured
/// source Postgres -- entirely from env-sourced configuration
/// (`snapshot::acquisition::AcquisitionConfig::from_env`). Never invoked by
/// `muse`'s normal service startup (see the gate in `main` above) and never
/// invoked by the test suite -- an operator runs this deliberately, out of
/// band, exactly as AC1 requires.
///
/// If a snapshot/test database DSN is ALSO configured
/// (`MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL`), each successful
/// acquisition records a provenance row (AC4) -- guard-checked (AC5) via the
/// same `snapshot::load::connect_snapshot_db` path everything else in this
/// pipeline uses. Without one configured, acquisition still runs and prints
/// each artifact's checksum, but records no provenance (loading + normalizing
/// into the isolated DB is a separate operator step, `snapshot::normalize`).
async fn run_snapshot_acquire_cli() -> anyhow::Result<()> {
    use crate::snapshot::{acquisition, load, provenance};

    init_tracing("info");

    let config = acquisition::AcquisitionConfig::from_env();
    let snapshot_db = load::connect_snapshot_db_from_env().await?;
    if let Some(pool) = &snapshot_db {
        load::migrate_snapshot_db(pool).await?;
    } else {
        tracing::warn!(
            "no {}/{} configured -- acquisition will run but no provenance will be recorded",
            load::SNAPSHOT_DATABASE_URL_VAR,
            load::TEST_DATABASE_URL_VAR,
        );
    }

    let Some(output_dir) = config.output_dir.clone() else {
        anyhow::bail!("MUSE_SNAPSHOT_OUTPUT_DIR is not set -- nowhere to write acquired artifacts");
    };

    let sqlite_sources: &[(
        Option<std::path::PathBuf>,
        provenance::SnapshotSourceKind,
        &str,
    )] = &[
        (
            config.plex_sqlite_path.clone(),
            provenance::SnapshotSourceKind::PlexSqlite,
            "plex-library.sqlite",
        ),
        (
            config.tautulli_sqlite_path.clone(),
            provenance::SnapshotSourceKind::TautulliSqlite,
            "tautulli-history.sqlite",
        ),
        (
            config.arr_sqlite_path.clone(),
            provenance::SnapshotSourceKind::ArrSqlite,
            "arr.sqlite",
        ),
    ];

    let mut acquired = 0usize;
    for (source_path, source_kind, dest_name) in sqlite_sources {
        let Some(source_path) = source_path else {
            continue;
        };
        let dest_path = output_dir.join(dest_name);
        let copied = acquisition::copy_sqlite_snapshot(*source_kind, source_path, &dest_path)?;
        let checksum = provenance::checksum_file(&copied.dest_path)?;
        tracing::info!(
            source = %source_kind,
            dest = %copied.dest_path.display(),
            bytes = copied.bytes_copied,
            checksum = %checksum,
            "acquired snapshot artifact"
        );
        if let Some(pool) = &snapshot_db {
            provenance::record(
                pool,
                &provenance::NewSnapshotProvenance {
                    source_identity: source_kind.as_str().to_string(),
                    source_kind: *source_kind,
                    snapshot_taken_at: chrono::Utc::now(),
                    checksum_sha256: checksum,
                    notes: Some(format!(
                        "acquired via `muse snapshot-acquire` ({dest_name})"
                    )),
                },
            )
            .await?;
        }
        acquired += 1;
    }

    if let Some(source_url) = config.source_postgres_url.as_deref() {
        // Lighter defensive check on the acquisition SOURCE DSN: reading a
        // live source is by-design (that's what acquisition is), but a bare
        // `muse`/prod-marked source is almost certainly a misconfiguration.
        // This is NOT the load-path guard (that protects the isolated test
        // DB the pipeline connects to); it's a nicety on the operator's
        // source input.
        snapshot::guard::validate_not_prod_source(source_url)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let dump_path = output_dir.join("muse-postgres.dump");
        let pg_dump = acquisition::PgDumpCommand::new(&config, dump_path.clone())?;
        let status = pg_dump.spawn()?;
        if !status.success() {
            anyhow::bail!("pg_dump exited with status: {status}");
        }
        let checksum = provenance::checksum_file(&dump_path)?;
        tracing::info!(dest = %dump_path.display(), checksum = %checksum, "acquired muse-postgres pg_dump snapshot");
        if let Some(pool) = &snapshot_db {
            provenance::record(
                pool,
                &provenance::NewSnapshotProvenance {
                    source_identity: provenance::SnapshotSourceKind::MusePostgres
                        .as_str()
                        .to_string(),
                    source_kind: provenance::SnapshotSourceKind::MusePostgres,
                    snapshot_taken_at: chrono::Utc::now(),
                    checksum_sha256: checksum,
                    notes: Some("acquired via `muse snapshot-acquire` (pg_dump)".to_string()),
                },
            )
            .await?;
        }
        acquired += 1;
    }

    if acquired == 0 {
        tracing::warn!(
            "no snapshot sources were configured (MUSE_SNAPSHOT_PLEX_SQLITE_PATH / \
             MUSE_SNAPSHOT_TAUTULLI_SQLITE_PATH / MUSE_SNAPSHOT_ARR_SQLITE_PATH / \
             MUSE_SNAPSHOT_SOURCE_POSTGRES_URL) -- nothing to acquire"
        );
    }

    Ok(())
}

fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
