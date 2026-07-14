//! MUSET-03 AC1: out-of-band, read-only snapshot ACQUISITION.
//!
//! This module is CODE an operator runs deliberately (via the `muse
//! snapshot-acquire` subcommand, gated in `main()` behind an explicit
//! `snapshot-acquire` argv check that returns before any normal service
//! bootstrap -- see `run_snapshot_acquire_cli` in `src/main.rs` -- so it
//! never runs as part of `muse`'s normal service startup, which invokes the
//! binary with no arguments). It is never invoked by the test suite and
//! never invoked automatically:
//!
//! - **SQLite sources** (Plex library db, Tautulli history db, an *arr db):
//!   a plain read-only file copy. The source is opened read-only and never
//!   written to; the destination is a snapshot artifact path the operator
//!   controls.
//! - **The muse Postgres source**: a `pg_dump` invocation, built here as a
//!   parameterized `std::process::Command` -- this module only *constructs*
//!   the command (fully unit-testable without ever running `pg_dump` or
//!   touching a live database); actually spawning it is a separate,
//!   explicit step (`PgDumpCommand::spawn`) the operator CLI calls.
//!
//! Every parameter (source paths, source DSN) comes from the environment via
//! [`AcquisitionConfig::from_env`] -- **never a hardcoded live path, host, or
//! credential** (S1/S7). None of the env vars this module reads are secret
//! *values* themselves (they are file paths / a DSN materialized by the
//! operator's own shell at run time, exactly like `MUSE_DATABASE_URL` is for
//! the main service) -- this module never embeds a literal DSN, host, or
//! credential in source.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{MuseError, MuseResult};
use crate::snapshot::provenance::SnapshotSourceKind;

/// Where an operator points the acquisition step -- entirely env-driven, so
/// nothing here is a hardcoded live location. Every field is optional: an
/// operator acquiring only one source (e.g. just the muse Postgres snapshot)
/// simply leaves the others unset.
#[derive(Debug, Clone, Default)]
pub struct AcquisitionConfig {
    /// Path to a read-only copy of, or direct read-only mount of, the Plex
    /// library SQLite file. `MUSE_SNAPSHOT_PLEX_SQLITE_PATH`.
    pub plex_sqlite_path: Option<PathBuf>,
    /// `MUSE_SNAPSHOT_TAUTULLI_SQLITE_PATH`.
    pub tautulli_sqlite_path: Option<PathBuf>,
    /// `MUSE_SNAPSHOT_ARR_SQLITE_PATH`.
    pub arr_sqlite_path: Option<PathBuf>,
    /// Connection string for the SOURCE muse Postgres database, used ONLY as
    /// the argument to `pg_dump` (never connected-to directly by this crate;
    /// see the module doc). `MUSE_SNAPSHOT_SOURCE_POSTGRES_URL`. Deliberately
    /// a distinct env var from `MUSE_DATABASE_URL` (the app's own runtime DB)
    /// and from `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL` (the
    /// isolated destination) -- three different roles, three different vars,
    /// so a copy-paste mistake can't quietly point one at another's target.
    pub source_postgres_url: Option<String>,
    /// Directory acquisition artifacts are written to.
    /// `MUSE_SNAPSHOT_OUTPUT_DIR`.
    pub output_dir: Option<PathBuf>,
    /// Path to the `pg_dump` binary. `MUSE_SNAPSHOT_PG_DUMP_PATH`, defaults
    /// to `"pg_dump"` (resolved via `$PATH`), same posture as
    /// `MUSE_FFMPEG_PATH`/`DEFAULT_FFMPEG_PATH` elsewhere in this crate.
    pub pg_dump_path: String,
}

const DEFAULT_PG_DUMP_PATH: &str = "pg_dump";

impl AcquisitionConfig {
    pub fn from_env() -> Self {
        Self {
            plex_sqlite_path: std::env::var("MUSE_SNAPSHOT_PLEX_SQLITE_PATH")
                .ok()
                .map(PathBuf::from),
            tautulli_sqlite_path: std::env::var("MUSE_SNAPSHOT_TAUTULLI_SQLITE_PATH")
                .ok()
                .map(PathBuf::from),
            arr_sqlite_path: std::env::var("MUSE_SNAPSHOT_ARR_SQLITE_PATH")
                .ok()
                .map(PathBuf::from),
            source_postgres_url: std::env::var("MUSE_SNAPSHOT_SOURCE_POSTGRES_URL").ok(),
            output_dir: std::env::var("MUSE_SNAPSHOT_OUTPUT_DIR")
                .ok()
                .map(PathBuf::from),
            pg_dump_path: std::env::var("MUSE_SNAPSHOT_PG_DUMP_PATH")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_PG_DUMP_PATH.to_string()),
        }
    }
}

/// Result of a completed read-only file-copy acquisition.
#[derive(Debug, Clone)]
pub struct CopiedArtifact {
    pub source_kind: SnapshotSourceKind,
    pub dest_path: PathBuf,
    pub bytes_copied: u64,
}

/// Copy a SQLite source file to `dest_path`, read-only on the source side.
///
/// Opens the source with a read-only [`std::fs::File`] handle first (proving
/// intent + failing fast if the path isn't readable) before delegating to
/// [`std::fs::copy`] -- `fs::copy` itself never opens its source for
/// writing, so this is a genuine read-only acquisition: nothing this
/// function does can mutate `source_path`.
pub fn copy_sqlite_snapshot(
    source_kind: SnapshotSourceKind,
    source_path: &Path,
    dest_path: &Path,
) -> MuseResult<CopiedArtifact> {
    // Prove read-only-ness explicitly rather than relying on fs::copy's
    // internals: open for read only, then drop the handle immediately.
    {
        let _read_only_handle = std::fs::File::open(source_path).map_err(|e| {
            MuseError::Internal(anyhow::anyhow!(
                "opening snapshot source {source_path:?} read-only: {e}"
            ))
        })?;
    }

    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            MuseError::Internal(anyhow::anyhow!(
                "creating snapshot output dir {parent:?}: {e}"
            ))
        })?;
    }

    let bytes_copied = std::fs::copy(source_path, dest_path).map_err(|e| {
        MuseError::Internal(anyhow::anyhow!(
            "copying snapshot artifact {source_path:?} -> {dest_path:?}: {e}"
        ))
    })?;

    Ok(CopiedArtifact {
        source_kind,
        dest_path: dest_path.to_path_buf(),
        bytes_copied,
    })
}

/// A parameterized `pg_dump` invocation, built but not yet spawned.
///
/// Construction never touches the network or a process table -- it is pure
/// argument assembly, fully unit-testable. Only [`PgDumpCommand::spawn`]
/// actually runs the external `pg_dump` binary, and only the operator CLI
/// (`muse snapshot-acquire`, `run_snapshot_acquire_cli` in `src/main.rs`)
/// calls `spawn`.
#[derive(Debug, Clone)]
pub struct PgDumpCommand {
    pg_dump_path: String,
    source_dsn: String,
    output_path: PathBuf,
}

impl PgDumpCommand {
    /// Build a `pg_dump` invocation against `source_dsn`, writing a
    /// custom-format (`-Fc`) archive to `output_path`. `source_dsn` is taken
    /// verbatim from [`AcquisitionConfig::source_postgres_url`] -- this
    /// function does not validate or transform it; it is the operator's
    /// responsibility to point it at the intended source (this is
    /// acquisition tooling, not the snapshot-DB connection path the S1/S9
    /// guard in `snapshot::guard` protects -- see the module-level "why no
    /// guard call here" note below).
    ///
    /// Deliberately NOT guarded by `snapshot::guard::validate_snapshot_dsn`:
    /// that guard protects the DESTINATION (the isolated test/snapshot DB
    /// this pipeline loads INTO and that tests connect to) from ever being a
    /// live DSN. This is the opposite direction -- reading FROM the real
    /// muse Postgres is the intended, documented purpose of `pg_dump`
    /// acquisition (AC1: "the mechanism ... as CODE/scripts that an operator
    /// runs"). The safety property here is procedural (out-of-band, operator
    /// invoked, never called by test code or automatically) rather than a
    /// DSN pattern-match.
    pub fn new(config: &AcquisitionConfig, output_path: PathBuf) -> MuseResult<Self> {
        let source_dsn = config.source_postgres_url.clone().ok_or_else(|| {
            MuseError::Config("MUSE_SNAPSHOT_SOURCE_POSTGRES_URL is not set".to_string())
        })?;

        Ok(Self {
            pg_dump_path: config.pg_dump_path.clone(),
            source_dsn,
            output_path,
        })
    }

    /// The argv this command will run with, for inspection/testing without
    /// spawning a process.
    pub fn args(&self) -> Vec<String> {
        vec![
            "-Fc".to_string(),
            "--no-owner".to_string(),
            "--no-privileges".to_string(),
            "-f".to_string(),
            self.output_path.to_string_lossy().into_owned(),
            "--dbname".to_string(),
            self.source_dsn.clone(),
        ]
    }

    fn build_command(&self) -> Command {
        let mut cmd = Command::new(&self.pg_dump_path);
        cmd.args(self.args());
        cmd
    }

    /// Actually run `pg_dump`. This is the one point in the whole crate that
    /// spawns a process against a (potentially live) Postgres source -- it
    /// is called ONLY from `run_snapshot_acquire_cli` in `src/main.rs` (the
    /// `muse snapshot-acquire` operator CLI), never from `muse`'s service
    /// startup and never from any `#[cfg(test)]` code (see the crate-level
    /// snapshot module doc's "never executed by tests" contract).
    pub fn spawn(&self) -> MuseResult<std::process::ExitStatus> {
        if let Some(parent) = self.output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MuseError::Internal(anyhow::anyhow!(
                    "creating snapshot output dir {parent:?}: {e}"
                ))
            })?;
        }
        self.build_command()
            .status()
            .map_err(|e| MuseError::Internal(anyhow::anyhow!("spawning pg_dump: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_env_reads_snapshot_specific_vars_not_the_app_or_dest_vars() {
        // Guard against accidental var-name collisions with
        // MUSE_DATABASE_URL / MUSE_SNAPSHOT_DATABASE_URL: acquisition's
        // source var is its own name.
        std::env::remove_var("MUSE_SNAPSHOT_SOURCE_POSTGRES_URL");
        let cfg = AcquisitionConfig::from_env();
        assert!(cfg.source_postgres_url.is_none());
        assert_eq!(cfg.pg_dump_path, DEFAULT_PG_DUMP_PATH);
    }

    #[test]
    fn copy_sqlite_snapshot_copies_content_and_never_needs_write_access_to_source() {
        let dir =
            std::env::temp_dir().join(format!("muset03-acquisition-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.sqlite");
        let dest = dir.join("nested").join("dest.sqlite");
        std::fs::write(&source, b"fake sqlite bytes").unwrap();

        // Make the source read-only on disk to prove the copy path never
        // needs write access to it.
        let mut perms = std::fs::metadata(&source).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&source, perms).unwrap();

        let result = copy_sqlite_snapshot(SnapshotSourceKind::PlexSqlite, &source, &dest).unwrap();
        assert_eq!(result.bytes_copied, 18);
        assert_eq!(std::fs::read(&dest).unwrap(), b"fake sqlite bytes");

        // Restore write perms so the temp dir can be cleaned up.
        let mut perms = std::fs::metadata(&source).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&source, perms).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_sqlite_snapshot_fails_cleanly_on_missing_source() {
        let dir = std::env::temp_dir().join(format!(
            "muset03-acquisition-missing-{}",
            uuid::Uuid::new_v4()
        ));
        let source = dir.join("does-not-exist.sqlite");
        let dest = dir.join("dest.sqlite");
        assert!(copy_sqlite_snapshot(SnapshotSourceKind::TautulliSqlite, &source, &dest).is_err());
    }

    #[test]
    fn pg_dump_command_new_requires_source_postgres_url() {
        let config = AcquisitionConfig {
            source_postgres_url: None,
            ..AcquisitionConfig::default()
        };
        let result = PgDumpCommand::new(&config, PathBuf::from("/tmp/out.dump"));
        assert!(result.is_err());
    }

    #[test]
    fn pg_dump_command_args_are_parameterized_never_hardcoded() {
        let config = AcquisitionConfig {
            source_postgres_url: Some("postgres://user:pass@example-source:5432/muse".to_string()),
            pg_dump_path: "pg_dump".to_string(),
            ..AcquisitionConfig::default()
        };
        let cmd = PgDumpCommand::new(&config, PathBuf::from("/tmp/muse-source.dump")).unwrap();
        let args = cmd.args();
        assert!(args.contains(&"-Fc".to_string()));
        assert!(args.contains(&"postgres://user:pass@example-source:5432/muse".to_string()));
        assert!(args.contains(&"/tmp/muse-source.dump".to_string()));
        // The DSN came from config, not a literal in this module's source --
        // proven by round-tripping a distinctive test value through it.
    }
}
