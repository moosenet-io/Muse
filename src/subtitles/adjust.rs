//! SUBS-01 — persisting fetched and re-timed subtitles.
//!
//! # The one rule this module exists to enforce
//!
//! **An original subtitle is never rewritten in place.** Applying an offset
//! produces a NEW file in Muse's own subtitle store; the source — an embedded
//! stream, a sidecar the operator or Radarr put there, or the untouched
//! provider download — is left exactly as it was.
//!
//! That is not politeness. An offset is a judgement call made from a
//! measurement with a stated confidence (see [`super::sync`]), and judgement
//! calls get revised. If the shift was wrong, the operator needs the original
//! back; if it was right, they may still want to re-derive from the original
//! after a better measurement. Overwriting destroys both options, and it
//! destroys them silently, at the exact moment the operator is least likely to
//! be watching — they asked for a small timing tweak, not a data migration.
//!
//! It also matters because the *arr fleet on this deployment has `recycleBin:
//! ""` on every instance: there is no undo anywhere else in the pipeline.
//!
//! # Where things are written
//!
//! Only ever under `MUSE_SUBTITLE_STORE_DIR`, never into the library root. The
//! library is a read-only boundary throughout this crate
//! ([`crate::library::sidecar`], [`crate::library::scan`]), and a subtitle
//! feature is not a good enough reason to be the first module to breach it.
//! With no store configured, this module refuses to write and says so — it
//! does not fall back to writing beside the media file.

use std::path::{Path, PathBuf};

use crate::error::{MuseError, MuseResult};

use super::cues::{apply_offset, OffsetApplied, SubtitleFormat};

/// The result of persisting an adjusted subtitle.
#[derive(Debug, Clone, PartialEq)]
pub struct AdjustedSubtitle {
    /// Absolute path of the NEW file. Never the source path.
    pub path: PathBuf,
    /// What the pure offset pass did.
    pub applied: OffsetApplied,
}

/// Build the store-relative filename for a subtitle.
/// **Pure**, so the naming scheme is testable without a filesystem.
///
/// Shape: `<media_item_id>.<language>.<discriminator>[.offset<±ms>].<ext>`
///
/// The offset is IN THE NAME, deliberately. An operator listing the store can
/// see at a glance which files are shifted and by how much, and a
/// re-derivation at a different offset lands on a different filename rather
/// than clobbering the previous attempt — so two proposals can be compared
/// side by side instead of one silently replacing the other.
pub fn store_filename(
    media_item_id: i64,
    language: &str,
    discriminator: &str,
    offset_ms: i64,
    format: SubtitleFormat,
) -> String {
    let language = sanitize_component(language);
    let discriminator = sanitize_component(discriminator);
    let offset_part = if offset_ms == 0 {
        String::new()
    } else if offset_ms > 0 {
        format!(".offset+{offset_ms}ms")
    } else {
        format!(".offset{offset_ms}ms")
    };
    format!(
        "{media_item_id}.{language}.{discriminator}{offset_part}.{}",
        format.extension()
    )
}

/// Reduce a filename component to a safe token. **Pure.**
///
/// Everything that is not an ASCII alphanumeric, `-` or `_` becomes `_`. This
/// is a hard allowlist, not an escape of known-bad characters: the components
/// include a provider-supplied id and a language string, and neither is
/// trusted input. A denylist that forgot one separator would let a provider id
/// like `../../etc/cron.d/x` escape the store directory entirely.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Bound the length so a hostile id cannot blow past the filesystem's own
    // name limit and make the write fail in a confusing way.
    let bounded: String = cleaned.chars().take(48).collect();
    if bounded.is_empty() {
        "unknown".to_string()
    } else {
        bounded
    }
}

/// Resolve the configured subtitle store, creating it if needed.
///
/// `None` in config is a hard error here rather than a fallback: writing into
/// the library instead would breach the read-only boundary the rest of the
/// crate maintains, and doing it as an unannounced fallback is the worst way
/// to breach it.
pub fn resolve_store(store_dir: Option<&str>) -> MuseResult<PathBuf> {
    let Some(dir) = store_dir.map(str::trim).filter(|d| !d.is_empty()) else {
        return Err(MuseError::Config(
            "subtitles: MUSE_SUBTITLE_STORE_DIR is not set, so Muse has nowhere to write a \
             fetched or re-timed subtitle — it will NOT write into the library root instead"
                .into(),
        ));
    };
    let path = PathBuf::from(dir);
    std::fs::create_dir_all(&path).map_err(|e| {
        MuseError::Config(format!(
            "subtitles: the subtitle store {} could not be created: {e}",
            path.display()
        ))
    })?;
    Ok(path)
}

/// Write `text` into the store under `filename`.
///
/// Refuses to overwrite an existing file — `create_new(true)`. A collision
/// means two different subtitles claimed the same identity, which is a bug in
/// the naming scheme, and silently overwriting would destroy whichever one
/// landed first.
fn write_new(store: &Path, filename: &str, text: &str) -> MuseResult<PathBuf> {
    use std::io::Write;

    let path = store.join(filename);
    // Defence in depth against a sanitizer bug: the resolved path must still
    // be a direct child of the store.
    if path.parent() != Some(store) {
        return Err(MuseError::Internal(anyhow::anyhow!(
            "subtitles: refusing to write outside the subtitle store"
        )));
    }

    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(MuseError::Conflict(format!(
                "subtitles: {} already exists — refusing to overwrite an existing subtitle",
                path.display()
            )))
        }
        Err(e) => {
            return Err(MuseError::upstream(format!(
                "subtitles: could not create {}: {e}",
                path.display()
            )))
        }
    };
    file.write_all(text.as_bytes())
        .map_err(|e| MuseError::upstream(format!("subtitles: could not write {}: {e}", path.display())))?;
    file.sync_all()
        .map_err(|e| MuseError::upstream(format!("subtitles: could not flush {}: {e}", path.display())))?;
    Ok(path)
}

/// Store a subtitle exactly as fetched, with no offset applied.
///
/// This is the pristine copy every later adjustment is derived FROM, which is
/// what makes an adjustment reversible.
pub fn store_original(
    store_dir: Option<&str>,
    media_item_id: i64,
    language: &str,
    discriminator: &str,
    format: SubtitleFormat,
    text: &str,
) -> MuseResult<PathBuf> {
    let store = resolve_store(store_dir)?;
    let filename = store_filename(media_item_id, language, discriminator, 0, format);
    write_new(&store, &filename, text)
}

/// Apply an operator-confirmed offset and write the result as a NEW file.
///
/// `source_text` is read by the caller from wherever the subtitle lives; this
/// function never opens the source, so it structurally cannot write back to
/// it. The returned [`AdjustedSubtitle::path`] is always a fresh file in the
/// store.
///
/// A zero offset is rejected: it would create a duplicate of the original
/// under a second name and record an "adjustment" that adjusted nothing.
pub fn write_adjusted(
    store_dir: Option<&str>,
    media_item_id: i64,
    language: &str,
    discriminator: &str,
    format: SubtitleFormat,
    source_text: &str,
    offset_ms: i64,
) -> MuseResult<AdjustedSubtitle> {
    if offset_ms == 0 {
        return Err(MuseError::BadRequest(
            "subtitles: an offset of 0ms would not change anything — nothing was written".into(),
        ));
    }

    // The pure pass first: if the subtitle is malformed, nothing is written at
    // all, so a failed adjustment cannot leave a half-file in the store.
    let applied = apply_offset(source_text, format, offset_ms)
        .map_err(|e| MuseError::BadRequest(format!("subtitles: {e}")))?;

    let store = resolve_store(store_dir)?;
    let filename = store_filename(media_item_id, language, discriminator, offset_ms, format);
    let path = write_new(&store, &filename, &applied.text)?;

    Ok(AdjustedSubtitle { path, applied })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SRT: &str = "1\n00:00:20,000 --> 00:00:24,400\nHello there.\n";

    fn unique_store(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("muse-subs-store-{name}-{}", uuid::Uuid::new_v4().simple()));
        dir
    }

    // ---------- naming ----------

    #[test]
    fn the_filename_records_the_offset_so_two_attempts_do_not_collide() {
        let a = store_filename(7, "en", "wyzie-123", 0, SubtitleFormat::SubRip);
        let b = store_filename(7, "en", "wyzie-123", 2_500, SubtitleFormat::SubRip);
        let c = store_filename(7, "en", "wyzie-123", -2_500, SubtitleFormat::SubRip);
        assert_eq!(a, "7.en.wyzie-123.srt");
        assert_eq!(b, "7.en.wyzie-123.offset+2500ms.srt");
        assert_eq!(c, "7.en.wyzie-123.offset-2500ms.srt");
        assert_ne!(a, b);
        assert_ne!(b, c, "a +2500 and a -2500 attempt must not share a filename");
    }

    #[test]
    fn filename_components_are_allowlisted_so_a_hostile_id_cannot_escape_the_store() {
        // The provider id is untrusted input. A denylist that missed a
        // separator would let this reach outside the store directory.
        let name = store_filename(1, "../../etc", "../../../cron.d/evil", 0, SubtitleFormat::SubRip);
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert!(!name.contains('\\'), "{name}");

        for hostile in ["../x", "a/b", "a\\b", "a\0b", "..", "."] {
            let component = sanitize_component(hostile);
            assert!(!component.contains('/'), "{hostile} -> {component}");
            assert!(!component.contains('\\'), "{hostile} -> {component}");
            assert!(!component.contains('\0'), "{hostile} -> {component}");
            assert!(!component.contains(".."), "{hostile} -> {component}");
        }
    }

    #[test]
    fn an_empty_or_overlong_component_is_bounded_not_passed_through() {
        assert_eq!(sanitize_component(""), "unknown");
        assert_eq!(sanitize_component("   "), "unknown");
        assert_eq!(sanitize_component("!!!"), "___");
        assert!(sanitize_component(&"a".repeat(500)).len() <= 48);
    }

    // ---------- the never-in-place rule ----------

    #[test]
    fn applying_an_offset_writes_a_new_file_and_leaves_the_source_untouched() {
        // THE rule of this module.
        let store = unique_store("never-in-place");
        let source_dir = unique_store("never-in-place-src");
        fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("Movie.en.srt");
        fs::write(&source_path, SRT).unwrap();

        let source_text = fs::read_to_string(&source_path).unwrap();
        let adjusted = write_adjusted(
            store.to_str(),
            42,
            "en",
            "sidecar",
            SubtitleFormat::SubRip,
            &source_text,
            2_500,
        )
        .unwrap();

        assert_ne!(adjusted.path, source_path);
        assert!(adjusted.path.starts_with(&store), "the adjusted copy must live in the store");
        assert_eq!(
            fs::read_to_string(&source_path).unwrap(),
            SRT,
            "the ORIGINAL subtitle must be byte-identical after an adjustment"
        );
        let written = fs::read_to_string(&adjusted.path).unwrap();
        assert!(written.contains("00:00:22,500 --> 00:00:26,900"));
        assert_eq!(adjusted.applied.cues_shifted, 1);

        fs::remove_dir_all(&store).ok();
        fs::remove_dir_all(&source_dir).ok();
    }

    #[test]
    fn nothing_is_written_into_the_library_when_no_store_is_configured() {
        // The fallback that must NOT exist.
        let err = write_adjusted(None, 1, "en", "sidecar", SubtitleFormat::SubRip, SRT, 1_000).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MUSE_SUBTITLE_STORE_DIR"), "{msg}");
        assert!(
            msg.contains("will NOT write into the library"),
            "the refusal must be explicit: {msg}"
        );

        assert!(resolve_store(None).is_err());
        assert!(resolve_store(Some("   ")).is_err());
    }

    #[test]
    fn a_malformed_subtitle_writes_nothing_at_all_rather_than_a_partial_file() {
        let store = unique_store("malformed");
        let err = write_adjusted(
            store.to_str(),
            1,
            "en",
            "sidecar",
            SubtitleFormat::SubRip,
            "1\n00:00:20,000 --> BROKEN\nHi\n",
            1_000,
        )
        .unwrap_err();
        assert!(matches!(err, MuseError::BadRequest(_)), "got {err:?}");

        // The store must be empty (or absent) — no half-written file.
        let entries = fs::read_dir(&store).map(|d| d.count()).unwrap_or(0);
        assert_eq!(entries, 0, "a failed adjustment must leave nothing behind");

        fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn a_zero_offset_adjustment_is_refused_rather_than_duplicating_the_original() {
        let store = unique_store("zero");
        let err = write_adjusted(store.to_str(), 1, "en", "x", SubtitleFormat::SubRip, SRT, 0).unwrap_err();
        assert!(matches!(err, MuseError::BadRequest(_)), "got {err:?}");
        fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn an_existing_file_is_never_silently_overwritten() {
        let store = unique_store("collision");
        let first = store_original(store.to_str(), 1, "en", "wyzie-1", SubtitleFormat::SubRip, SRT).unwrap();
        assert!(first.exists());

        let err = store_original(store.to_str(), 1, "en", "wyzie-1", SubtitleFormat::SubRip, "different").unwrap_err();
        assert!(matches!(err, MuseError::Conflict(_)), "got {err:?}");
        assert_eq!(
            fs::read_to_string(&first).unwrap(),
            SRT,
            "the first file must survive a colliding write"
        );

        fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn storing_the_original_keeps_a_pristine_copy_to_re_derive_from() {
        let store = unique_store("original");
        let original = store_original(store.to_str(), 5, "en", "wyzie-9", SubtitleFormat::SubRip, SRT).unwrap();
        assert_eq!(fs::read_to_string(&original).unwrap(), SRT);
        assert!(
            !original.to_string_lossy().contains("offset"),
            "the unshifted original must not be named as an offset copy"
        );

        // Two different offsets, both derived from the pristine copy, both
        // preserved side by side.
        let source = fs::read_to_string(&original).unwrap();
        let a = write_adjusted(store.to_str(), 5, "en", "wyzie-9", SubtitleFormat::SubRip, &source, 1_000).unwrap();
        let b = write_adjusted(store.to_str(), 5, "en", "wyzie-9", SubtitleFormat::SubRip, &source, -1_000).unwrap();
        assert!(a.path.exists() && b.path.exists());
        assert_ne!(a.path, b.path);
        assert_eq!(
            fs::read_to_string(&original).unwrap(),
            SRT,
            "the pristine original must survive every derivation"
        );

        fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn the_store_directory_is_created_if_it_does_not_exist() {
        let store = unique_store("mkdir").join("nested").join("deeper");
        assert!(!store.exists());
        let resolved = resolve_store(store.to_str()).unwrap();
        assert!(resolved.exists());
        fs::remove_dir_all(store.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn an_adjusted_ass_subtitle_keeps_its_styling() {
        let store = unique_store("ass");
        let ass = "[Script Info]\nTitle: T\n\n[Events]\nDialogue: 0,0:00:20.00,0:00:24.40,Default,,0,0,0,,Hi, there\n";
        let adjusted = write_adjusted(
            store.to_str(),
            1,
            "ja",
            "embedded-3",
            SubtitleFormat::AdvancedSubStation,
            ass,
            500,
        )
        .unwrap();
        let written = fs::read_to_string(&adjusted.path).unwrap();
        assert!(written.contains("[Script Info]"));
        assert!(written.contains("Hi, there"));
        assert!(written.contains("0:00:20.50,0:00:24.90"));
        assert!(adjusted.path.to_string_lossy().ends_with(".ass"));
        fs::remove_dir_all(&store).ok();
    }
}
