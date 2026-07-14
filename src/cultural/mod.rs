//! MUSEX-07 (Plane TERM #383): the what's-hot / "the talk" cultural layer.
//!
//! ## What this adds
//! Pulls TRENDING (TMDb trending — reusing the existing MUSE-19
//! `crate::trending::TmdbClient` integration — plus, config-gated, Trakt
//! trending/scrobble-adjacent watcher counts) and "the TALK" (comment/
//! rating volume, real via Trakt when configured) through the
//! [`source::TrendSource`] seam ([`source::TmdbTrendSource`]/
//! [`source::TraktTrendSource`], both config-gated and inert unless their
//! API key env var is set — see [`crate::config::Config`]), cached behind
//! [`cache::TrendCache`] to respect API rate limits.
//!
//! That trending/talk data is then INTERSECTED with the account's actual
//! library ownership (`media_items`) AND taste signal (`taste_profile`,
//! `crate::persona::blend::cosine_similarity` against the same embeddings
//! `curation::candidates::gather_taste_candidates` already reuses) to
//! produce [`CulturalPick`]s: titles that are culturally live AND owned AND
//! taste-relevant. All three legs are HARD gates — a trending title you own
//! but whose taste relevance is below [`TASTE_RELEVANCE_MIN`] (or has no
//! computable taste signal at all) is DROPPED, not merely ranked lower, so
//! the "the talk" surface ([`CulturalPick::headline`]) is a genuine
//! three-way intersection, not "everything you own that's trending, sorted
//! by taste."
//!
//! ## Entry point + cold-start routing
//! [`recommend_cultural`] is the single entry point a channel/GUI calls; it
//! is where [`is_profile_sparse`] actually TRIGGERS the fallback. It loads
//! the account's `taste_profile` once and branches:
//! - **not sparse** → [`the_talk_surface`], the trending ∩ library ∩ taste
//!   intersection above;
//! - **sparse** (no profile / no centroid / thin genre affinity) →
//!   [`cold_start_recommendations`], since the intersection has no usable
//!   centroid and [`build_cultural_picks`]'s taste filter would drop
//!   everything. The fallback surfaces trend entries ranked by a
//!   TASTE-NEIGHBOR signal (a persona centroid, when the account has one —
//!   see `crate::persona`) where available, plain trend popularity
//!   otherwise — "what's hot that people like you love," degrading to
//!   "what's hot," but never nothing. This closes the "brand-new account
//!   has nothing to recommend" gap.
//! The routing is proven wired (not dead code) by
//! `live_tests::sparse_profile_routes_to_cold_start_while_rich_profile_uses_the_intersection`.
//!
//! ## No-PII-egress
//! Every DB-touching function in this module resolves account-scoped data
//! (library ownership, taste centroid, persona) LOCALLY and only ever
//! passes a plain [`source::TrendQuery`]/[`source::TalkQuery`] — region,
//! window, kind, or a set of public catalog ids — to the configured
//! [`source::TrendSource`]. See `source`'s module doc for the full
//! guarantee and its negative test.
//!
//! ## Pure core / DB-wrapper split
//! Both [`build_cultural_picks`] (the trending∩library∩taste intersection)
//! and [`select_cold_start_picks`] (the sparse-profile fallback) are pure
//! functions over already-resolved inputs — no DB, no `TrendSource` call —
//! unit-tested directly below. [`intersect_trending_library_taste`] and
//! [`cold_start_recommendations`] are the thin, `db_gated`-tested DB
//! wrappers that resolve those inputs and call the pure core, matching this
//! crate's existing split (e.g. `channels::serendipity`'s pure eligibility
//! rules vs `channels::director`'s DB-touching scheduler).

pub mod cache;
#[cfg(test)]
mod live_tests;
pub mod source;

use std::cmp::Ordering;
use std::collections::HashMap;

use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::embedding::{EmbeddingEntityKind, DEFAULT_EMBEDDING_MODEL};
use crate::models::media_metadata::MediaKind;
use crate::models::taste::TasteProfile;
use crate::persona::blend::cosine_similarity;
use crate::repo;

use cache::TrendCache;
use source::{TalkEntry, TalkQuery, TrendEntry, TrendQuery, TrendSource};

/// Below this many distinct scored genres in `taste_profile.genre_affinity`,
/// an account's taste signal is treated as SPARSE for cold-start purposes.
/// `crate::fixtures::cold_start_empty`'s profile has an empty `{}` affinity
/// map (0 genres) — this threshold (3) is set comfortably above that so a
/// freshly-onboarded account with only one or two watched titles (a couple
/// of genre entries, not yet a real taste signal) also gets the cold-start
/// treatment rather than a thin, likely-noisy taste-tier result.
pub const SPARSE_GENRE_AFFINITY_MIN: usize = 3;

/// `true` when `profile` doesn't carry enough signal for the normal
/// taste-tier path (`curation::candidates::gather_taste_candidates`) to be
/// meaningful — no profile at all, no computed centroid (that function's
/// own cold-start check), or a genre-affinity map below
/// [`SPARSE_GENRE_AFFINITY_MIN`] distinct genres.
pub fn is_profile_sparse(profile: Option<&TasteProfile>) -> bool {
    match profile {
        None => true,
        Some(p) => {
            if p.overall_centroid.is_none() {
                return true;
            }
            let genre_count = p.genre_affinity.as_object().map(|m| m.len()).unwrap_or(0);
            genre_count < SPARSE_GENRE_AFFINITY_MIN
        }
    }
}

/// Minimum cosine similarity (`crate::persona::blend::cosine_similarity`,
/// range `[-1.0, 1.0]`) between an owned trending title's embedding and the
/// account's taste centroid for that title to count as TASTE-RELEVANT — the
/// third leg of the trending ∩ library ∩ taste intersection. A title that
/// is trending and owned but scores below this (or has no computable taste
/// signal at all — no embedding yet, in the non-cold-start path) is DROPPED
/// from `the_talk_surface`, not merely ranked lower: the AC asks for
/// "culturally live that YOU OWN *and would like*," which is an
/// intersection, not "everything you own, sorted by taste."
///
/// `0.2` is a conservative, deliberately-inclusive floor (matching the
/// hedged posture of `Config::recall_vector_max_distance`'s doc): raw
/// `nomic-embed-text` cosine similarities between genuinely related titles
/// run well above it, while it still excludes the near-orthogonal
/// (`~0.0`) and anti-correlated (`< 0.0`) matches that would make the
/// surface "trending and owned" rather than "trending, owned, and to your
/// taste." Not tuned against a production corpus yet — a later item can
/// raise it once real distributions are observed, same as
/// `recall_vector_max_distance`.
pub const TASTE_RELEVANCE_MIN: f32 = 0.2;

// --- the intersection: trending ∩ library ∩ taste ---------------------

/// A resolved library instance for a trend entry — it's OWNED, not just
/// known-about (contrast `repo::trending::TrendingNotInLibraryRow`, MUSE-11's
/// not-in-library source, which is this table's exact complement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryMatch {
    pub media_metadata_id: i64,
    pub media_item_id: i64,
}

/// The "the talk" signal attached to a [`CulturalPick`], when a configured
/// [`TrendSource`] returned one (see [`source::TmdbTrendSource::talk`],
/// which is always `NotImplemented` — TMDb has no comment volume — vs
/// [`source::TraktTrendSource::talk`], real when Trakt is configured).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TalkSignal {
    pub talk_score: f64,
    pub comment_count: Option<i64>,
    pub rating_count: Option<i64>,
}

/// One title that is trending right now AND owned in the account's library
/// — the "the talk" surface's unit. `taste_fit`/`talk` are `None` when that
/// signal wasn't available (no taste centroid yet, or the configured
/// `TrendSource` doesn't provide `talk`), never fabricated.
#[derive(Debug, Clone, PartialEq)]
pub struct CulturalPick {
    pub media_metadata_id: i64,
    pub media_item_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub kind: MediaKind,
    pub popularity: f64,
    pub talk: Option<TalkSignal>,
    /// Real cosine similarity (`crate::persona::blend::cosine_similarity`)
    /// to the account's `taste_profile.overall_centroid`, when both the
    /// profile and the title's own embedding are known. `None` — never a
    /// guessed value — otherwise.
    pub taste_fit: Option<f32>,
}

impl CulturalPick {
    /// A grounded, human-readable "the talk" sentence for the channel/GUI —
    /// e.g. "Severance is what everyone's talking about and you've got
    /// it." Only claims "everyone's talking about" when a real `talk`
    /// signal backs it (`self.talk` is `Some` with nonzero comment/rating
    /// volume); otherwise falls back to a trending-only phrasing that makes
    /// no talk-volume claim it can't ground.
    pub fn headline(&self) -> String {
        let has_talk_signal = self
            .talk
            .map(|t| t.comment_count.unwrap_or(0) > 0 || t.rating_count.unwrap_or(0) > 0)
            .unwrap_or(false);

        if has_talk_signal {
            format!(
                "\"{}\" is what everyone's talking about and you've got it.",
                self.title
            )
        } else {
            format!(
                "\"{}\" is trending right now and you've got it.",
                self.title
            )
        }
    }
}

/// Pure core of the trending ∩ library ∩ taste intersection: given trend
/// entries, which of them are OWNED (`library`, keyed by
/// `TrendEntry::external_id`), any "the talk" signal known for them
/// (`talk`), the account's taste centroid (`taste_centroid`, when known),
/// and each owned title's own embedding (`embeddings`, when known), build
/// the ranked [`CulturalPick`] list. No DB access, no `TrendSource` call —
/// see the module doc's "pure core / DB-wrapper split".
///
/// This enforces the FULL three-way intersection, not just two legs:
/// 1. A trend entry not present in `library` is dropped (not owned).
/// 2. An owned entry whose taste relevance is below [`TASTE_RELEVANCE_MIN`]
///    — including one with NO computable taste signal at all (no
///    `taste_centroid`, or no embedding for the title) — is ALSO dropped.
///    This is the leg finding #2 added: the result is "trending titles you
///    own AND would like," never "everything you own that's trending,
///    ranked by taste." (The cold-start path in
///    [`select_cold_start_picks`] is the deliberate exception — it runs
///    precisely when there's no usable taste centroid to intersect against.)
///
/// Every surviving pick therefore has a real `taste_fit >=
/// TASTE_RELEVANCE_MIN`. Ranking among survivors: taste-fit descending,
/// then talk-score descending, then raw popularity descending — popularity
/// is only ever a tiebreak among already-taste-relevant picks, never a way
/// to surface a taste-irrelevant one. Ties beyond that are insertion-order
/// stable (`sort_by`'s own guarantee), matching
/// `curation::recommend::rank_candidates`'s "no invented tiebreak" posture.
pub fn build_cultural_picks(
    entries: &[TrendEntry],
    library: &HashMap<String, LibraryMatch>,
    talk: &HashMap<String, TalkEntry>,
    taste_centroid: Option<&[f32]>,
    embeddings: &HashMap<String, Vec<f32>>,
) -> Vec<CulturalPick> {
    let mut picks: Vec<CulturalPick> = entries
        .iter()
        .filter_map(|entry| {
            // Leg 1: owned?
            let lib = library.get(&entry.external_id)?;

            // Leg 3: taste-relevant? Requires BOTH a centroid to compare
            // against AND an embedding for this title; a missing either =
            // no computable taste signal = not taste-relevant = dropped.
            let taste_fit = taste_centroid.and_then(|centroid| {
                embeddings
                    .get(&entry.external_id)
                    .map(|item_vec| cosine_similarity(item_vec, centroid))
            })?;
            if taste_fit < TASTE_RELEVANCE_MIN {
                return None;
            }

            let talk_signal = talk.get(&entry.external_id).map(|t| TalkSignal {
                talk_score: t.talk_score,
                comment_count: t.comment_count,
                rating_count: t.rating_count,
            });

            Some(CulturalPick {
                media_metadata_id: lib.media_metadata_id,
                media_item_id: lib.media_item_id,
                title: entry.title.clone(),
                year: entry.year,
                kind: entry.kind,
                popularity: entry.popularity,
                talk: talk_signal,
                taste_fit: Some(taste_fit),
            })
        })
        .collect();

    picks.sort_by(|a, b| {
        rank_key(a).cmp(&rank_key(b)).then_with(|| {
            b.popularity
                .partial_cmp(&a.popularity)
                .unwrap_or(Ordering::Equal)
        })
    });
    picks
}

/// Total-orderable rank tuple for [`build_cultural_picks`]'s sort. Every
/// pick reaching this point has already passed the [`TASTE_RELEVANCE_MIN`]
/// filter, so `taste_fit` is always `Some` here; the `unwrap_or` is a
/// defensive floor, never exercised in the wired path. Taste-fit
/// descending (via a millipoint integer — `f32` isn't `Ord`), then
/// talk-score descending (same integer trick).
fn rank_key(pick: &CulturalPick) -> (i64, i64) {
    // Descending sort achieved by negating a scaled integer.
    let taste_rank = pick
        .taste_fit
        .map(|f| -((f * 1_000_000.0) as i64))
        .unwrap_or(0);
    let talk_rank = pick
        .talk
        .map(|t| -((t.talk_score * 1_000_000.0) as i64))
        .unwrap_or(0);
    (taste_rank, talk_rank)
}

/// DB-touching wrapper: resolve `entries` against the account's library +
/// taste profile + embeddings, then hand off to [`build_cultural_picks`].
/// `talk_entries` is passed in already-fetched (via [`TrendCache::talk`] at
/// the caller) rather than fetched here, keeping this function's own DB
/// touch scoped to library/taste/embedding resolution.
pub async fn intersect_trending_library_taste(
    pool: &PgPool,
    account_id: i64,
    entries: &[TrendEntry],
    talk_entries: Vec<TalkEntry>,
) -> MuseResult<Vec<CulturalPick>> {
    let mut library: HashMap<String, LibraryMatch> = HashMap::new();
    let mut embeddings: HashMap<String, Vec<f32>> = HashMap::new();

    for entry in entries {
        let Some(media_metadata_id) =
            repo::media_metadata::find_by_tmdb_id(pool, entry.kind, &entry.external_id).await?
        else {
            continue; // Muse has no catalog entry for this trending title at all
        };

        let items = repo::media_item::list_by_metadata(pool, media_metadata_id).await?;
        let Some(item) = items
            .iter()
            .find(|i| i.in_library)
            .or_else(|| items.first())
        else {
            continue; // known to TMDb/Trakt, but not owned -- not "the talk" surface's job
        };

        library.insert(
            entry.external_id.clone(),
            LibraryMatch {
                media_metadata_id,
                media_item_id: item.id,
            },
        );

        if let Ok(Some(emb)) = repo::embedding::get(
            pool,
            EmbeddingEntityKind::MediaItem.as_str(),
            item.id,
            DEFAULT_EMBEDDING_MODEL,
        )
        .await
        {
            embeddings.insert(entry.external_id.clone(), emb.embedding.as_slice().to_vec());
        }
    }

    let profile = repo::taste::get_profile(pool, account_id).await?;
    let taste_centroid: Option<Vec<f32>> = profile
        .and_then(|p| p.overall_centroid)
        .map(|v| v.as_slice().to_vec());

    let talk: HashMap<String, TalkEntry> = talk_entries
        .into_iter()
        .map(|t| (t.external_id.clone(), t))
        .collect();

    Ok(build_cultural_picks(
        entries,
        &library,
        &talk,
        taste_centroid.as_deref(),
        &embeddings,
    ))
}

/// Fetch trending (cached, rate-limit-respecting) + the "the talk" volume
/// for those same titles, then intersect against `account_id`'s library +
/// taste — the full, end-to-end "the talk" surface a channel/GUI calls.
/// Any `TrendSource::talk` failure (e.g. `TmdbTrendSource`'s
/// `NotImplemented`, or an unconfigured/unreachable Trakt) degrades to an
/// empty talk map rather than failing the whole surface — a `CulturalPick`
/// with `talk: None` is still a valid trending-and-owned pick.
pub async fn the_talk_surface(
    pool: &PgPool,
    account_id: i64,
    source: &dyn TrendSource,
    cache: &TrendCache,
    region: &str,
) -> MuseResult<Vec<CulturalPick>> {
    let trend_query = TrendQuery {
        region: region.to_string(),
        window: crate::trending::TrendingWindow::Day,
        kind: None,
    };
    let entries = cache.trending(source, &trend_query).await?;

    let external_ids: Vec<String> = entries.iter().map(|e| e.external_id.clone()).collect();
    let talk_entries = if external_ids.is_empty() {
        Vec::new()
    } else {
        cache
            .talk(source, &TalkQuery { external_ids })
            .await
            .unwrap_or_default()
    };

    intersect_trending_library_taste(pool, account_id, &entries, talk_entries).await
}

// --- cold-start: sparse taste signal -> trend + taste-neighbor --------

/// One cold-start pick: a trend entry, optionally ranked by a
/// TASTE-NEIGHBOR similarity (see [`select_cold_start_picks`]'s doc).
#[derive(Debug, Clone, PartialEq)]
pub struct ColdStartPick {
    pub entry: TrendEntry,
    /// Cosine similarity to a taste-neighbor centroid (see
    /// [`select_cold_start_picks`]), when one was available. `None` means
    /// "no neighbor signal" — the pick still surfaces, ranked by raw trend
    /// popularity instead ("what's hot that people are watching," the
    /// AC's own fallback-of-the-fallback framing).
    pub neighbor_fit: Option<f32>,
}

/// Pure core of the cold-start fallback: given trend entries, a
/// TASTE-NEIGHBOR centroid (e.g. a persona centroid — see
/// `cold_start_recommendations`'s doc for where that comes from) and each
/// entry's own embedding (when resolvable), rank entries by neighbor
/// similarity when a neighbor centroid is available, otherwise by raw trend
/// popularity ("what's hot that people like you love" degrading cleanly to
/// "what's hot," never an empty result — the AC's own two-tier framing).
///
/// Never filters by library ownership or an account's own (sparse/absent)
/// taste centroid — that's precisely the point of a cold-start path: it
/// runs when the normal taste-tier and the-talk-surface paths have nothing
/// to say.
pub fn select_cold_start_picks(
    entries: &[TrendEntry],
    neighbor_centroid: Option<&[f32]>,
    embeddings: &HashMap<String, Vec<f32>>,
) -> Vec<ColdStartPick> {
    let mut picks: Vec<ColdStartPick> = entries
        .iter()
        .map(|entry| {
            let neighbor_fit = neighbor_centroid.and_then(|centroid| {
                embeddings
                    .get(&entry.external_id)
                    .map(|item_vec| cosine_similarity(item_vec, centroid))
            });
            ColdStartPick {
                entry: entry.clone(),
                neighbor_fit,
            }
        })
        .collect();

    picks.sort_by(|a, b| {
        match (a.neighbor_fit, b.neighbor_fit) {
            (Some(af), Some(bf)) => bf.partial_cmp(&af).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Less, // a known-neighbor-similar pick outranks an unknown one
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
        .then_with(|| {
            b.entry
                .popularity
                .partial_cmp(&a.entry.popularity)
                .unwrap_or(Ordering::Equal)
        })
    });
    picks
}

/// DB-touching cold-start wrapper. Callers gate this on
/// [`is_profile_sparse`] themselves (this function always runs the
/// fallback path unconditionally — it doesn't re-check sparsity, matching
/// `curation::candidates::gather_taste_candidates`'s own "the caller decides
/// which source(s) to gather" posture).
///
/// TASTE-NEIGHBOR: the account's own personas
/// (`repo::persona::list_for_account` — includes personas the account is a
/// member of via a SHARED/household persona, per that function's own doc)
/// may carry more signal than a thin `taste_profile` even when the profile
/// itself is sparse (e.g. a shared household persona derived from a
/// co-viewer's heavier watch history, or a context-cluster persona from a
/// few sessions concentrated in one bucket). The first persona (by that
/// function's deterministic `(name, kind, id)` order) is used as the
/// neighbor centroid when one exists; `None` when the account has no
/// persona yet either, in which case [`select_cold_start_picks`] degrades
/// to plain trend popularity.
pub async fn cold_start_recommendations(
    pool: &PgPool,
    account_id: i64,
    source: &dyn TrendSource,
    cache: &TrendCache,
    region: &str,
) -> MuseResult<Vec<ColdStartPick>> {
    let trend_query = TrendQuery {
        region: region.to_string(),
        window: crate::trending::TrendingWindow::Week,
        kind: None,
    };
    let entries = cache.trending(source, &trend_query).await?;

    let personas = repo::persona::list_for_account(pool, account_id).await?;
    let neighbor_centroid: Option<Vec<f32>> =
        personas.first().map(|p| p.centroid.as_slice().to_vec());

    let mut embeddings: HashMap<String, Vec<f32>> = HashMap::new();
    if neighbor_centroid.is_some() {
        for entry in &entries {
            let Ok(Some(media_metadata_id)) =
                repo::media_metadata::find_by_tmdb_id(pool, entry.kind, &entry.external_id).await
            else {
                continue;
            };
            let Ok(items) = repo::media_item::list_by_metadata(pool, media_metadata_id).await
            else {
                continue;
            };
            let Some(item) = items.into_iter().next() else {
                continue;
            };
            if let Ok(Some(emb)) = repo::embedding::get(
                pool,
                EmbeddingEntityKind::MediaItem.as_str(),
                item.id,
                DEFAULT_EMBEDDING_MODEL,
            )
            .await
            {
                embeddings.insert(entry.external_id.clone(), emb.embedding.as_slice().to_vec());
            }
        }
    }

    Ok(select_cold_start_picks(
        &entries,
        neighbor_centroid.as_deref(),
        &embeddings,
    ))
}

// --- entry point: sparse-aware routing --------------------------------

/// What [`recommend_cultural`] returned, tagged with WHICH path produced it
/// — so a caller (and the routing test) can tell a sparse-profile
/// cold-start result from a rich-profile taste-intersection one, rather
/// than the two collapsing into an untyped list.
#[derive(Debug, Clone, PartialEq)]
pub enum CulturalRecommendations {
    /// Rich-profile path: the trending ∩ library ∩ taste intersection
    /// ([`the_talk_surface`] / [`build_cultural_picks`]).
    TasteIntersection(Vec<CulturalPick>),
    /// Sparse-profile cold-start fallback ([`cold_start_recommendations`] /
    /// [`select_cold_start_picks`]): trend + taste-neighbor / popularity.
    ColdStart(Vec<ColdStartPick>),
}

/// The single cultural-layer entry point a channel/GUI calls. THIS is where
/// [`is_profile_sparse`] actually triggers the cold-start fallback (finding
/// #3): it loads the account's `taste_profile` and branches —
///
/// - **sparse** (no profile / no centroid / thin genre affinity) →
///   [`cold_start_recommendations`] ("what's hot that people like you love,"
///   degrading to "what's hot"), because the normal taste-intersection has
///   no usable centroid to intersect against and
///   [`build_cultural_picks`]'s [`TASTE_RELEVANCE_MIN`] filter would drop
///   everything;
/// - **not sparse** → [`the_talk_surface`], the real trending ∩ library ∩
///   taste intersection.
///
/// Loading the profile once here and branching (rather than letting
/// `the_talk_surface` re-derive sparsity) keeps the decision in one place
/// and makes it directly testable — see
/// `live_tests::sparse_profile_routes_to_cold_start_while_rich_profile_uses_the_intersection`.
pub async fn recommend_cultural(
    pool: &PgPool,
    account_id: i64,
    source: &dyn TrendSource,
    cache: &TrendCache,
    region: &str,
) -> MuseResult<CulturalRecommendations> {
    let profile = repo::taste::get_profile(pool, account_id).await?;

    if is_profile_sparse(profile.as_ref()) {
        let picks = cold_start_recommendations(pool, account_id, source, cache, region).await?;
        Ok(CulturalRecommendations::ColdStart(picks))
    } else {
        let picks = the_talk_surface(pool, account_id, source, cache, region).await?;
        Ok(CulturalRecommendations::TasteIntersection(picks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::media_metadata::MediaKind;

    fn trend_entry(external_id: &str, title: &str, popularity: f64) -> TrendEntry {
        TrendEntry {
            external_id: external_id.to_string(),
            kind: MediaKind::Show,
            title: title.to_string(),
            year: Some(2022),
            popularity,
        }
    }

    fn profile_with_genre_count(n: usize, has_centroid: bool) -> TasteProfile {
        let mut map = serde_json::Map::new();
        for i in 0..n {
            map.insert(format!("genre_{i}"), serde_json::json!(1.0));
        }
        TasteProfile {
            account_id: 1,
            genre_affinity: serde_json::Value::Object(map),
            person_affinity: serde_json::json!({}),
            keyword_affinity: serde_json::json!({}),
            runtime_pref: None,
            quality_sensitivity: None,
            overall_centroid: has_centroid.then(|| pgvector::Vector::from(vec![0.1_f32; 4])),
            computed_at: chrono::Utc::now(),
            model_notes: None,
        }
    }

    // --- is_profile_sparse ---------------------------------------------

    #[test]
    fn no_profile_is_sparse() {
        assert!(is_profile_sparse(None));
    }

    #[test]
    fn profile_with_no_centroid_is_sparse_even_with_many_genres() {
        let profile = profile_with_genre_count(10, false);
        assert!(is_profile_sparse(Some(&profile)));
    }

    #[test]
    fn profile_below_genre_threshold_is_sparse() {
        let profile = profile_with_genre_count(1, true);
        assert!(is_profile_sparse(Some(&profile)));
    }

    #[test]
    fn profile_at_or_above_genre_threshold_is_not_sparse() {
        let profile = profile_with_genre_count(SPARSE_GENRE_AFFINITY_MIN, true);
        assert!(!is_profile_sparse(Some(&profile)));
    }

    // --- build_cultural_picks: the intersection --------------------------

    /// Leg 1 of the intersection: a trending title not owned is dropped
    /// even when it IS taste-relevant (embedding present + above the
    /// cutoff) — ownership is a hard gate, not a soft signal.
    #[test]
    fn intersection_drops_trending_titles_not_in_library() {
        let entries = vec![
            trend_entry("1", "Owned", 50.0),
            trend_entry("2", "Not Owned", 90.0),
        ];
        let mut library = HashMap::new();
        library.insert(
            "1".to_string(),
            LibraryMatch {
                media_metadata_id: 100,
                media_item_id: 200,
            },
        );

        // Both titles are taste-relevant; only "1" is owned.
        let centroid = vec![1.0, 0.0];
        let mut embeddings = HashMap::new();
        embeddings.insert("1".to_string(), vec![1.0, 0.0]);
        embeddings.insert("2".to_string(), vec![1.0, 0.0]);

        let picks = build_cultural_picks(
            &entries,
            &library,
            &HashMap::new(),
            Some(&centroid),
            &embeddings,
        );

        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].title, "Owned");
    }

    /// Leg 3, part A: an owned trending title with NO computable taste
    /// signal (no embedding for it, or no centroid at all) is DROPPED, not
    /// merely ranked last — that's what makes this a real three-way
    /// intersection rather than "everything owned, ranked by taste."
    #[test]
    fn intersection_drops_owned_titles_without_a_computable_taste_signal() {
        let entries = vec![
            trend_entry("1", "No Embedding", 99.0),
            trend_entry("2", "Embedded", 10.0),
        ];
        let mut library = HashMap::new();
        library.insert(
            "1".to_string(),
            LibraryMatch {
                media_metadata_id: 1,
                media_item_id: 1,
            },
        );
        library.insert(
            "2".to_string(),
            LibraryMatch {
                media_metadata_id: 2,
                media_item_id: 2,
            },
        );

        let mut embeddings = HashMap::new();
        embeddings.insert("2".to_string(), vec![1.0, 0.0]); // only "2" has an embedding
        let centroid = vec![1.0, 0.0]; // identical vector -> cosine similarity 1.0

        let picks = build_cultural_picks(
            &entries,
            &library,
            &HashMap::new(),
            Some(&centroid),
            &embeddings,
        );

        assert_eq!(
            picks.len(),
            1,
            "the owned-but-un-embedded title must be dropped, not surfaced with a null taste signal"
        );
        assert_eq!(picks[0].title, "Embedded");
        assert!(picks[0].taste_fit.is_some());

        // With no centroid at all, EVERY owned title is dropped (the
        // non-cold-start intersection has nothing to intersect against).
        let picks_no_centroid =
            build_cultural_picks(&entries, &library, &HashMap::new(), None, &embeddings);
        assert!(
            picks_no_centroid.is_empty(),
            "with no taste centroid, the taste-intersection path yields nothing (cold-start is the exception)"
        );
    }

    /// Leg 3, part B: an owned trending title BELOW [`TASTE_RELEVANCE_MIN`]
    /// is dropped even at very high popularity, and popularity is only a
    /// tiebreak AMONG the taste-relevant survivors — never a way to surface
    /// a taste-irrelevant one.
    #[test]
    fn intersection_drops_taste_irrelevant_titles_and_keeps_popularity_as_tiebreak() {
        let entries = vec![
            trend_entry("1", "Popular But Off-Taste", 99.0),
            trend_entry("2", "Relevant Lower Pop", 40.0),
            trend_entry("3", "Relevant Higher Pop", 80.0),
        ];
        let mut library = HashMap::new();
        for id in ["1", "2", "3"] {
            library.insert(
                id.to_string(),
                LibraryMatch {
                    media_metadata_id: id.parse().unwrap(),
                    media_item_id: id.parse().unwrap(),
                },
            );
        }

        let centroid = vec![1.0, 0.0];
        let mut embeddings = HashMap::new();
        embeddings.insert("1".to_string(), vec![0.0, 1.0]); // orthogonal -> cosine 0.0 < 0.2, dropped
        embeddings.insert("2".to_string(), vec![1.0, 0.0]); // cosine 1.0
        embeddings.insert("3".to_string(), vec![1.0, 0.0]); // cosine 1.0

        let picks = build_cultural_picks(
            &entries,
            &library,
            &HashMap::new(),
            Some(&centroid),
            &embeddings,
        );

        assert_eq!(
            picks.len(),
            2,
            "the off-taste title is dropped despite the highest popularity"
        );
        assert!(picks.iter().all(|p| p.title != "Popular But Off-Taste"));
        // Equal taste_fit + no talk -> popularity tiebreak among relevants.
        assert_eq!(picks[0].title, "Relevant Higher Pop");
        assert_eq!(picks[1].title, "Relevant Lower Pop");
    }

    #[test]
    fn headline_claims_talk_only_when_a_real_talk_signal_backs_it() {
        let base = CulturalPick {
            media_metadata_id: 1,
            media_item_id: 1,
            title: "Severance".to_string(),
            year: Some(2022),
            kind: MediaKind::Show,
            popularity: 90.0,
            talk: None,
            taste_fit: None,
        };

        assert!(base.headline().contains("trending right now"));
        assert!(!base.headline().contains("everyone's talking"));

        let with_talk = CulturalPick {
            talk: Some(TalkSignal {
                talk_score: 0.9,
                comment_count: Some(80),
                rating_count: None,
            }),
            ..base.clone()
        };
        assert!(with_talk.headline().contains("everyone's talking"));

        let zero_talk = CulturalPick {
            talk: Some(TalkSignal {
                talk_score: 0.0,
                comment_count: Some(0),
                rating_count: Some(0),
            }),
            ..base
        };
        assert!(
            !zero_talk.headline().contains("everyone's talking"),
            "a talk entry with zero actual volume must not be phrased as if it were culturally live"
        );
    }

    // --- select_cold_start_picks: the sparse-profile fallback -----------

    #[test]
    fn cold_start_activates_and_returns_non_empty_from_trends_with_no_neighbor() {
        let entries = vec![
            trend_entry("1", "Hot Show", 80.0),
            trend_entry("2", "Hotter Show", 95.0),
        ];

        let picks = select_cold_start_picks(&entries, None, &HashMap::new());

        assert!(
            !picks.is_empty(),
            "cold-start must still return trend picks even with zero taste-neighbor signal"
        );
        assert_eq!(picks.len(), 2);
        assert_eq!(
            picks[0].entry.title, "Hotter Show",
            "no neighbor signal -> pure popularity ranking"
        );
        assert!(picks.iter().all(|p| p.neighbor_fit.is_none()));
    }

    #[test]
    fn cold_start_prefers_taste_neighbor_similarity_over_raw_popularity() {
        let entries = vec![
            trend_entry("1", "Popular But Off-Taste", 99.0),
            trend_entry("2", "Less Popular But On-Taste", 40.0),
        ];
        let mut embeddings = HashMap::new();
        embeddings.insert("2".to_string(), vec![1.0, 0.0]);
        let neighbor_centroid = vec![1.0, 0.0];

        let picks = select_cold_start_picks(&entries, Some(&neighbor_centroid), &embeddings);

        assert_eq!(
            picks[0].entry.title, "Less Popular But On-Taste",
            "taste-neighbor similarity must outrank raw popularity for cold-start (\"what's hot that people like you love\")"
        );
        assert!(picks[0].neighbor_fit.is_some());
    }

    #[test]
    fn cold_start_never_panics_on_empty_trend_input() {
        let picks = select_cold_start_picks(&[], Some(&[1.0, 0.0]), &HashMap::new());
        assert!(picks.is_empty());
    }
}
