//! MUSEL-B1 — the read-only filesystem library scanner.
//!
//! Walks a configured library root (`MUSE_LIBRARY_ROOT`, see
//! [`crate::config::Config::library_root`]), matches each media file it
//! finds to an *existing* `media_metadata` row (never creates a new title —
//! see [`scan`]'s module doc for why), records the on-disk file as a
//! `media_files` row, and pulls sidecar `.nfo`/poster/fanart art beside the
//! media (see [`sidecar`]).
//!
//! **READ-ONLY is a hard constraint** (spec MUSEL-B0/B1): nothing in this
//! module ever opens a file inside the library root for writing, creates,
//! removes, or renames anything under it. Every filesystem call this module
//! makes into the library root is one of: `read_dir`, `symlink_metadata`,
//! `metadata`, or `File::open` with `OpenOptions::new().read(true)` (never
//! `.write(true)`/`.create(true)`/`.append(true)`). Persistence goes only to
//! Muse's own Postgres database. See `src/library/scan.rs`'s
//! `no_write_create_remove_calls_in_the_module_source` test for the
//! structural proof, and `fixture_scan_leaves_the_library_byte_for_byte_unchanged`
//! for the behavioral proof.

pub mod scan;
pub mod sidecar;
