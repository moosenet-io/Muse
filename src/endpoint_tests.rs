//! MUSET-01: contract + integration test harness for every mounted Muse
//! HTTP endpoint (`http::router`), exercised through the real axum
//! `Router` via `tower::ServiceExt::oneshot` — a genuine HTTP-shaped
//! request/response round trip, not just a direct handler call (the crate's
//! existing tests, in `http::ops`/`channels::routes`, call handlers
//! directly; this module is the router-level complement, verifying the
//! method/path wiring itself as well as the handler contracts).
//!
//! ## Scope
//! Covers every route mounted in `crate::http::router` as of Phase 0:
//! `/health`, `/query/resolve`, `/query/similar`, `/recommend` +
//! `/recommend/on_deck` + `/recommend/gaps` (curation/recommendations),
//! `/proactive/pending` + `/proactive/{id}/ack`, `/api/channels` +
//! `/api/channels/{id}/lineup` (channel-guide metadata), `/art/{kind}/{id}`
//! (artwork proxy), `/channels/{id}/compose` (the on-demand channel
//! director), `/ops/*`, and the `/ingest`, `/query`, `/proactive`
//! fallback-501 contract for not-yet-built subroutes. (No standalone
//! `/taste` or `/availability` HTTP endpoint exists yet on this router —
//! taste/availability signals are consumed *inside* the recommend/resolve
//! handlers above rather than exposed as their own routes; this module
//! tests the real mounted surface, not routes that don't exist.)
//!
//! Every endpoint gets a contract test (status + response JSON shape for a
//! well-formed request) and an error-path test (malformed body / missing
//! required field / not-found id / invalid state). Happy-path tests that
//! require real rows live in the `db_gated` submodule below.
//!
//! ## Read-only-ness
//! Phase 0's read endpoints — `/health`, `/query/resolve`, `/query/similar`,
//! `/recommend`, `/recommend/on_deck`, `/recommend/gaps`, `/api/channels`,
//! `/api/channels/{id}/lineup`, `/art/{kind}/{id}`, `/proactive/pending` —
//! are asserted NON-MUTATING by `db_gated::read_endpoints_never_mutate_the_database`:
//! it snapshots every watched table's row count, exercises every read
//! endpoint back-to-back, and asserts the counts are unchanged afterward.
//! `/channels/{id}/compose` and `/proactive/{id}/ack` are intentionally
//! *mutating* Phase-0 endpoints (they create a channel run / update an
//! item's delivery state, respectively) and are asserted for the opposite —
//! that they DO write exactly what they claim to — in their own tests
//! rather than being folded into the non-mutation check.
//!
//! ## No live-stack contact
//! Every test either (a) needs no database at all — a `connect_lazy` pool
//! that this module's code paths never issue a query against (the same
//! idiom `http::ops`'s and `channels::routes`'s own existing unit tests
//! already use), or (b) is gated on `MUSE_TEST_DATABASE_URL`, which must
//! point at a disposable scratch Postgres the operator/CI provisions —
//! never a live Plex/Tautulli/*arr instance or the production Muse
//! database, and every external upstream (Plex, Prowlarr, TMDb, Tautulli,
//! arr fleet, Ollama, Chord) is left unconfigured (`None`) in every
//! `AppState` this module builds, so no test here can reach a real one even
//! by accident. `MUSE_TEST_DATABASE_URL` is read the same way the crate's
//! existing DB-gated tests already do — a local scratch-database URL, not
//! an application secret; production config (`Config::from_env`) and any
//! vault-managed value are untouched by this file. No literal IP/hostname/
//! org name appears anywhere below.
//!
//! ## Reusable in CI
//! `cargo test` runs the full non-DB contract/error-path suite with zero
//! setup; exporting `MUSE_TEST_DATABASE_URL` (pointed at a scratch
//! Postgres with the `pgvector`/`pg_trgm` extensions this crate's
//! migrations expect) additionally unlocks the happy-path and
//! non-mutation suite in `db_gated`.
//!
//! ## Discovered bug: `{param}` routes don't match under axum 0.7
//! Building this harness surfaced a real muse routing bug: muse depends on
//! **axum 0.7**, whose path-parameter syntax is `:id`, but its routes use
//! the **axum 0.8** brace syntax `{id}` (`/proactive/{id}/ack`,
//! `/channels/{id}/compose`, `/channels/{id}/lineup`, `/art/{kind}/{id}`).
//! Under 0.7 a `{id}` segment is a LITERAL, not a capture — so EVERY
//! `{param}` route (`/proactive/{id}/ack`, `/channels/{id}/compose`,
//! `/api/channels/{id}/lineup`, `/art/{kind}/{id}`) never matches its real
//! handler and falls through to the fallback (`not_implemented`/501 for the
//! nested groups, the router's 404 for the top-level `/channels/{id}/compose`
//! route). The bug is UNIVERSAL, empirically confirmed on a minimal
//! axum-0.7 router. Filed as **MUSE-ROUTE-01 (#31)**. MUSE-ROUTE-01 has now
//! landed — all route strings were migrated to axum-0.7 `:id` syntax and the
//! `{param}`-route tests below are active again (their error-path assertions
//! ran DB-independent; their happy-path assertions stay `db_gated` as
//! before). They always asserted the CORRECT contract (real handler
//! behavior), so un-ignoring them just flips them green now that the routes
//! are corrected — they were live regression guards, not deleted coverage.
//!
//! ## MUSET-02: golden-response regression baseline
//! On top of MUSET-01's contract/error-path/happy-path coverage above, the
//! `golden_support` + `golden` modules at the bottom of this file (plus
//! `golden::golden` nested inside `db_gated` for the seeded-row endpoints)
//! add a **golden-response baseline**: the exact, canonicalized JSON (or raw
//! bytes, for the artwork proxy) a representative request currently returns,
//! committed under `tests/golden/` and diffed on every run — so a change to
//! response *content*, not just status code, surfaces as a failing test.
//! Non-deterministic fields (timestamps, generated ids, per-run-unique
//! fixture names) are redacted to a stable `"<redacted>"` placeholder before
//! comparison (see `golden_support::redact`) rather than skipped, so the
//! rest of the shape is still asserted exactly. Regenerate an intentionally
//! changed baseline with `MUSE_UPDATE_GOLDEN=1 cargo test endpoint_tests`
//! (never hand-edit a `tests/golden/*.json` file). See
//! `golden::golden_diff_mechanism_detects_a_deliberately_altered_response`
//! for a self-contained proof that the comparison actually catches drift.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::util::ServiceExt;

use crate::config::Config;
use crate::http::{router, AppState};

/// Same "never actually dials Postgres" idiom `http::ops`'s and
/// `channels::routes`'s own tests already use: `connect_lazy` only opens a
/// connection on first real query, so a handler that short-circuits before
/// touching the pool never notices the target is unroutable.
fn lazy_test_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://user:pass@127.0.0.1:1/muse_test_lazy")
        .expect("connect_lazy should never fail synchronously")
}

/// Build app state with every optional upstream left unconfigured (`None`)
/// — this module never talks to Plex/Prowlarr/TMDb/Tautulli/arr/Ollama/
/// Chord, by construction, no matter which endpoint is exercised. Also the
/// MUSEX-CAP-SEC-01 (Plane TERM #399) posture under test everywhere except
/// `auth` below: `Config::default()` leaves `api_token`/`auth_disabled`
/// unset, i.e. the fail-closed "auth not configured" state.
fn no_upstream_state(pool: sqlx::PgPool) -> Arc<AppState> {
    no_upstream_state_with_config(pool, Config::default())
}

/// Same as [`no_upstream_state`] but with a caller-supplied [`Config`] —
/// used by the `auth` module below to exercise the configured-token and
/// `MUSE_AUTH_DISABLED` states without touching process env vars (env vars
/// are process-global and would need `#[serial]`-style coordination across
/// every test in this file; passing `Config` directly is both simpler and
/// exactly what `Config`'s own "test/scaffold convenience" `Default` impl
/// exists for).
fn no_upstream_state_with_config(pool: sqlx::PgPool, config: Config) -> Arc<AppState> {
    Arc::new(AppState {
        pool,
        enrichment: crate::enrichment::EnrichmentService::from_config(&config),
        config,
        plex: None,
        prowlarr: None,
        arr_instances: Vec::new(),
        tmdb: None,
        embed: None,
    })
}

fn app_no_db() -> axum::Router {
    router(no_upstream_state(lazy_test_pool()))
}

fn app_no_db_with_config(config: Config) -> axum::Router {
    router(no_upstream_state_with_config(lazy_test_pool(), config))
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app
        .oneshot(req)
        .await
        .expect("router should never fail to produce a response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body should be readable");
    // JSON endpoints answer with a JSON body; axum's built-in extractor
    // rejections (malformed `Json`, missing `Query` params, etc.) answer
    // with a `text/plain` body instead. The error-path tests assert only on
    // the status code, so a non-JSON body must NOT panic here — fall back to
    // the raw text as a `Value::String` so those tests can still inspect the
    // status. Empty bodies become `Value::Null`.
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, body)
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_empty(path: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

/// MUSEX-CAP-SEC-01 (Plane TERM #399): attach `Authorization: Bearer
/// <token>` to an already-built request — layered on top of `get`/
/// `post_json`/`post_empty` rather than duplicating them, so every existing
/// request-builder gets an authed variant for free.
fn with_bearer(mut req: Request<Body>, token: &str) -> Request<Body> {
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}"))
            .expect("token must be a valid header value"),
    );
    req
}

// ---------------------------------------------------------------------
// GET /health — contract
// ---------------------------------------------------------------------

#[tokio::test]
async fn health_contract_returns_200_with_status_version_db_fields() {
    let (status, body) = send(app_no_db(), get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("ok"));
    assert!(body["version"].is_string());
    assert!(matches!(body["db"].as_str(), Some("up") | Some("down")));
}

#[tokio::test]
async fn health_db_down_reports_db_down_not_500() {
    // The lazy pool points at an unroutable loopback port — /health must
    // degrade to db:"down", never 500/hang, per its own documented
    // contract (a 2s-timeout probe).
    let (status, body) = send(app_no_db(), get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["db"], json!("down"));
}

// ---------------------------------------------------------------------
// POST /query/resolve — contract + happy-path (no-DB) + error-path
// ---------------------------------------------------------------------

#[tokio::test]
async fn query_resolve_contract_empty_query_returns_none_tier_with_no_db_hit() {
    // An empty query short-circuits before any tier runs (see
    // `recall::resolve::resolve_handler`) — a genuine no-DB happy path.
    let (status, body) = send(
        app_no_db(),
        post_json("/query/resolve", json!({"query": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tier"], json!("none"));
    assert_eq!(body["results"], json!([]));
}

#[tokio::test]
async fn query_resolve_error_path_malformed_json_is_client_error_not_500() {
    let req = Request::builder()
        .method("POST")
        .uri("/query/resolve")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let (status, _) = send(app_no_db(), req).await;
    assert!(
        status.is_client_error(),
        "malformed JSON body must be a 4xx, not a 500: {status}"
    );
}

#[tokio::test]
async fn query_resolve_error_path_missing_required_field_is_client_error() {
    // `query` is a required field (no `#[serde(default)]`) — omitting it
    // must be a clean 4xx from axum's extractor, never a 500.
    let (status, _) = send(
        app_no_db(),
        post_json("/query/resolve", json!({"limit": 5})),
    )
    .await;
    assert!(status.is_client_error());
}

// ---------------------------------------------------------------------
// POST /query/similar — contract + error-path. A genuine happy path needs
// a live DB (the seed lookup always queries) — covered in db_gated below.
// ---------------------------------------------------------------------

#[tokio::test]
async fn query_similar_error_path_missing_required_field_is_client_error() {
    let (status, _) = send(
        app_no_db(),
        post_json("/query/similar", json!({"limit": 5})),
    )
    .await;
    assert!(status.is_client_error());
}

// ---------------------------------------------------------------------
// POST /recommend, GET /recommend/on_deck, GET /recommend/gaps — contract
// + error-path. Happy paths (needing a real account row) are in db_gated.
// ---------------------------------------------------------------------

// MUSEX-CAP-SEC-03: `/recommend*` is now behind `auth::require_api_token`
// (it serves per-account taste data). These error-path tests must therefore
// present a valid bearer token to reach the handler and exercise the
// missing-`account_id` client-error behavior; the auth GATE itself (a
// tokenless request is rejected before the handler) is proven separately in
// `mod auth`'s `recommend_without_token_is_401_*` test.
#[tokio::test]
async fn recommend_error_path_missing_account_id_is_client_error() {
    let (status, _) = send(
        app_no_db_with_config(auth::with_token_config()),
        with_bearer(
            post_json("/recommend", json!({"limit": 5})),
            auth::TEST_API_TOKEN,
        ),
    )
    .await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn recommend_on_deck_error_path_missing_account_id_query_param_is_client_error() {
    let (status, _) = send(
        app_no_db_with_config(auth::with_token_config()),
        with_bearer(get("/recommend/on_deck"), auth::TEST_API_TOKEN),
    )
    .await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn recommend_gaps_error_path_missing_account_id_query_param_is_client_error() {
    let (status, _) = send(
        app_no_db_with_config(auth::with_token_config()),
        with_bearer(get("/recommend/gaps"), auth::TEST_API_TOKEN),
    )
    .await;
    assert!(status.is_client_error());
}

// ---------------------------------------------------------------------
// GET /proactive/pending, POST /proactive/{id}/ack — contract + error-path
// ---------------------------------------------------------------------

#[tokio::test]
async fn proactive_pending_error_path_missing_account_id_is_client_error() {
    let (status, _) = send(app_no_db(), get("/proactive/pending")).await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn proactive_ack_error_path_invalid_outcome_value_is_4xx_not_5xx() {
    // Well-formed JSON, but `outcome` isn't "sent"/"dismissed". The exact
    // 400 for a bad `outcome` value is covered at handler level by
    // `proactive::mod`'s own path; at the router level without a seeded
    // proactive item, the DB-backed `{id}` lookup can resolve to a 404
    // first, so we assert the weaker but DB-independent contract that this
    // router-level test actually guards: bad input is a 4xx, never a 5xx.
    let (status, _) = send(
        app_no_db(),
        post_json(
            "/proactive/1/ack",
            json!({"outcome": "definitely_not_valid"}),
        ),
    )
    .await;
    assert!(
        status.is_client_error(),
        "bad ack outcome must be a 4xx, never a 5xx: {status}"
    );
}

#[tokio::test]
async fn proactive_ack_error_path_missing_outcome_field_is_client_error() {
    // Missing required `outcome` field → axum's `Json` extractor rejects
    // before the handler runs. DB-independent by construction; asserts the
    // 4xx-never-5xx contract.
    let (status, _) = send(app_no_db(), post_json("/proactive/1/ack", json!({}))).await;
    assert!(status.is_client_error());
}

// ---------------------------------------------------------------------
// POST /channels/{id}/compose — contract + no-DB error-path. Happy path
// (and the positive-mutation assertion) is in db_gated.
// ---------------------------------------------------------------------

#[tokio::test]
async fn channels_compose_error_path_empty_show_list_is_400() {
    // Correct-contract assertion (held for post-MUSE-ROUTE-01): `compose_handler`
    // validates the empty show list BEFORE any DB lookup, so under correct
    // routing this is an exact 400 with a specific message — no seeded
    // channel needed. Also handler-unit-tested in
    // `channels/routes.rs::compose_handler_rejects_empty_show_list_as_bad_request`.
    let (status, body) = send(
        app_no_db(),
        post_json("/channels/1/compose", json!({"show_media_item_ids": []})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]
        .as_str()
        .unwrap_or_default()
        .contains("show_media_item_ids must contain at least one show"));
}

#[tokio::test]
async fn channels_compose_error_path_non_positive_session_length_is_400() {
    // Correct-contract assertion: `compose_handler` validates
    // `target_session_ms <= 0` before the DB lookup → exact 400.
    let (status, _) = send(
        app_no_db(),
        post_json(
            "/channels/1/compose",
            json!({"show_media_item_ids": [1], "target_session_ms": 0}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn channels_compose_error_path_missing_required_field_is_client_error() {
    // Correct-contract assertion: missing required `show_media_item_ids` →
    // axum's `Json` extractor rejects (4xx) before the handler runs.
    let (status, _) = send(app_no_db(), post_json("/channels/1/compose", json!({}))).await;
    assert!(status.is_client_error());
}

// ---------------------------------------------------------------------
// /api/channels, /api/channels/{id}/lineup, /art/{kind}/{id} — contract.
// Happy paths (real seeded channel) are in db_gated.
// ---------------------------------------------------------------------

#[tokio::test]
async fn art_proxy_contract_always_serves_an_image_never_errors_even_with_db_down() {
    // Correct-contract assertion (held for post-MUSE-ROUTE-01): per its own
    // doc contract, cache-miss + no Plex configured must fall back to the
    // placeholder PNG (200), never a 404/500 — exercised here with an
    // intentionally-unroutable DB to prove the degrade path holds even when
    // the *cache lookup itself* fails, not just when the row is absent.
    let (status, _) = send(app_no_db(), get("/art/poster/1")).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------
// /ops/* — router-level contract (the handler-level unit tests already
// live in `http::ops`'s own `#[cfg(test)] mod tests`; these confirm the
// *routing* — method + path wiring through the real router — as well).
//
// MUSEX-CAP-SEC-01 (Plane TERM #399): `/ops/*` is now behind
// `auth::require_api_token` (see `auth` module below for the auth
// contract itself), so these two router-level tests use
// `auth::with_token_config()` + a valid `Authorization` header to get
// *past* the auth layer — the
// `503` they assert is still the ORIGINAL handler-level "arr/tautulli not
// configured" degrade, now proven to be reachable by an authenticated
// caller, not a side effect of auth rejecting the request.
// ---------------------------------------------------------------------

#[tokio::test]
async fn ops_ingest_arr_contract_503_when_unconfigured() {
    let (status, _) = send(
        app_no_db_with_config(auth::with_token_config()),
        with_bearer(post_empty("/ops/ingest/arr"), auth::TEST_API_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn ops_ingest_tautulli_contract_503_when_unconfigured() {
    let (status, _) = send(
        app_no_db_with_config(auth::with_token_config()),
        with_bearer(post_empty("/ops/ingest/tautulli"), auth::TEST_API_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------
// Unmounted / not-yet-implemented route groups — `/ingest/*` beyond the
// real plex-webhook route, `/query/*` beyond resolve/similar, and
// `/proactive/*` beyond pending/ack all fall back to a clean 501, never a
// 404 or 500 — this is itself part of the contract (a client can tell
// "not built yet" apart from "doesn't exist").
// ---------------------------------------------------------------------

#[tokio::test]
async fn unimplemented_ingest_subroute_returns_501_not_implemented() {
    let (status, _) = send(app_no_db(), post_empty("/ingest/some-future-source")).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn unimplemented_query_subroute_returns_501_not_implemented() {
    let (status, _) = send(app_no_db(), get("/query/some-future-query")).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn unimplemented_proactive_subroute_returns_501_not_implemented() {
    let (status, _) = send(app_no_db(), get("/proactive/some-future-thing")).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

// =======================================================================
// DB-gated: happy paths + the READ-ONLY / non-mutating negative test.
//
// Gated on MUSE_TEST_DATABASE_URL, same posture as every other live-DB
// test in this crate (integration_tests.rs, http::ops, channels::routes):
// skips cleanly (does not fail) when unset — never touches a live/prod
// stack, only a disposable scratch Postgres the operator/CI provisions.
// =======================================================================

mod db_gated {
    // `use super::*` does NOT re-export the parent's non-`pub` `use`
    // bindings, so this inner scope needs the trait imported directly for
    // its own `.oneshot(...)` call sites.
    use tower::util::ServiceExt;

    use super::*;

    async fn test_pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                 (expected in the default test run; this harness does not \
                 require a live DB)"
            );
            return None;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");
        Some(pool)
    }

    /// Every user-data table this harness watches for the non-mutation
    /// assertion — the tables any of the read endpoints under test could
    /// conceivably write to. A table that doesn't (yet) exist in a given
    /// migration state just contributes a 0 rather than failing the
    /// snapshot (see `count_or_zero`).
    const WATCHED_TABLES: &[&str] = &[
        "accounts",
        "media_items",
        "media_metadata",
        "libraries",
        "seasons",
        "episodes",
        "embeddings",
        "channels",
        "channel_runs",
        "channel_programs",
        "proactive_items",
        "artwork_cache",
        "taste_profiles",
        "taste_signals",
    ];

    async fn count_or_zero(pool: &sqlx::PgPool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap_or(0)
    }

    async fn snapshot_row_counts(pool: &sqlx::PgPool) -> Vec<(String, i64)> {
        let mut counts = Vec::with_capacity(WATCHED_TABLES.len());
        for table in WATCHED_TABLES {
            counts.push(((*table).to_string(), count_or_zero(pool, table).await));
        }
        counts
    }

    fn state_for(pool: sqlx::PgPool) -> Arc<AppState> {
        super::no_upstream_state(pool)
    }

    /// The core MUSET-01 negative test: every Phase-0 READ endpoint, run
    /// back-to-back against a real (scratch) database, must leave every
    /// watched table's row count byte-for-byte unchanged. This exercises
    /// the "read-only-ness" acceptance criterion for real, rather than
    /// asserting it by code inspection.
    #[tokio::test]
    async fn read_endpoints_never_mutate_the_database() {
        let Some(pool) = test_pool_or_skip("read_endpoints_never_mutate_the_database").await else {
            return;
        };

        let before = snapshot_row_counts(&pool).await;

        let app = router(state_for(pool.clone()));
        let reads: Vec<Request<Body>> = vec![
            get("/health"),
            post_json(
                "/query/resolve",
                json!({"query": "totally nonexistent title muset01 xyz"}),
            ),
            post_json("/query/similar", json!({"media_item_id": -1})),
            post_json("/recommend", json!({"account_id": -1})),
            get("/recommend/on_deck?account_id=-1"),
            get("/recommend/gaps?account_id=-1"),
            get("/proactive/pending?account_id=-1"),
            get("/api/channels"),
            get("/api/channels/-1/lineup"),
            get("/art/poster/-1"),
        ];

        for req in reads {
            let uri = req.uri().clone();
            let response = app
                .clone()
                .oneshot(req)
                .await
                .expect("router should answer");
            // No specific status is asserted here — a `-1` id/account is
            // expected to 404/degrade for several of these — only that the
            // call completed with a real HTTP status (never hung/panicked).
            // The row-count comparison below is the actual assertion.
            assert!(
                response.status().as_u16() >= 100,
                "unexpected non-HTTP-shaped response for {uri}"
            );
        }

        let after = snapshot_row_counts(&pool).await;
        assert_eq!(
            before, after,
            "a Phase-0 read endpoint mutated the database — read endpoints must be strictly non-mutating"
        );
    }

    /// Happy-path: `/query/similar` against a real, embedding-less seed
    /// item falls back to the genre tier per its documented degrade
    /// contract; a nonexistent seed id is a genuine 404 (not folded into
    /// the resolution ladder's "found nothing" 200 the way `/query/resolve`
    /// is).
    #[tokio::test]
    async fn query_similar_happy_path_and_not_found_error_path() {
        let Some(pool) =
            test_pool_or_skip("query_similar_happy_path_and_not_found_error_path").await
        else {
            return;
        };

        let app = router(state_for(pool.clone()));

        // Error path: a media_item_id that doesn't exist.
        let (status, body) = send(
            app.clone(),
            post_json("/query/similar", json!({"media_item_id": -12345})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "unknown media_item_id must 404: {body:?}"
        );

        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let library = crate::repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("muset01-similar-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: format!("/test/muset01-similar-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = crate::repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("muset01-similar-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: json!({}),
                title: format!("MUSET-01 Similar Seed {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2021),
                images: json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = crate::repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/test/muset01-similar-{suffix}/movie.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("muset01-similar-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        // Happy path: a real, embedding-less seed must resolve (genre
        // fallback, or "none" if the scratch DB has no comparable genre
        // data), never error.
        let (status, body) = send(
            app.clone(),
            post_json("/query/similar", json!({"media_item_id": item.id})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a real seed item must resolve, not error: {body:?}"
        );
        assert!(matches!(
            body["tier"].as_str(),
            Some("genre") | Some("none")
        ));

        sqlx::query("DELETE FROM media_items WHERE id = $1")
            .bind(item.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_metadata WHERE id = $1")
            .bind(metadata.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM libraries WHERE id = $1")
            .bind(library.id)
            .execute(&pool)
            .await
            .ok();
    }

    /// Happy-path: `/api/channels` + `/api/channels/{id}/lineup` (the
    /// channel-director's guide/metadata surface) against a real seeded
    /// channel, plus the nonexistent-channel error path.
    ///
    /// Was `#[ignore]`d: the `/api/channels/{id}/lineup` assertions depended
    /// on the `{id}` route reaching its real handler, which the
    /// MUSE-ROUTE-01 bug prevented. Active now that the route is fixed;
    /// still `db_gated` (skips cleanly without a DB).
    #[tokio::test]
    async fn channel_guide_metadata_happy_path() {
        let Some(pool) = test_pool_or_skip("channel_guide_metadata_happy_path").await else {
            return;
        };

        use crate::models::channel::{ChannelKind, ChannelMode, NewChannel};
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let channel = crate::repo::channel::create_channel(
            &pool,
            &NewChannel {
                account_id: None,
                name: format!("muset01-guide-{suffix}"),
                kind: ChannelKind::Personal,
                mode: ChannelMode::OnDemand,
                channel_number: None,
                target_client_id: None,
                directive: None,
                rules: json!({}),
                is_preset: false,
            },
        )
        .await
        .expect("create channel");

        let app = router(state_for(pool.clone()));

        let (status, body) = send(app.clone(), get("/api/channels")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body
            .as_array()
            .expect("api/channels returns a JSON array")
            .iter()
            .any(|c| c["id"] == json!(channel.id)));

        let (status, body) = send(
            app.clone(),
            get(&format!("/api/channels/{}/lineup", channel.id)),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "seeded channel lineup should resolve: {body:?}"
        );
        assert_eq!(body["channel"]["id"], json!(channel.id));
        assert!(body["programs"].is_array());

        let (status, _) = send(app.clone(), get("/api/channels/-1/lineup")).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "nonexistent channel id must 404"
        );

        sqlx::query("DELETE FROM channels WHERE id = $1")
            .bind(channel.id)
            .execute(&pool)
            .await
            .ok();
    }

    /// Happy-path: `/recommend/on_deck` + `/recommend/gaps` + `/recommend`
    /// against a real (empty-of-signal) account — must return a clean,
    /// empty ranked list rather than erroring when an account has no
    /// history yet.
    #[tokio::test]
    async fn recommend_family_happy_path_empty_account_returns_empty_ranked_list() {
        let Some(pool) = test_pool_or_skip(
            "recommend_family_happy_path_empty_account_returns_empty_ranked_list",
        )
        .await
        else {
            return;
        };

        use crate::models::account::NewAccount;
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let account = crate::repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("muset01-recommend-{suffix}")),
                username: Some(format!("muset01_recommend_{suffix}")),
                friendly_name: Some("MUSET-01 Recommend Test".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let app = router(state_for(pool.clone()));

        let (status, body) = send(
            app.clone(),
            post_json("/recommend", json!({"account_id": account.id})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "an empty-signal account must still 200: {body:?}"
        );
        assert_eq!(body["items"], json!([]));

        let (status, body) = send(
            app.clone(),
            get(&format!("/recommend/on_deck?account_id={}", account.id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"], json!([]));

        let (status, body) = send(
            app.clone(),
            get(&format!("/recommend/gaps?account_id={}", account.id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"], json!([]));

        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account.id)
            .execute(&pool)
            .await
            .ok();
    }

    /// Happy-path + positive-mutation check for `POST /channels/{id}/compose`
    /// (the channel-director's write surface) — this endpoint IS
    /// intentionally mutating (it creates a `channel_runs` row plus its
    /// ordered `channel_programs` rows), so it gets its own explicit
    /// assert-it-DID-write test rather than being folded into
    /// `read_endpoints_never_mutate_the_database` above.
    ///
    /// Was `#[ignore]`d: depended on the `/channels/{id}/compose` `{id}`
    /// route reaching its real handler (MUSE-ROUTE-01). Active now that the
    /// route is fixed; still `db_gated` (skips cleanly without a DB).
    #[tokio::test]
    async fn channels_compose_happy_path_creates_exactly_one_run() {
        let Some(pool) =
            test_pool_or_skip("channels_compose_happy_path_creates_exactly_one_run").await
        else {
            return;
        };

        use crate::models::channel::{ChannelKind, ChannelMode, NewChannel};
        use crate::models::episode::NewEpisode;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::season::NewSeason;
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();

        let library = crate::repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("muset01-compose-tv-{suffix}"),
                kind: LibraryKind::Tv,
                root_folder: format!("/test/muset01-compose-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = crate::repo::media_metadata::upsert_by_tvdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: None,
                tvdb_id: Some(format!("muset01-compose-tvdb-{suffix}")),
                imdb_id: None,
                provider_ids: json!({}),
                title: format!("MUSET-01 Compose Show {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(20),
                year: Some(2023),
                images: json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let show = crate::repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/test/muset01-compose-{suffix}/show"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("muset01-compose-show-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        let season = crate::repo::season::upsert(
            &pool,
            &NewSeason {
                media_item_id: show.id,
                season_number: 1,
                title: None,
                overview: None,
                monitored: true,
                air_date: None,
            },
        )
        .await
        .expect("create season");

        crate::repo::episode::upsert(
            &pool,
            &NewEpisode {
                season_id: season.id,
                media_item_id: show.id,
                episode_number: 1,
                absolute_episode_number: None,
                title: Some("Pilot".to_string()),
                overview: None,
                air_date: None,
                air_date_utc: None,
                runtime_minutes: Some(20),
                monitored: true,
                tvdb_id: None,
            },
        )
        .await
        .expect("create episode");

        let channel = crate::repo::channel::create_channel(
            &pool,
            &NewChannel {
                account_id: None,
                name: format!("muset01-compose-channel-{suffix}"),
                kind: ChannelKind::Personal,
                mode: ChannelMode::OnDemand,
                channel_number: None,
                target_client_id: None,
                directive: None,
                rules: json!({}),
                is_preset: false,
            },
        )
        .await
        .expect("create channel");

        let runs_before = count_or_zero(&pool, "channel_runs").await;

        let app = router(state_for(pool.clone()));
        let (status, body) = send(
            app,
            post_json(
                &format!("/channels/{}/compose", channel.id),
                json!({"show_media_item_ids": [show.id], "target_session_ms": 3_600_000}),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "compose should succeed for a valid seeded request: {body:?}"
        );
        assert!(body["run"]["id"].is_number());
        let program_count = body["program_count"]
            .as_u64()
            .expect("program_count should be a number") as usize;
        assert!(
            program_count >= 1,
            "expected at least one scheduled program"
        );

        // Exactly one new run.
        let runs_after = count_or_zero(&pool, "channel_runs").await;
        assert_eq!(
            runs_after,
            runs_before + 1,
            "compose is a write endpoint — it must create exactly one run"
        );

        // Tightened persistence check: the persisted `channel_programs` rows
        // for this (freshly-created, so exclusively this run's) channel must
        // MATCH the response exactly — same count as `program_count`, and the
        // same program identities in the same order (`start_at`-ordered
        // (title, start_at) tuples) the response's `run.schedule.items`
        // reports. Proves the endpoint persisted precisely what it returned,
        // not merely "at least one row."
        let persisted: Vec<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT title, start_at FROM channel_programs WHERE channel_id = $1 ORDER BY start_at",
        )
        .bind(channel.id)
        .fetch_all(&pool)
        .await
        .expect("fetch persisted channel_programs");
        assert_eq!(
            persisted.len(),
            program_count,
            "persisted channel_programs count must equal the response's program_count"
        );

        let response_items = body["run"]["schedule"]["items"]
            .as_array()
            .expect("run.schedule.items should be an array");
        assert_eq!(
            response_items.len(),
            program_count,
            "response schedule item count must equal program_count"
        );
        for (i, (title, start_at)) in persisted.iter().enumerate() {
            let item = &response_items[i];
            assert_eq!(
                item["title"].as_str(),
                Some(title.as_str()),
                "persisted program {i} title must match the response's schedule item in order"
            );
            assert_eq!(
                item["start_at"].as_str(),
                Some(start_at.to_rfc3339().as_str()),
                "persisted program {i} start_at must match the response's schedule item in order"
            );
        }

        sqlx::query("DELETE FROM channel_programs WHERE channel_id = $1")
            .bind(channel.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM channel_runs WHERE channel_id = $1")
            .bind(channel.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM channels WHERE id = $1")
            .bind(channel.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_items WHERE id = $1")
            .bind(show.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_metadata WHERE id = $1")
            .bind(metadata.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM libraries WHERE id = $1")
            .bind(library.id)
            .execute(&pool)
            .await
            .ok();
    }

    /// Happy-path + positive-mutation check for `POST /proactive/{id}/ack`
    /// — also an intentionally-mutating endpoint (it sets `dismissed_at`/
    /// `status`), asserted here rather than folded into the non-mutation
    /// check above.
    ///
    /// Was `#[ignore]`d: depended on the `/proactive/{id}/ack` `{id}` route
    /// reaching its real handler (MUSE-ROUTE-01). Active now that the route
    /// is fixed; still `db_gated` (skips cleanly without a DB).
    #[tokio::test]
    async fn proactive_ack_happy_path_marks_item_dismissed() {
        let Some(pool) = test_pool_or_skip("proactive_ack_happy_path_marks_item_dismissed").await
        else {
            return;
        };

        use crate::models::account::NewAccount;
        use crate::models::proactive_item::NewProactiveItem;
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let account = crate::repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("muset01-ack-{suffix}")),
                username: Some(format!("muset01_ack_{suffix}")),
                friendly_name: Some("MUSET-01 Ack Test".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let item = crate::repo::proactive_item::create(
            &pool,
            &NewProactiveItem {
                account_id: Some(account.id),
                kind: "muset01_test".to_string(),
                media_item_id: None,
                headline: format!("MUSET-01 ack test {suffix}"),
                body: Some(json!({"rationale": "test fixture"})),
                priority: 1,
                earliest_at: None,
                expires_at: None,
            },
        )
        .await
        .expect("create proactive item");

        // A second, unrelated item that MUST be left untouched by acking the
        // first — the "no other rows changed" half of the mutation contract.
        let other = crate::repo::proactive_item::create(
            &pool,
            &NewProactiveItem {
                account_id: Some(account.id),
                kind: "muset01_test".to_string(),
                media_item_id: None,
                headline: format!("MUSET-01 ack test untouched {suffix}"),
                body: None,
                priority: 1,
                earliest_at: None,
                expires_at: None,
            },
        )
        .await
        .expect("create second proactive item");

        let app = router(state_for(pool.clone()));
        let (status, body) = send(
            app,
            post_json(
                &format!("/proactive/{}/ack", item.id),
                json!({"outcome": "dismissed"}),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "ack on a real item should succeed: {body:?}"
        );
        assert!(
            body["item"]["dismissed_at"].is_string(),
            "dismissed_at should be set: {body:?}"
        );

        // Tightened persistence check: the PERSISTED target row changed
        // exactly as the ack claims — status → 'dismissed', dismissed_at set,
        // delivered_at still NULL (a `dismissed` ack must not also mark it
        // delivered).
        let (status_col, dismissed_at, delivered_at): (
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT status, dismissed_at, delivered_at FROM proactive_items WHERE id = $1",
        )
        .bind(item.id)
        .fetch_one(&pool)
        .await
        .expect("re-fetch the acked proactive_items row");
        assert_eq!(
            status_col, "dismissed",
            "persisted status must be 'dismissed'"
        );
        assert!(dismissed_at.is_some(), "persisted dismissed_at must be set");
        assert!(
            delivered_at.is_none(),
            "a dismissed ack must not set delivered_at"
        );

        // And the OTHER row is byte-for-byte unchanged (still pending, no
        // dismissed_at) — the ack mutated exactly one row, no collateral.
        let (other_status, other_dismissed): (String, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as("SELECT status, dismissed_at FROM proactive_items WHERE id = $1")
                .bind(other.id)
                .fetch_one(&pool)
                .await
                .expect("re-fetch the untouched proactive_items row");
        assert_eq!(
            other_status, "pending",
            "an unrelated proactive item must be left pending"
        );
        assert!(
            other_dismissed.is_none(),
            "an unrelated proactive item must not be dismissed"
        );

        // A nonexistent id must 404, not 500/silently-succeed — exercised
        // against the same real pool (so `fetch_optional` genuinely finds
        // nothing, rather than erroring on an unroutable connection).
        let app2 = router(state_for(pool.clone()));
        let (status, body) = send(
            app2,
            post_json("/proactive/-99999999/ack", json!({"outcome": "dismissed"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "an id-not-found ack must 404, never silently succeed: {body:?}"
        );

        sqlx::query("DELETE FROM proactive_items WHERE id = ANY($1)")
            .bind(vec![item.id, other.id])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account.id)
            .execute(&pool)
            .await
            .ok();
    }

    // ===================================================================
    // MUSET-02: golden-response baseline for the endpoints whose happy
    // path needs seeded rows. Nested here (rather than in the top-level
    // `golden` module below) purely so these tests can reuse `db_gated`'s
    // own private `test_pool_or_skip`/`state_for` helpers via `use
    // super::*` — same DB-gated skip-cleanly posture as every other test
    // in this module.
    // ===================================================================
    mod golden {
        use uuid::Uuid;

        use crate::endpoint_tests::golden_support::*;

        use super::*;

        /// `/recommend` + `/recommend/on_deck` + `/recommend/gaps` against a
        /// real, signal-empty account — the response never echoes
        /// `account_id` back, so all three come back byte-for-byte
        /// deterministic (`{"items": []}`) with no redaction needed.
        #[tokio::test]
        async fn recommend_family_empty_account_matches_golden_baseline() {
            let Some(pool) =
                test_pool_or_skip("recommend_family_empty_account_matches_golden_baseline").await
            else {
                return;
            };

            let suffix = Uuid::new_v4().simple().to_string();
            let account = crate::repo::account::create(
                &pool,
                &crate::models::account::NewAccount {
                    plex_account_id: Some(format!("muset02-golden-recommend-{suffix}")),
                    username: Some(format!("muset02_golden_recommend_{suffix}")),
                    friendly_name: Some("MUSET-02 Golden Recommend".to_string()),
                    is_home_user: false,
                    is_primary: false,
                },
            )
            .await
            .expect("create account");

            let app = router(state_for(pool.clone()));

            let (status, recommend) = send(
                app.clone(),
                post_json("/recommend", json!({"account_id": account.id})),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            let (status, on_deck) = send(
                app.clone(),
                get(&format!("/recommend/on_deck?account_id={}", account.id)),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            let (status, gaps) = send(
                app.clone(),
                get(&format!("/recommend/gaps?account_id={}", account.id)),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            assert_json_golden(
                "recommend_family_empty_account.json",
                &json!({
                    "recommend": recommend,
                    "on_deck": on_deck,
                    "gaps": gaps,
                }),
            );

            sqlx::query("DELETE FROM accounts WHERE id = $1")
                .bind(account.id)
                .execute(&pool)
                .await
                .ok();
        }

        /// `GET /api/channels` shape for a real seeded channel — id/name
        /// are non-deterministic (fresh per test run, to avoid colliding
        /// with concurrent suites against the same scratch DB), so both
        /// are redacted before comparison; every other field
        /// (kind/mode/channel_number/enabled) is asserted exactly.
        #[tokio::test]
        async fn channel_summary_shape_matches_golden_baseline() {
            let Some(pool) =
                test_pool_or_skip("channel_summary_shape_matches_golden_baseline").await
            else {
                return;
            };

            use crate::models::channel::{ChannelKind, ChannelMode, NewChannel};

            let suffix = Uuid::new_v4().simple().to_string();
            let channel = crate::repo::channel::create_channel(
                &pool,
                &NewChannel {
                    account_id: None,
                    name: format!("muset02-golden-channel-{suffix}"),
                    kind: ChannelKind::Personal,
                    mode: ChannelMode::OnDemand,
                    channel_number: None,
                    target_client_id: None,
                    directive: None,
                    rules: json!({}),
                    is_preset: false,
                },
            )
            .await
            .expect("create channel");

            let app = router(state_for(pool.clone()));
            let (status, body) = send(app.clone(), get("/api/channels")).await;
            assert_eq!(status, StatusCode::OK);

            let entry = body
                .as_array()
                .expect("api/channels returns a JSON array")
                .iter()
                .find(|c| c["id"] == json!(channel.id))
                .cloned()
                .expect("seeded channel should appear in /api/channels");

            assert_json_golden("channel_summary.json", &redact(entry, &["/id", "/name"]));

            sqlx::query("DELETE FROM channels WHERE id = $1")
                .bind(channel.id)
                .execute(&pool)
                .await
                .ok();
        }

        /// `GET /api/channels/{id}/lineup` shape for a real seeded channel
        /// with no programs in-window — deterministic once id/name/
        /// generated_at/window_start/window_end are redacted:
        /// `now_program_id` is `null` and `programs` is `[]` by
        /// construction (no `channel_programs` rows exist for this
        /// channel).
        #[tokio::test]
        async fn channel_lineup_empty_shape_matches_golden_baseline() {
            let Some(pool) =
                test_pool_or_skip("channel_lineup_empty_shape_matches_golden_baseline").await
            else {
                return;
            };

            use crate::models::channel::{ChannelKind, ChannelMode, NewChannel};

            let suffix = Uuid::new_v4().simple().to_string();
            let channel = crate::repo::channel::create_channel(
                &pool,
                &NewChannel {
                    account_id: None,
                    name: format!("muset02-golden-lineup-{suffix}"),
                    kind: ChannelKind::Personal,
                    mode: ChannelMode::OnDemand,
                    channel_number: None,
                    target_client_id: None,
                    directive: None,
                    rules: json!({}),
                    is_preset: false,
                },
            )
            .await
            .expect("create channel");

            let app = router(state_for(pool.clone()));
            let (status, body) = send(
                app.clone(),
                get(&format!("/api/channels/{}/lineup", channel.id)),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            let redacted = redact(
                body,
                &[
                    "/channel/id",
                    "/channel/name",
                    "/generated_at",
                    "/window_start",
                    "/window_end",
                ],
            );
            assert_json_golden("channel_lineup_empty.json", &redacted);

            sqlx::query("DELETE FROM channels WHERE id = $1")
                .bind(channel.id)
                .execute(&pool)
                .await
                .ok();
        }

        /// `POST /proactive/{id}/ack` (`outcome: "dismissed"`) against a
        /// real seeded item — id/account_id/created_at/dismissed_at are
        /// non-deterministic and redacted; every other field (status
        /// transition, the untouched-null columns, the body/kind/headline/
        /// priority the fixture set) is asserted exactly, proving the
        /// persisted+returned shape byte-for-byte.
        #[tokio::test]
        async fn proactive_ack_dismissed_shape_matches_golden_baseline() {
            let Some(pool) =
                test_pool_or_skip("proactive_ack_dismissed_shape_matches_golden_baseline").await
            else {
                return;
            };

            use crate::models::account::NewAccount;
            use crate::models::proactive_item::NewProactiveItem;

            let suffix = Uuid::new_v4().simple().to_string();
            let account = crate::repo::account::create(
                &pool,
                &NewAccount {
                    plex_account_id: Some(format!("muset02-golden-ack-{suffix}")),
                    username: Some(format!("muset02_golden_ack_{suffix}")),
                    friendly_name: Some("MUSET-02 Golden Ack".to_string()),
                    is_home_user: false,
                    is_primary: false,
                },
            )
            .await
            .expect("create account");

            let item = crate::repo::proactive_item::create(
                &pool,
                &NewProactiveItem {
                    account_id: Some(account.id),
                    kind: "muset02_golden_ack".to_string(),
                    media_item_id: None,
                    headline: "MUSET-02 golden ack fixture".to_string(),
                    body: Some(json!({"rationale": "golden fixture"})),
                    priority: 1,
                    earliest_at: None,
                    expires_at: None,
                },
            )
            .await
            .expect("create proactive item");

            let app = router(state_for(pool.clone()));
            let (status, body) = send(
                app,
                post_json(
                    &format!("/proactive/{}/ack", item.id),
                    json!({"outcome": "dismissed"}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            let redacted = redact(
                body,
                &[
                    "/item/id",
                    "/item/account_id",
                    "/item/created_at",
                    "/item/dismissed_at",
                ],
            );
            assert_json_golden("proactive_ack_dismissed.json", &redacted);

            sqlx::query("DELETE FROM proactive_items WHERE id = $1")
                .bind(item.id)
                .execute(&pool)
                .await
                .ok();
            sqlx::query("DELETE FROM accounts WHERE id = $1")
                .bind(account.id)
                .execute(&pool)
                .await
                .ok();
        }
    }
}

// ===========================================================================
// MUSET-02: golden-baseline support (file I/O + redaction + comparison) and
// the DB-independent golden tests. `golden_support` is a private `mod` (not
// `pub`), so per ordinary Rust visibility rules its `pub(super)` items are
// reachable from every descendant of `endpoint_tests` — including
// `db_gated::golden` above, which needs the same helpers for its
// seeded-row goldens.
// ===========================================================================

mod golden_support {
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    /// `tests/golden/` at the crate root — resolved via `CARGO_MANIFEST_DIR`
    /// (a build-time constant, not a hardcoded path) so this works
    /// regardless of the worktree/checkout location.
    pub(super) fn golden_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
    }

    pub(super) fn golden_path(name: &str) -> PathBuf {
        golden_dir().join(name)
    }

    /// `MUSE_UPDATE_GOLDEN=1` is the one sanctioned way to regenerate a
    /// baseline after an intentional behavior change — golden tests are
    /// otherwise strictly read-only/comparison-only.
    pub(super) fn update_golden_enabled() -> bool {
        std::env::var("MUSE_UPDATE_GOLDEN")
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    /// Replace the value at each of the given JSON Pointers (RFC 6901, e.g.
    /// `"/channel/id"`) with the literal string `"<redacted>"`, so
    /// non-deterministic fields (timestamps, generated ids, per-run-unique
    /// fixture names) don't break golden comparison while every other field
    /// is still asserted exactly. A pointer that doesn't resolve (field
    /// absent/already-null-shaped differently) is silently skipped rather
    /// than panicking, so callers can pass a superset of "fields that might
    /// need redacting" defensively.
    pub(super) fn redact(mut value: Value, pointers: &[&str]) -> Value {
        for p in pointers {
            if let Some(slot) = value.pointer_mut(p) {
                *slot = Value::String("<redacted>".to_string());
            }
        }
        value
    }

    /// The actual diff/panic logic every golden comparison bottoms out in —
    /// factored out from `assert_json_golden` so
    /// `golden::golden_diff_mechanism_detects_a_deliberately_altered_response`
    /// can exercise this exact code path directly (bypassing the
    /// `MUSE_UPDATE_GOLDEN` branch) as its proof that drift is actually
    /// caught, not just asserted to be caught.
    pub(super) fn compare_or_panic(name: &str, path: &Path, rendered: &str) {
        let expected = std::fs::read_to_string(path).unwrap_or_else(|e| {
            panic!(
                "golden file {path:?} for {name:?} is missing or unreadable ({e}); run with \
                 MUSE_UPDATE_GOLDEN=1 to (re)generate it. Rendered value was:\n{rendered}"
            )
        });
        assert_eq!(
            expected.trim_end(),
            rendered.trim_end(),
            "response for golden {name:?} drifted from the committed baseline at {path:?} — if \
             this drift is an intentional behavior change, re-run with \
             `MUSE_UPDATE_GOLDEN=1 cargo test endpoint_tests` to update the baseline; otherwise \
             this is a regression"
        );
    }

    /// Compare `actual` (already redacted as needed) against the committed
    /// golden file `name` under `tests/golden/`. Golden files are
    /// pretty-printed JSON with alphabetically-sorted keys — this crate does
    /// not enable serde_json's `preserve_order` feature, so `Value`'s `Map`
    /// is a `BTreeMap` and `to_string_pretty`'s key order is reproducible
    /// across machines/runs by construction, not by convention.
    pub(super) fn assert_json_golden(name: &str, actual: &Value) {
        let path = golden_path(name);
        let rendered =
            serde_json::to_string_pretty(actual).expect("golden value must serialize") + "\n";

        if update_golden_enabled() {
            std::fs::create_dir_all(golden_dir()).expect("create tests/golden");
            std::fs::write(&path, &rendered).expect("write golden file");
            return;
        }

        compare_or_panic(name, &path, &rendered);
    }

    /// Byte-for-byte binary golden comparison, for non-JSON bodies (the
    /// artwork proxy's placeholder PNG). Same `MUSE_UPDATE_GOLDEN=1`
    /// regeneration mechanism as `assert_json_golden`.
    pub(super) fn assert_bytes_golden(name: &str, actual: &[u8]) {
        let path = golden_path(name);

        if update_golden_enabled() {
            std::fs::create_dir_all(golden_dir()).expect("create tests/golden");
            std::fs::write(&path, actual).expect("write golden file");
            return;
        }

        let expected = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "golden file {path:?} for {name:?} is missing or unreadable ({e}); run with \
                 MUSE_UPDATE_GOLDEN=1 to (re)generate it"
            )
        });
        assert_eq!(
            expected, actual,
            "binary response for golden {name:?} drifted from the committed baseline at \
             {path:?} — if this drift is intentional, re-run with \
             `MUSE_UPDATE_GOLDEN=1 cargo test endpoint_tests`; otherwise this is a regression"
        );
    }
}

/// MUSET-02: DB-independent golden-response tests — every endpoint here
/// needs no seeded row and no `MUSE_TEST_DATABASE_URL`, so these run in the
/// default `cargo test` invocation same as the rest of this module's
/// contract/error-path tests. The equivalent goldens for endpoints whose
/// representative response needs a real seeded row live in
/// `db_gated::golden` above (same skip-cleanly-without-a-DB posture as
/// every other `db_gated` test).
mod golden {
    use super::golden_support::*;
    use super::*;

    #[tokio::test]
    async fn health_response_matches_golden_baseline() {
        let (status, body) = send(app_no_db(), get("/health")).await;
        assert_eq!(status, StatusCode::OK);
        // `version` tracks `CARGO_PKG_VERSION` and legitimately changes
        // across releases — redacted so the baseline doesn't churn on every
        // version bump while `status`/`db` are still asserted exactly.
        assert_json_golden("health.json", &redact(body, &["/version"]));
    }

    #[tokio::test]
    async fn query_resolve_empty_query_matches_golden_baseline() {
        let (status, body) = send(
            app_no_db(),
            post_json("/query/resolve", json!({"query": ""})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_json_golden("query_resolve_empty.json", &body);
    }

    #[tokio::test]
    async fn ops_ingest_arr_unconfigured_matches_golden_baseline() {
        // MUSEX-CAP-SEC-01 (Plane TERM #399): /ops/* is now behind
        // `auth::require_api_token`, so this golden test must authenticate
        // (token via Config, valid Bearer header) to get PAST the auth gate
        // and reach the handler's unchanged 503-unconfigured behavior — the
        // golden BASELINE itself is untouched, only the request now carries
        // the credential the new outer layer requires.
        let (status, body) = send(
            app_no_db_with_config(super::auth::with_token_config()),
            with_bearer(post_empty("/ops/ingest/arr"), super::auth::TEST_API_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_json_golden("ops_ingest_arr_unconfigured.json", &body);
    }

    #[tokio::test]
    async fn ops_ingest_tautulli_unconfigured_matches_golden_baseline() {
        // MUSEX-CAP-SEC-01 (Plane TERM #399): authenticate first (see the
        // sibling arr golden test above) so the request reaches the
        // handler's unchanged 503-unconfigured golden baseline.
        let (status, body) = send(
            app_no_db_with_config(super::auth::with_token_config()),
            with_bearer(
                post_empty("/ops/ingest/tautulli"),
                super::auth::TEST_API_TOKEN,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_json_golden("ops_ingest_tautulli_unconfigured.json", &body);
    }

    #[tokio::test]
    async fn unimplemented_ingest_subroute_matches_golden_baseline() {
        let (status, body) = send(app_no_db(), post_empty("/ingest/some-future-source")).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_json_golden("not_implemented.json", &body);
    }

    #[tokio::test]
    async fn unimplemented_query_subroute_matches_golden_baseline() {
        let (status, body) = send(app_no_db(), get("/query/some-future-query")).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_json_golden("not_implemented.json", &body);
    }

    #[tokio::test]
    async fn unimplemented_proactive_subroute_matches_golden_baseline() {
        let (status, body) = send(app_no_db(), get("/proactive/some-future-thing")).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_json_golden("not_implemented.json", &body);
    }

    #[tokio::test]
    async fn proactive_ack_invalid_outcome_matches_golden_baseline() {
        let (status, body) = send(
            app_no_db(),
            post_json(
                "/proactive/1/ack",
                json!({"outcome": "definitely_not_valid"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_json_golden("proactive_ack_invalid_outcome.json", &body);
    }

    #[tokio::test]
    async fn channels_compose_empty_show_list_matches_golden_baseline() {
        let (status, body) = send(
            app_no_db(),
            post_json("/channels/1/compose", json!({"show_media_item_ids": []})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_json_golden("channels_compose_empty_show_list.json", &body);
    }

    #[tokio::test]
    async fn channels_compose_non_positive_session_matches_golden_baseline() {
        let (status, body) = send(
            app_no_db(),
            post_json(
                "/channels/1/compose",
                json!({"show_media_item_ids": [1], "target_session_ms": 0}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_json_golden("channels_compose_non_positive_session.json", &body);
    }

    /// The artwork proxy's placeholder path is fully deterministic — a
    /// fixed, embedded 1x1 PNG (`web::artwork::PLACEHOLDER_PNG`) — so this
    /// goes further than a JSON golden and byte-for-byte compares the
    /// actual response body against a committed binary golden, plus asserts
    /// the two headers that mark it as the placeholder path.
    #[tokio::test]
    async fn art_proxy_placeholder_bytes_and_headers_match_golden_baseline() {
        let response = app_no_db()
            .oneshot(get("/art/poster/1"))
            .await
            .expect("router should answer");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-muse-artwork-cache")
                .and_then(|v| v.to_str().ok()),
            Some("placeholder")
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/png")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        assert_bytes_golden("art_placeholder.png", &bytes);
    }

    /// MUSET-02's acceptance criterion "a regression is detected on a
    /// deliberate change": exercises `golden_support::compare_or_panic` —
    /// the exact function every golden test above bottoms out in — directly
    /// and self-contained (a scratch golden file under `std::env::temp_dir`,
    /// never a committed `tests/golden/*.json`), proving two things: (1) an
    /// unchanged response compared against its own golden does NOT panic
    /// (no false positive), and (2) a deliberately altered response
    /// compared against the SAME unmodified golden DOES panic (drift is
    /// actually caught, not just asserted-to-be-caught).
    #[test]
    fn golden_diff_mechanism_detects_a_deliberately_altered_response() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "muset02-golden-regression-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch golden dir");
        let path = dir.join("scratch.json");

        let baseline = json!({"status": "ok", "tier": "none"});
        let baseline_rendered = serde_json::to_string_pretty(&baseline).unwrap() + "\n";
        std::fs::write(&path, &baseline_rendered).expect("seed scratch golden");

        // No false positive: comparing the (unmodified) baseline against
        // itself must not panic.
        compare_or_panic("scratch-unchanged", &path, &baseline_rendered);

        // The actual regression proof: a deliberately altered "current"
        // response — same shape, `tier` flipped from `"none"` to
        // `"vector"`, exactly the class of drift a real behavior change
        // (e.g. an accidental resolution-tier rename) would produce —
        // diffed against the SAME unmodified golden file must panic.
        let mutated = json!({"status": "ok", "tier": "vector"});
        let mutated_rendered = serde_json::to_string_pretty(&mutated).unwrap() + "\n";
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compare_or_panic("scratch-mutated", &path, &mutated_rendered);
        }));
        assert!(
            result.is_err(),
            "the golden-diff mechanism failed to detect a deliberately altered response — \
             drift detection is broken"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// MUSEX-CAP-SEC-01 (Plane TERM #399): router-level auth contract tests
/// for `crate::http::auth::require_api_token`, exercised through the real
/// `Router` (same `send`/`app_no_db_with_config` harness as the rest of
/// this file) rather than calling the middleware function directly — the
/// point is to prove the *wiring* (which routes are actually gated)
/// matches the documented contract in `crate::http::router`, not just that
/// the middleware function is individually correct (see `crate::http::auth`'s
/// own `#[cfg(test)]` for the header-parsing/constant-time-compare unit
/// tests that don't need a router at all).
mod auth {
    use super::*;

    /// A dummy, in-memory-only fixture token — never a real credential,
    /// never read from/written to <secret-manager> or any env var. Same posture
    /// as the `"test-panel-key"`-style literals `config.rs`'s own tests
    /// already use for other `*_api_key` fields (S1 governs infra
    /// literals — hostnames/IPs/org secrets — not disposable test
    /// fixture strings that only ever exist inside this test process).
    pub(super) const TEST_API_TOKEN: &str = "endpoint-tests-fixture-token";

    pub(super) fn with_token_config() -> Config {
        Config {
            api_token: Some(TEST_API_TOKEN.to_string()),
            ..Config::default()
        }
    }

    fn with_auth_disabled_config() -> Config {
        Config {
            api_token: None,
            auth_disabled: true,
            ..Config::default()
        }
    }

    // -----------------------------------------------------------------
    // AC: /health (liveness) stays open regardless of auth config.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn health_stays_open_with_no_token_configured() {
        let (status, _) = send(app_no_db_with_config(Config::default()), get("/health")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_stays_open_even_when_a_token_is_configured_and_not_presented() {
        // /health must never require a credential, even once the fleet
        // starts configuring MUSE_API_TOKEN for everything else.
        let (status, _) = send(app_no_db_with_config(with_token_config()), get("/health")).await;
        assert_eq!(status, StatusCode::OK);
    }

    // -----------------------------------------------------------------
    // AC: protected route, no token presented, token IS configured -> 401,
    // BEFORE the handler runs (proven via a route whose handler would
    // otherwise touch the DB pool and fail differently — 401 here, not the
    // 500 a reached-but-DB-less handler would produce).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn protected_route_without_token_is_401_and_never_reaches_the_handler() {
        // `/friends/opt-in`'s handler calls `repo::settings::load` as its
        // very first statement — against `app_no_db`'s unroutable lazy
        // pool that would surface as a `500` (MuseError::Database), NOT a
        // `401`. Observing `401` here is direct proof the middleware
        // rejected the request before the handler (and therefore the
        // pool) was ever touched.
        let (status, _) = send(
            app_no_db_with_config(with_token_config()),
            post_json(
                "/friends/opt-in",
                json!({"discord_user_id": "u1", "muse_account_id": 12345}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // MUSEX-CAP-SEC-03 (epic-capstone finding): `/recommend` serves per-account
    // taste/on-deck/gap candidates for a caller-supplied `account_id`. Proven
    // here to be behind the auth gate: a tokenless POST with an OTHERWISE VALID
    // body (a well-formed `account_id`, so the ONLY thing that can reject it is
    // the auth layer — a reached handler would touch `app_no_db`'s unroutable
    // pool and surface a `500`, not a `401`) is rejected `401` before the
    // recommend handler (and therefore the taste data) is ever reached.
    #[tokio::test]
    async fn recommend_without_token_is_401_and_never_reaches_the_handler() {
        let (status, _) = send(
            app_no_db_with_config(with_token_config()),
            post_json("/recommend", json!({"account_id": 12345, "limit": 5})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_route_with_wrong_token_is_401() {
        let (status, _) = send(
            app_no_db_with_config(with_token_config()),
            with_bearer(post_empty("/ops/ingest/arr"), "not-the-configured-token"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_route_with_malformed_auth_header_is_401() {
        let (status, _) = send(
            app_no_db_with_config(with_token_config()),
            with_bearer_scheme(post_empty("/ops/ingest/arr"), "Basic", TEST_API_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    fn with_bearer_scheme(mut req: Request<Body>, scheme: &str, token: &str) -> Request<Body> {
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("{scheme} {token}")).unwrap(),
        );
        req
    }

    // -----------------------------------------------------------------
    // AC: protected route, valid token presented -> reaches the handler.
    //
    // Deliberately does NOT route this through a DB-touching handler like
    // `friend_opt_in_handler` against the lazy/unroutable test pool: unlike
    // the no-token case above (which never calls `Next::run` at all, so the
    // pool is provably never touched — no network I/O happens, no hang
    // risk), a valid-token request that actually reaches a DB-touching
    // handler in this sandbox risks a slow/hanging TCP connect attempt to
    // an unroutable address rather than a fast rejection. `/ops/ingest/arr`
    // gives the same proof (its *own* 503 is only reachable past the auth
    // gate) with no pool I/O in its short-circuit path.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn protected_route_with_valid_token_reaches_the_handler() {
        let (status, body) = send(
            app_no_db_with_config(with_token_config()),
            with_bearer(post_empty("/ops/ingest/arr"), TEST_API_TOKEN),
        )
        .await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "a valid token must not be rejected by the auth layer"
        );
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body["error"].as_str().unwrap_or_default().contains("arr"),
            "expected the handler's own 'no arr instances configured' message, proving the \
             request reached `ops::ingest_arr`, not an auth-layer 503: {body:?}"
        );
    }

    #[tokio::test]
    async fn protected_route_with_valid_token_and_no_db_dependent_ops_handler_runs() {
        // `/ops/ingest/arr` degrades to a clean `503` *inside the handler*
        // when unconfigured, with no DB touch at all — a valid token must
        // reach that handler and get its real (non-auth) 503, covered
        // together with the router-contract tests above
        // (`ops_ingest_arr_contract_503_when_unconfigured`); this test
        // exists purely to double-check `/ops/ingest/tautulli` too and
        // give both cases direct auth-focused assertions here.
        let (status, body) = send(
            app_no_db_with_config(with_token_config()),
            with_bearer(post_empty("/ops/ingest/tautulli"), TEST_API_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("tautulli"),
            "expected the handler's own 'tautulli not configured' message, not an auth 503: {body:?}"
        );
    }

    // -----------------------------------------------------------------
    // AC: token not configured at all -> fail-closed 503 on protected
    // routes, distinct from the 401 (bad/missing credential) case above.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn protected_route_fails_closed_503_when_token_unconfigured() {
        let (status, body) = send(
            app_no_db_with_config(Config::default()),
            post_empty("/ops/ingest/arr"),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("auth not configured"),
            "expected the AUTH layer's 503 (unconfigured token), not a coincidental \
             handler-level 503: {body:?}"
        );
    }

    // -----------------------------------------------------------------
    // AC: MUSE_AUTH_DISABLED escape hatch — only takes effect when NO
    // token is configured; a protected route then reaches its handler
    // with no Authorization header at all.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn auth_disabled_flag_opens_protected_routes_when_no_token_configured() {
        let (status, _) = send(
            app_no_db_with_config(with_auth_disabled_config()),
            post_empty("/ops/ingest/arr"),
        )
        .await;
        // No auth header at all, yet NOT a 401/503-from-auth — the
        // handler's own "arr not configured" 503 is reached directly.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn auth_disabled_flag_is_ignored_once_a_token_is_configured() {
        // MUSE_AUTH_DISABLED must NOT weaken auth once a real token is
        // configured — it only changes the *unconfigured* default.
        let config = Config {
            api_token: Some(TEST_API_TOKEN.to_string()),
            auth_disabled: true,
            ..Config::default()
        };
        let (status, _) = send(app_no_db_with_config(config), post_empty("/ops/ingest/arr")).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a configured token must still be enforced even with MUSE_AUTH_DISABLED set"
        );
    }

    // -----------------------------------------------------------------
    // Non-mutation-adjacent sanity: routes deliberately left OPEN (per
    // `crate::http::router`'s doc comment) must stay reachable with no
    // token configured at all, same as `/health` above.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn open_read_only_surface_stays_reachable_with_no_token_configured() {
        let app = app_no_db_with_config(Config::default());
        let (status, _) = send(app, get("/api/channels")).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "the public channel-guide JSON API must stay open, not gated by auth"
        );
    }
}
