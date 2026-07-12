//! `GET /muse.m3u` — the M3U-playlist alternative to HDHomeRun-emulation
//! for Plex/other players that prefer an M3U+XMLTV tuner definition over
//! discovery. One `#EXTINF` line per linear channel, in lineup order.

use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use crate::http::AppState;

use super::hdhr::{channel_ref, lineup_entries};

/// Render the M3U playlist body from lineup entries. Pure/sync so it's
/// trivially unit-testable without a DB.
pub fn render(entries: &[super::hdhr::LineupEntry]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for entry in entries {
        // `entry.url` is `{base}/auto/v{id}` (see `hdhr::lineup_entries`) —
        // the channel id is the last path segment, reused here as the
        // stable tvg-id instead of re-deriving it from GuideNumber (which
        // is mutable/operator-facing).
        let id_str = entry
            .url
            .rsplit("/auto/v")
            .next()
            .unwrap_or_default();
        let tvg_id = id_str
            .parse::<i64>()
            .map(channel_ref)
            .unwrap_or_else(|_| entry.guide_number.clone());
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"{tvg_id}\" tvg-name=\"{name}\" tvg-chno=\"{num}\" group-title=\"Muse\",{name}\n{url}\n",
            tvg_id = tvg_id,
            name = escape_attr(&entry.guide_name),
            num = entry.guide_number,
            url = entry.url,
        ));
    }
    out
}

/// M3U attribute values are double-quoted; escape any embedded `"`.
fn escape_attr(value: &str) -> String {
    value.replace('"', "'")
}

/// `GET /muse.m3u`.
pub async fn muse_m3u(State(state): State<Arc<AppState>>) -> crate::error::MuseResult<impl IntoResponse> {
    let entries = lineup_entries(&state).await?;
    let body = render(&entries);
    Ok((
        [(header::CONTENT_TYPE, "audio/x-mpegurl; charset=utf-8")],
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::hdhr::LineupEntry;
    use super::*;

    #[test]
    fn render_produces_well_formed_m3u() {
        let entries = vec![
            LineupEntry {
                guide_number: "101".to_string(),
                guide_name: "Saturday Morning".to_string(),
                url: "http://192.0.2.10:8090/auto/v5".to_string(),
            },
            LineupEntry {
                guide_number: "102".to_string(),
                guide_name: "90s \"Chaos\"".to_string(),
                url: "http://192.0.2.10:8090/auto/v6".to_string(),
            },
        ];

        let m3u = render(&entries);

        assert!(m3u.starts_with("#EXTM3U\n"));
        assert!(m3u.contains("tvg-id=\"muse-5\""));
        assert!(m3u.contains("tvg-chno=\"101\""));
        assert!(m3u.contains("group-title=\"Muse\",Saturday Morning"));
        assert!(m3u.contains("http://192.0.2.10:8090/auto/v5"));
        // embedded quote in the second channel's name must not corrupt the
        // attribute quoting
        assert!(m3u.contains("tvg-name=\"90s 'Chaos'\""));
        assert_eq!(m3u.lines().count(), 5); // header + 2 * (EXTINF + URL)
    }

    #[test]
    fn render_of_empty_lineup_is_just_the_header() {
        assert_eq!(render(&[]), "#EXTM3U\n");
    }
}
