//! Thin repo layer over the MUSE-02 arr-shaped core schema.
//!
//! All queries use **runtime** sqlx (`sqlx::query`/`sqlx::query_as`) per the
//! MUSE-02 build constraint — never the `query!`/`query_as!` compile-time
//! macros, since the crate must build without a live database.

pub mod account;
pub mod acquisition;
pub mod artwork_cache;
pub mod availability;
pub mod channel;
pub mod dashboard;
pub mod embedding;
pub mod episode;
pub mod external_enrichment;
pub mod friend_opt_in;
pub mod household;
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
pub mod settings;
pub mod subtitle;
pub mod taste;
pub mod taste_divergence;
pub mod trending;
pub mod watch_stats;

/// Classify a bounded fetch (`LIMIT 2`) into "no match" / "exactly one
/// match" / "ambiguous — more than one match". MACT-02 (Plane MUSE #122)
/// review finding: for a mutation with real-world blast radius (stopping
/// someone's stream), a `LIMIT 1 ORDER BY ... DESC` tiebreak on a
/// non-unique column silently picks a target when there could be more than
/// one candidate — a renamed/duplicate `plex_clients.name`, or a reused
/// `play_sessions.session_key`. Ambiguity must be a REFUSAL, not a
/// tiebreak: the caller fetches at most 2 rows and this decides whether
/// that's zero, exactly one, or "there could be more than one" (in which
/// case which one it actually is doesn't matter — it refuses either way).
///
/// Deliberately generic and pure (no I/O) so the refusal DECISION has fast
/// unit coverage that runs unconditionally, independent of
/// `MUSE_TEST_DATABASE_URL` and of the SQL query that produced the rows —
/// see `repo::tests::at_most_one_classifies_zero_one_and_many` and the
/// `plex_control`/`play_session` call sites that plug real row types in.
#[derive(Debug)]
pub enum AtMostOne<T> {
    None,
    One(T),
    Ambiguous,
}

pub fn at_most_one<T>(mut rows: Vec<T>) -> AtMostOne<T> {
    match rows.len() {
        0 => AtMostOne::None,
        1 => AtMostOne::One(rows.remove(0)),
        _ => AtMostOne::Ambiguous,
    }
}

/// Classify a resolution attempt that has a FRESHNESS requirement, not just
/// a uniqueness one — MACT-02 (Plane MUSE #122) review finding, cycle 2:
/// "`LIMIT 2` fixed *ambiguity*, but resolution also needs *freshness*.
/// Uniqueness is not identity." A row set that is unambiguous under
/// [`at_most_one`] can still be the WRONG answer if it's stale — a
/// `plex_clients` row nobody pruned, sharing a display name with a
/// newly-connected device the stale row's `machine_identifier` no longer
/// belongs to. So a caller fetches only rows passing an app-defined
/// freshness cutoff, and ALSO checks (cheaply, only when needed) whether
/// any match exists at all regardless of freshness — this function turns
/// those two pieces of information into one of four outcomes:
///
/// - `NoMatch` — nothing matched, fresh or not. Same "nothing to find"
///   posture as [`AtMostOne::None`].
/// - `StaleOnly` — at least one match exists, but none passed the caller's
///   freshness cutoff. A DISTINCT refusal from `NoMatch` (there WAS
///   something, it just isn't trustworthy right now) and from `Ambiguous`
///   (there's no multiplicity question here — the problem is currency, not
///   count). Never silently promoted to `Found` just because it happens to
///   be the only stale row.
/// - `Ambiguous` — more than one FRESH match; see [`AtMostOne::Ambiguous`].
/// - `Found(T)` — exactly one match passed the freshness cutoff.
///
/// Pure and generic (no I/O, no timestamp comparison — the caller does the
/// `last_seen_at >= cutoff` filtering in SQL and passes the already-fresh
/// rows here), so this has fast unit coverage that runs unconditionally —
/// see `repo::tests::classify_with_freshness_*`.
#[derive(Debug)]
pub enum FreshnessLookup<T> {
    NoMatch,
    StaleOnly,
    Ambiguous,
    Found(T),
}

pub fn classify_with_freshness<T>(fresh_rows: Vec<T>, any_match_exists: bool) -> FreshnessLookup<T> {
    match at_most_one(fresh_rows) {
        AtMostOne::One(v) => FreshnessLookup::Found(v),
        AtMostOne::Ambiguous => FreshnessLookup::Ambiguous,
        AtMostOne::None => {
            if any_match_exists {
                FreshnessLookup::StaleOnly
            } else {
                FreshnessLookup::NoMatch
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_most_one_classifies_zero_one_and_many() {
        assert!(matches!(at_most_one::<i32>(vec![]), AtMostOne::None));
        assert!(matches!(at_most_one(vec![42]), AtMostOne::One(42)));
        assert!(matches!(at_most_one(vec![1, 2]), AtMostOne::Ambiguous));
    }

    #[test]
    fn at_most_one_is_ambiguous_regardless_of_how_many_beyond_two() {
        // The caller is expected to bound the fetch at LIMIT 2 -- this
        // proves the classification itself doesn't need that bound to be
        // correct: 3+ rows is exactly as "ambiguous" as 2.
        assert!(matches!(at_most_one(vec![1, 2, 3]), AtMostOne::Ambiguous));
    }

    #[test]
    fn at_most_one_preserves_the_single_match_unmodified() {
        let only = "the-one-match".to_string();
        match at_most_one(vec![only.clone()]) {
            AtMostOne::One(v) => assert_eq!(v, only),
            other => panic!("expected One(_), got {other:?}"),
        }
    }

    // -- classify_with_freshness (MACT-02 cycle 2) -----------------------

    #[test]
    fn classify_with_freshness_no_fresh_rows_and_no_stale_ones_is_no_match() {
        assert!(matches!(
            classify_with_freshness::<i32>(vec![], false),
            FreshnessLookup::NoMatch
        ));
    }

    #[test]
    fn classify_with_freshness_no_fresh_rows_but_a_stale_one_exists_is_stale_only() {
        // The whole point of this cycle's fix: a match existing is NOT
        // enough on its own -- an unambiguous but stale match must refuse,
        // distinctly from "nothing matched at all".
        assert!(matches!(
            classify_with_freshness::<i32>(vec![], true),
            FreshnessLookup::StaleOnly
        ));
    }

    #[test]
    fn classify_with_freshness_exactly_one_fresh_row_is_found_regardless_of_stale_flag() {
        // `any_match_exists` only matters when there are ZERO fresh rows --
        // it must never suppress a genuinely fresh, unambiguous match.
        assert!(matches!(
            classify_with_freshness(vec![42], true),
            FreshnessLookup::Found(42)
        ));
        assert!(matches!(
            classify_with_freshness(vec![42], false),
            FreshnessLookup::Found(42)
        ));
    }

    #[test]
    fn classify_with_freshness_more_than_one_fresh_row_is_ambiguous() {
        assert!(matches!(
            classify_with_freshness(vec![1, 2], true),
            FreshnessLookup::Ambiguous
        ));
    }

    #[test]
    fn classify_with_freshness_found_preserves_the_value_unmodified() {
        let only = "the-fresh-one".to_string();
        match classify_with_freshness(vec![only.clone()], true) {
            FreshnessLookup::Found(v) => assert_eq!(v, only),
            other => panic!("expected Found(_), got {other:?}"),
        }
    }
}
