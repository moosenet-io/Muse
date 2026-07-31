//! MUSE #108: `GET /api/search` — free-text metadata search across every configured
//! provider, with each provider's real state reported alongside the results.
//!
//! Muse could already search (`MetadataProvider::search`, implemented by `TmdbClient` and
//! `TvdbClient`), but nothing exposed it over HTTP, so the GUI had no way to find something
//! to request. This is that door.
//!
//! ## The provider list IS the answer, not decoration
//!
//! The response carries a `providers` array describing the clients as they are actually
//! constructed on this deployment — name, mode, whether configured, and **which kinds each
//! one can search in the mode it is currently in**. That last part is the whole reason this
//! endpoint reports rather than just returns:
//!
//!   - key-less TMDb (the Radarr metadata proxy) searches **movies only**
//!   - key-less TVDB (Sonarr's Skyhook) searches **series only**
//!
//! So on a key-less deployment — which is the default, and what <host> runs — full coverage
//! exists only by combining the two. If one of them is down, the result list is not merely
//! shorter: it is *missing a whole media kind*, and a caller who cannot see that would read
//! a partial answer as a complete one.
//!
//! ## Never fail-open into a false empty
//!
//! Every provider call is captured independently. A provider that errors reports
//! `status: "error"` with its message and does not contribute results; a provider that
//! genuinely found nothing reports `status: "ok"` with `result_count: 0`. Those two are
//! different facts and the response keeps them apart.
//!
//! This is the direct lesson of MUSE #106, where Skyhook search asked a wrong URL, got a 404,
//! and fail-open turned it into an empty list that was indistinguishable from "no results" —
//! for months, on every key-less deployment. An endpoint that collapses "could not search"
//! into "found nothing" recreates that bug at the API layer, so this one refuses to.
//!
//! ## Read-only
//!
//! Mounted on the PUBLIC router beside `/api/discover` and `/api/library`: it exposes no
//! per-account data. It performs no writes and cannot trigger a grab — filing a request is a
//! separate, gated call (`crate::http::requests`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::metadata::{MediaKind, MetadataProvider, ProviderMetadata};

/// Bound on results returned per provider/kind pair. Providers already return a relevance
/// -ordered page; this only stops a pathological response from becoming a huge payload.
const MAX_PER_PROVIDER: usize = 40;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// The free-text term. Required — see the handler on why an absent term is a 400 and
    /// not an empty result set.
    pub q: Option<String>,
    /// `movie`, `series`/`show`, or `all` (default).
    pub kind: Option<String>,
}

/// Which media kinds the caller asked about.
fn requested_kinds(raw: Option<&str>) -> MuseResult<Vec<MediaKind>> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(vec![MediaKind::Movie, MediaKind::Series]),
        Some(k) => match k.to_ascii_lowercase().as_str() {
            "all" => Ok(vec![MediaKind::Movie, MediaKind::Series]),
            "movie" | "movies" => Ok(vec![MediaKind::Movie]),
            // Muse names this kind `Series`; parts of the ecosystem say "show" or "tv".
            // All spellings are accepted rather than making callers guess which one wins.
            "series" | "show" | "shows" | "tv" => Ok(vec![MediaKind::Series]),
            other => Err(MuseError::BadRequest(format!(
                "unknown kind {other:?}; expected one of: movie, series, all"
            ))),
        },
    }
}

fn kind_str(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Series => "series",
    }
}

/// One provider as this deployment actually has it — not a static catalog entry.
struct ProviderEntry {
    name: &'static str,
    mode: &'static str,
    /// `None` when the provider is NOT configured on this deployment. It is still reported —
    /// see `providers()`.
    configured: bool,
    /// The kinds this provider can search IN ITS CURRENT MODE. Key-less modes are
    /// single-kind; see the module doc.
    searchable: Vec<MediaKind>,
    /// `None` for an unconfigured provider: there is nothing to search with, but the entry
    /// still appears so the caller can see that the provider exists and is switched off.
    client: Option<Box<dyn MetadataProvider>>,
}

/// Build the provider set from live state, exactly as the rest of Muse constructs it.
///
/// TMDb is taken from `AppState` (constructed once at boot). TVDB is built here from config,
/// which is the same thing `web::dashboard` does — it is not held on `AppState` today, and
/// putting it there is a wider change than this endpoint needs.
fn providers(state: &AppState) -> Vec<ProviderEntry> {
    let mut out: Vec<ProviderEntry> = Vec::new();

    // EVERY known provider gets an entry, configured or not. An earlier version pushed only
    // successfully-constructed clients and hard-coded `configured: true`, so an unconfigured
    // provider vanished from the array entirely — and the caller could not tell "TVDB is not
    // set up" from "this endpoint does not know about TVDB". It saw only an uncovered kind,
    // with no way to learn why (gpt56). The `providers` array is documented as the operator's
    // answer to "what metadata APIs do I have"; a provider that is off is part of that answer.
    if let Some(tmdb) = state.tmdb.clone() {
        let proxy = tmdb.is_proxy_mode();
        out.push(ProviderEntry {
            name: "tmdb",
            configured: true,
            mode: if proxy { "radarr_proxy" } else { "api" },
            // AMETA-1/2: the key-less Radarr proxy serves movie lookup/search only. Claiming
            // series coverage here would make a missing half look like an empty half.
            searchable: if proxy {
                vec![MediaKind::Movie]
            } else {
                vec![MediaKind::Movie, MediaKind::Series]
            },
            client: Some(Box::new(tmdb)),
        });
    } else {
        out.push(ProviderEntry {
            name: "tmdb",
            configured: false,
            mode: "unconfigured",
            searchable: Vec::new(),
            client: None,
        });
    }

    if let Some(tvdb) = crate::metadata::tvdb::TvdbClient::from_config(&state.config) {
        let skyhook = tvdb.is_skyhook_mode();
        out.push(ProviderEntry {
            name: "tvdb",
            configured: true,
            mode: if skyhook { "skyhook" } else { "api" },
            // AMETA-3: Skyhook is Sonarr's series proxy — no movies.
            searchable: if skyhook {
                vec![MediaKind::Series]
            } else {
                vec![MediaKind::Movie, MediaKind::Series]
            },
            client: Some(Box::new(tvdb)),
        });
    } else {
        out.push(ProviderEntry {
            name: "tvdb",
            configured: false,
            mode: "unconfigured",
            searchable: Vec::new(),
            client: None,
        });
    }

    out
}

/// The provider ids a hit carries, lowercased, as `(provider, id)` pairs.
fn id_pairs(meta: &ProviderMetadata) -> Vec<(String, String)> {
    meta.provider_ids
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect()
}

/// `media_metadata.kind` is the DB enum `media_kind`, whose variants are `movie` and `show`.
/// That is a DIFFERENT spelling from this endpoint's wire value (`movie`/`series`) and from
/// `metadata::MediaKind`. Converted explicitly rather than by string reuse, so the two
/// vocabularies cannot drift into each other.
fn db_kind(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Series => "show",
    }
}

/// One catalog row's identifying facts, as stored.
#[derive(Debug, Clone)]
struct RowFacts {
    tmdb: Option<String>,
    tvdb: Option<String>,
    imdb: Option<String>,
    /// A `media_items` row exists — the same join `repo::dashboard`'s library query uses. A
    /// `media_metadata` row alone is NOT ownership: `/api/discover` writes metadata rows for
    /// trending titles precisely because they are not held.
    owned: bool,
}

/// The catalog rows reachable from a search hit's identifiers.
struct CatalogIndex {
    /// `(provider, id, db_kind)` -> every row carrying that identifier. A `Vec` because
    /// `imdb_id` has NO uniqueness constraint (only a plain INDEX, migration 0005), unlike
    /// `UNIQUE(kind, tmdb_id)` / `UNIQUE(kind, tvdb_id)`.
    by_key: HashMap<(String, String, &'static str), Vec<i64>>,
    rows: HashMap<i64, RowFacts>,
}

/// What can be said about ONE search hit, after every identifier it carries has been examined.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HitResolution {
    /// `Some(true)`/`Some(false)` when the hit pins exactly one catalog row. `None` means
    /// UNKNOWN — the identifiers did not agree, or nothing could be checked — and must NEVER
    /// be rendered as "not owned".
    pub in_library: Option<bool>,
    /// Also tri-state, for the same reason: when the hit carries no identifier this endpoint
    /// can look up, whether Muse has the title is unknown rather than false.
    pub in_catalog: Option<bool>,
    pub metadata_id: Option<i64>,
    pub ambiguous: bool,
    pub resolution: Resolution,
}

/// A single hit identifier, paired with what the candidate row says about it.
///
/// The distinction this type exists to make: an identifier the matched row does NOT RECORD
/// carries no information, while one the row records DIFFERENTLY is a genuine contradiction.
/// Collapsing the two was the last real defect here — treating "not recorded" as doubt would
/// make almost every hit unknown, because the catalog is sparse. Measured on the live library:
/// `tvdb_id` is null on 228 of 300 owned rows (76%), so a TVDB hit against a row that only
/// records tmdb/imdb is the NORMAL case, not a warning sign.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum IdentifierCheck {
    /// The row carries this exact identifier.
    Corroborates,
    /// The row carries a DIFFERENT value for this provider — the hit and the row disagree.
    Contradicts,
    /// The row does not record this provider's id at all. No information either way.
    Silent,
}

/// Why a hit resolved the way it did. Carried on the wire so a caller never has to infer the
/// difference between "we checked and it is not there" and "we could not check".
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Resolution {
    /// Exactly one catalog row, nothing contradicting it. `in_library` is definite.
    Settled,
    /// Indexed identifiers were checked and matched no catalog row. `in_library` is a
    /// definite false.
    Absent,
    /// The hit carried NO identifier this endpoint can look up, so nothing was checked.
    NoIndexedIdentifier,
    /// Several distinct catalog rows were reachable.
    AmbiguousRows,
    /// One row, but an identifier it records disagrees with the hit.
    Contradicted,
}

impl Resolution {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Resolution::Settled => "settled",
            Resolution::Absent => "absent",
            Resolution::NoIndexedIdentifier => "no_indexed_identifier",
            Resolution::AmbiguousRows => "ambiguous_rows",
            Resolution::Contradicted => "contradicted",
        }
    }
}

/// Decide what one hit supports, given the distinct catalog rows its identifiers reach and
/// how the single candidate row (when there is one) answers each identifier.
///
/// `indexed_identifiers` is how many of the hit's ids this endpoint can actually look up
/// (tmdb/tvdb/imdb — the indexed columns). It matters for the zero-candidate case: finding no
/// row is only a NEGATIVE ANSWER if something was actually asked. See below.
///
/// Conservative in exactly one direction: it settles only when a single row is pinned and
/// nothing contradicts it. Everything else is unknown, never a guess.
pub(crate) fn resolve_hit(
    candidate_rows: usize,
    indexed_identifiers: usize,
    checks: &[IdentifierCheck],
    owned: Option<bool>,
) -> HitResolution {
    if candidate_rows == 0 {
        // "No indexed candidate found" is NOT "never catalogued" (codex, gpt56). Candidate
        // collection only consults tmdb/tvdb/imdb; ids living in the `provider_ids` jsonb
        // (tvrage, tvmaze, anilist) are not queried, so a hit carrying only those was never
        // actually looked up and nothing can be concluded from the silence.
        if indexed_identifiers == 0 {
            return HitResolution {
                in_library: None,
                in_catalog: None,
                metadata_id: None,
                ambiguous: true,
                resolution: Resolution::NoIndexedIdentifier,
            };
        }
        // Something WAS asked and answered nothing: a title with no catalog row cannot have a
        // media_items row. This is the one negative that is fully supported.
        return HitResolution {
            in_library: Some(false),
            in_catalog: Some(false),
            metadata_id: None,
            ambiguous: false,
            resolution: Resolution::Absent,
        };
    }

    // Several distinct rows reachable: Muse knows a title here but cannot say WHICH row this
    // hit is, so it can answer neither ownership nor identity.
    if candidate_rows > 1 {
        return HitResolution {
            in_library: None,
            // Also unknown, not true. A row matched one of the hit's identifiers on a unique
            // key, so "some catalog row carries this id" IS a fact — but `in_catalog` claims
            // something narrower, that MUSE HAS THIS TITLE, and when identifiers point at
            // different rows that is exactly what is not established (codex). The weaker fact
            // is not lost: `resolution` reports ambiguous_rows.
            in_catalog: None,
            metadata_id: None,
            ambiguous: true,
            resolution: Resolution::AmbiguousRows,
        };
    }

    // Exactly one candidate row — but an identifier that disagrees with it means the hit and
    // the row are probably not the same title.
    if checks.iter().any(|c| *c == IdentifierCheck::Contradicts) {
        return HitResolution {
            in_library: None,
            // Same reasoning as the ambiguous case above: an identifier disagreeing with the
            // row is evidence the hit and the row are different titles, so this title's
            // presence in the catalog is unestablished. `resolution` says contradicted.
            in_catalog: None,
            metadata_id: None,
            ambiguous: true,
            resolution: Resolution::Contradicted,
        };
    }

    HitResolution {
        in_library: owned,
        in_catalog: Some(true),
        metadata_id: None, // filled by the caller, which holds the row id
        ambiguous: false,
        resolution: Resolution::Settled,
    }
}

/// Build a catalog index for every identifier appearing in these hits.
///
/// One query for the whole result set rather than per-hit: a search returns tens of titles and
/// a per-hit lookup would be tens of round trips on an interactive path.
async fn build_catalog_index(
    state: &AppState,
    hits: &[(&'static str, MediaKind, ProviderMetadata)],
) -> MuseResult<CatalogIndex> {
    let mut tmdb_ids: HashSet<String> = HashSet::new();
    let mut tvdb_ids: HashSet<String> = HashSet::new();
    let mut imdb_ids: HashSet<String> = HashSet::new();

    for (_, _, meta) in hits {
        for (provider, id) in id_pairs(meta) {
            match provider.as_str() {
                "tmdb" => tmdb_ids.insert(id),
                "tvdb" => tvdb_ids.insert(id),
                "imdb" => imdb_ids.insert(id),
                // Other providers (tvrage, tvmaze, anilist...) live in the `provider_ids`
                // jsonb rather than an indexed column. They are not queried here, so they can
                // neither corroborate nor contradict -- see `identifier_checks`.
                _ => false,
            };
        }
    }

    let mut index = CatalogIndex {
        by_key: HashMap::new(),
        rows: HashMap::new(),
    };
    if tmdb_ids.is_empty() && tvdb_ids.is_empty() && imdb_ids.is_empty() {
        return Ok(index);
    }

    let tmdb: Vec<String> = tmdb_ids.into_iter().collect();
    let tvdb: Vec<String> = tvdb_ids.into_iter().collect();
    let imdb: Vec<String> = imdb_ids.into_iter().collect();

    // All three id columns are `text` (migration 0005), so no casts are needed.
    let rows: Vec<(i64, String, Option<String>, Option<String>, Option<String>, bool)> =
        sqlx::query_as(
            r#"
            SELECT
                mm.id,
                mm.kind::text,
                mm.tmdb_id,
                mm.tvdb_id,
                mm.imdb_id,
                EXISTS (SELECT 1 FROM media_items mi WHERE mi.media_metadata_id = mm.id) AS owned
            FROM media_metadata mm
            WHERE mm.tmdb_id = ANY($1) OR mm.tvdb_id = ANY($2) OR mm.imdb_id = ANY($3)
            "#,
        )
        .bind(&tmdb)
        .bind(&tvdb)
        .bind(&imdb)
        .fetch_all(&state.pool)
        .await?;

    for (id, kind, tmdb_id, tvdb_id, imdb_id, owned) in rows {
        let kind_key: &'static str = match kind.as_str() {
            "movie" => "movie",
            "show" => "show",
            other => {
                tracing::warn!(kind = other, "MUSE #108: unknown media_kind; skipping row");
                continue;
            }
        };
        for (provider, value) in [
            ("tmdb", tmdb_id.clone()),
            ("tvdb", tvdb_id.clone()),
            ("imdb", imdb_id.clone()),
        ] {
            if let Some(v) = value {
                index
                    .by_key
                    .entry((provider.to_string(), v, kind_key))
                    .or_default()
                    .push(id);
            }
        }
        index.rows.insert(
            id,
            RowFacts {
                tmdb: tmdb_id,
                tvdb: tvdb_id,
                imdb: imdb_id,
                owned,
            },
        );
    }
    Ok(index)
}

/// How each of a hit's identifiers answers against one candidate row.
///
/// Every identifier the hit carries is examined. An earlier version silently DROPPED any that
/// did not resolve to a database key, which made the disagreement cases unreachable through
/// the endpoint and let one matching id assert definite ownership (codex, gpt56).
pub(crate) fn identifier_checks(
    hit_ids: &[(String, String)],
    row_tmdb: Option<&str>,
    row_tvdb: Option<&str>,
    row_imdb: Option<&str>,
) -> Vec<IdentifierCheck> {
    hit_ids
        .iter()
        .filter_map(|(provider, id)| {
            let stored = match provider.as_str() {
                "tmdb" => row_tmdb,
                "tvdb" => row_tvdb,
                "imdb" => row_imdb,
                // Not an indexed column -- unknowable here, so omitted rather than counted as
                // either agreement or disagreement.
                _ => return None,
            };
            Some(match stored {
                None => IdentifierCheck::Silent,
                Some(v) if v == id => IdentifierCheck::Corroborates,
                Some(_) => IdentifierCheck::Contradicts,
            })
        })
        .collect()
}

/// Which requested kinds no provider searched SUCCESSFULLY.
///
/// Coverage is a property of the KIND, across all providers — not of a provider. The first
/// implementation derived it from a provider-wide status, so a provider that searched movies
/// fine and failed on series marked BOTH uncovered (codex, gpt56).
///
/// `outcomes` is one entry per (provider, kind) attempt. A kind with NO entry at all is
/// uncovered: silence is not coverage — it means no configured provider could search it.
pub(crate) fn uncovered_kinds(
    outcomes: &[(&'static str, bool)],
    requested: &[&'static str],
) -> Vec<&'static str> {
    requested
        .iter()
        .copied()
        .filter(|k| !outcomes.iter().any(|(kind, ok)| kind == k && *ok))
        .collect()
}

pub async fn get_search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchQuery>,
) -> MuseResult<Json<Value>> {
    let query = params.q.unwrap_or_default().trim().to_string();
    if query.is_empty() {
        // A 400, deliberately, rather than `{results: []}`. An empty result set would say
        // "nothing matches", which is a claim about the catalogue; no term was supplied, so
        // nothing was searched and nothing can be claimed.
        return Err(MuseError::BadRequest(
            "q must not be empty — supply a search term".to_string(),
        ));
    }
    let kinds = requested_kinds(params.kind.as_deref())?;

    let entries = providers(&state);
    let mut provider_reports: Vec<Value> = Vec::new();
    let mut hits: Vec<(&'static str, MediaKind, ProviderMetadata)> = Vec::new();
    // Per (kind) success across ALL providers. Coverage is a property of the KIND, not of a
    // provider: a provider that fails on series while succeeding on movies must not mark
    // movies uncovered (codex, gpt56).
    let mut outcomes: Vec<(&'static str, bool)> = Vec::new();

    for entry in &entries {
        // Only the kinds the caller asked for AND this provider can serve in its mode.
        let to_search: Vec<MediaKind> = kinds
            .iter()
            .copied()
            .filter(|k| entry.searchable.contains(k))
            .collect();

        // One report per kind, because one status per provider cannot express "movies fine,
        // series broken" — and that is the exact state a half-configured deployment is in.
        let mut kind_reports: Vec<Value> = Vec::new();

        for kind in &to_search {
            let Some(client) = entry.client.as_ref() else {
                // Unreachable in practice: an unconfigured provider has no searchable kinds,
                // so `to_search` is empty. Handled rather than unwrapped so the invariant
                // cannot become a panic if `searchable` is ever set independently.
                break;
            };
            match client.search(&query, *kind).await {
                Ok(found) => {
                    let returned = found.len();
                    // Detect truncation by what the provider ACTUALLY returned, before
                    // discarding anything — `.take()` alone leaves the caller unable to tell
                    // "exactly 40 results" from "40 of many" (gpt56).
                    let truncated = returned > MAX_PER_PROVIDER;
                    if truncated {
                        tracing::info!(
                            provider = entry.name, kind = kind_str(*kind), returned,
                            limit = MAX_PER_PROVIDER,
                            "MUSE #108: provider results truncated; reported to the caller"
                        );
                    }
                    for meta in found.into_iter().take(MAX_PER_PROVIDER) {
                        hits.push((entry.name, *kind, meta));
                    }
                    outcomes.push((kind_str(*kind), true));
                    kind_reports.push(json!({
                        "kind": kind_str(*kind),
                        "status": "ok",
                        "error": Value::Null,
                        "result_count": returned.min(MAX_PER_PROVIDER),
                        "truncated": truncated,
                        "provider_returned": returned,
                        "limit": MAX_PER_PROVIDER,
                    }));
                }
                Err(e) => {
                    // Recorded against this provider AND kind, and reported; never folded
                    // into the result list as an absence. See the module doc.
                    tracing::warn!(provider = entry.name, kind = kind_str(*kind), error = %e,
                        "MUSE #108: provider search failed; reporting as error, not as empty");
                    outcomes.push((kind_str(*kind), false));
                    kind_reports.push(json!({
                        "kind": kind_str(*kind),
                        "status": "error",
                        "error": e.to_string(),
                        "result_count": 0,
                        "truncated": false,
                        "provider_returned": Value::Null,
                        "limit": MAX_PER_PROVIDER,
                    }));
                }
            }
        }

        // A requested kind this provider CANNOT search in its current mode gets an explicit
        // entry rather than being omitted. Silence in the `kinds` array read as "not asked
        // about", which is indistinguishable from "asked and returned nothing" unless the
        // caller cross-references searchable_kinds — and a per-kind array that quietly omits
        // kinds is exactly the sort of gap that makes a partial answer look whole (gpt56).
        for kind in &kinds {
            if !entry.searchable.contains(kind) {
                kind_reports.push(json!({
                    "kind": kind_str(*kind),
                    "status": "not_consulted",
                    "error": Value::Null,
                    "reason": format!(
                        "{} in {} mode cannot search {}",
                        entry.name, entry.mode, kind_str(*kind)
                    ),
                    "result_count": 0,
                    "truncated": false,
                    "provider_returned": Value::Null,
                    "limit": MAX_PER_PROVIDER,
                }));
            }
        }

        // Rolled up over the kinds actually ATTEMPTED — a not_consulted kind is neither a
        // success nor a failure, and counting it as either would misreport the provider.
        let attempted: Vec<&Value> = kind_reports
            .iter()
            .filter(|r| r.get("status").and_then(Value::as_str) != Some("not_consulted"))
            .collect();
        let all_error = !attempted.is_empty()
            && attempted
                .iter()
                .all(|r| r.get("status").and_then(Value::as_str) == Some("error"));
        let any_error = attempted
            .iter()
            .any(|r| r.get("status").and_then(Value::as_str) == Some("error"));
        let total: u64 = kind_reports
            .iter()
            .filter_map(|r| r.get("result_count").and_then(Value::as_u64))
            .sum();

        provider_reports.push(json!({
            "name": entry.name,
            "mode": entry.mode,
            "configured": entry.configured,
            "searchable_kinds": entry.searchable.iter().copied().map(kind_str).collect::<Vec<_>>(),
            // What this provider was actually asked for on THIS request — the intersection of
            // the caller's kinds and its own coverage. Empty means it was not consulted, which
            // is why it contributed nothing; that is not the same as finding nothing.
            "searched_kinds": to_search.iter().copied().map(kind_str).collect::<Vec<_>>(),
            // Rolled up from the per-kind results, and PARTIAL is its own state: a provider
            // that answered for one kind and failed for another is neither ok nor error, and
            // flattening it to either would misdescribe half of what happened.
            "status": if attempted.is_empty() {
                "not_consulted"
            } else if all_error {
                "error"
            } else if any_error {
                "partial"
            } else {
                "ok"
            },
            "kinds": kind_reports,
            "result_count": total,
        }));
    }

    // Which requested kinds NO provider searched successfully. The caller needs this to know
    // whether a short list is the honest answer or a hole: on a key-less deployment losing
    // one provider removes an entire media kind from the results.
    let requested: Vec<&'static str> = kinds.iter().copied().map(kind_str).collect();
    let uncovered = uncovered_kinds(&outcomes, &requested);

    let catalog = build_catalog_index(&state, &hits).await?;

    let results: Vec<Value> = hits
        .iter()
        .map(|(provider, kind, meta)| {
            // EVERY identifier is consulted -- see identifier_checks and resolve_hit.
            let hit_ids = id_pairs(meta);
            let mut candidates: Vec<i64> = hit_ids
                .iter()
                .filter_map(|(provider, id)| {
                    catalog
                        .by_key
                        .get(&(provider.clone(), id.clone(), db_kind(*kind)))
                })
                .flatten()
                .copied()
                .collect();
            candidates.sort_unstable();
            candidates.dedup();

            let single = if candidates.len() == 1 {
                catalog.rows.get(&candidates[0]).map(|r| (candidates[0], r))
            } else {
                None
            };
            let checks = single
                .map(|(_, r)| {
                    identifier_checks(&hit_ids, r.tmdb.as_deref(), r.tvdb.as_deref(), r.imdb.as_deref())
                })
                .unwrap_or_default();
            // How many of this hit's ids this endpoint can actually look up. Zero means
            // nothing was checked, which is not the same as nothing being found.
            let indexed = hit_ids
                .iter()
                .filter(|(p, _)| matches!(p.as_str(), "tmdb" | "tvdb" | "imdb"))
                .count();
            let mut known = resolve_hit(
                candidates.len(),
                indexed,
                &checks,
                single.map(|(_, r)| r.owned),
            );
            // The row id is only asserted when resolve_hit actually settled on it.
            if !known.ambiguous && known.in_catalog == Some(true) {
                known.metadata_id = single.map(|(id, _)| id);
            }

            json!({
                "provider": provider,
                "kind": kind_str(*kind),
                "title": meta.title,
                "year": meta.year,
                "overview": meta.overview,
                "first_aired": meta.first_aired,
                "rating": meta.rating,
                "provider_ids": meta.provider_ids,
                // Provider-hosted art. Muse's own `/art/...` proxy only serves titles that
                // are already in the library, so a search hit that is NOT owned has no Muse
                // art url — the remote one is passed through as-is and may be null.
                "poster_url": meta.images.poster_url.clone(),
                // TRI-STATE. true/false when the hit resolves to exactly one catalog row;
                // NULL when it does not, which means UNKNOWN and must not be rendered as
                // "not in library". Ownership is a media_items row — the same join the
                // library query uses — because a media_metadata row alone means only that
                // Muse has SEEN the title (every trending title on /api/discover has one).
                "in_library": known.in_library,
                "in_catalog": known.in_catalog,
                // Why the two flags above say what they say — so a caller never has to infer
                // the difference between "checked and absent" and "could not check".
                "resolution": known.resolution.as_str(),
                // Null when the hit's identifiers do not agree on a single unambiguous row.
                // Naming one anyway would be an attribution nothing supports.
                "media_metadata_id": known.metadata_id,
                "ambiguous_match": known.ambiguous,
            })
        })
        .collect();

    Ok(Json(json!({
        "query": query,
        "requested_kinds": kinds.iter().copied().map(kind_str).collect::<Vec<_>>(),
        "providers": provider_reports,
        // True when every requested kind was searched by at least one healthy provider.
        "complete": uncovered.is_empty(),
        "uncovered_kinds": uncovered,
        "results": results,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parsing_accepts_every_spelling_and_rejects_the_rest() {
        assert_eq!(requested_kinds(None).unwrap().len(), 2);
        assert_eq!(requested_kinds(Some("all")).unwrap().len(), 2);
        assert_eq!(requested_kinds(Some("  ")).unwrap().len(), 2);
        assert_eq!(requested_kinds(Some("Movie")).unwrap(), vec![MediaKind::Movie]);
        for spelling in ["series", "show", "shows", "tv", "TV"] {
            assert_eq!(
                requested_kinds(Some(spelling)).unwrap(),
                vec![MediaKind::Series],
                "{spelling} should select series",
            );
        }
        // An unknown kind is a 400, NOT a silent fallback to "all". Quietly widening the
        // search would answer a question the caller did not ask and label it as theirs.
        assert!(requested_kinds(Some("anime")).is_err());
    }

    #[test]
    fn db_kind_uses_the_db_vocabulary_not_the_wire_one() {
        // media_kind is ('movie','show'); the wire says ('movie','series'). Conflating them
        // would silently fail every series in_library match.
        assert_eq!(db_kind(MediaKind::Series), "show");
        assert_eq!(kind_str(MediaKind::Series), "series");
        assert_eq!(db_kind(MediaKind::Movie), "movie");
    }

    /// Coverage is a property of the KIND, across all providers — not of a provider.
    ///
    /// The original implementation derived it from a provider-wide status, so a provider that
    /// searched movies successfully and failed on series marked BOTH uncovered. On a key-less
    /// deployment that is the difference between "your movie results are complete" and a
    /// spurious warning that movie coverage is missing.
    /// The PRODUCTION function, not a reimplementation of it. An earlier version of these
    /// tests copied the logic into the test module, which would have passed happily while the
    /// handler did something else entirely — the same class of false-green as a mock that
    /// disagrees with its endpoint.
    use super::uncovered_kinds as coverage;

    #[test]
    fn one_kind_failing_does_not_mark_the_other_uncovered() {
        // tmdb ok on movie, tvdb error on series
        assert_eq!(
            coverage(&[("movie", true), ("series", false)], &["movie", "series"]),
            vec!["series"],
        );
    }

    #[test]
    fn a_kind_is_covered_when_any_provider_succeeds_at_it() {
        // Two providers both asked for series; one fails, one succeeds. Covered.
        assert_eq!(
            coverage(&[("series", false), ("series", true)], &["series"]),
            Vec::<&str>::new(),
        );
    }

    #[test]
    fn a_kind_no_provider_could_search_is_uncovered() {
        // Nothing reported an outcome for `series` at all — e.g. every configured provider is
        // movie-only in its current mode. Silence is NOT coverage.
        assert_eq!(
            coverage(&[("movie", true)], &["movie", "series"]),
            vec!["series"],
        );
    }








    #[test]
    fn a_hit_reaching_no_catalog_row_is_definitively_not_in_the_library() {
        // The one negative that IS fully supported: never catalogued => no media_items row.
        let r = resolve_hit(0, 1, &[], None);
        assert_eq!(r.in_library, Some(false));
        assert_eq!(r.in_catalog, Some(false));
        assert!(!r.ambiguous);
    }

    #[test]
    fn one_row_with_nothing_contradicting_settles_ownership_either_way() {
        let owned = resolve_hit(1, 1, &[IdentifierCheck::Corroborates], Some(true));
        assert_eq!(owned.in_library, Some(true));
        assert!(!owned.ambiguous);

        let not_owned = resolve_hit(1, 1, &[IdentifierCheck::Corroborates], Some(false));
        assert_eq!(not_owned.in_library, Some(false));
        assert_eq!(not_owned.in_catalog, Some(true), "known to Muse, just not held");
    }

    #[test]
    fn an_identifier_the_row_does_not_record_is_not_doubt() {
        // THE case that makes this feature usable rather than perpetually "unknown".
        // Measured live: tvdb_id is null on 228 of 300 owned rows, so a hit carrying a tvdb id
        // against a row that records only tmdb/imdb is the NORMAL case. Silence is not
        // disagreement.
        let r = resolve_hit(
            1,
            2,
            &[IdentifierCheck::Corroborates, IdentifierCheck::Silent],
            Some(true),
        );
        assert_eq!(r.in_library, Some(true));
        assert!(!r.ambiguous);
    }

    #[test]
    fn an_identifier_the_row_records_differently_is_disagreement() {
        // The row says imdb=tt111 and the hit says imdb=tt222: probably not the same title,
        // so no ownership claim is made in EITHER direction.
        let r = resolve_hit(
            1,
            2,
            &[IdentifierCheck::Corroborates, IdentifierCheck::Contradicts],
            Some(true),
        );
        assert_eq!(r.in_library, None, "a contradiction cannot settle ownership");
        assert_eq!(r.metadata_id, None);
        assert!(r.ambiguous);
        assert_eq!(r.in_catalog, None, "identity unestablished, so catalog presence is too");
    }

    #[test]
    fn several_candidate_rows_make_ownership_unknown() {
        // Reachable because imdb_id has no uniqueness constraint.
        let r = resolve_hit(2, 1, &[], None);
        assert_eq!(r.in_library, None);
        assert_eq!(r.metadata_id, None);
        assert!(r.ambiguous);
        assert_eq!(r.in_catalog, None);
    }

    #[test]
    fn a_hit_with_no_lookupable_identifier_is_unknown_not_absent() {
        // Candidate collection only consults tmdb/tvdb/imdb. A hit carrying only ids from the
        // provider_ids jsonb (tvrage, tvmaze, anilist) was never actually looked up, so zero
        // candidates means nothing was ASKED — not that the title is absent (codex, gpt56).
        let r = resolve_hit(0, 0, &[], None);
        assert_eq!(r.in_library, None, "nothing was checked, so nothing is known");
        assert_eq!(r.in_catalog, None);
        assert_eq!(r.resolution, Resolution::NoIndexedIdentifier);
        assert!(r.ambiguous);

        // With at least one indexed id checked, zero rows IS a real negative.
        let asked = resolve_hit(0, 1, &[], None);
        assert_eq!(asked.in_library, Some(false));
        assert_eq!(asked.resolution, Resolution::Absent);
    }

    #[test]
    fn resolution_reports_why_each_state_was_reached() {
        assert_eq!(resolve_hit(1, 1, &[IdentifierCheck::Silent], Some(true)).resolution, Resolution::Settled);
        assert_eq!(resolve_hit(2, 1, &[], None).resolution, Resolution::AmbiguousRows);
        assert_eq!(
            resolve_hit(1, 1, &[IdentifierCheck::Contradicts], Some(true)).resolution,
            Resolution::Contradicted,
        );
    }

    #[test]
    fn identifier_checks_classifies_all_three_answers_and_skips_unindexed_providers() {
        let ids = vec![
            ("tmdb".to_string(), "286217".to_string()),
            ("imdb".to_string(), "tt999".to_string()),
            ("tvdb".to_string(), "121361".to_string()),
            // Lives in the provider_ids jsonb, not an indexed column: unknowable here, so it
            // must be OMITTED rather than counted as agreement or disagreement.
            ("tvrage".to_string(), "42".to_string()),
        ];
        let checks = identifier_checks(&ids, Some("286217"), None, Some("tt111"));
        assert_eq!(
            checks,
            vec![
                IdentifierCheck::Corroborates, // tmdb matches
                IdentifierCheck::Contradicts,  // imdb differs
                IdentifierCheck::Silent,       // row records no tvdb id
            ],
            "the unindexed provider must not appear at all",
        );
    }

    /// These hit the REAL upstreams. Ignored by default so the suite stays hermetic and
    /// offline-safe; run with `--ignored` to check the provider contracts still hold.
    ///
    /// They exist because MUSE #106 was a wrong URL that every mocked test happily agreed
    /// with — the mocks encoded the same mistake as the code. A periodic check against the
    /// live service is the only thing that catches that class of error.
    mod live {
        use crate::metadata::{MediaKind, MetadataProvider};
        use crate::metadata::tvdb::TvdbClient;
        use crate::trending::TmdbClient;

        #[tokio::test]
        #[ignore = "hits api.radarr.video"]
        async fn keyless_tmdb_searches_movies() {
            let c = TmdbClient::new_proxy(crate::trending::client::DEFAULT_RADARR_PROXY_URL)
                .expect("proxy client");
            let hits = c.search("the martian", MediaKind::Movie).await.expect("search");
            assert!(!hits.is_empty(), "keyless movie search should return hits");
        }

        #[tokio::test]
        #[ignore = "hits skyhook.sonarr.tv"]
        async fn keyless_tvdb_searches_series() {
            // The MUSE #106 regression check, against the live service rather than a mock.
            let c = TvdbClient::new_skyhook(crate::metadata::tvdb::DEFAULT_SKYHOOK_URL)
                .expect("skyhook client");
            let hits = c.search("thrones", MediaKind::Series).await.expect("search");
            assert!(!hits.is_empty(), "keyless series search should return hits");
        }

        #[tokio::test]
        #[ignore = "hits skyhook.sonarr.tv"]
        async fn keyless_tvdb_has_no_movie_coverage() {
            // Pins the asymmetry the endpoint reports: Skyhook is series-only, so a movie
            // search there is empty by DESIGN, not by failure.
            let c = TvdbClient::new_skyhook(crate::metadata::tvdb::DEFAULT_SKYHOOK_URL)
                .expect("skyhook client");
            let hits = c.search("the martian", MediaKind::Movie).await.expect("search");
            assert!(hits.is_empty(), "skyhook serves series only");
        }
    }
}
