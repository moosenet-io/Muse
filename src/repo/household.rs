//! MUSE #85: household viewing analytics for the Constellation web GUI's
//! Muse "taste" panel.
//!
//! ## Why this is a new module rather than more `web::graph`
//! `Terminus/constellation-web/src/hooks/useMuse.ts` fetches
//! `/api/graph/watch-history` and `/api/graph/group-dynamics` with a plain
//! parameterless `GET`, and expects **household account analytics**:
//!
//! ```text
//! MuseWatchHistory  { series: [{ date, [participant]: number }] }
//! MuseGroupDynamics { rows: [{ participant, watched_together_pct,
//!                              favorite_genre, sessions }] }
//! ```
//!
//! The MUSEX-17 handlers already mounted at those paths are a different
//! thing entirely: they are **client-fed** KG visualizations (the caller
//! POSTs `friends`/`watches`/`co_views`/`personas` and the server assembles
//! an opt-in-filtered graph), keyed on *Discord friend identities*, and they
//! return `TasteMapViz`/`GroupDynamicsViz` shapes. Adding a `GET` verb that
//! reused them would assemble an EMPTY `GraphSourceInput` and return an empty
//! visualization on every call — a 200 that the GUI renders as data, i.e. a
//! confident "nobody watches anything together".
//!
//! So these read the durable household record — `play_sessions` × `accounts`
//! — instead. No Discord opt-in is involved because no Discord identity is
//! involved: these are the operator's own household accounts, which is why
//! both endpoints stay on the protected router.

use chrono::NaiveDate;
use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};

/// One (day, participant) bucket. The GUI pivots these into its
/// `series: [{ date, <participant>: n, ... }]` row-per-day shape; the SQL
/// deliberately returns the long/tidy form so a day with no activity for one
/// participant simply has no row, rather than the query having to synthesize
/// a zero for every account × every day.
#[derive(Debug, Clone, FromRow)]
pub struct WatchHistoryBucket {
    pub day: NaiveDate,
    pub participant: String,
    pub sessions: i64,
}

/// Sessions per day per household account over the trailing `days` window.
///
/// `make_interval(days => $1)` is used rather than string-concatenating an
/// interval literal — the parameter stays a bound integer and never becomes
/// SQL text.
pub async fn watch_history(pool: &PgPool, days: i32) -> MuseResult<Vec<WatchHistoryBucket>> {
    sqlx::query_as::<_, WatchHistoryBucket>(
        r#"
        SELECT
            ps.started_at::date AS day,
            COALESCE(a.friendly_name, a.username) AS participant,
            COUNT(*)::bigint AS sessions
        FROM play_sessions ps
        JOIN accounts a ON a.id = ps.account_id
        WHERE ps.started_at >= now() - make_interval(days => $1)
        GROUP BY 1, 2
        ORDER BY 1, 2
        "#,
    )
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One row per household account that has any recorded session.
#[derive(Debug, Clone, FromRow)]
pub struct GroupDynamicsRow {
    pub participant: String,
    pub sessions: i64,
    /// Sessions of this account that overlap IN TIME with a session belonging
    /// to a different account — the observable proxy for "watched together".
    /// See [`group_dynamics`] for why this is a proxy and not a fact.
    pub together_sessions: i64,
    /// `None` when this account's watched titles have no genre rows at all
    /// (nothing watched yet, or metadata not enriched) — never a fabricated
    /// placeholder genre.
    pub favorite_genre: Option<String>,
}

/// Per-account session counts, co-viewing overlap, and most-watched genre.
///
/// ## `together_sessions` is a time-overlap proxy, stated plainly
/// Muse has no "these people sat on the same couch" record. What it does have
/// is per-account sessions with start/stop times, so this counts a session as
/// "together" when it overlaps the wall-clock window of a session on a
/// *different* account. That genuinely catches co-viewing on one household's
/// accounts, and genuinely miscounts two people independently watching
/// different things at the same time. The endpoint's field is named
/// `watched_together_pct` because that is the GUI's contract; this doc is the
/// place the proxy is recorded, and `web::household`'s handler doc repeats it.
///
/// A session's end is `stopped_at` when known, else
/// `started_at + duration_ms`. When BOTH are absent the window collapses to
/// zero length, and the `together` CTE excludes such sessions explicitly (see
/// its positive-length guards) — they are counted in `sessions` but never in
/// `together_sessions`. So a session whose end is unknown is never *claimed*
/// as co-viewed, which makes `watched_together_pct` a floor rather than an
/// exact figure on data with missing stop times.
///
/// Two earlier versions of this comment were wrong and both were caught in
/// review: the first claimed such a session "has a real window" (it does
/// not), and the second claimed the strict `<` predicates alone excluded it
/// (they do not — a zero-length window strictly inside another session
/// satisfies both). Hence the explicit guards rather than a relied-upon
/// side effect. On the current data set the distinction is moot — all 1,544
/// `play_sessions` rows have a `stopped_at` — but the guarantee should hold
/// for data that does not.
///
/// The fix was verified empirically against Postgres by replaying both
/// predicates over synthetic rows: the unguarded version counted the
/// zero-length session, the guarded one counts only the genuine overlap and
/// still excludes adjacent-but-not-overlapping sessions.
pub async fn group_dynamics(pool: &PgPool) -> MuseResult<Vec<GroupDynamicsRow>> {
    sqlx::query_as::<_, GroupDynamicsRow>(
        r#"
        WITH s AS (
            SELECT
                ps.id,
                ps.account_id,
                ps.started_at,
                COALESCE(
                    ps.stopped_at,
                    ps.started_at + make_interval(secs => COALESCE(ps.duration_ms, 0) / 1000.0)
                ) AS ended_at
            FROM play_sessions ps
        ),
        together AS (
            SELECT DISTINCT s1.id
            FROM s s1
            JOIN s s2
              ON s2.account_id <> s1.account_id
             -- Both windows must have POSITIVE length. Without these two
             -- guards a zero-length window (no `stopped_at` AND no
             -- `duration_ms`) still satisfies the strict overlap predicates
             -- whenever its instant falls strictly inside the other session
             -- --   s1=[t,t] vs s2=[t-1,t+1]  =>  t < t+1 and t-1 < t  --
             -- so it WOULD be counted, while the otherwise-identical case
             -- starting exactly on s2's start would not. That knife-edge
             -- inconsistency is the bug a reviewer caught; excluding
             -- unknown-length sessions outright is both consistent and the
             -- conservative direction.
             AND s1.ended_at > s1.started_at
             AND s2.ended_at > s2.started_at
             AND s1.started_at < s2.ended_at
             AND s2.started_at < s1.ended_at
        ),
        fav AS (
            SELECT DISTINCT ON (ps.account_id)
                ps.account_id,
                g.name AS genre
            FROM play_sessions ps
            JOIN media_items mi ON mi.id = ps.media_item_id
            JOIN media_metadata_genres mg ON mg.media_metadata_id = mi.media_metadata_id
            JOIN genres g ON g.id = mg.genre_id
            GROUP BY ps.account_id, g.name
            ORDER BY ps.account_id, COUNT(*) DESC, g.name
        )
        SELECT
            COALESCE(a.friendly_name, a.username) AS participant,
            COUNT(s.id)::bigint AS sessions,
            COUNT(t.id)::bigint AS together_sessions,
            fav.genre AS favorite_genre
        FROM accounts a
        JOIN s ON s.account_id = a.id
        LEFT JOIN together t ON t.id = s.id
        LEFT JOIN fav ON fav.account_id = a.id
        GROUP BY a.id, a.friendly_name, a.username, fav.genre
        ORDER BY sessions DESC, participant
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
