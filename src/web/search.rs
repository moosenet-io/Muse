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

/// Which of these titles Muse already holds, as the set of `(provider, id)` pairs that match
/// a row in `media_metadata`, mapped to that row's id.
///
/// One query for the whole result set rather than per-hit: a search returns tens of titles and
/// a per-hit lookup would be tens of round trips on an interactive path.
async fn resolve_in_library(
    state: &AppState,
    hits: &[(&'static str, MediaKind, ProviderMetadata)],
) -> MuseResult<HashMap<(String, String), i64>> {
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

    let mut found: HashMap<(String, String), i64> = HashMap::new();
    if tmdb_ids.is_empty() && tvdb_ids.is_empty() && imdb_ids.is_empty() {
        return Ok(found);
    }

    let tmdb: Vec<String> = tmdb_ids.into_iter().collect();
    let tvdb: Vec<String> = tvdb_ids.into_iter().collect();
    let imdb: Vec<String> = imdb_ids.into_iter().collect();

    // The three id columns are queried in one pass. `id::text` because the columns are typed
    // per provider while the provider hands us strings.
    let rows: Vec<(i64, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT id, tmdb_id::text, tvdb_id::text, imdb_id
        FROM media_metadata
        WHERE (tmdb_id::text = ANY($1)) OR (tvdb_id::text = ANY($2)) OR (imdb_id = ANY($3))
        "#,
    )
    .bind(&tmdb)
    .bind(&tvdb)
    .bind(&imdb)
    .fetch_all(&state.pool)
    .await?;

    for (id, tmdb_id, tvdb_id, imdb_id) in rows {
        if let Some(v) = tmdb_id {
            found.insert(("tmdb".to_string(), v), id);
        }
        if let Some(v) = tvdb_id {
            found.insert(("tvdb".to_string(), v), id);
        }
        if let Some(v) = imdb_id {
            found.insert(("imdb".to_string(), v), id);
        }
    }
    Ok(found)
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

    for entry in &entries {
        // Only the kinds the caller asked for AND this provider can serve in its mode.
        let to_search: Vec<MediaKind> = kinds
            .iter()
            .copied()
            .filter(|k| entry.searchable.contains(k))
            .collect();

        let mut errors: Vec<String> = Vec::new();
        let mut count = 0usize;

        for kind in &to_search {
            match entry.client.search(&query, *kind).await {
                Ok(found) => {
                    for meta in found.into_iter().take(MAX_PER_PROVIDER) {
                        count += 1;
                        hits.push((entry.name, *kind, meta));
                    }
                }
                Err(e) => {
                    // Recorded against this provider and reported; never folded into the
                    // result list as an absence. See the module doc.
                    tracing::warn!(provider = entry.name, kind = kind_str(*kind), error = %e,
                        "MUSE #108: provider search failed; reporting as error, not as empty");
                    errors.push(format!("{}: {e}", kind_str(*kind)));
                }
            }
        }

        provider_reports.push(json!({
            "name": entry.name,
            "mode": entry.mode,
            "configured": true,
            "searchable_kinds": entry.searchable.iter().copied().map(kind_str).collect::<Vec<_>>(),
            // What this provider was actually asked for on THIS request — the intersection of
            // the caller's kinds and its own coverage. Empty means it was not consulted, which
            // is why it contributed nothing; that is not the same as finding nothing.
            "searched_kinds": to_search.iter().copied().map(kind_str).collect::<Vec<_>>(),
            "status": if errors.is_empty() { "ok" } else { "error" },
            "error": if errors.is_empty() { Value::Null } else { json!(errors.join("; ")) },
            "result_count": count,
        }));
    }

    // Which requested kinds NO healthy provider covered. The caller needs this to know
    // whether a short list is the honest answer or a hole: on a key-less deployment losing
    // one provider removes an entire media kind from the results.
    let mut uncovered: Vec<&'static str> = Vec::new();
    for kind in &kinds {
        let covered = entries.iter().zip(provider_reports.iter()).any(|(e, r)| {
            e.searchable.contains(kind) && r.get("status").and_then(Value::as_str) == Some("ok")
        });
        if !covered {
            uncovered.push(kind_str(*kind));
        }
    }

    let in_library = resolve_in_library(&state, &hits).await?;

    let results: Vec<Value> = hits
        .iter()
        .map(|(provider, kind, meta)| {
            let library_id = id_pairs(meta)
                .into_iter()
                .find_map(|pair| in_library.get(&pair).copied());
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
                "in_library": library_id.is_some(),
                "media_metadata_id": library_id,
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
