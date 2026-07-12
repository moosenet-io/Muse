//! `GET /xmltv.xml` — XMLTV EPG generated straight from the `channel_programs`
//! grid (MUSE-23) for every linear channel, covering the same rolling
//! window the scheduler (`super::scheduler`) keeps filled
//! (`Config::channel_guide_window_hours`, spec pre-flight default 48h).

use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};

use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::{Channel, ChannelProgram};
use crate::repo;

use super::hdhr::channel_ref;

/// XMLTV's required timestamp shape: `YYYYMMDDHHMMSS +0000`. Programs are
/// always stored/rendered in UTC, so the offset is always `+0000`.
fn xmltv_timestamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%d%H%M%S +0000").to_string()
}

/// Escape the five XML-special characters for safe use in text content and
/// double-quoted attribute values alike.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Parse a `subtitle` of the "S2E4" shape (see the migration's row-shape
/// comment) into `(season, episode)`, if it matches. Anything else (a
/// human subtitle, `None`, an interstitial) simply omits `<episode-num>`.
fn parse_season_episode(subtitle: &str) -> Option<(u32, u32)> {
    let rest = subtitle.strip_prefix('S')?;
    let (season_str, rest) = rest.split_once('E')?;
    let season = season_str.parse().ok()?;
    let episode = rest.parse().ok()?;
    Some((season, episode))
}

/// Render the full XMLTV document from a channel list and, per channel, its
/// programme grid. Pure/sync so it's unit-testable without a DB.
pub fn render(channels: &[Channel], programs_by_channel: &[(i64, Vec<ChannelProgram>)]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<tv generator-info-name=\"muse\" source-info-name=\"Muse TV\">\n");

    for channel in channels {
        let id = channel_ref(channel.id);
        out.push_str(&format!("  <channel id=\"{id}\">\n"));
        out.push_str(&format!(
            "    <display-name>{}</display-name>\n",
            xml_escape(&channel.name)
        ));
        out.push_str("  </channel>\n");
    }

    for (channel_id, programs) in programs_by_channel {
        let id = channel_ref(*channel_id);
        for program in programs {
            out.push_str(&format!(
                "  <programme start=\"{start}\" stop=\"{stop}\" channel=\"{id}\">\n",
                start = xmltv_timestamp(program.start_at),
                stop = xmltv_timestamp(program.end_at),
            ));
            out.push_str(&format!(
                "    <title lang=\"en\">{}</title>\n",
                xml_escape(&program.title)
            ));
            if let Some(subtitle) = &program.subtitle {
                out.push_str(&format!(
                    "    <sub-title lang=\"en\">{}</sub-title>\n",
                    xml_escape(subtitle)
                ));
            }
            if let Some(desc) = &program.description {
                out.push_str(&format!(
                    "    <desc lang=\"en\">{}</desc>\n",
                    xml_escape(desc)
                ));
            }
            if let Some(icon) = &program.artwork_url {
                out.push_str(&format!("    <icon src=\"{}\"/>\n", xml_escape(icon)));
            }
            if let Some(subtitle) = &program.subtitle {
                if let Some((season, episode)) = parse_season_episode(subtitle) {
                    // XMLTV `onscreen` numbering is 1-based, matching the
                    // "S2E4" convention already stored in `subtitle`.
                    out.push_str(&format!(
                        "    <episode-num system=\"onscreen\">S{season}E{episode}</episode-num>\n"
                    ));
                    out.push_str(&format!(
                        "    <episode-num system=\"xmltv_ns\">{}.{}.</episode-num>\n",
                        season.saturating_sub(1),
                        episode.saturating_sub(1)
                    ));
                }
            }
            out.push_str("  </programme>\n");
        }
    }

    out.push_str("</tv>\n");
    out
}

/// `GET /xmltv.xml`.
pub async fn xmltv_xml(State(state): State<Arc<AppState>>) -> MuseResult<impl IntoResponse> {
    let channels = repo::channel::list_linear_channels(&state.pool).await?;
    let from = Utc::now();
    let to = from + chrono::Duration::hours(state.config.channel_guide_window_hours);

    let mut programs_by_channel = Vec::with_capacity(channels.len());
    for channel in &channels {
        let programs = repo::channel::list_programs_in_window(&state.pool, channel.id, from, to).await?;
        programs_by_channel.push((channel.id, programs));
    }

    let body = render(&channels, &programs_by_channel);
    Ok((
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChannelKind, ChannelMode, ChannelProgramItemType};
    use chrono::TimeZone;

    fn fixture_channel(id: i64, name: &str) -> Channel {
        Channel {
            id,
            account_id: None,
            name: name.to_string(),
            kind: ChannelKind::Preset,
            mode: ChannelMode::Linear,
            channel_number: Some(101.0),
            target_client_id: None,
            directive: None,
            rules: serde_json::json!({}),
            is_preset: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn fixture_program(channel_id: i64, subtitle: Option<&str>, start: DateTime<Utc>, end: DateTime<Utc>) -> ChannelProgram {
        ChannelProgram {
            id: 1,
            channel_id,
            item_type: ChannelProgramItemType::Episode,
            media_item_id: None,
            episode_id: Some(1),
            interstitial_id: None,
            title: "Pilot's \"Debut\" & More".to_string(),
            subtitle: subtitle.map(|s| s.to_string()),
            description: Some("A show begins.".to_string()),
            artwork_url: None,
            start_at: start,
            end_at: end,
            duration_ms: (end - start).num_milliseconds(),
            rationale: None,
            play_event_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn xmltv_timestamp_matches_the_spec_shape() {
        let dt = Utc.with_ymd_and_hms(2026, 7, 12, 8, 30, 0).unwrap();
        assert_eq!(xmltv_timestamp(dt), "20260712083000 +0000");
    }

    #[test]
    fn parse_season_episode_handles_the_s_e_shape() {
        assert_eq!(parse_season_episode("S2E4"), Some((2, 4)));
        assert_eq!(parse_season_episode("S10E1"), Some((10, 1)));
        assert_eq!(parse_season_episode("Director's Cut"), None);
        assert_eq!(parse_season_episode("Saturday Morning Bumper"), None);
    }

    #[test]
    fn render_produces_well_formed_xmltv_with_escaping_and_episode_num() {
        let channel = fixture_channel(5, "90s \"Chaos\" & Friends");
        let start = Utc.with_ymd_and_hms(2026, 7, 12, 8, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 7, 12, 8, 22, 0).unwrap();
        let program = fixture_program(5, Some("S2E4"), start, end);

        let xml = render(&[channel], &[(5, vec![program])]);

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(xml.contains("<channel id=\"muse-5\">"));
        assert!(xml.contains("<display-name>90s &quot;Chaos&quot; &amp; Friends</display-name>"));
        assert!(xml.contains("<title lang=\"en\">Pilot&apos;s &quot;Debut&quot; &amp; More</title>"));
        assert!(xml.contains("start=\"20260712080000 +0000\""));
        assert!(xml.contains("stop=\"20260712082200 +0000\""));
        assert!(xml.contains("<episode-num system=\"onscreen\">S2E4</episode-num>"));
        assert!(xml.contains("<episode-num system=\"xmltv_ns\">1.3.</episode-num>"));
        assert!(xml.trim_end().ends_with("</tv>"));

        // well-formedness: every opened tag closes, in a naive stack sense
        let opens = xml.matches("<programme ").count();
        let closes = xml.matches("</programme>").count();
        assert_eq!(opens, closes);
    }

    #[test]
    fn render_omits_episode_num_for_non_matching_subtitle() {
        let channel = fixture_channel(5, "Bumpers");
        let start = Utc.with_ymd_and_hms(2026, 7, 12, 8, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 7, 12, 8, 0, 15).unwrap();
        let program = fixture_program(5, Some("Saturday Morning Bumper"), start, end);

        let xml = render(&[channel], &[(5, vec![program])]);

        assert!(!xml.contains("episode-num"));
    }

    #[test]
    fn render_of_no_channels_is_still_well_formed() {
        let xml = render(&[], &[]);
        assert_eq!(
            xml,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv generator-info-name=\"muse\" source-info-name=\"Muse TV\">\n</tv>\n"
        );
    }
}
