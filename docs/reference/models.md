# models

Domain models (161 KG nodes): the typed row structs and `New*` insert structs for the
MUSE-02 arr-shaped core schema, plus the MUSE-03 telemetry/taste/embeddings/enrichment
schema layered on top. MUSE-02 models map 1:1 onto `migrations/0000_*.sql` through
`0011_*.sql`; MUSE-03 models (`account`, `play_event`, `play_session`, `watch_stats`,
`embedding`, `taste`, `proactive_item`, `external_enrichment`) onto `0012_*.sql` through
`0022_*.sql`. Structural divergences from the founding spec are documented inline in the
migrations and summarized per module.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `models::embedding::NewEmbedding::nomic` | fn | `src/models/embedding.rs` | Constructor for a `nomic-embed-text` embedding row (entity kind + id + vector + source text) |
| `models::embedding::EmbeddingEntityKind::as_str` | fn | `src/models/embedding.rs` | Enum-to-column mapping for what an embedding describes |
| `models::media_item::NewMediaItem` | struct | `src/models/media_item.rs` | Insert shape for a library-owned title |
| `models::media_file::MediaFile::revision` | fn | `src/models/media_file.rs` | Reassembles the flattened quality/revision columns into a typed `Revision` |
| `models::availability::Availability` | struct | `src/models/availability.rs` | The per-title availability rollup row Prowlarr report-pull maintains |
| `models::episode::Episode` | struct | `src/models/episode.rs` | Episode row (series → season → episode hierarchy) |
| `models::account::NewAccount` | struct | `src/models/account.rs` | Insert shape for a household account |
| `models::artwork_cache::ArtworkCache` | struct | `src/models/artwork_cache.rs` | Cached artwork row backing the `/art/{kind}/{id}` proxy |

## How it connects

`models` is the shared vocabulary of the crate: `repo` functions take and return these
types; `arr::ingest` maps *arr API responses into `New*` structs; `decision` consumes
`models::quality::{QualityDefinition, QualityProfile, CustomFormat,
QualityProfileFormat}` and `models::release::Release` exactly as-is (which is what let
the release-decision engine merge independently of the acquisition branches);
`persona`/`taste_model` build on `models::persona`/`models::taste`. `models` itself
depends on nothing but serde/sqlx/pgvector derive machinery.

## Configuration

None — pure data types.

## Notes and gaps

- Later feature waves added further model modules beyond the MUSE-02/03 set (`persona`,
  `channel`, `interstitial`, `quality`, `release`, `acquisition`, `friend_opt_in`,
  `premiere_discussion` …) — the authoritative list is `src/models/mod.rs` and the
  migration files up through `0105_media_requests_monitored_item.sql`.
- This page does not restate the table-by-table data model; see
  [docs/schema.md](../schema.md).
