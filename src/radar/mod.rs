//! "You vs the masses" taste divergence radar (MUSE-20, spec §3.7/§4c).
//!
//! Rendering is downstream (Lumina/Soma) — this module only computes the
//! `taste_divergence` DATA row ([`divergence::recompute_divergence`]) and
//! exposes a read helper ([`divergence::latest_divergence`]), plus the
//! population-side distribution math
//! ([`divergence::compute_population_distributions`]) that fills in the
//! `population_profile.genre_distribution`/`decade_distribution` seams
//! MUSE-19 deliberately left empty/NULL (see
//! `migrations/0043_population_profile.sql` and
//! `crate::trending::compute_population_profile`'s doc comment). No
//! HTTP/tool surface lives here — see `src/repo/taste_divergence.rs` for
//! the underlying queries this module's formulas are built from, and
//! `migrations/0044_taste_divergence.sql` for the table.

pub mod divergence;

pub use divergence::{
    compute_population_distributions, latest_divergence, recompute_divergence,
    recompute_divergence_for_region,
};
