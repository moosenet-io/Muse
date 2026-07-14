//! Domain models for the MUSE-02 arr-shaped core schema, plus the MUSE-03
//! telemetry/taste/embeddings/enrichment schema layered on top of it.
//!
//! MUSE-02 models map 1:1 onto the tables created by `migrations/0000_*.sql`
//! through `migrations/0011_*.sql`. MUSE-03 models (`account`, `play_event`,
//! `play_session`, `watch_stats`, `embedding`, `taste`, `proactive_item`,
//! `external_enrichment`) map onto `migrations/0012_*.sql` through
//! `migrations/0022_*.sql`. See `specs/S96-muse-foundation.md` §3.1-3.2
//! (arr core) and §3.3-3.5 (telemetry/taste/embeddings/enrichment) for the
//! original conceptual schema and
//! `<path>/spec-staging/muse/ARR-BLUEPRINT.md` for the real
//! Radarr/Sonarr recon the MUSE-02 schema was refined against — several
//! structural divergences from the spec are documented inline in the
//! migrations and summarized in each module here.

pub mod account;
pub mod artwork_cache;
pub mod availability;
pub mod channel;
pub mod embedding;
pub mod episode;
pub mod external_enrichment;
pub mod indexer;
pub mod interstitial;
pub mod library;
pub mod media_file;
pub mod media_item;
pub mod media_metadata;
pub mod persona;
pub mod play_event;
pub mod play_session;
pub mod premiere_discussion;
pub mod proactive_item;
pub mod quality;
pub mod release;
pub mod season;
pub mod taste;
pub mod taste_divergence;
pub mod taxonomy;
pub mod trending;
pub mod watch_stats;

pub use account::Account;
pub use artwork_cache::ArtworkCache;
pub use availability::Availability;
pub use channel::{
    Channel, ChannelKind, ChannelMode, ChannelProgram, ChannelProgramItemType, ChannelRun,
    ChannelRunStatus,
};
pub use embedding::{Embedding, EmbeddingEntityKind, EmbeddingMatch};
pub use episode::Episode;
pub use external_enrichment::ExternalEnrichment;
pub use indexer::{Indexer, NewIndexer};
pub use interstitial::{Interstitial, InterstitialKind};
pub use library::{Library, LibraryKind};
pub use media_file::{MediaFile, ReleaseTypeKind};
pub use media_item::MediaItem;
pub use media_metadata::{MediaKind, MediaMetadata};
pub use play_event::PlayEvent;
pub use play_session::{DecisionKind, PlaySession, PlaySessionMediaInfo};
pub use premiere_discussion::{DiscussionPost, DiscussionThread};
pub use proactive_item::ProactiveItem;
pub use quality::{CustomFormat, QualityDefinition, QualityProfile, QualityProfileFormat};
pub use release::{NewRelease, Release};
pub use season::Season;
pub use taste::{TasteContextCentroid, TasteProfile, TasteSignal};
pub use taste_divergence::{NewTasteDivergence, TasteDivergence};
pub use taxonomy::{Collection, Genre, MediaMetadataCredit, Person, Tag};
pub use trending::{
    NewPopulationProfile, NewStreamingAvailability, NewTrendingSnapshot, PopulationProfile,
    StreamingAvailability, TrendingSnapshot,
};
pub use watch_stats::{Rating, WatchStats, WatchlistEntry};
