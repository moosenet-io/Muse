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
}
