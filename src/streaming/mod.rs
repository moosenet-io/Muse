//! The ffmpeg channel streaming engine (MUSE-29, spec §4d-E) — the real
//! implementation behind MUSE-28's `/auto/v{channel_id}` stub. Per linear
//! channel, concats the scheduled `channel_programs` grid (library files +
//! interstitials) into one continuous MPEG-TS stream, with join-mid-stream
//! semantics: tuning in lands at the current position of what's "on now".
//!
//! - [`onnow`] — the pure "what's on now + seek offset + what's next" math.
//! - [`ffmpeg`] — the pure ffmpeg-argument builder + path/spawn-error
//!   helpers.
//! - This module (`mod.rs`) — the one impure layer: DB lookups to resolve a
//!   `channel_programs` row to a real file path, and the HTTP handler that
//!   spawns ffmpeg per program and chains their stdout into the response
//!   body.
//!
//! **Benign playback-only**: every DB call here is a read; nothing in this
//! module writes to `media_items`/`media_files`/*arr state.

pub mod ffmpeg;
pub mod onnow;

use std::collections::VecDeque;
use std::io;
use std::process::Stdio;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::models::channel::{ChannelProgram, ChannelProgramItemType};
use crate::repo;

/// HTTP handler for `/auto/v{channel_id}` — the exact path shape every
/// `/discover.json`/`/lineup.json`/`/muse.m3u` URL already advertises (see
/// `tuner::hdhr`/`tuner::m3u`). Replaces MUSE-28's `stream_stub`.
pub async fn stream_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<i64>,
) -> Response {
    match build_stream_response(&state, channel_id).await {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}

async fn build_stream_response(state: &AppState, channel_id: i64) -> MuseResult<Response> {
    let channel = repo::channel::get_channel(&state.pool, channel_id).await?;

    // Best-effort: top off the rolling guide window before resolving
    // on-now, so a freshly-created (or momentarily-behind) channel doesn't
    // 503 just because the background scheduler tick (MUSE-28) hasn't run
    // yet. A failure here is logged, not fatal — whatever is already
    // scheduled is still worth trying to resolve.
    let window = Duration::hours(state.config.channel_guide_window_hours);
    if let Err(e) = crate::tuner::scheduler::ensure_rolling_window(&state.pool, &channel, window).await {
        tracing::warn!(channel_id, error = %e, "failed to top off guide window before streaming; continuing with whatever is already scheduled");
    }

    let now = Utc::now();
    let programs = repo::channel::list_programs_in_window(&state.pool, channel_id, now, now + window).await?;

    let Some(on_now) = onnow::resolve_on_now(&programs, now) else {
        return Err(MuseError::ServiceUnavailable(format!(
            "channel {channel_id} has no program currently scheduled"
        )));
    };

    let mut playlist: VecDeque<(ChannelProgram, i64)> = VecDeque::with_capacity(1 + on_now.upcoming.len());
    playlist.push_back((on_now.current, on_now.seek_ms));
    playlist.extend(on_now.upcoming.into_iter().map(|p| (p, 0)));

    // Resolve + spawn the FIRST entry before committing to a response, so a
    // resolution/spawn failure on "now" is a clean 501/503 rather than a
    // response that starts streaming and then dies.
    let (first_program, first_seek) = playlist.pop_front().expect("just pushed at least one entry");
    let Some(first_path) = resolve_file_path(&state.pool, &state.config.media_root, &first_program).await? else {
        return Err(MuseError::ServiceUnavailable(format!(
            "channel {channel_id}'s on-now program (id={}) has no resolvable media file",
            first_program.id
        )));
    };

    let first_child = match spawn_ffmpeg(&state.config.ffmpeg_path, &first_path, first_seek) {
        Ok(child) => child,
        Err(e) => {
            return Err(match ffmpeg::classify_spawn_error(&e) {
                ffmpeg::StreamAvailability::BinaryMissing => MuseError::NotImplemented,
                ffmpeg::StreamAvailability::SpawnError => {
                    MuseError::ServiceUnavailable(format!("ffmpeg failed to start: {e}"))
                }
            });
        }
    };

    let pool = state.pool.clone();
    let media_root = state.config.media_root.clone();
    let ffmpeg_path = state.config.ffmpeg_path.clone();

    // Rest-of-playlist entries haven't been spawned yet — resolved lazily,
    // one at a time, inside the generator below, so a bad row further down
    // the grid never blocks (or fails) the ones before it.
    let rest: Vec<(ChannelProgram, i64)> = playlist.into_iter().collect();

    let byte_stream = async_stream::stream! {
        // (program, seek_ms, already-spawned child if any)
        let mut queue: VecDeque<(ChannelProgram, i64, Option<Child>)> = VecDeque::with_capacity(1 + rest.len());
        queue.push_back((first_program, first_seek, Some(first_child)));
        for (program, seek_ms) in rest {
            queue.push_back((program, seek_ms, None));
        }

        while let Some((program, seek_ms, spawned)) = queue.pop_front() {
            let mut child = match spawned {
                Some(c) => c,
                None => {
                    let path = match resolve_file_path(&pool, &media_root, &program).await {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            tracing::warn!(program_id = program.id, "no resolvable media file for scheduled program; skipping in stream");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(program_id = program.id, error = %e, "failed resolving media file; skipping in stream");
                            continue;
                        }
                    };
                    match spawn_ffmpeg(&ffmpeg_path, &path, seek_ms) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(program_id = program.id, error = %e, "ffmpeg failed to start for scheduled program; skipping");
                            continue;
                        }
                    }
                }
            };

            let Some(mut stdout) = child.stdout.take() else {
                tracing::warn!(program_id = program.id, "ffmpeg child had no stdout pipe; skipping");
                continue;
            };

            let mut buf = [0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => yield Ok::<Bytes, io::Error>(Bytes::copy_from_slice(&buf[..n])),
                    Err(e) => {
                        tracing::warn!(program_id = program.id, error = %e, "error reading ffmpeg stdout; ending stream");
                        return;
                    }
                }
            }
            let _ = child.wait().await;
        }
    };

    let body = Body::from_stream(byte_stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .body(body)
        .expect("static-header stream response should always build"))
}

/// Resolve a scheduled `channel_programs` row to a real, on-disk file path,
/// or `Ok(None)` when nothing is resolvable yet (no file attached to the
/// episode/movie, or the interstitial has no `file_path` populated —
/// migrations/0098). Never errors just because a program is unresolvable;
/// only a genuine DB failure is `Err`.
async fn resolve_file_path(pool: &PgPool, media_root: &str, program: &ChannelProgram) -> MuseResult<Option<String>> {
    let relative = match program.item_type {
        ChannelProgramItemType::Episode => {
            let Some(episode_id) = program.episode_id else {
                return Ok(None);
            };
            repo::media_file::list_for_episode(pool, episode_id)
                .await?
                .into_iter()
                .next()
                .map(|f| f.relative_path)
        }
        ChannelProgramItemType::Movie => {
            let Some(media_item_id) = program.media_item_id else {
                return Ok(None);
            };
            repo::media_file::list_by_media_item(pool, media_item_id)
                .await?
                .into_iter()
                .next()
                .map(|f| f.relative_path)
        }
        ChannelProgramItemType::Interstitial => {
            let Some(interstitial_id) = program.interstitial_id else {
                return Ok(None);
            };
            return Ok(repo::interstitial::get_file_path(pool, interstitial_id)
                .await?
                .map(|p| ffmpeg::join_media_path(media_root, &p)));
        }
    };
    Ok(relative.map(|rel| ffmpeg::join_media_path(media_root, &rel)))
}

fn spawn_ffmpeg(ffmpeg_path: &str, file_path: &str, seek_ms: i64) -> io::Result<Child> {
    Command::new(ffmpeg_path)
        .args(ffmpeg::build_args(file_path, seek_ms))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- live-DB test (MUSE-29) --------------------------------------
    //
    // Gated on MUSE_TEST_DATABASE_URL per the crate-wide convention (see
    // src/integration_tests.rs) — skips cleanly when unset. Seeds its own
    // library/show/episode/media_file/channel/channel_program with a unique
    // suffix so it's safe alongside any other live-DB test sharing the
    // database, and asserts only on-now RESOLUTION (the pure math wired to
    // real repo calls) — it does not invoke ffmpeg or hit the HTTP handler,
    // per the "never run ffmpeg in tests" gate rule.
    #[tokio::test]
    async fn on_now_resolves_the_right_seeded_program_for_a_given_now() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 on_now_resolves_the_right_seeded_program_for_a_given_now \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        use crate::models::channel::{ChannelKind, ChannelMode, NewChannel, NewChannelProgram};
        use crate::models::episode::NewEpisode;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_file::{NewMediaFile, ReleaseTypeKind, Revision};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::season::NewSeason;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("muse29_stream_{suffix}"),
                kind: LibraryKind::Tv,
                root_folder: "/media/TV/".to_string(),
                source_arr_name: Some("sonarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let show_metadata = repo::media_metadata::upsert_by_tvdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: None,
                tvdb_id: Some(format!("tvdb-muse29-{suffix}")),
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Muse29 Test Show {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: Some("en".to_string()),
                status: Some("continuing".to_string()),
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(30),
                year: Some(1999),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert show media_metadata");

        let show_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: show_metadata.id,
                path: format!("/media/TV/Muse29 Test Show {suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("upsert show media_item");

        let season = repo::season::upsert(
            &pool,
            &NewSeason {
                media_item_id: show_item.id,
                season_number: 1,
                title: None,
                overview: None,
                monitored: true,
                air_date: None,
            },
        )
        .await
        .expect("upsert season");

        let episode = repo::episode::upsert(
            &pool,
            &NewEpisode {
                season_id: season.id,
                media_item_id: show_item.id,
                episode_number: 1,
                absolute_episode_number: Some(1),
                title: Some("Pilot".to_string()),
                overview: None,
                air_date: None,
                air_date_utc: None,
                runtime_minutes: Some(30),
                monitored: true,
                tvdb_id: None,
            },
        )
        .await
        .expect("upsert episode");
        repo::episode::set_has_file(&pool, episode.id, true)
            .await
            .expect("mark episode has_file");

        let media_file = repo::media_file::create(
            &pool,
            &NewMediaFile {
                media_item_id: show_item.id,
                relative_path: format!("TV/Muse29 Test Show {suffix}/S01E01.mkv"),
                size_bytes: Some(1_000_000),
                release_group: None,
                languages: vec!["eng".to_string()],
                release_type: ReleaseTypeKind::Single,
                quality_tier_id: None,
                revision: Revision {
                    version: 1,
                    real: 0,
                    is_repack: false,
                },
            },
        )
        .await
        .expect("create media_file");
        repo::media_file::attach_to_episode(&pool, episode.id, media_file.id)
            .await
            .expect("attach media_file to episode");

        let channel = repo::channel::create_channel(
            &pool,
            &NewChannel {
                account_id: None,
                name: format!("Muse29 Stream Test {suffix}"),
                kind: ChannelKind::Preset,
                mode: ChannelMode::Linear,
                channel_number: None,
                target_client_id: None,
                directive: None,
                rules: serde_json::json!({}),
                is_preset: false,
            },
        )
        .await
        .expect("create linear channel");

        // Hand-place a single 30-minute program starting 10 minutes ago, so
        // `now` lands 10 minutes into it — a deterministic, DB-independent
        // expectation for the seek offset.
        let start_at = Utc::now() - Duration::minutes(10);
        let end_at = start_at + Duration::minutes(30);
        let program = repo::channel::create_program(
            &pool,
            &NewChannelProgram {
                channel_id: channel.id,
                item_type: ChannelProgramItemType::Episode,
                media_item_id: None,
                episode_id: Some(episode.id),
                interstitial_id: None,
                title: "Pilot".to_string(),
                subtitle: Some("S1E1".to_string()),
                description: None,
                artwork_url: None,
                start_at,
                end_at,
                duration_ms: 30 * 60_000,
                rationale: Some("muse29 test fixture".to_string()),
            },
        )
        .await
        .expect("create channel_program");

        let now = Utc::now();
        let programs = repo::channel::list_programs_in_window(&pool, channel.id, now - Duration::minutes(1), now + Duration::minutes(1))
            .await
            .expect("list programs in window");
        // Scope to this test's own program id — a shared muse_test DB may
        // have other channels' rows, but list_programs_in_window is already
        // filtered by channel_id so this is just a sanity check.
        assert!(programs.iter().any(|p| p.id == program.id));

        let on_now = onnow::resolve_on_now(&programs, now).expect("a program should be on now");
        assert_eq!(on_now.current.id, program.id);
        // ~10 minutes in, allowing slack for wall-clock time elapsed during
        // the test itself between computing start_at and calling resolve.
        assert!(
            on_now.seek_ms >= 9 * 60_000 && on_now.seek_ms <= 11 * 60_000,
            "expected seek_ms around 10 minutes, got {}",
            on_now.seek_ms
        );

        // And the file-path resolution glue (resolve_file_path) picks up
        // the attached media_file's relative_path.
        let resolved = resolve_file_path(&pool, "", &on_now.current)
            .await
            .expect("resolve_file_path should not error");
        assert_eq!(resolved, Some(media_file.relative_path.clone()));
    }
}
