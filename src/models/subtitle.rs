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
