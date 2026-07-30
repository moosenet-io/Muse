//! MUSE #100: derive optimized poster renditions from the cached original — the
//! Plex/Jellyfin model. Capture the provider's image ONCE (that stays
//! `artwork_cache`'s original row), then resize/encode into a small, indexed set
//! of renditions so a grid tile costs ~15 KB instead of the ~1.9 MB master.
//!
//! ## Why the size ladder is an ALLOWLIST and not a `?w=` passthrough
//! A caller-supplied width handed to an image resizer is an amplification
//! vector, not a convenience: every distinct value mints a new decode + encode
//! (CPU) *and* a new cached blob (storage), so `?w=1,2,3,…` is a cheap way for
//! one client to fill the table and pin the CPU. The ladder is therefore a fixed
//! set, and an off-ladder width is REJECTED rather than clamped — a silent clamp
//! would tell a client it got the size it asked for when it did not.
//!
//! ## Why JPEG only, for now
//! The `image` crate at the version this crate pins supports WebP **decode**
//! only, so producing WebP/AVIF needs a new encoder dependency. The cache key
//! already carries `format` (see `0109_artwork_renditions.sql`), so adding a
//! format later is a new row per entity — not a migration and not a cache wipe.
//! Until then every rendition is JPEG and content negotiation is a no-op; this
//! module does not pretend otherwise.

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::ImageReader;
use std::io::Cursor;

use crate::error::{MuseError, MuseResult};

/// The rendition ladder, in px. Deliberately small:
/// - `160` — a library poster-wall tile (the MGUI-01 grid draws ~112px; 160
///   covers it plus modest hidpi without a second entry)
/// - `320` — a 2× tile / rail card
/// - `640` — a media-detail poster
///
/// Anything larger should use the original rather than mint a near-master copy.
pub const RENDITION_WIDTHS: [i32; 3] = [160, 320, 640];

/// The only container produced today. See the module doc.
pub const RENDITION_FORMAT: &str = "jpeg";
pub const RENDITION_CONTENT_TYPE: &str = "image/jpeg";

/// JPEG quality for renditions. 82 is the usual visual-transparency knee for
/// photographic posters; below ~75 ringing shows on title text.
const JPEG_QUALITY: u8 = 82;

/// Validate a requested width against the ladder.
///
/// `Ok(None)` means the caller asked for no rendition (serve the original).
/// `Err(())` means the caller asked for a width that is not on the ladder — the
/// handler turns that into a `400`, deliberately NOT a clamp.
pub fn parse_width(raw: Option<&str>) -> Result<Option<i32>, ()> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let w: i32 = trimmed.parse().map_err(|_| ())?;
    if RENDITION_WIDTHS.contains(&w) {
        Ok(Some(w))
    } else {
        Err(())
    }
}

/// Decode `original`, resize to `width` preserving aspect ratio, re-encode JPEG.
///
/// CPU-bound and therefore expected to be called inside `spawn_blocking` — a
/// multi-megabyte decode on the async runtime would stall unrelated requests.
/// The caller owns that; this function is deliberately synchronous so it cannot
/// be `await`ed onto the reactor by accident.
///
/// Never upscales: a master narrower than the requested width is re-encoded at
/// its own size. Upscaling would spend CPU and bytes to produce a blurrier image
/// than the original, and would make a rendition *larger* than its master.
pub fn render_jpeg(original: &[u8], width: i32) -> MuseResult<Vec<u8>> {
    let reader = ImageReader::new(Cursor::new(original))
        .with_guessed_format()
        .map_err(|e| MuseError::Internal(anyhow::anyhow!("artwork: unreadable image: {e}")))?;

    let img = reader
        .decode()
        .map_err(|e| MuseError::Internal(anyhow::anyhow!("artwork: decode failed: {e}")))?;

    let target = u32::try_from(width)
        .map_err(|_| MuseError::Internal(anyhow::anyhow!("artwork: negative width")))?;

    // `resize` fits WITHIN the box, so a tall poster is bounded by width while
    // its height follows the aspect ratio. The generous height bound is what
    // makes width the effective constraint.
    let resized = if img.width() <= target {
        img
    } else {
        img.resize(target, u32::MAX / 2, FilterType::Lanczos3)
    };

    // Posters are photographic; drop alpha so the JPEG encoder gets a channel
    // layout it can actually write (it rejects RGBA).
    let rgb = resized.to_rgb8();

    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| MuseError::Internal(anyhow::anyhow!("artwork: encode failed: {e}")))?;
    Ok(out)
}

/// A STRONG ETag derived from the rendition's own BYTES.
///
/// The first version of this took a `master_fingerprint` argument and the caller
/// passed it `"{kind}:{id}:{variant}"` — the cache KEY, which never changes when
/// the master image changes. Combined with a long `max-age`, that served a stale
/// thumbnail forever and answered `304` to a client holding the old one. Both
/// reviewers caught it. Hashing the bytes removes the possibility of that class
/// of mistake entirely: the validator cannot disagree with the representation it
/// validates, because it is computed FROM it.
///
/// (Rendition rows are also purged whenever the master is rewritten — see
/// `repo::artwork_cache::delete_renditions`. The byte-ETag and the purge are
/// belt and braces: the purge stops a stale rendition existing, the ETag stops a
/// stale one being revalidated as fresh.)
/// SHA-256 of `bytes`, hex, unquoted — the provenance/identity primitive.
/// [`rendition_etag`] is this value wrapped as an HTTP entity-tag.
pub fn content_hash(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        // Infallible: writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub fn rendition_etag(bytes: &[u8]) -> String {
    // The FULL digest. A truncated prefix is operationally fine for cache
    // validation, but a reviewer rightly noted it weakens a strong validator for
    // no real saving — 64 hex chars in a header costs nothing.
    format!("\"{}\"", content_hash(bytes))
}

/// Variants this endpoint will serve. An ALLOWLIST because `variant` is part of
/// the cache identity: an unbounded `?variant=` lets one client mint unlimited
/// cache rows and provoke unlimited provider lookups, which is the same
/// amplification problem the width ladder closes (a reviewer spotted that the
/// width fix left this half open).
///
/// Contents are the variants actually in use — `poster`/`backdrop`/`nfo` exist in
/// `artwork_cache` today and the code additionally writes `fanart`. Adding a new
/// variant means adding it here on purpose.
pub const ALLOWED_VARIANTS: [&str; 4] = ["poster", "backdrop", "fanart", "nfo"];

/// Variants a RENDITION may be derived from. Narrower than [`ALLOWED_VARIANTS`]:
/// `nfo` is not an image, so resizing it is meaningless and a `?w=` against it is
/// a client error rather than a silently-ignored parameter.
pub const RENDERABLE_VARIANTS: [&str; 3] = ["poster", "backdrop", "fanart"];

pub fn variant_allowed(variant: &str) -> bool {
    ALLOWED_VARIANTS.contains(&variant)
}

pub fn variant_renderable(variant: &str) -> bool {
    RENDERABLE_VARIANTS.contains(&variant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_parsing_accepts_only_the_ladder() {
        assert_eq!(parse_width(None), Ok(None));
        assert_eq!(parse_width(Some("")), Ok(None));
        assert_eq!(parse_width(Some("160")), Ok(Some(160)));
        assert_eq!(parse_width(Some(" 640 ")), Ok(Some(640)));
    }

    /// The security-relevant half: an off-ladder width must be REJECTED, not
    /// clamped. Clamping would let `?w=161`…`?w=99999` each mint a distinct
    /// cache blob + decode while reporting success.
    #[test]
    fn off_ladder_widths_are_rejected_not_clamped() {
        for bad in ["161", "1", "0", "-160", "99999", "abc", "160px", "1e3"] {
            assert_eq!(parse_width(Some(bad)), Err(()), "{bad} must be rejected");
        }
    }

    /// The regression that matters most: the validator must track the BYTES.
    /// The original implementation derived it from the cache key, so it never
    /// changed when the image did — serving a stale thumbnail behind a long
    /// max-age and answering 304 to a client holding the old one.
    #[test]
    fn etag_is_derived_from_the_bytes_not_the_cache_key() {
        let a = rendition_etag(b"rendition-bytes-A");
        let b = rendition_etag(b"rendition-bytes-B");
        assert_ne!(a, b, "different bytes MUST produce different validators");
        assert_eq!(a, rendition_etag(b"rendition-bytes-A"), "and it must be stable");
        assert!(a.starts_with('"') && a.ends_with('"'), "strong validator, quoted");
        assert!(!a.starts_with("W/"), "not weak — it is computed from the representation");
    }

    #[test]
    fn content_hash_is_the_full_sha256_and_the_etag_wraps_it() {
        let h = content_hash(b"poster-bytes");
        assert_eq!(h.len(), 64, "full SHA-256, not a truncated prefix");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(rendition_etag(b"poster-bytes"), format!("\"{h}\""));
        assert_ne!(h, content_hash(b"poster-byteS"), "one bit changes the identity");
    }

    #[test]
    fn variant_allowlist_bounds_the_cache_key() {
        for ok in ALLOWED_VARIANTS {
            assert!(variant_allowed(ok), "{ok} is in use and must be served");
        }
        // An unbounded ?variant= would let a client mint unlimited cache rows.
        for bad in ["", "poster ", "POSTER", "../etc", "a".repeat(200).as_str(), "thumb"] {
            assert!(!variant_allowed(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn nfo_is_servable_but_never_renderable() {
        assert!(variant_allowed("nfo"), "existing nfo rows must still be readable");
        assert!(!variant_renderable("nfo"), "an nfo is not an image; ?w= on it is a client error");
        assert!(variant_renderable("poster"));
    }

    /// Renders a real (tiny) JPEG through the full decode→resize→encode path and
    /// asserts the rendition is smaller than its master and is itself decodable.
    #[test]
    fn rendering_shrinks_a_master_and_produces_a_valid_jpeg() {
        // A 400x600 synthetic poster with varying content (a flat colour would
        // compress to almost nothing and make the size assertion vacuous).
        let mut master = image::RgbImage::new(400, 600);
        for (x, y, px) in master.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8]);
        }
        let mut master_jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut master_jpeg, 95)
            .encode_image(&master)
            .expect("encode master");

        let out = render_jpeg(&master_jpeg, 160).expect("render");
        assert!(!out.is_empty());
        assert!(
            out.len() < master_jpeg.len(),
            "a 160px rendition ({} bytes) must be smaller than its 400px master ({} bytes)",
            out.len(),
            master_jpeg.len()
        );

        let decoded = ImageReader::new(Cursor::new(&out))
            .with_guessed_format()
            .expect("guess format")
            .decode()
            .expect("rendition must itself be a decodable image");
        assert_eq!(decoded.width(), 160, "width is the constraint");
        assert_eq!(decoded.height(), 240, "aspect ratio preserved (400x600 -> 160x240)");
    }

    /// Never upscale: a rendition must never be larger than its master.
    #[test]
    fn a_master_narrower_than_the_target_is_not_upscaled() {
        let mut small = image::RgbImage::new(100, 150);
        for (x, y, px) in small.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        let mut small_jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut small_jpeg, 95)
            .encode_image(&small)
            .expect("encode");

        let out = render_jpeg(&small_jpeg, 640).expect("render");
        let decoded = ImageReader::new(Cursor::new(&out))
            .with_guessed_format()
            .expect("guess")
            .decode()
            .expect("decode");
        assert_eq!(decoded.width(), 100, "must not upscale a 100px master to 640px");
    }

    #[test]
    fn garbage_input_is_an_error_not_a_panic() {
        assert!(render_jpeg(b"definitely not an image", 160).is_err());
        assert!(render_jpeg(&[], 160).is_err());
    }
}
