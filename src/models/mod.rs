//! Domain models for the MUSE-02 arr-shaped core schema.
//!
//! These map 1:1 onto the tables created by `migrations/0000_*.sql` through
//! `migrations/0011_*.sql`. See `specs/S96-muse-foundation.md` §3.1-3.2 for
//! the original conceptual schema and
//! `<path>/spec-staging/muse/ARR-BLUEPRINT.md` for the real
//! Radarr/Sonarr recon this schema was refined against — several structural
//! divergences from the spec are documented inline in the migrations and
//! summarized in each module here.

pub mod availability;
pub mod episode;
pub mod indexer;
pub mod library;
pub mod media_file;
pub mod media_item;
pub mod media_metadata;
pub mod quality;
pub mod release;
pub mod season;
pub mod taxonomy;

pub use availability::Availability;
pub use episode::Episode;
pub use indexer::{Indexer, NewIndexer};
pub use library::{Library, LibraryKind};
pub use media_file::{MediaFile, ReleaseTypeKind};
pub use media_item::MediaItem;
pub use media_metadata::{MediaKind, MediaMetadata};
pub use quality::{CustomFormat, QualityDefinition, QualityProfile, QualityProfileFormat};
pub use release::{NewRelease, Release};
pub use season::Season;
pub use taxonomy::{Collection, Genre, MediaMetadataCredit, Person, Tag};
