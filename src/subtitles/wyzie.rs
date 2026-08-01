//! SUBS-01 — the Wyzie subtitle provider client.
//!
//! A typed, read-only HTTP client in the same shape as
//! [`crate::trending::TmdbClient`]: constructed from [`Config`], persists
//! nothing itself, and takes a base-URL override so tests can point it at an
//! httpmock server.
//!
//! # Secrets
//!
//! The API key comes from `WYZIE_KEY`, read through [`Config`] exactly as
//! `TMDB_API_KEY` is, and is wrapped in [`QbitPassword`] so a stray `{:?}` on
//! the config or the client cannot print it. It is attached with
//! `reqwest`'s `.query()` builder and **never** interpolated into a string
//! that could reach a log, an error, or an HTTP response body — see
//! [`SAFE_ENDPOINT`] and the `errors_never_disclose_the_api_key` test.
//!
//! `None` key means the provider tier is simply unavailable. Discovery still
//! reports embedded and sidecar subtitles; the provider tier reports itself as
//! unconfigured, which is a different fact from "no subtitles found" and is
//! kept distinct all the way to the API response.
//!
//! # Fail loud
//!
//! A transport failure, a non-2xx status, or a body that does not parse is an
//! [`MuseError`]. It is never an empty result list. An empty list means the
//! provider answered successfully and had nothing — the two must not be
//! conflated, because "no subtitles exist for this film" and "the provider is
//! down" lead an operator to completely different next actions.

use std::time::Duration;

use serde::Deserialize;

use crate::config::Config;
use crate::download::config::QbitPassword;
use crate::error::{MuseError, MuseResult};

use super::cues::SubtitleFormat;
use super::rank::Candidate;
use super::{AvailableSubtitle, SubtitleSource};

/// Wyzie's search endpoint.
pub const DEFAULT_BASE_URL: &str = "https://sub.wyzie.io";

/// The provider name persisted in the `source` discriminant and shown to the
/// operator.
pub const PROVIDER_NAME: &str = "wyzie";

/// The endpoint description used in EVERY error message this module produces.
///
/// Deliberately a constant with no interpolation: the real request URL carries
/// the API key in its query string, so formatting a `reqwest::Url` (or a
/// `reqwest::Error`, which embeds the URL) into an error would leak the
/// credential into logs and into HTTP responses. Errors name the endpoint,
/// the status, and nothing else.
const SAFE_ENDPOINT: &str = "wyzie /search";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Largest subtitle body Muse will accept from the provider (2 MiB).
///
/// A subtitle for a three-hour film is tens of kilobytes; ASS with heavy
/// styling might reach a few hundred. Two megabytes is far above any
/// legitimate case and well below anything that could exhaust memory, so a
/// hostile or misconfigured endpoint cannot stream an unbounded body into the
/// process.
const MAX_SUBTITLE_BYTES: usize = 2 * 1024 * 1024;

/// One search result, as Wyzie returns it.
///
/// Field names follow the wire format. Only the fields Muse actually uses are
/// modelled; unknown fields are ignored by serde, so a provider-side addition
/// cannot break parsing.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WyzieSubtitle {
    pub id: String,
    /// Direct download URL for the subtitle body.
    pub url: String,
    /// `"srt"`, `"ass"`, ... Used to pick a [`SubtitleFormat`]; an unknown or
    /// image-based value yields `None` there rather than a wrong parse.
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub display: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    /// The release this subtitle was cut for. The single most important
    /// ranking signal — see [`super::rank`].
    #[serde(rename = "matchedRelease", default)]
    pub matched_release: Option<String>,
    #[serde(rename = "isHearingImpaired", default)]
    pub is_hearing_impaired: bool,
    /// Machine-generated. Deprioritised in ranking and surfaced to the
    /// operator, never silently mixed in.
    #[serde(default)]
    pub ai: bool,
    #[serde(rename = "downloadCount", default)]
    pub download_count: i64,
}

impl WyzieSubtitle {
    /// The text format this subtitle can be parsed and shifted as, if any.
    pub fn subtitle_format(&self) -> Option<SubtitleFormat> {
        self.format.as_deref().and_then(SubtitleFormat::from_extension)
    }

    /// Project onto the provider-agnostic ranking input.
    pub fn as_candidate(&self) -> Candidate {
        Candidate {
            id: self.id.clone(),
            // An empty `matchedRelease` string is normalized to `None` here so
            // the ranker sees "the provider did not say" rather than "the
            // provider said the empty release", which would score as a
            // mismatch against a file whose release we do know.
            matched_release: self
                .matched_release
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            machine_generated: self.ai,
            hearing_impaired: self.is_hearing_impaired,
            download_count: self.download_count,
        }
    }

    /// Project onto the tier-agnostic availability type.
    pub fn as_available(&self) -> AvailableSubtitle {
        AvailableSubtitle {
            source: SubtitleSource::Provider {
                provider: PROVIDER_NAME.to_string(),
                provider_id: self.id.clone(),
                machine_generated: self.ai,
            },
            language: self.language.as_deref().map(|l| l.trim().to_ascii_lowercase()),
            display: self.display.clone(),
            format: self.subtitle_format(),
            // Wyzie does not expose a forced-narrative flag in the fields
            // verified against the live API, so this is left false rather than
            // guessed at from the filename. A wrong `forced` flag would make
            // `select_preferred` silently exclude a perfectly good subtitle.
            forced: false,
            hearing_impaired: self.is_hearing_impaired,
        }
    }
}

/// A read-only Wyzie client.
#[derive(Clone)]
pub struct WyzieClient {
    http: reqwest::Client,
    base_url: String,
    /// Wrapped so `Debug` on this struct — or on anything holding it — cannot
    /// print the credential.
    api_key: QbitPassword,
}

// Hand-written for the same reason `FoundryConfig`'s is: a derived `Debug`
// would print `base_url` (harmless) but the derive is what future fields would
// ride in on. Shape only.
impl std::fmt::Debug for WyzieClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WyzieClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl WyzieClient {
    /// Build a client against a specific base URL and key. Used directly by
    /// tests (pointing at httpmock); production goes through
    /// [`WyzieClient::from_config`].
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: QbitPassword::from(api_key.into()),
        })
    }

    /// Construct from [`Config`], or `None` when `WYZIE_KEY` is unset.
    ///
    /// `None` is the graceful-degrade posture every optional integration in
    /// this crate uses: the provider tier is unavailable, and discovery still
    /// works from embedded and sidecar sources. The caller must report
    /// "provider not configured" rather than "no subtitles found".
    pub fn from_config(config: &Config) -> Option<Self> {
        let key = config.wyzie_key.as_ref()?;
        match Self::new(
            config
                .wyzie_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            key.expose().to_string(),
        ) {
            Ok(client) => Some(client),
            Err(e) => {
                // Note the error carries no URL and no key.
                tracing::warn!(error = %e, "subtitles: could not build the Wyzie client");
                None
            }
        }
    }

    /// Search for subtitles for an IMDb id in one language.
    ///
    /// Only the two query parameters verified against the live API (`id`,
    /// `language`) plus the key are sent. Unverified parameters are
    /// deliberately not guessed at: a parameter the API does not recognise may
    /// be ignored, or may change the result set in a way Muse would then
    /// silently mis-rank.
    ///
    /// An empty result vector means the provider answered and had nothing.
    /// Every failure — transport, non-2xx, unparseable body — is an `Err`.
    pub async fn search(&self, imdb_id: &str, language: &str) -> MuseResult<Vec<WyzieSubtitle>> {
        let imdb_id = imdb_id.trim();
        if imdb_id.is_empty() {
            return Err(MuseError::BadRequest(
                "subtitles: an IMDb id is required to search the provider".into(),
            ));
        }

        let response = self
            .http
            .get(format!("{}/search", self.base_url))
            .query(&[
                ("id", imdb_id),
                ("language", language.trim()),
                // Attached via the query builder, never string-interpolated.
                ("key", self.api_key.expose()),
            ])
            .send()
            .await
            // `reqwest::Error`'s Display embeds the request URL, which carries
            // the key — so the transport error is REPLACED, not wrapped.
            .map_err(|e| {
                MuseError::upstream(format!(
                    "{SAFE_ENDPOINT}: the request could not be completed ({})",
                    transport_reason(&e)
                ))
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("{SAFE_ENDPOINT} returned {status}"),
            });
        }

        let body = response.text().await.map_err(|e| {
            MuseError::upstream(format!(
                "{SAFE_ENDPOINT}: the response body could not be read ({})",
                transport_reason(&e)
            ))
        })?;

        parse_search_response(&body)
    }

    /// Download one subtitle's body.
    ///
    /// `url` comes from a search result Muse itself fetched — it is not
    /// operator input — but it is still validated as HTTP(S) before use, so a
    /// compromised or malformed provider response cannot make Muse open a
    /// `file://` URL and read an arbitrary local file into the library.
    pub async fn download(&self, url: &str) -> MuseResult<String> {
        let parsed = reqwest::Url::parse(url.trim())
            .map_err(|_| MuseError::upstream(format!("{SAFE_ENDPOINT}: a result carried an unusable download URL")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(MuseError::upstream(format!(
                "{SAFE_ENDPOINT}: refusing a subtitle download over the `{}` scheme — only http/https",
                parsed.scheme()
            )));
        }

        let response = self.http.get(parsed).send().await.map_err(|e| {
            MuseError::upstream(format!(
                "{SAFE_ENDPOINT}: the subtitle download could not be completed ({})",
                transport_reason(&e)
            ))
        })?;

        let status = response.status();
        if !status.is_success() {
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("{SAFE_ENDPOINT}: the subtitle download returned {status}"),
            });
        }

        let bytes = response.bytes().await.map_err(|e| {
            MuseError::upstream(format!(
                "{SAFE_ENDPOINT}: the subtitle body could not be read ({})",
                transport_reason(&e)
            ))
        })?;

        decode_subtitle_body(&bytes)
    }
}

/// Describe a transport failure WITHOUT its URL.
///
/// `reqwest::Error`'s own `Display` includes the request URL, and this
/// client's request URL carries the API key. So the classification is rebuilt
/// from the error's predicates instead of formatted from it. This is the one
/// place that decision is made; every error path in this module routes through
/// it.
fn transport_reason(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else if e.is_decode() {
        "the response could not be decoded"
    } else if e.is_body() {
        "the response body failed"
    } else {
        "a transport error occurred"
    }
}

/// Parse a search response body. **Pure**, so the wire-format handling is
/// testable without HTTP.
///
/// Accepts both a bare array (the shape verified against the live API) and an
/// object wrapping one under a `data`/`results`/`subtitles` key, because a
/// provider that adds an envelope should degrade to a clear error at worst —
/// but an unrecognised shape is an ERROR, never an empty list.
pub fn parse_search_response(body: &str) -> MuseResult<Vec<WyzieSubtitle>> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(MuseError::upstream(format!(
            "{SAFE_ENDPOINT}: the response body was empty — this is a provider failure, \
             not an absence of subtitles"
        )));
    }

    if let Ok(list) = serde_json::from_str::<Vec<WyzieSubtitle>>(trimmed) {
        return Ok(list);
    }

    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        MuseError::upstream(format!(
            "{SAFE_ENDPOINT}: the response was not valid JSON ({e}) — this is a provider \
             failure, not an absence of subtitles"
        ))
    })?;

    for key in ["data", "results", "subtitles"] {
        if let Some(inner) = value.get(key) {
            if let Ok(list) = serde_json::from_value::<Vec<WyzieSubtitle>>(inner.clone()) {
                return Ok(list);
            }
        }
    }

    Err(MuseError::upstream(format!(
        "{SAFE_ENDPOINT}: the response JSON was not a recognised subtitle list — this is a \
         provider failure, not an absence of subtitles"
    )))
}

/// Decode a downloaded subtitle body into text. **Pure.**
///
/// Wyzie reports `encoding` per result and the observed value is UTF-8, but a
/// subtitle corpus is full of legacy Windows-1252 and Latin-1 files, so a
/// strict `from_utf8` would reject perfectly usable subtitles. The rule is:
///
/// - Enforce the size bound first.
/// - Reject a body that is empty, or that contains a NUL byte (a NUL means
///   this is binary — a `.sup`, a zip, an HTML error page's gzip — not text,
///   and parsing it as text would produce garbage cues rather than an error).
/// - Strip a UTF-8 BOM if present, since it would otherwise ride into the
///   first timestamp token and break parsing.
/// - Otherwise decode lossily, so a Latin-1 accented character becomes a
///   replacement character rather than failing the whole fetch.
pub fn decode_subtitle_body(bytes: &[u8]) -> MuseResult<String> {
    if bytes.len() > MAX_SUBTITLE_BYTES {
        return Err(MuseError::upstream(format!(
            "{SAFE_ENDPOINT}: the subtitle body is {} bytes, over the {MAX_SUBTITLE_BYTES}-byte limit",
            bytes.len()
        )));
    }
    if bytes.is_empty() {
        return Err(MuseError::upstream(format!(
            "{SAFE_ENDPOINT}: the subtitle body was empty"
        )));
    }
    if bytes.contains(&0) {
        return Err(MuseError::upstream(format!(
            "{SAFE_ENDPOINT}: the subtitle body is binary, not text — refusing to parse it as \
             a subtitle"
        )));
    }

    let without_bom = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    Ok(String::from_utf8_lossy(without_bom).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response body in the shape verified against the live Wyzie API.
    const LIVE_SHAPE: &str = r#"[
      {
        "id": "1234",
        "url": "https://sub.wyzie.io/dl/1234.srt",
        "format": "srt",
        "encoding": "UTF-8",
        "language": "en",
        "display": "English",
        "flagUrl": "https://example.invalid/en.svg",
        "fileName": "The.Martian.2015.1080p.BluRay.x264-SPARKS.srt",
        "media": "movie",
        "origin": "opensubtitles",
        "matchedRelease": "The.Martian.2015.1080p.BluRay.x264-SPARKS",
        "matchedFilter": "release",
        "downloadCount": 4211,
        "isHearingImpaired": false,
        "ai": false
      },
      {
        "id": "5678",
        "url": "https://sub.wyzie.io/dl/5678.srt",
        "format": "srt",
        "language": "en",
        "matchedRelease": "",
        "downloadCount": 12,
        "isHearingImpaired": true,
        "ai": true
      }
    ]"#;

    #[test]
    fn parses_the_live_response_shape() {
        let results = parse_search_response(LIVE_SHAPE).unwrap();
        assert_eq!(results.len(), 2);

        let first = &results[0];
        assert_eq!(first.id, "1234");
        assert_eq!(first.language.as_deref(), Some("en"));
        assert_eq!(
            first.matched_release.as_deref(),
            Some("The.Martian.2015.1080p.BluRay.x264-SPARKS")
        );
        assert_eq!(first.download_count, 4211);
        assert!(!first.ai);
        assert!(!first.is_hearing_impaired);
        assert_eq!(first.subtitle_format(), Some(SubtitleFormat::SubRip));

        let second = &results[1];
        assert!(second.ai, "the machine-generated flag must be carried, not dropped");
        assert!(second.is_hearing_impaired);
    }

    #[test]
    fn unknown_wire_fields_do_not_break_parsing() {
        // `flagUrl`, `media`, `origin`, `matchedFilter` are in the live shape
        // above and are not modelled; a provider adding more must not break
        // the client.
        let body = r#"[{"id":"1","url":"https://x.invalid/1.srt","brandNewField":{"a":1}}]"#;
        let results = parse_search_response(body).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn an_empty_matched_release_is_normalized_to_unknown_not_to_a_mismatch() {
        // Scoring an empty string as a MISMATCH would penalise a subtitle for
        // a fact the provider never asserted.
        let results = parse_search_response(LIVE_SHAPE).unwrap();
        assert_eq!(results[1].as_candidate().matched_release, None);
    }

    #[test]
    fn a_genuinely_empty_result_list_is_ok_and_distinct_from_a_failure() {
        // "The provider answered and had nothing" is a legitimate success.
        let results = parse_search_response("[]").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn a_provider_failure_is_never_an_empty_result_list() {
        // The absolute rule. Every one of these must be Err, because each
        // would otherwise read as "no subtitles exist for this film".
        for bad in [
            "",
            "   ",
            "not json at all",
            "{\"error\":\"rate limited\"}",
            "<html><body>502 Bad Gateway</body></html>",
            "{}",
            "null",
            "42",
        ] {
            let result = parse_search_response(bad);
            assert!(result.is_err(), "{bad:?} must be an error, not an empty list");
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("not an absence of subtitles"),
                "the error must say it is a failure, not an absence: {msg}"
            );
        }
    }

    #[test]
    fn an_enveloped_response_is_accepted_but_an_unrecognised_one_is_an_error() {
        let enveloped = r#"{"data":[{"id":"1","url":"https://x.invalid/1.srt"}]}"#;
        assert_eq!(parse_search_response(enveloped).unwrap().len(), 1);
        let unknown = r#"{"payload":{"nested":[]}}"#;
        assert!(parse_search_response(unknown).is_err());
    }

    #[test]
    fn errors_never_disclose_the_api_key() {
        // Every error this module can produce is built from SAFE_ENDPOINT and
        // a status/reason — never from a URL. This asserts the property on the
        // constant that enforces it plus the parse-path errors.
        assert!(!SAFE_ENDPOINT.contains("key"));
        assert!(!SAFE_ENDPOINT.contains("http"), "the safe endpoint must not be a URL");

        for bad in ["", "garbage", "{}"] {
            let msg = parse_search_response(bad).unwrap_err().to_string();
            assert!(!msg.contains("key="), "an error leaked a key parameter: {msg}");
            assert!(!msg.contains("sub.wyzie.io"), "an error leaked the request URL: {msg}");
        }

        let msg = decode_subtitle_body(&[0u8, 1, 2]).unwrap_err().to_string();
        assert!(!msg.contains("key="));
    }

    #[test]
    fn the_client_debug_impl_redacts_the_key() {
        let client = WyzieClient::new("https://sub.wyzie.io", "super-secret-key").unwrap();
        let rendered = format!("{client:?}");
        assert!(
            !rendered.contains("super-secret-key"),
            "the key must never reach a Debug rendering: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn the_client_is_none_without_a_configured_key_rather_than_using_a_default() {
        let mut config = Config::default();
        config.wyzie_key = None;
        assert!(
            WyzieClient::from_config(&config).is_none(),
            "no key must mean no provider tier — never a hardcoded fallback credential"
        );

        config.wyzie_key = Some(QbitPassword::from("k".to_string()));
        assert!(WyzieClient::from_config(&config).is_some());
    }

    #[tokio::test]
    async fn searching_without_an_imdb_id_is_a_bad_request_not_a_silent_empty_result() {
        let client = WyzieClient::new("https://sub.wyzie.io", "k").unwrap();
        let err = client.search("  ", "en").await.unwrap_err();
        assert!(matches!(err, MuseError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_non_http_download_url_is_refused_by_the_scheme_guard_specifically() {
        // Defence against a compromised/malformed provider response turning a
        // subtitle fetch into a local file read.
        //
        // Asserting merely `is_err()` is NOT enough, and an earlier version of
        // this test made exactly that mistake: with the scheme guard removed,
        // `file:///etc/passwd` still errors — reqwest refuses the scheme
        // itself — so the test passed while the guard was gone. It must assert
        // that MUSE refused it, by the wording only the guard produces.
        let client = WyzieClient::new("https://sub.wyzie.io", "k").unwrap();

        for url in ["file:///etc/passwd", "ftp://x.invalid/a.srt", "data:text/plain,hi"] {
            let err = client.download(url).await.unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("only http/https"),
                "{url} must be refused by Muse's own scheme guard, not incidentally by the \
                 HTTP client: {msg}"
            );
        }

        // A string that is not a URL at all fails earlier, at parsing.
        let msg = client.download("not a url").await.unwrap_err().to_string();
        assert!(msg.contains("unusable download URL"), "{msg}");
    }

    // ---------- body decoding ----------

    #[test]
    fn decodes_a_utf8_body_and_strips_a_bom() {
        let text = decode_subtitle_body("1\n00:00:20,000 --> 00:00:24,400\nHi\n".as_bytes()).unwrap();
        assert!(text.starts_with('1'));

        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(b"1\n00:00:20,000 --> 00:00:24,400\nHi\n");
        let text = decode_subtitle_body(&with_bom).unwrap();
        assert!(
            text.starts_with('1'),
            "a BOM must be stripped or it rides into the first token: {text:?}"
        );
    }

    #[test]
    fn a_latin1_body_decodes_lossily_rather_than_failing_the_whole_fetch() {
        // 0xE9 is 'é' in Latin-1 and invalid UTF-8. The subtitle is still
        // usable; only that character degrades.
        let body = b"1\n00:00:20,000 --> 00:00:24,400\nCaf\xE9\n";
        let text = decode_subtitle_body(body).unwrap();
        assert!(text.contains("00:00:20,000 --> 00:00:24,400"));
    }

    #[test]
    fn a_binary_or_empty_or_oversized_body_is_refused() {
        assert!(decode_subtitle_body(b"").is_err(), "empty must be an error");
        assert!(
            decode_subtitle_body(b"PK\x03\x04\x00\x00").is_err(),
            "a NUL-bearing binary body must not be parsed as text"
        );
        let huge = vec![b'a'; MAX_SUBTITLE_BYTES + 1];
        assert!(decode_subtitle_body(&huge).is_err(), "the size bound must be enforced");
        let at_limit = vec![b'a'; MAX_SUBTITLE_BYTES];
        assert!(decode_subtitle_body(&at_limit).is_ok(), "the bound is inclusive");
    }

    // ---------- projections ----------

    #[test]
    fn projecting_to_an_available_subtitle_carries_the_machine_generated_flag() {
        let results = parse_search_response(LIVE_SHAPE).unwrap();
        let available = results[1].as_available();
        match &available.source {
            SubtitleSource::Provider {
                provider,
                machine_generated,
                ..
            } => {
                assert_eq!(provider, PROVIDER_NAME);
                assert!(machine_generated, "the AI flag must survive the projection");
            }
            other => panic!("expected a provider source, got {other:?}"),
        }
        assert!(available.hearing_impaired);
        assert_eq!(available.source.preference_rank(), 2, "provider is the last tier");
    }

    #[test]
    fn an_unknown_or_image_format_projects_to_no_shiftable_format() {
        let body = r#"[{"id":"1","url":"https://x.invalid/1.sup","format":"sup"}]"#;
        let results = parse_search_response(body).unwrap();
        assert_eq!(results[0].subtitle_format(), None);
        assert!(
            !results[0].as_available().is_shiftable(),
            "an image-based subtitle must not claim to be shiftable"
        );
    }
}
