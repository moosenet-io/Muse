//! MUSET-03 AC4: snapshot provenance -- source identity, vintage timestamp,
//! and checksum, recorded per snapshot load so a test run against the
//! isolated snapshot DB is reproducible against a known data vintage.
//!
//! Provenance is persisted to the `snapshot_provenance` table (see
//! `migrations/0099_snapshot_provenance.sql`) in the isolated snapshot/test
//! Postgres itself -- never anywhere near the source systems.

use std::fmt;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};

/// Which upstream a snapshot artifact was taken from. Deliberately an enum
/// (not a free-form string) at the Rust API boundary -- persisted as text
/// (see `as_str`/`parse`) so the schema can grow new sources without a
/// migration, while call sites stay typo-proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotSourceKind {
    /// Plex's library SQLite database (`com.plexapp.plugins.library.db`).
    PlexSqlite,
    /// Tautulli's playback-history SQLite database.
    TautulliSqlite,
    /// One of the *arr (Radarr/Sonarr/Prowlarr) SQLite databases.
    ArrSqlite,
    /// A `pg_dump` archive of the production muse Postgres database.
    MusePostgres,
}

impl SnapshotSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotSourceKind::PlexSqlite => "plex-library-sqlite",
            SnapshotSourceKind::TautulliSqlite => "tautulli-history-sqlite",
            SnapshotSourceKind::ArrSqlite => "arr-sqlite",
            SnapshotSourceKind::MusePostgres => "muse-postgres",
        }
    }
}

impl fmt::Display for SnapshotSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for SnapshotSourceKind {
    type Err = MuseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plex-library-sqlite" => Ok(SnapshotSourceKind::PlexSqlite),
            "tautulli-history-sqlite" => Ok(SnapshotSourceKind::TautulliSqlite),
            "arr-sqlite" => Ok(SnapshotSourceKind::ArrSqlite),
            "muse-postgres" => Ok(SnapshotSourceKind::MusePostgres),
            other => Err(MuseError::Config(format!(
                "unknown snapshot source kind: {other}"
            ))),
        }
    }
}

/// A single snapshot-load provenance record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotProvenance {
    pub id: i64,
    pub source_identity: String,
    pub source_kind: String,
    pub snapshot_taken_at: DateTime<Utc>,
    pub checksum_sha256: String,
    pub notes: Option<String>,
    pub loaded_at: DateTime<Utc>,
}

/// The inputs needed to record a new provenance row -- everything an
/// acquisition step knows about the snapshot artifact it just produced.
#[derive(Debug, Clone)]
pub struct NewSnapshotProvenance {
    /// A logical, non-identifying label for the source, e.g.
    /// "plex-library-sqlite" or "muse-postgres-pg_dump". MUST NOT contain a
    /// hostname/IP/credential (S1) -- it describes *what* was snapshotted,
    /// not *where from*.
    pub source_identity: String,
    pub source_kind: SnapshotSourceKind,
    pub snapshot_taken_at: DateTime<Utc>,
    pub checksum_sha256: String,
    pub notes: Option<String>,
}

/// Compute the SHA-256 checksum of a snapshot artifact file (a copied SQLite
/// db or a `pg_dump` archive) and return it hex-encoded.
///
/// This never opens the ORIGINAL source path -- callers pass the path to the
/// already-copied artifact (see `snapshot::acquisition`), keeping this
/// function itself agnostic to whether the source was ever live.
pub fn checksum_file(path: &Path) -> MuseResult<String> {
    let bytes = std::fs::read(path).map_err(|e| {
        MuseError::Internal(anyhow::anyhow!("reading snapshot artifact {path:?}: {e}"))
    })?;
    Ok(checksum_bytes(&bytes))
}

/// Compute the SHA-256 checksum of in-memory bytes (e.g. a `pg_dump` byte
/// stream captured without touching disk) and return it hex-encoded.
pub fn checksum_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Insert a provenance record into the isolated snapshot/test database's
/// `snapshot_provenance` table.
///
/// Callers MUST have already validated `pool`'s DSN via
/// `snapshot::guard::validate_snapshot_dsn` -- this function does not
/// re-validate (see `snapshot::load::connect_snapshot_db`, the one
/// sanctioned entry point that does).
pub async fn record(pool: &PgPool, new: &NewSnapshotProvenance) -> MuseResult<SnapshotProvenance> {
    sqlx::query_as::<_, SnapshotProvenance>(
        r#"
        INSERT INTO snapshot_provenance
            (source_identity, source_kind, snapshot_taken_at, checksum_sha256, notes)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, source_identity, source_kind, snapshot_taken_at, checksum_sha256, notes, loaded_at
        "#,
    )
    .bind(&new.source_identity)
    .bind(new.source_kind.as_str())
    .bind(new.snapshot_taken_at)
    .bind(&new.checksum_sha256)
    .bind(&new.notes)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// List provenance records, most-recently-loaded first -- lets a test run
/// (or an operator) confirm exactly which snapshot vintages back the
/// currently-loaded isolated test database.
pub async fn list(pool: &PgPool) -> MuseResult<Vec<SnapshotProvenance>> {
    sqlx::query_as::<_, SnapshotProvenance>(
        r#"
        SELECT id, source_identity, source_kind, snapshot_taken_at, checksum_sha256, notes, loaded_at
        FROM snapshot_provenance
        ORDER BY loaded_at DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for SnapshotProvenance {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        Ok(SnapshotProvenance {
            id: row.try_get("id")?,
            source_identity: row.try_get("source_identity")?,
            source_kind: row.try_get("source_kind")?,
            snapshot_taken_at: row.try_get("snapshot_taken_at")?,
            checksum_sha256: row.try_get("checksum_sha256")?,
            notes: row.try_get("notes")?,
            loaded_at: row.try_get("loaded_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_bytes_is_stable_and_content_sensitive() {
        let a = checksum_bytes(b"hello snapshot");
        let b = checksum_bytes(b"hello snapshot");
        let c = checksum_bytes(b"hello snapshot!");
        assert_eq!(a, b, "same content must hash identically");
        assert_ne!(a, c, "different content must hash differently");
        assert_eq!(a.len(), 64, "sha256 hex digest is 64 chars");
    }

    #[test]
    fn checksum_file_matches_checksum_bytes() {
        let dir =
            std::env::temp_dir().join(format!("muset03-checksum-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact.bin");
        std::fs::write(&path, b"snapshot artifact contents").unwrap();

        let via_file = checksum_file(&path).unwrap();
        let via_bytes = checksum_bytes(b"snapshot artifact contents");
        assert_eq!(via_file, via_bytes);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn source_kind_round_trips_through_its_string_form() {
        for kind in [
            SnapshotSourceKind::PlexSqlite,
            SnapshotSourceKind::TautulliSqlite,
            SnapshotSourceKind::ArrSqlite,
            SnapshotSourceKind::MusePostgres,
        ] {
            let s = kind.as_str();
            let parsed: SnapshotSourceKind = s.parse().unwrap();
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn source_kind_rejects_unknown_string() {
        let result = "definitely-not-a-source".parse::<SnapshotSourceKind>();
        assert!(result.is_err());
    }
}
