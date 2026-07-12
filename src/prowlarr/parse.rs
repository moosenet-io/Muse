//! Release-name parser v0 (MUSE-16 §4b-D: "a deterministic release-name
//! parser ... populates the parsed_* columns + `parse_confidence`").
//!
//! This is the *arr parsing brain, v0: deterministic, no network, no
//! external crate — release names are tokenized on the standard scene-name
//! delimiters (`.`, `_`, `-`, space) and matched against small known-token
//! tables (resolution/source/codec/HDR). Deliberately conservative: fields
//! that don't clearly match a known token are left `None` rather than
//! guessed at, since a wrong parse silently feeding curation is worse than a
//! visibly-incomplete one. AI-augmented parsing is a Phase-1 concern per the
//! spec; this only needs to be "good enough" to drive parse_confidence and
//! basic curation filtering.

use serde::{Deserialize, Serialize};

/// The output of parsing one raw release-name string.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParsedRelease {
    pub title: Option<String>,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    /// Combined quality label, e.g. `"BluRay-1080p"` / `"WEB-DL-2160p"`.
    pub quality: Option<String>,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub hdr: Vec<String>,
    pub edition: Option<String>,
    pub release_group: Option<String>,
    pub proper_repack: bool,
    /// Freeleech detected from the release *name* itself (a fallback: the
    /// Prowlarr client also checks tracker-reported flags, which are more
    /// reliable when present — see `ProwlarrRelease::is_freeleech`).
    pub freeleech: bool,
    /// 0.0-1.0: fraction of the attribute checks below that matched
    /// something. Not a statistical confidence — a simple, legible signal
    /// for "how much of this name did the parser understand."
    pub confidence: f32,
}

const RESOLUTIONS: &[&str] = &["2160p", "1080p", "720p", "480p", "4k"];
const SOURCES: &[&str] = &[
    "bluray", "blu-ray", "bdrip", "brrip", "remux", "web-dl", "webdl", "webrip", "web", "hdtv",
    "dvdrip", "dvd",
];
const VIDEO_CODECS: &[&str] = &["x264", "x265", "h264", "h265", "hevc", "avc", "av1"];
const AUDIO_CODECS: &[&str] = &[
    "truehd", "dts-hd", "dts", "ddp5.1", "ddp7.1", "dd5.1", "dd7.1", "ddp", "eac3", "ac3", "aac",
    "flac", "opus",
];
const HDR_FLAGS: &[&str] = &["hdr10+", "hdr10", "hdr", "dv", "dolby.vision", "dolby vision"];
const EDITION_PHRASES: &[&str] = &[
    "directors.cut",
    "director's cut",
    "directors cut",
    "extended",
    "unrated",
    "theatrical",
    "remastered",
    "imax",
];

/// Parse a raw release-name string into its component attributes.
pub fn parse_release_name(name: &str) -> ParsedRelease {
    let raw = name.trim();
    let mut checks_total: u32 = 0;
    let mut checks_matched: u32 = 0;

    // Strip a trailing file extension if present (some indexers/feeds
    // include it, most don't).
    let without_ext = strip_known_extension(raw);
    // Freeleech is checked against the *whole* name (before the group split
    // below) since it's sometimes tagged after the group, e.g.
    // "...-GROUP.FREELEECH" or "...-FL" — splitting the group first would
    // silently drop it.
    let full_lower = without_ext.to_lowercase();

    // The release group conventionally lives after the *last* '-' in the
    // name, e.g. "...x264-GROUP". Guard against false positives: a bare
    // trailing token that's empty, purely numeric, or contains whitespace is
    // not a group tag (WEB-DL's internal '-' is inside a token, not at the
    // end, so this doesn't misfire on it in practice).
    let (body, release_group) = split_release_group(without_ext);
    checks_total += 1;
    if release_group.is_some() {
        checks_matched += 1;
    }

    // Two views of the body: `lower_body` keeps '.' intact (dots turned to
    // spaces would break compound tokens like "DDP5.1" or "Directors.Cut"
    // apart), used for substring checks; `normalized`/`tokens` replace
    // delimiters with spaces for whole-token matching (resolution/source/
    // codec/season-episode).
    let lower_body = body.to_lowercase();
    let normalized = body.replace(['_', '.'], " ");
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    let lower_tokens: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();

    let proper_repack = lower_tokens.iter().any(|t| t == "proper" || t == "repack");
    checks_total += 1;
    if proper_repack {
        checks_matched += 1;
    }

    let freeleech = full_lower.contains("freeleech") || full_lower.contains("[fl]");
    checks_total += 1;
    if freeleech {
        checks_matched += 1;
    }

    let year_idx = tokens.iter().position(|t| is_plausible_year(t));
    let year = year_idx.and_then(|i| tokens[i].parse::<i32>().ok());
    checks_total += 1;
    if year.is_some() {
        checks_matched += 1;
    }

    let (season, episode) = find_season_episode(&lower_tokens);
    checks_total += 1;
    if season.is_some() || episode.is_some() {
        checks_matched += 1;
    }

    let resolution = find_first_known(&lower_tokens, RESOLUTIONS).map(normalize_resolution);
    checks_total += 1;
    if resolution.is_some() {
        checks_matched += 1;
    }

    let source = find_first_known(&lower_tokens, SOURCES).map(normalize_source);
    checks_total += 1;
    if source.is_some() {
        checks_matched += 1;
    }

    let video_codec = find_first_known(&lower_tokens, VIDEO_CODECS).map(|s| s.to_string());
    checks_total += 1;
    if video_codec.is_some() {
        checks_matched += 1;
    }

    // Audio codecs include compound tokens ("ddp5.1") that survive better as
    // substring checks against the un-tokenized (but lowercased) body.
    let audio_codec = AUDIO_CODECS
        .iter()
        .find(|needle| contains_word(&lower_body, needle))
        .map(|s| s.to_string());
    checks_total += 1;
    if audio_codec.is_some() {
        checks_matched += 1;
    }

    let hdr: Vec<String> = HDR_FLAGS
        .iter()
        .filter(|needle| contains_word(&lower_body, needle))
        .map(|s| s.to_uppercase().replace(' ', "."))
        .collect();
    checks_total += 1;
    if !hdr.is_empty() {
        checks_matched += 1;
    }

    let edition = EDITION_PHRASES
        .iter()
        .find(|phrase| lower_body.contains(*phrase))
        .map(|s| title_case(s));
    checks_total += 1;
    if edition.is_some() {
        checks_matched += 1;
    }

    // Title = everything before the first attribute-like token we found
    // (year, season/episode marker, resolution, or source — whichever comes
    // first), title-cased back from the delimiter-normalized tokens.
    let title_end = [
        year_idx,
        lower_tokens.iter().position(|t| looks_like_season_episode(t)),
        lower_tokens
            .iter()
            .position(|t| RESOLUTIONS.contains(&t.as_str())),
        lower_tokens.iter().position(|t| SOURCES.contains(&t.as_str())),
    ]
    .into_iter()
    .flatten()
    .min();

    let title_tokens: &[&str] = match title_end {
        Some(end) => &tokens[..end],
        None => &tokens[..],
    };
    let title = if title_tokens.is_empty() {
        None
    } else {
        Some(title_tokens.join(" "))
    };
    checks_total += 1;
    if title.is_some() {
        checks_matched += 1;
    }

    let quality = match (&source, &resolution) {
        (Some(s), Some(r)) => Some(format!("{s}-{r}")),
        (Some(s), None) => Some(s.clone()),
        (None, Some(r)) => Some(r.clone()),
        (None, None) => None,
    };

    let confidence = if checks_total == 0 {
        0.0
    } else {
        checks_matched as f32 / checks_total as f32
    };

    ParsedRelease {
        title,
        year,
        season,
        episode,
        quality,
        resolution,
        source,
        video_codec,
        audio_codec,
        hdr,
        edition,
        release_group,
        proper_repack,
        freeleech,
        confidence,
    }
}

fn strip_known_extension(name: &str) -> &str {
    for ext in [".mkv", ".mp4", ".avi", ".nzb", ".torrent"] {
        if let Some(stripped) = name.strip_suffix(ext) {
            return stripped;
        }
        if let Some(stripped) = name
            .to_lowercase()
            .strip_suffix(ext)
            .map(|_| &name[..name.len() - ext.len()])
        {
            return stripped;
        }
    }
    name
}

fn split_release_group(name: &str) -> (&str, Option<String>) {
    match name.rfind('-') {
        Some(idx) => {
            let candidate = name[idx + 1..].trim();
            // A real group tag is a single bare token: no internal '.' (rules
            // out matching mid-token hyphens like "WEB-DL" as a false
            // group-split, since the tail "DL.x264" would carry a dot from
            // the rest of the name), no whitespace, not purely numeric, and
            // not itself a known quality/codec token.
            let looks_like_group = !candidate.is_empty()
                && !candidate.contains(char::is_whitespace)
                && !candidate.contains('.')
                && !candidate.chars().all(|c| c.is_ascii_digit())
                && !is_known_attribute_token(&candidate.to_lowercase());
            if looks_like_group {
                (&name[..idx], Some(candidate.to_string()))
            } else {
                (name, None)
            }
        }
        None => (name, None),
    }
}

fn is_known_attribute_token(token: &str) -> bool {
    RESOLUTIONS.contains(&token)
        || SOURCES.contains(&token)
        || VIDEO_CODECS.contains(&token)
        || token == "dl"
}

fn is_plausible_year(token: &str) -> bool {
    token.len() == 4
        && token.chars().all(|c| c.is_ascii_digit())
        && token
            .parse::<i32>()
            .map(|y| (1900..=2099).contains(&y))
            .unwrap_or(false)
}

/// Matches `S01E02` (season+episode), `S01` (season-pack, no episode), and
/// tolerates a second trailing episode marker in multi-episode releases
/// (`S01E01E02`) by only taking the first episode number found.
fn find_season_episode(lower_tokens: &[String]) -> (Option<i32>, Option<i32>) {
    for token in lower_tokens {
        if let Some((s, e)) = parse_season_episode_token(token) {
            return (Some(s), e);
        }
    }
    (None, None)
}

fn looks_like_season_episode(token: &str) -> bool {
    parse_season_episode_token(token).is_some()
}

fn parse_season_episode_token(token: &str) -> Option<(i32, Option<i32>)> {
    let bytes = token.as_bytes();
    if bytes.first().map(|b| b.to_ascii_lowercase()) != Some(b's') {
        return None;
    }
    let rest = &token[1..];
    let season_digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if season_digits.is_empty() {
        return None;
    }
    let season: i32 = season_digits.parse().ok()?;
    let after_season = &rest[season_digits.len()..];

    if after_season.is_empty() {
        return Some((season, None));
    }

    if after_season.as_bytes().first().map(|b| b.to_ascii_lowercase()) == Some(b'e') {
        let ep_digits: String = after_season[1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if ep_digits.is_empty() {
            return None;
        }
        let episode: i32 = ep_digits.parse().ok()?;
        return Some((season, Some(episode)));
    }

    None
}

fn find_first_known<'a>(lower_tokens: &[String], known: &'a [&'a str]) -> Option<&'a str> {
    lower_tokens
        .iter()
        .find_map(|t| known.iter().find(|k| *k == t).copied())
}

/// Whether `haystack` contains `needle` at a word-ish boundary (not preceded
/// or followed by an alphanumeric char) — a lightweight substitute for regex
/// word-boundary matching, sufficient for the small known-token lists here.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = haystack[..abs]
            .chars()
            .next_back()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        let after_idx = abs + needle.len();
        let after_ok = haystack[after_idx..]
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len().max(1);
        if start >= haystack.len() {
            break;
        }
    }
    false
}

fn normalize_resolution(token: &str) -> String {
    if token == "4k" {
        "2160p".to_string()
    } else {
        token.to_string()
    }
}

fn normalize_source(token: &str) -> String {
    match token {
        "bluray" | "blu-ray" => "BluRay".to_string(),
        "bdrip" => "BDRip".to_string(),
        "brrip" => "BRRip".to_string(),
        "remux" => "Remux".to_string(),
        "web-dl" | "webdl" => "WEB-DL".to_string(),
        "webrip" => "WEBRip".to_string(),
        "web" => "WEB".to_string(),
        "hdtv" => "HDTV".to_string(),
        "dvdrip" => "DVDRip".to_string(),
        "dvd" => "DVD".to_string(),
        other => title_case(other),
    }
}

fn title_case(phrase: &str) -> String {
    phrase
        .split(['.', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_movie_release() {
        let p = parse_release_name("The.Matrix.1999.1080p.BluRay.x264-GROUP");
        assert_eq!(p.title.as_deref(), Some("The Matrix"));
        assert_eq!(p.year, Some(1999));
        assert_eq!(p.resolution.as_deref(), Some("1080p"));
        assert_eq!(p.source.as_deref(), Some("BluRay"));
        assert_eq!(p.video_codec.as_deref(), Some("x264"));
        assert_eq!(p.release_group.as_deref(), Some("GROUP"));
        assert_eq!(p.quality.as_deref(), Some("BluRay-1080p"));
        assert!(p.season.is_none());
        assert!(!p.proper_repack);
        assert!(!p.freeleech);
        assert!(p.confidence >= 0.5);
    }

    #[test]
    fn parses_a_tv_episode_release_with_audio_codec() {
        let p = parse_release_name("Show.Name.S02E05.720p.WEB-DL.DD5.1.x264-TEAM");
        assert_eq!(p.title.as_deref(), Some("Show Name"));
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(5));
        assert_eq!(p.resolution.as_deref(), Some("720p"));
        assert_eq!(p.source.as_deref(), Some("WEB-DL"));
        assert_eq!(p.audio_codec.as_deref(), Some("dd5.1"));
        assert_eq!(p.release_group.as_deref(), Some("TEAM"));
    }

    #[test]
    fn parses_hdr_and_dolby_vision_flags() {
        let p =
            parse_release_name("Movie.Name.2020.2160p.BluRay.REMUX.HDR10.DV.x265-GRP");
        assert_eq!(p.year, Some(2020));
        assert_eq!(p.resolution.as_deref(), Some("2160p"));
        assert_eq!(p.video_codec.as_deref(), Some("x265"));
        assert!(p.hdr.iter().any(|h| h == "HDR10"));
        assert!(p.hdr.iter().any(|h| h == "DV"));
    }

    #[test]
    fn detects_proper_and_repack() {
        let proper = parse_release_name("Movie.Name.2015.PROPER.1080p.BluRay.x264-GRP");
        assert!(proper.proper_repack);

        let repack = parse_release_name("Movie.Name.2015.REPACK.1080p.BluRay.x264-GRP");
        assert!(repack.proper_repack);

        let neither = parse_release_name("Movie.Name.2015.1080p.BluRay.x264-GRP");
        assert!(!neither.proper_repack);
    }

    #[test]
    fn detects_freeleech_from_the_title_itself() {
        let p = parse_release_name("Some.Release.Name.1080p.WEB.x264-FL.FREELEECH");
        assert!(p.freeleech);

        let not_fl = parse_release_name("Some.Release.Name.1080p.WEB.x264-FL");
        assert!(!not_fl.freeleech);
    }

    #[test]
    fn season_pack_without_episode_number() {
        let p = parse_release_name("Show.Name.S01.1080p.WEB-DL.x264-GROUP");
        assert_eq!(p.season, Some(1));
        assert!(p.episode.is_none());
    }

    #[test]
    fn no_group_when_name_has_no_trailing_dash_group() {
        let p = parse_release_name("Show.Name.S01E01.1080p.WEB-DL.x264.mkv");
        assert!(p.release_group.is_none());
    }

    #[test]
    fn edition_phrase_detected() {
        let p = parse_release_name("Movie.Name.2010.Directors.Cut.1080p.BluRay.x264-GRP");
        assert_eq!(p.edition.as_deref(), Some("Directors Cut"));
    }

    #[test]
    fn confidence_is_low_for_an_unstructured_name() {
        let p = parse_release_name("just_some_random_words");
        assert!(p.confidence < 0.3);
        assert!(p.resolution.is_none());
        assert!(p.source.is_none());
    }

    #[test]
    fn empty_input_does_not_panic() {
        let p = parse_release_name("");
        assert_eq!(p.confidence, 0.0);
        assert!(p.title.is_none());
    }
}
