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
    /// The kinds this provider can search IN ITS CURRENT MODE. Key-less modes are
    /// single-kind; see the module doc.
    searchable: Vec<MediaKind>,
    client: Box<dyn MetadataProvider>,
}

/// Build the provider set from live state, exactly as the rest of Muse constructs it.
///
/// TMDb is taken from `AppState` (constructed once at boot). TVDB is built here from config,
/// which is the same thing `web::dashboard` does — it is not held on `AppState` today, and
/// putting it there is a wider change than this endpoint needs.
fn providers(state: &AppState) -> Vec<ProviderEntry> {
    let mut out: Vec<ProviderEntry> = Vec::new();

    if let Some(tmdb) = state.tmdb.clone() {
        let proxy = tmdb.is_proxy_mode();
        out.push(ProviderEntry {
            name: "tmdb",
            mode: if proxy { "radarr_proxy" } else { "api" },
            // AMETA-1/2: the key-less Radarr proxy serves movie lookup/search only. Claiming
            // series coverage here would make a missing half look like an empty half.
            searchable: if proxy {
                vec![MediaKind::Movie]
            } else {
                vec![MediaKind::Movie, MediaKind::Series]
            },
            client: Box::new(tmdb),
        });
    }

    if let Some(tvdb) = crate::metadata::tvdb::TvdbClient::from_config(&state.config) {
        let skyhook = tvdb.is_skyhook_mode();
        out.push(ProviderEntry {
            name: "tvdb",
            mode: if skyhook { "skyhook" } else { "api" },
            // AMETA-3: Skyhook is Sonarr's series proxy — no movies.
            searchable: if skyhook {
                vec![MediaKind::Series]
            } else {
                vec![MediaKind::Movie, MediaKind::Series]
            },
            client: Box::new(tvdb),
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

/// What Muse knows about a title it recognized.
#[derive(Debug, Clone, Copy)]
struct LibraryHit {
    /// `None` when MORE THAN ONE metadata row matched this key, so no single row can be
    /// named. See `resolve_in_library` on why that is reachable for IMDb.
    metadata_id: Option<i64>,
    /// True when more than one metadata row matched — the caller is told rather than handed
    /// an arbitrary winner.
    ambiguous: bool,
    /// A `media_metadata` row alone does NOT mean the title is held: `/api/discover` writes
    /// metadata rows for TRENDING titles precisely because they are *not* in the library
    /// (`list_trending_not_in_library`). Ownership is a `media_items` row — the same join
    /// `repo::dashboard`'s library query uses. Treating a catalog row as ownership would have
    /// marked every discoverable title as already owned (codex).
    owned: bool,
}

/// Collapse the metadata rows matching one `(provider, id, kind)` key into a single answer.
///
/// `owned` is ANY: if any row carrying this id is held, then a title with this id IS in the
/// library, which is exactly what the flag claims. The METADATA ID is the part that cannot be
/// determined under ambiguity, so that is what goes `None` — rather than naming a row we
/// cannot justify picking (codex).
pub(crate) fn resolve_match(rows: &[(i64, bool)]) -> (Option<i64>, bool, bool) {
    let ambiguous = rows.len() > 1;
    let owned = rows.iter().any(|(_, o)| *o);
    let metadata_id = if ambiguous { None } else { rows.first().map(|(id, _)| *id) };
    (metadata_id, ambiguous, owned)
}

/// Which of these titles Muse already knows, keyed by `(provider, id, db_kind)`.
///
/// The KIND is part of the key deliberately. `media_metadata` is `UNIQUE (kind, tmdb_id)` and
/// `UNIQUE (kind, tvdb_id)`, so the same provider id can legitimately exist as both a movie and
/// a show. Keying on `(provider, id)` alone let one row overwrite the other in the map and
/// could report a movie as owned on the strength of a series row, or vice versa (codex).
///
/// One query for the whole result set rather than per-hit: a search returns tens of titles and
/// a per-hit lookup would be tens of round trips on an interactive path.
async fn resolve_in_library(
    state: &AppState,
    hits: &[(&'static str, MediaKind, ProviderMetadata)],
) -> MuseResult<HashMap<(String, String, &'static str), LibraryHit>> {
    let mut tmdb_ids: HashSet<String> = HashSet::new();
    let mut tvdb_ids: HashSet<String> = HashSet::new();
    let mut imdb_ids: HashSet<String> = HashSet::new();

    for (_, _, meta) in hits {
        for (provider, id) in id_pairs(meta) {
            match provider.as_str() {
                "tmdb" => tmdb_ids.insert(id),
                "tvdb" => tvdb_ids.insert(id),
                "imdb" => imdb_ids.insert(id),
                _ => false,
            };
        }
    }

    let mut found: HashMap<(String, String, &'static str), LibraryHit> = HashMap::new();
    if tmdb_ids.is_empty() && tvdb_ids.is_empty() && imdb_ids.is_empty() {
        return Ok(found);
    }

    let tmdb: Vec<String> = tmdb_ids.into_iter().collect();
    let tvdb: Vec<String> = tvdb_ids.into_iter().collect();
    let imdb: Vec<String> = imdb_ids.into_iter().collect();

    // `tmdb_id`/`tvdb_id`/`imdb_id` are all `text` columns (migration 0005), so no casts are
    // needed — an earlier version cast them anyway, which was noise that could have hidden a
    // genuine type mismatch behind a silent coercion.
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

    // Accumulated, not `insert`-ed, because a key can legitimately match several rows.
    // `media_metadata` is UNIQUE(kind, tmdb_id) and UNIQUE(kind, tvdb_id), but `imdb_id` has
    // only a plain INDEX — no uniqueness at all (migration 0005). So two same-kind rows may
    // share an IMDb id, and a bare `insert` would silently keep whichever the database
    // happened to return last, attributing `owned` and `metadata_id` to an arbitrary one of
    // them (codex).
    let mut matches: HashMap<(String, String, &'static str), Vec<(i64, bool)>> = HashMap::new();
    for (id, kind, tmdb_id, tvdb_id, imdb_id, owned) in rows {
        // Leaked once per distinct kind string the DB can produce (two), not per row — the
        // enum has exactly the two variants below and anything else is skipped rather than
        // silently bucketed into one of them.
        let kind_key: &'static str = match kind.as_str() {
            "movie" => "movie",
            "show" => "show",
            other => {
                tracing::warn!(kind = other, "MUSE #108: unknown media_kind; skipping row");
                continue;
            }
        };
        for (provider, value) in [("tmdb", tmdb_id), ("tvdb", tvdb_id), ("imdb", imdb_id)] {
            if let Some(v) = value {
                matches
                    .entry((provider.to_string(), v, kind_key))
                    .or_default()
                    .push((id, owned));
            }
        }
    }

    for (key, rows) in matches {
        let (metadata_id, ambiguous, owned) = resolve_match(&rows);
        if ambiguous {
            tracing::warn!(
                provider = key.0.as_str(), id = key.1.as_str(), kind = key.2, count = rows.len(),
                "MUSE #108: several metadata rows share this provider id; reporting ambiguous"
            );
        }
        found.insert(key, LibraryHit { metadata_id, ambiguous, owned });
    }
    Ok(found)
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
            match entry.client.search(&query, *kind).await {
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
            "configured": true,
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

    let in_library = resolve_in_library(&state, &hits).await?;

    let results: Vec<Value> = hits
        .iter()
        .map(|(provider, kind, meta)| {
            // Matched on (provider, id, KIND) — see resolve_in_library on why the kind
            // belongs in the key.
            let known = id_pairs(meta)
                .into_iter()
                .find_map(|(provider, id)| in_library.get(&(provider, id, db_kind(*kind))).copied());
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
                // `in_library` means Muse HOLDS this title (a media_items row), which is
                // what the library query itself requires. A metadata row alone means only
                // that Muse has seen the title — every trending title on /api/discover has
                // one — so the two are reported separately rather than conflated.
                "in_library": known.map(|k| k.owned).unwrap_or(false),
                "in_catalog": known.is_some(),
                // Null when several catalog rows share this id (possible for IMDb, which has
                // no uniqueness constraint) — the title is known, but no single row can be
                // named, and naming an arbitrary one would be a fabricated attribution.
                "media_metadata_id": known.and_then(|k| k.metadata_id),
                "ambiguous_match": known.map(|k| k.ambiguous).unwrap_or(false),
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
    fn an_ambiguous_id_names_no_row_but_still_reports_ownership() {
        // One row, unambiguous: name it.
        assert_eq!(resolve_match(&[(7, true)]), (Some(7), false, true));
        assert_eq!(resolve_match(&[(7, false)]), (Some(7), false, false));

        // Two rows sharing an id (reachable for IMDb — no uniqueness constraint on that
        // column). We still know a title with this id is held, but not WHICH catalog row it
        // is, so the id goes null rather than picking an arbitrary winner.
        let (id, ambiguous, owned) = resolve_match(&[(7, false), (9, true)]);
        assert_eq!(id, None, "must not attribute to an arbitrary row");
        assert!(ambiguous);
        assert!(owned, "any owned row means a title with this id is held");

        // Ambiguous and none owned.
        let (id, ambiguous, owned) = resolve_match(&[(7, false), (9, false)]);
        assert_eq!(id, None);
        assert!(ambiguous);
        assert!(!owned);
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
