//! `subtitle_selections` — which subtitle is active for a media item, where it
//! came from, and what timing offset an operator confirmed (SUBS-01).
//!
//! See `migrations/0110_subtitle_selections.sql` for the invariants the
//! database itself enforces: the source discriminant shape, the
//! offset-implies-confirmation rule, and one-active-per-item-per-language.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::subtitles::cues::SubtitleFormat;
use crate::subtitles::SubtitleSource;

/// The `source` column's permitted values. Kept as constants rather than
/// inline literals so the repo, the model and the CHECK constraint cannot
/// drift apart.
pub const SOURCE_EMBEDDED: &str = "embedded";
pub const SOURCE_SIDECAR: &str = "sidecar";
pub const SOURCE_PROVIDER: &str = "provider";

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SubtitleSelection {
    pub id: i64,
    pub media_item_id: i64,
    pub language: Option<String>,
    pub source: String,

    pub embedded_stream_index: Option<i32>,
    pub embedded_codec: Option<String>,

    pub sidecar_path: Option<String>,

    pub provider: Option<String>,
    pub provider_subtitle_id: Option<String>,
    pub provider_url: Option<String>,
    pub provider_machine_generated: bool,

    pub storage_path: Option<String>,
    /// The subtitle as first obtained, never rewritten by an adjustment.
    ///
    /// Every adjustment derives from this, so `offset_ms` stays absolute and
    /// applying +1000ms then +2000ms yields +2000ms of shift rather than
    /// +3000ms. See `adjustment_source_path`.
    pub original_storage_path: Option<String>,

    /// The APPLIED offset. Only ever non-zero via an operator confirmation.
    pub offset_ms: i64,
    pub offset_confirmed_at: Option<DateTime<Utc>>,

    /// The detector's latest measurement. **Not applied.**
    pub proposed_offset_ms: Option<i64>,
    pub proposed_confidence: Option<String>,
    pub proposed_at: Option<DateTime<Utc>>,

    pub forced: bool,
    pub hearing_impaired: bool,
    pub is_active: bool,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SubtitleSelection {
    /// Reconstruct the typed [`SubtitleSource`] from the discriminant columns.
    ///
    /// `None` when the row's columns do not match its `source` value. That
    /// should be impossible — the migration's CHECK constraint forbids it —
    /// but the read path returns `None` rather than guessing, because a row
    /// that somehow got past the constraint must not be resolved to a
    /// plausible-looking wrong subtitle.
    pub fn typed_source(&self) -> Option<SubtitleSource> {
        match self.source.as_str() {
            SOURCE_EMBEDDED => Some(SubtitleSource::Embedded {
                stream_index: u32::try_from(self.embedded_stream_index?).ok()?,
                codec: self.embedded_codec.clone()?,
            }),
            SOURCE_SIDECAR => Some(SubtitleSource::Sidecar {
                path: self.sidecar_path.clone()?,
            }),
            SOURCE_PROVIDER => Some(SubtitleSource::Provider {
                provider: self.provider.clone()?,
                provider_id: self.provider_subtitle_id.clone()?,
                machine_generated: self.provider_machine_generated,
            }),
            _ => None,
        }
    }

    /// The preference tier this row sits in, or `None` for an unrecognised
    /// source.
    pub fn preference_rank(&self) -> Option<u8> {
        self.typed_source().map(|s| s.preference_rank())
    }

    /// The path holding the subtitle text Muse would read for this selection,
    /// if there is a file at all.
    ///
    /// Prefers `storage_path` (Muse's own copy, possibly re-timed) over
    /// `sidecar_path` (the original in the library). `None` for an embedded
    /// track, whose text lives inside the container and must be extracted
    /// rather than read.
    pub fn readable_path(&self) -> Option<&str> {
        self.storage_path.as_deref().or(self.sidecar_path.as_deref())
    }

    /// The text an adjustment must be computed FROM.
    ///
    /// Deliberately NOT [`Self::readable_path`], which prefers
    /// `storage_path` — that is the currently-serving file, and after one
    /// adjustment it is already shifted. Re-shifting it compounds: +1000ms
    /// then +2000ms would put +3000ms of shift in a row recording 2000.
    /// `offset_ms` is absolute, so the source must be immutable.
    ///
    /// Falls back to `storage_path` only when no original was recorded, which
    /// is the pre-migration case for a row whose offset was already applied.
    /// That fallback is the old compounding behaviour and is why the backfill
    /// only claims rows with `offset_ms = 0`, where storage_path IS pristine.
    pub fn adjustment_source_path(&self) -> Option<&str> {
        self.original_storage_path
            .as_deref()
            .or(self.sidecar_path.as_deref())
            .or(self.storage_path.as_deref())
    }

    /// The text format, inferred from the stored path's extension or the
    /// embedded codec. `None` means Muse cannot re-time this subtitle.
    pub fn format(&self) -> Option<SubtitleFormat> {
        if let Some(path) = self.readable_path() {
            if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
                return SubtitleFormat::from_extension(ext);
            }
        }
        self.embedded_codec
            .as_deref()
            .and_then(crate::subtitles::discover::format_for_codec)
    }

    /// Whether an operator has confirmed a timing adjustment for this
    /// subtitle. Reads the confirmation timestamp, not the offset, because
    /// that is the field that records a human decision.
    pub fn has_confirmed_offset(&self) -> bool {
        self.offset_confirmed_at.is_some() && self.offset_ms != 0
    }
}

/// Fields accepted when recording a subtitle for an item.
///
/// Deliberately has no `offset_ms`: a selection is always created unadjusted.
/// An offset arrives later, through the confirm-an-offset path, and only from
/// an operator — there is no way to construct a pre-shifted selection, which
/// is the type-level half of the same rule the migration's CHECK enforces.
#[derive(Debug, Clone)]
pub struct NewSubtitleSelection {
    pub media_item_id: i64,
    pub language: Option<String>,
    pub source: SubtitleSource,
    pub storage_path: Option<String>,
    pub provider_url: Option<String>,
    pub forced: bool,
    pub hearing_impaired: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_row(storage: Option<&str>, original: Option<&str>) -> SubtitleSelection {
        SubtitleSelection {
            id: 1,
            media_item_id: 7,
            language: Some("en".into()),
            source: "provider".into(),
            embedded_stream_index: None,
            embedded_codec: None,
            sidecar_path: None,
            provider: Some("wyzie".into()),
            provider_subtitle_id: Some("x1".into()),
            provider_url: None,
            provider_machine_generated: false,
            storage_path: storage.map(str::to_string),
            original_storage_path: original.map(str::to_string),
            offset_ms: 0,
            offset_confirmed_at: None,
            proposed_offset_ms: None,
            proposed_confidence: None,
            proposed_at: None,
            forced: false,
            hearing_impaired: false,
            is_active: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Codex, SUBS-01 gate: applying an offset twice compounded.
    ///
    /// After a +1000ms adjustment, `storage_path` points at the SHIFTED copy.
    /// A later +2000ms request that read through `readable_path()` shifted
    /// that already-shifted text again — the file held +3000ms while the row
    /// recorded 2000. `offset_ms` is absolute, so every adjustment has to be
    /// derived from the same pristine text.
    #[test]
    fn an_adjustment_is_always_derived_from_the_pristine_original() {
        // The state after one adjustment: storage_path is the shifted copy,
        // original_storage_path still points at the download.
        let row = provider_row(
            Some("/store/sel-1.en.+1000.srt"),
            Some("/store/sel-1.en.original.srt"),
        );
        assert_eq!(
            row.adjustment_source_path(),
            Some("/store/sel-1.en.original.srt"),
            "a second adjustment must start from the original, not the shifted copy"
        );
        // ...while the serving path is still the adjusted file.
        assert_eq!(row.readable_path(), Some("/store/sel-1.en.+1000.srt"));
    }

    /// A sidecar's immutable original is the sidecar itself, which lives in
    /// the read-only library and is never rewritten.
    #[test]
    fn a_sidecar_adjustment_derives_from_the_sidecar_not_the_adjusted_copy() {
        let mut row = provider_row(Some("/store/adjusted.srt"), None);
        row.source = "sidecar".into();
        row.provider = None;
        row.provider_subtitle_id = None;
        row.sidecar_path = Some("/library/Movie/Movie.en.srt".into());
        assert_eq!(
            row.adjustment_source_path(),
            Some("/library/Movie/Movie.en.srt")
        );
    }

    /// The pre-migration fallback, stated rather than silent: a row adjusted
    /// before `original_storage_path` existed has no recoverable original, so
    /// it falls back to the old behaviour. The backfill deliberately claims
    /// only unadjusted rows, where storage_path IS pristine.
    #[test]
    fn a_row_with_no_recorded_original_falls_back_rather_than_returning_none() {
        let row = provider_row(Some("/store/only.srt"), None);
        assert_eq!(row.adjustment_source_path(), Some("/store/only.srt"));
    }
}
