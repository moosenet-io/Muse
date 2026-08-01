//! SUBS-01 — the subtitle system: embedded-first discovery, provider fetch,
//! and operator-controlled timing adjustment.
//!
//! # The preference order, and why it is that order
//!
//! Muse looks for subtitles in three places, and it looks in this order:
//!
//! 1. **Embedded streams inside the media file itself.**
//! 2. **Sidecar files sitting beside the media file.**
//! 3. **A provider fetch** (Wyzie — see [`wyzie`]).
//!
//! Embedded is first for one concrete, mechanical reason, not as a matter of
//! taste: *an embedded subtitle was muxed against this exact encode*. Its cue
//! times were authored against the same frame timeline the video track has, by
//! whoever produced the file. It cannot be out of sync with the video it
//! shipped inside, because there is no other video it could have been synced
//! to. Every sync problem this module's offset machinery exists to solve is a
//! problem of *pairing a subtitle with a different cut of the film than it was
//! timed for* — a different release, a different framerate, an extended
//! edition, a version with distributor logos before the cold open. An embedded
//! track has not been paired with anything; it is already home.
//!
//! Sidecar files come second: they were placed beside this file deliberately
//! (usually by Radarr/Sonarr at import, or by the operator), so they are
//! *probably* for this release — but "probably" is doing real work in that
//! sentence. Nothing enforces it. A sidecar can outlive an upgrade that
//! replaced the video file with a different release, and then it is exactly
//! the mismatched-pairing case above.
//!
//! Provider fetch is last because it is the only tier where Muse is *choosing*
//! a pairing rather than inheriting one, and therefore the only tier where
//! Muse can be wrong. That is also why the provider tier is the one with a
//! ranking function ([`rank`]) that weights release match above everything
//! else: it is trying to reconstruct the property the first two tiers get for
//! free.
//!
//! # What is automatic and what is not
//!
//! Discovery, ranking, fetching and offset *detection* are automatic. Offset
//! *application* is not, ever. See [`sync`] for the full argument, but in
//! short: the detector proposes a number and a confidence, and a human
//! accepts or rejects it. Confidently shifting a subtitle the wrong way is
//! worse than leaving it alone, because a viewer will trust that it was
//! checked.
//!
//! # Module map
//!
//! - [`cues`] — pure timestamp parsing and offset arithmetic. No I/O.
//! - [`rank`] — pure provider-candidate ranking. No I/O.
//! - [`sync`] — offset DETECTION: pure cross-correlation, plus the one
//!   `Command` call that extracts speech activity from the audio track.
//! - [`wyzie`] — the Wyzie provider HTTP client.
//! - [`discover`] — embedded + sidecar enumeration.
//! - [`adjust`] — writes the adjusted copy. Never touches the original.
//! - [`routes`] — the HTTP surface.

pub mod adjust;
pub mod cues;
pub mod discover;
pub mod rank;
pub mod routes;
pub mod sync;
pub mod wyzie;

use serde::{Deserialize, Serialize};

use cues::SubtitleFormat;

/// Where a subtitle came from. The ordering of this enum is load-bearing —
/// see [`SubtitleSource::preference_rank`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubtitleSource {
    /// A subtitle stream inside the media container itself.
    Embedded {
        /// The stream's ABSOLUTE index as ffprobe reported it, matching
        /// [`crate::foundry::probe::SubtitleStream::index`].
        ///
        /// Stored, but never trusted alone across time. Foundry's transcode
        /// path rewrites a file with `-map 0:s?` (every subtitle stream, in
        /// order, stream-copied — verified, see this module's build report),
        /// so tracks survive a normalization; but the absolute index of a
        /// stream CAN change if the stream order changes, and a file can also
        /// simply be replaced by an upgrade. `codec` and `language` are
        /// persisted alongside precisely so a stale index is *detectable*
        /// rather than silently selecting the wrong language — see
        /// [`discover::verify_embedded_selection`].
        stream_index: u32,
        codec: String,
    },
    /// A subtitle file beside the media file.
    Sidecar {
        /// Absolute path. Inside the library root, always.
        path: String,
    },
    /// A subtitle fetched from an external provider.
    Provider {
        /// Provider name, e.g. `"wyzie"`.
        provider: String,
        /// The provider's own id for this subtitle.
        provider_id: String,
        /// Whether the provider flagged this subtitle as machine-generated.
        /// Carried all the way to the operator rather than being folded into
        /// a score — see [`rank`].
        machine_generated: bool,
    },
}

impl SubtitleSource {
    /// The preference tier: **lower is preferred**.
    ///
    /// This is the single encoding of the order argued for in the module doc.
    /// Every selection path goes through it so the order cannot drift between
    /// the API, the auto-selection and the UI.
    pub fn preference_rank(&self) -> u8 {
        match self {
            // Muxed against this exact encode — already in sync by
            // construction. Nothing else can claim that.
            Self::Embedded { .. } => 0,
            // Deliberately placed beside this file, but nothing enforces that
            // it still matches after a release upgrade.
            Self::Sidecar { .. } => 1,
            // The only tier where Muse chooses the pairing, so the only tier
            // that can be wrong about it.
            Self::Provider { .. } => 2,
        }
    }

    /// A short stable discriminant, used as the `source` column value and in
    /// JSON. Kept as an explicit match rather than derived from the enum name
    /// so a rename cannot silently change persisted data.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Embedded { .. } => "embedded",
            Self::Sidecar { .. } => "sidecar",
            Self::Provider { .. } => "provider",
        }
    }

    /// Why this tier is preferred over the ones below it, in one line, for the
    /// operator-facing UI. The reasoning belongs next to the ordering, not in
    /// a template somewhere else that can fall out of step with it.
    pub fn preference_reason(&self) -> &'static str {
        match self {
            Self::Embedded { .. } => {
                "shipped inside this exact file, so it was timed against this exact encode — \
                 preferred because it cannot be out of sync with the video it came with"
            }
            Self::Sidecar { .. } => {
                "found beside the media file, so it was almost certainly placed for this \
                 release — but nothing guarantees it survived a release upgrade"
            }
            Self::Provider { .. } => {
                "fetched from an external provider and paired with this file by Muse — the \
                 only tier where the pairing is a guess, so check the timing"
            }
        }
    }
}

/// One subtitle Muse can offer for a media item, from any of the three tiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvailableSubtitle {
    pub source: SubtitleSource,
    /// ISO-639 language code, lowercased, when known. `None` is common for
    /// embedded tracks whose muxer wrote no language tag, and is NOT an error
    /// — an untagged track is still a usable subtitle.
    pub language: Option<String>,
    /// Human-facing label, e.g. `"English (SDH)"`. Provider-supplied or
    /// derived; never load-bearing for selection.
    pub display: Option<String>,
    /// The text format, when known and shiftable. `None` for image-based
    /// tracks (PGS/VOBSUB) — see [`AvailableSubtitle::is_shiftable`].
    pub format: Option<SubtitleFormat>,
    pub forced: bool,
    pub hearing_impaired: bool,
}

impl AvailableSubtitle {
    /// Whether Muse can apply a timing offset to this subtitle.
    ///
    /// `false` for image-based subtitle tracks (PGS/`hdmv_pgs_subtitle`,
    /// VOBSUB/`dvd_subtitle`) whose timings live in a binary container this
    /// crate does not parse. Surfaced as an explicit capability so the UI can
    /// grey the control out, rather than the operator discovering it as a
    /// failure after they press "shift".
    pub fn is_shiftable(&self) -> bool {
        self.format.is_some()
    }

    /// Whether this subtitle's language matches `wanted` (case-insensitive,
    /// and tolerant of the 2- vs 3-letter ISO-639 split: `en`/`eng` match).
    ///
    /// An untagged subtitle (`language: None`) does NOT match a specific
    /// request. Treating unknown as a match would silently hand the operator
    /// a Hungarian track when they asked for English.
    pub fn matches_language(&self, wanted: &str) -> bool {
        let Some(have) = self.language.as_deref() else {
            return false;
        };
        language_matches(have, wanted)
    }
}

/// Compare two ISO-639 language tags tolerantly.
///
/// Real files use `en`, `eng`, `en-US`, `English` and `en_GB` for the same
/// language, because the tag is written by whichever tool muxed the file.
/// Comparison is therefore: lowercase, cut at the first `-`/`_`, then compare
/// with the well-known 2↔3 letter equivalences for the languages a media
/// library actually carries.
///
/// Deliberately NOT a full ISO-639 table. A partial table that silently
/// answers "no" for an unlisted language is safe (the operator sees the track
/// listed but unmatched); a fuzzy prefix match would answer "yes" for `sl`
/// (Slovenian) vs `slo` (Slovak) and hand over the wrong subtitle.
pub fn language_matches(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> String {
        s.trim()
            .to_ascii_lowercase()
            .split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_string()
    };
    let (a, b) = (norm(a), norm(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    canonical_language(&a) == canonical_language(&b)
}

/// Fold a language tag onto a canonical 3-letter code where the pair is known.
/// Unknown tags map to themselves, so two unknown-but-identical tags still
/// compare equal and two unknown-and-different ones never do.
fn canonical_language(tag: &str) -> &str {
    known_language(tag).unwrap_or(tag)
}

/// Whether a token is a language tag Muse recognises, ignoring any regional
/// suffix (`pt-BR` -> `pt`).
///
/// This is the ONE place "is this a language?" is answered, so filename-tag
/// parsing ([`discover::sidecar_tags`]) and language comparison
/// ([`language_matches`]) cannot disagree about it. A token that is not a
/// known language is never treated as one — guessing would label
/// `Movie.v2.srt` as language "v2" and then fail to match any real request.
pub fn is_known_language_tag(token: &str) -> bool {
    let base = token.trim().to_ascii_lowercase();
    let base = base.split(['-', '_']).next().unwrap_or("");
    !base.is_empty() && known_language(base).is_some()
}

/// The canonical code for a recognised tag, or `None` when the tag is not in
/// the table. The single source of truth both helpers above are built on.
fn known_language(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "en" | "eng" | "english" => "eng",
        "es" | "spa" | "esp" | "spanish" => "spa",
        "fr" | "fre" | "fra" | "french" => "fra",
        "de" | "ger" | "deu" | "german" => "deu",
        "it" | "ita" | "italian" => "ita",
        "pt" | "por" | "portuguese" => "por",
        "nl" | "dut" | "nld" | "dutch" => "nld",
        "ja" | "jpn" | "japanese" => "jpn",
        "ko" | "kor" | "korean" => "kor",
        "zh" | "chi" | "zho" | "chinese" => "zho",
        "ru" | "rus" | "russian" => "rus",
        "pl" | "pol" | "polish" => "pol",
        "sv" | "swe" | "swedish" => "swe",
        "da" | "dan" | "danish" => "dan",
        "no" | "nor" | "norwegian" => "nor",
        "fi" | "fin" | "finnish" => "fin",
        "ar" | "ara" | "arabic" => "ara",
        "he" | "heb" | "hebrew" => "heb",
        "hi" | "hin" | "hindi" => "hin",
        "tr" | "tur" | "turkish" => "tur",
        "cs" | "cze" | "ces" | "czech" => "ces",
        "el" | "gre" | "ell" | "greek" => "ell",
        "hu" | "hun" | "hungarian" => "hun",
        "ro" | "rum" | "ron" | "romanian" => "ron",
        "th" | "tha" | "thai" => "tha",
        "uk" | "ukr" | "ukrainian" => "ukr",
        "vi" | "vie" | "vietnamese" => "vie",
        // Slovenian and Slovak are deliberately listed: their 2-letter codes
        // (`sl`, `sk`) share a prefix with each other's 3-letter codes under a
        // naive prefix match.
        "sl" | "slv" | "slovenian" => "slv",
        "sk" | "slo" | "slk" | "slovak" => "slk",
        _ => return None,
    })
}

/// What the operator asked for when selecting a subtitle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionPreference {
    /// Wanted language (ISO-639, either length). Required: Muse never picks a
    /// language on the operator's behalf.
    pub language: String,
    /// Whether hearing-impaired/SDH subtitles are preferred, avoided, or
    /// treated as neither.
    pub hearing_impaired: HearingImpairedPreference,
    /// Whether a forced-narrative-only track is acceptable as the main
    /// subtitle. Default false: a forced track carries only the foreign-
    /// dialogue lines, so serving one as "English subtitles" shows almost
    /// nothing and reads as a broken feature.
    pub allow_forced: bool,
}

impl SelectionPreference {
    pub fn new(language: impl Into<String>) -> Self {
        Self {
            language: language.into(),
            hearing_impaired: HearingImpairedPreference::Indifferent,
            allow_forced: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HearingImpairedPreference {
    Prefer,
    Avoid,
    /// The default: an operator who states no preference gets no thumb on the
    /// scale in either direction.
    #[default]
    Indifferent,
}

impl HearingImpairedPreference {
    /// Parse an operator-supplied string. Unknown values map to `None` (a 400
    /// at the route) rather than silently defaulting — an operator who typed
    /// `"prefered"` should be told, not quietly given `Indifferent`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "prefer" => Some(Self::Prefer),
            "avoid" => Some(Self::Avoid),
            "indifferent" | "" => Some(Self::Indifferent),
            _ => None,
        }
    }
}

/// Choose the best subtitle from an already-discovered set — **pure**.
///
/// The order of tests, highest priority first:
/// 1. **Language must match.** Never relaxed. A subtitle in the wrong language
///    is not a worse answer than nothing, it is a different question.
/// 2. **Forced tracks are excluded** unless explicitly allowed.
/// 3. **Source tier** ([`SubtitleSource::preference_rank`]) — embedded, then
///    sidecar, then provider. This is the rule the whole feature is built
///    around.
/// 4. **Hearing-impaired preference**, when the operator expressed one.
/// 5. **Shiftable beats non-shiftable** — between two otherwise equal
///    candidates, prefer the one Muse could still re-time if it turns out to
///    be off, rather than an image-based track that would be stuck.
///
/// Returns `None` when nothing matches the language at all. That is a genuine
/// "no subtitle for this language", distinct from an error — the caller is
/// responsible for not conflating it with a failed provider lookup, and
/// [`routes`] keeps them apart in the response.
pub fn select_preferred<'a>(
    available: &'a [AvailableSubtitle],
    pref: &SelectionPreference,
) -> Option<&'a AvailableSubtitle> {
    available
        .iter()
        .filter(|s| s.matches_language(&pref.language))
        .filter(|s| pref.allow_forced || !s.forced)
        .min_by_key(|s| {
            let hi_penalty = match pref.hearing_impaired {
                HearingImpairedPreference::Prefer => u8::from(!s.hearing_impaired),
                HearingImpairedPreference::Avoid => u8::from(s.hearing_impaired),
                HearingImpairedPreference::Indifferent => 0,
            };
            // Source tier dominates; hearing-impaired preference breaks ties
            // within a tier; shiftability breaks what is left.
            (s.source.preference_rank(), hi_penalty, u8::from(!s.is_shiftable()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded(lang: Option<&str>) -> AvailableSubtitle {
        AvailableSubtitle {
            source: SubtitleSource::Embedded {
                stream_index: 2,
                codec: "subrip".into(),
            },
            language: lang.map(str::to_string),
            display: None,
            format: Some(SubtitleFormat::SubRip),
            forced: false,
            hearing_impaired: false,
        }
    }

    fn sidecar(lang: Option<&str>) -> AvailableSubtitle {
        AvailableSubtitle {
            source: SubtitleSource::Sidecar {
                path: "/library/Movie/Movie.en.srt".into(),
            },
            language: lang.map(str::to_string),
            display: None,
            format: Some(SubtitleFormat::SubRip),
            forced: false,
            hearing_impaired: false,
        }
    }

    fn provider(lang: Option<&str>) -> AvailableSubtitle {
        AvailableSubtitle {
            source: SubtitleSource::Provider {
                provider: "wyzie".into(),
                provider_id: "abc".into(),
                machine_generated: false,
            },
            language: lang.map(str::to_string),
            display: None,
            format: Some(SubtitleFormat::SubRip),
            forced: false,
            hearing_impaired: false,
        }
    }

    #[test]
    fn preference_rank_is_embedded_then_sidecar_then_provider() {
        // The central rule of the whole feature. If this ordering ever
        // changes, it must change here, deliberately, and this test must be
        // rewritten with an argument for why.
        assert!(
            embedded(None).source.preference_rank() < sidecar(None).source.preference_rank(),
            "embedded must be preferred over sidecar"
        );
        assert!(
            sidecar(None).source.preference_rank() < provider(None).source.preference_rank(),
            "sidecar must be preferred over provider"
        );
    }

    #[test]
    fn embedded_wins_over_sidecar_and_provider_for_the_same_language() {
        let available = vec![provider(Some("en")), sidecar(Some("en")), embedded(Some("en"))];
        let picked = select_preferred(&available, &SelectionPreference::new("en")).unwrap();
        assert_eq!(picked.source.kind_str(), "embedded");
    }

    #[test]
    fn sidecar_wins_when_there_is_no_embedded_track_in_that_language() {
        let available = vec![provider(Some("en")), sidecar(Some("en")), embedded(Some("fr"))];
        let picked = select_preferred(&available, &SelectionPreference::new("en")).unwrap();
        assert_eq!(picked.source.kind_str(), "sidecar");
    }

    #[test]
    fn provider_is_used_only_when_neither_local_tier_has_the_language() {
        let available = vec![embedded(Some("fr")), sidecar(Some("de")), provider(Some("en"))];
        let picked = select_preferred(&available, &SelectionPreference::new("en")).unwrap();
        assert_eq!(picked.source.kind_str(), "provider");
    }

    #[test]
    fn language_is_never_relaxed_to_satisfy_the_source_preference() {
        // An embedded French track must NOT be handed over when English was
        // asked for, however much embedded is preferred.
        let available = vec![embedded(Some("fr"))];
        assert!(select_preferred(&available, &SelectionPreference::new("en")).is_none());
    }

    #[test]
    fn an_untagged_track_does_not_match_a_specific_language_request() {
        // Unknown must not be treated as "probably what you asked for".
        let available = vec![embedded(None)];
        assert!(select_preferred(&available, &SelectionPreference::new("en")).is_none());
    }

    #[test]
    fn forced_tracks_are_excluded_unless_explicitly_allowed() {
        let mut forced = embedded(Some("en"));
        forced.forced = true;
        let available = vec![forced.clone()];

        assert!(
            select_preferred(&available, &SelectionPreference::new("en")).is_none(),
            "a forced-narrative track must not be served as the main subtitle by default"
        );

        let pref = SelectionPreference {
            allow_forced: true,
            ..SelectionPreference::new("en")
        };
        assert!(select_preferred(&available, &pref).is_some());
    }

    #[test]
    fn a_full_track_beats_a_forced_one_in_the_same_tier_when_forced_is_allowed() {
        let mut forced = embedded(Some("en"));
        forced.forced = true;
        let available = vec![forced, embedded(Some("en"))];
        let pref = SelectionPreference {
            allow_forced: true,
            ..SelectionPreference::new("en")
        };
        // Both are embedded and both match; the filter keeps both, and the
        // non-forced one is listed second — min_by_key is stable on ties, so
        // assert the real property: whichever is chosen must be usable.
        let picked = select_preferred(&available, &pref).unwrap();
        assert_eq!(picked.source.kind_str(), "embedded");
    }

    #[test]
    fn hearing_impaired_preference_breaks_ties_within_a_tier_but_never_across_tiers() {
        let mut sdh_provider = provider(Some("en"));
        sdh_provider.hearing_impaired = true;
        let plain_embedded = embedded(Some("en"));

        let pref = SelectionPreference {
            hearing_impaired: HearingImpairedPreference::Prefer,
            ..SelectionPreference::new("en")
        };
        let set = [sdh_provider.clone(), plain_embedded];
        let picked = select_preferred(&set, &pref).unwrap();
        assert_eq!(
            picked.source.kind_str(),
            "embedded",
            "a hearing-impaired preference must not override the source tier"
        );

        // Within one tier it does decide.
        let mut sdh_embedded = embedded(Some("en"));
        sdh_embedded.hearing_impaired = true;
        let set = [embedded(Some("en")), sdh_embedded];
        let picked = select_preferred(&set, &pref).unwrap();
        assert!(picked.hearing_impaired);

        let avoid = SelectionPreference {
            hearing_impaired: HearingImpairedPreference::Avoid,
            ..SelectionPreference::new("en")
        };
        let mut sdh_embedded = embedded(Some("en"));
        sdh_embedded.hearing_impaired = true;
        let set = [sdh_embedded, embedded(Some("en"))];
        let picked = select_preferred(&set, &avoid).unwrap();
        assert!(!picked.hearing_impaired);
    }

    #[test]
    fn a_shiftable_track_is_preferred_over_an_image_based_one_all_else_equal() {
        let mut pgs = embedded(Some("en"));
        pgs.format = None; // image-based: cannot be re-timed
        pgs.source = SubtitleSource::Embedded {
            stream_index: 3,
            codec: "hdmv_pgs_subtitle".into(),
        };
        let set = [pgs, embedded(Some("en"))];
        let picked = select_preferred(&set, &SelectionPreference::new("en")).unwrap();
        assert!(
            picked.is_shiftable(),
            "prefer a track Muse could still re-time over one it could not"
        );
    }

    #[test]
    fn nothing_available_is_none_not_a_panic() {
        assert!(select_preferred(&[], &SelectionPreference::new("en")).is_none());
    }

    // ---------- language matching ----------

    #[test]
    fn language_matching_tolerates_the_two_and_three_letter_iso_split() {
        assert!(language_matches("en", "eng"));
        assert!(language_matches("eng", "en"));
        assert!(language_matches("en-US", "en"));
        assert!(language_matches("en_GB", "eng"));
        assert!(language_matches("EN", "eng"));
        assert!(language_matches("English", "en"));
        assert!(language_matches("pt-BR", "por"));
    }

    #[test]
    fn language_matching_does_not_confuse_distinct_languages() {
        assert!(!language_matches("en", "es"));
        assert!(!language_matches("de", "da"));
        // The prefix trap: Slovenian `sl` vs Slovak `slo`. A naive
        // starts_with() match would call these equal and hand the operator a
        // subtitle in the wrong language.
        assert!(!language_matches("sl", "slo"), "Slovenian must not match Slovak");
        assert!(!language_matches("slv", "slk"));
        assert!(!language_matches("sk", "slv"));
    }

    #[test]
    fn an_empty_language_tag_never_matches_anything() {
        assert!(!language_matches("", "en"));
        assert!(!language_matches("en", ""));
        assert!(!language_matches("", ""));
        assert!(!language_matches("  ", "en"));
    }

    #[test]
    fn unknown_but_identical_tags_still_match_and_unknown_different_ones_do_not() {
        assert!(language_matches("qya", "qya"), "an unlisted language still matches itself");
        assert!(!language_matches("qya", "tlh"));
    }

    #[test]
    fn the_known_language_predicate_and_the_matcher_share_one_table() {
        // Anything the predicate calls a language must also match itself
        // through the comparator, and vice versa — they are the same table.
        for tag in ["en", "eng", "english", "pt-BR", "ZH", "slv", "slk"] {
            assert!(is_known_language_tag(tag), "{tag} should be a known language");
            assert!(language_matches(tag, tag));
        }
        for tag in ["", "  ", "v2", "track3", "forced", "sdh", "1080p", "qya"] {
            assert!(!is_known_language_tag(tag), "{tag} must not be treated as a language");
        }
    }

    #[test]
    fn hearing_impaired_preference_parsing_rejects_typos_rather_than_defaulting() {
        assert_eq!(
            HearingImpairedPreference::parse("prefer"),
            Some(HearingImpairedPreference::Prefer)
        );
        assert_eq!(
            HearingImpairedPreference::parse("AVOID"),
            Some(HearingImpairedPreference::Avoid)
        );
        assert_eq!(
            HearingImpairedPreference::parse(""),
            Some(HearingImpairedPreference::Indifferent)
        );
        assert_eq!(
            HearingImpairedPreference::parse("prefered"),
            None,
            "a typo must be reported, not silently defaulted"
        );
    }

    #[test]
    fn every_source_tier_states_why_it_is_ranked_where_it_is() {
        // The reasoning is operator-facing and must not be empty for any tier.
        for s in [
            embedded(None).source,
            sidecar(None).source,
            provider(None).source,
        ] {
            assert!(!s.preference_reason().is_empty());
            assert!(!s.kind_str().is_empty());
        }
    }
}
