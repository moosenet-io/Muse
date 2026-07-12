//! Thin repo layer over the MUSE-02 arr-shaped core schema.
//!
//! All queries use **runtime** sqlx (`sqlx::query`/`sqlx::query_as`) per the
//! MUSE-02 build constraint — never the `query!`/`query_as!` compile-time
//! macros, since the crate must build without a live database.

pub mod channel;
pub mod episode;
pub mod interstitial;
pub mod library;
pub mod media_file;
pub mod media_item;
pub mod media_metadata;
pub mod quality;
pub mod season;
pub mod trending;
