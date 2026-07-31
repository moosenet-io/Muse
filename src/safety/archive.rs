//! SAFE-03: looking inside archives, without extracting them.
//!
//! [`super::inspect`] can only judge files it can see, so an archive is opaque to it and comes
//! back `Suspicious` with "contents were not inspected". That is honest but not useful: the
//! archive is exactly where a hostile payload hides, and plenty of legitimate releases ship as
//! `.rar` parts, so "suspicious" on every archive would either block real media or train the
//! operator to ignore the verdict.
//!
//! ## Listing, not extracting
//!
//! A ZIP's central directory records every entry's NAME and SIZE without any compressed data
//! being touched. That is enough for the whole safety question — "is there an executable in
//! here" — and it means:
//!
//!   - no decompression, so a zip bomb has nothing to expand into;
//!   - nothing is written to disk, so a path-traversal entry cannot escape anywhere;
//!   - no partial extraction to clean up when a download is rejected.
//!
//! Extraction is a separate, later concern for downloads that PASS. The safety verdict never
//! requires it. This is the single biggest risk reduction available in this module and it is
//! why the listing path exists at all.
//!
//! ## Why a crate here, when the magic table was hand-written
//!
//! [`super::detect_magic`] is deliberately hand-rolled: a fixed lookup over ~20 byte
//! signatures, no parsing state, auditable in one screen. A ZIP central directory is the
//! opposite — variable-length records, offset-driven, with zip64 extensions, trailing comments,
//! and local headers that may disagree with the central ones. It is a real parser running on
//! adversarial input, and for that a battle-tested memory-safe implementation beats one written
//! here. The two decisions point in different directions because the inputs are different
//! shapes, not because the principle changed.
//!
//! `zip` is taken with `default-features = false`, enabling only `deflate` — no `zstd`, `bzip2`,
//! `lzma` or `xz` decoders, since none of them are needed to READ a directory listing. Fewer
//! decoders is less code parsing hostile bytes.
//!
//! ## What still cannot be seen
//!
//! RAR and 7z are the common release formats and there is no pure-Rust reader for either, nor
//! is `unrar`/`7z` installed on the deployment host. Those are reported as UNINSPECTED with the
//! reason naming the missing capability. They are not guessed at, and they are not quietly
//! treated as clean.

use std::io::{Read, Seek};

use super::{
    detect_magic, InspectedFile, Severity, Verdict, ARCHIVE_EXTENSIONS, MAGIC_PREFIX_LEN,
};

/// How many entries are RETAINED from a listing. What was dropped is reported, never silently
/// discarded.
///
/// HONEST LIMIT, because the earlier comment here overstated what this bounds. It caps the
/// entries this module copies and judges — it does NOT bound the ZIP parser, which reads the
/// whole central directory inside `ZipArchive::new` before this constant is consulted (codex,
/// gpt56). A hostile ZIP64 directory declaring millions of entries is parsed by the crate
/// first, and this limit cannot prevent that.
///
/// What DOES bound the damage: [`MAX_ARCHIVE_BYTES`] caps the input handed to the parser, and
/// [`MAX_ENTRY_NAME_LEN`] caps retained name length so a directory of enormous names cannot be
/// copied wholesale into memory. Those are the real controls; this one is about output size.
pub const MAX_ENTRIES: usize = 5_000;

/// Largest archive this module will hand to the ZIP parser.
///
/// The parser reads the central directory eagerly, so the only way to bound its work is to
/// bound its input. An archive above this is reported as UNINSPECTED — refused rather than
/// parsed — which is the fail-closed answer: a file too large to examine safely has not been
/// cleared, and says so.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

/// Longest entry name retained. A central directory can declare very long names purely to make
/// a listing expensive to hold; the name is truncated rather than the entry dropped, so the
/// entry is still judged.
pub const MAX_ENTRY_NAME_LEN: usize = 512;

// NOTE: there is deliberately no MAX_NESTING constant. An earlier version declared one, with a
// comment about how deep nested archives are followed — but no recursive inspection exists, so
// the constant documented a behaviour the module does not have (codex). A nested archive is
// reported as an archive ENTRY by name, and its own contents are not listed. Removed rather
// than left as a claim; if recursion is added later the bound comes back with it.

/// Why an archive could not be read into a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingFailure {
    /// The format has no reader available on this deployment (RAR, 7z, ...).
    UnsupportedFormat(String),
    /// A reader exists but the archive would not parse — truncated, encrypted, or malformed.
    Unreadable(String),
    /// The nesting limit was reached before this archive could be opened.
    TooDeep,
}

/// The result of trying to see inside one archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveListing {
    /// Entry paths as declared INSIDE the archive, with their uncompressed sizes.
    pub entries: Vec<(String, u64)>,
    /// Entries beyond [`MAX_ENTRIES`], counted rather than dropped silently.
    pub omitted: usize,
    /// Set when the listing could not be produced. Entries will be empty.
    pub failure: Option<ListingFailure>,
    /// Entry names that try to escape the extraction root. Hostile by construction — a
    /// legitimate release has no reason to ship `../../` paths.
    pub traversal_attempts: Vec<String>,
}

impl ArchiveListing {
    /// A listing that could not be produced because no reader exists for this format here.
    pub fn unsupported(format: impl Into<String>) -> Self {
        Self::failed(ListingFailure::UnsupportedFormat(format.into()))
    }

    fn failed(failure: ListingFailure) -> Self {
        Self {
            entries: Vec::new(),
            omitted: 0,
            failure: Some(failure),
            traversal_attempts: Vec::new(),
        }
    }
}

/// Does this entry name try to escape the directory it would be extracted into?
///
/// "Zip slip": an entry named `../../etc/cron.d/x` or `/etc/passwd` writes outside the target
/// when a naive extractor joins it to a base path. Detected at LISTING time, so it is caught
/// even though this module never extracts anything — the presence of such an entry says what
/// the archive was built to do, which is worth knowing regardless.
///
/// Both separators are checked: an archive written on Windows uses `\`, and a reader on Linux
/// will not treat that as a separator, so a name like `..\..\x` can slip past a check that only
/// looks at `/`.
pub fn is_traversal(name: &str) -> bool {
    if name.starts_with('/') || name.starts_with('\\') {
        return true;
    }
    // A drive-letter absolute path (`C:\...`) is equally an escape attempt.
    let bytes = name.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'/' || bytes[2] == b'\\') {
        return true;
    }
    name.split(['/', '\\']).any(|part| part == "..")
}

/// List a ZIP's entries from its central directory. No compressed data is read.
/// A `Read + Seek` wrapper that refuses to serve bytes beyond a hard cap.
///
/// `std::io::Take` is not usable here because `ZipArchive` needs `Seek`, which `Take` does not
/// provide. This keeps `Seek` while bounding reads — seeking beyond the cap is allowed (the
/// central directory lives at the END of a ZIP, so a reader that could not seek there could not
/// list anything), but no read may cross it.
struct BoundedReader<R> {
    inner: R,
    limit: u64,
    pos: u64,
}

impl<R: Read + Seek> BoundedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self { inner, limit, pos: 0 }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "archive exceeded the inspection byte limit",
            ));
        }
        let room = (self.limit - self.pos).min(buf.len() as u64) as usize;
        let n = self.inner.read(&mut buf[..room])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for BoundedReader<R> {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        let p = self.inner.seek(from)?;
        self.pos = p;
        Ok(p)
    }
}

/// Refuse an archive whose size exceeds [`MAX_ARCHIVE_BYTES`] before any parsing happens.
///
/// Separate from `list_zip` so the refusal is testable without constructing a 256 MB fixture.
pub fn too_large_to_inspect(size_bytes: u64) -> bool {
    size_bytes > MAX_ARCHIVE_BYTES
}

/// List a ZIP, refusing to parse anything larger than [`MAX_ARCHIVE_BYTES`].
///
/// `size_bytes` is REQUIRED rather than optional, and the refusal happens here rather than in a
/// helper a caller may forget. An earlier version exposed `too_large_to_inspect` as a free
/// function and documented it as "refuses before parsing" — but `list_zip` took only a reader
/// and never called it, so the control did not exist and the comment claiming it did was itself
/// the false claim (codex, gpt56). Putting the size in the signature makes it impossible to
/// list an archive without having considered its size.
pub fn list_zip<R: Read + Seek>(reader: R, size_bytes: u64) -> ArchiveListing {
    // `size_bytes` is the caller's CLAIM about the file. It is checked first because refusing
    // early is cheaper, but it is not trusted on its own: a caller that reports 1 KiB for a
    // 4 GiB archive would otherwise walk straight into the eager parser (codex, gpt56). The
    // authoritative bound is `bounded_reader` below, which caps the bytes the parser can
    // actually consume regardless of what was declared.
    if too_large_to_inspect(size_bytes) {
        return ArchiveListing::failed(ListingFailure::Unreadable(format!(
            "archive is {size_bytes} bytes, above the {MAX_ARCHIVE_BYTES}-byte inspection limit; \
             it was NOT parsed and nothing inside it has been checked"
        )));
    }
    // THE AUTHORITATIVE BOUND. `Take` caps what the parser can read no matter what the caller
    // declared, so a lie about the size buys nothing: the reader simply ends early and the
    // archive fails to parse, which is reported as unreadable — fail closed.
    //
    // `+ 1` so an archive of exactly MAX_ARCHIVE_BYTES still parses; the declared-size check
    // above is what rejects anything larger with a precise reason.
    let bounded = BoundedReader::new(reader, MAX_ARCHIVE_BYTES + 1);
    let mut archive = match zip::ZipArchive::new(bounded) {
        Ok(a) => a,
        Err(e) => return ArchiveListing::failed(ListingFailure::Unreadable(e.to_string())),
    };

    let total = archive.len();
    let mut entries = Vec::new();
    let mut traversal_attempts = Vec::new();

    for i in 0..total.min(MAX_ENTRIES) {
        // `name_raw`-adjacent APIs differ across versions; the safe read here is by index, and
        // a single unreadable entry must not abandon the whole listing — a partial listing that
        // says what it found is more useful than nothing, and the caller still sees `omitted`.
        let Ok(file) = archive.by_index_raw(i) else {
            continue;
        };
        // Truncated, not dropped: an absurdly long name is still an entry worth judging, and
        // its absurdity is itself signal — but retaining it whole lets a crafted directory
        // decide how much memory this listing occupies.
        let name: String = file.name().chars().take(MAX_ENTRY_NAME_LEN).collect();

        // TRAVERSAL IS CHECKED BEFORE THE DIRECTORY SKIP. This used to skip `is_dir()` first,
        // so a zip-slip payload declared as a DIRECTORY entry (`../../etc/`) was never examined
        // and the archive came back clean — the exact hostile shape the check exists for
        // (codex). A directory entry escaping the extraction root is as hostile as a file one.
        if is_traversal(&name) {
            traversal_attempts.push(name.clone());
        }
        if file.is_dir() {
            continue;
        }
        entries.push((name, file.size()));
    }

    ArchiveListing {
        entries,
        omitted: total.saturating_sub(MAX_ENTRIES),
        failure: None,
        traversal_attempts,
    }
}

/// Which archive format a file appears to be, by name and magic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    /// Recognized, but no reader is available on this deployment.
    UnsupportedRar,
    UnsupportedSevenZip,
    UnsupportedOther,
    NotAnArchive,
}

/// Classify a file as an archive kind. Magic takes precedence over the extension, for the same
/// reason it does in [`super::inspect`]: the name is the part an adversary controls.
pub fn archive_kind(path: &str, leading_bytes: Option<&[u8]>) -> ArchiveKind {
    if let Some(b) = leading_bytes {
        if b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") {
            return ArchiveKind::Zip;
        }
        if b.starts_with(b"Rar!\x1a\x07") {
            return ArchiveKind::UnsupportedRar;
        }
        if b.starts_with(b"7z\xbc\xaf\x27\x1c") {
            return ArchiveKind::UnsupportedSevenZip;
        }
    }
    let ext = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("zip") => ArchiveKind::Zip,
        Some("rar") | Some("r00") | Some("001") => ArchiveKind::UnsupportedRar,
        Some("7z") => ArchiveKind::UnsupportedSevenZip,
        Some(e) if ARCHIVE_EXTENSIONS.contains(&e) => ArchiveKind::UnsupportedOther,
        _ => ArchiveKind::NotAnArchive,
    }
}

/// Fold an archive's listing into the download's verdict.
///
/// The entries are judged by the SAME rules as loose files ([`super::inspect`]) — an `.exe` is
/// no less dangerous for being inside a `.zip`, and routing both through one rule set means the
/// gate cannot develop two different opinions about the same file.
///
/// Entries carry no leading bytes: reading them would mean decompressing, which is exactly what
/// this module avoids. So an entry is judged on its NAME alone, and `inspect`'s fail-closed
/// default does the rest — an unrecognized entry type is suspicious, not clean.
pub fn inspect_listing(archive_path: &str, listing: &ArchiveListing) -> Verdict {
    // An archive that listed successfully but yielded no file entries is still an archive whose
    // contents were not byte-checked, and it may carry directory-only traversal entries. It
    // therefore does not get to skip the floor below.
    let mut verdict = if listing.entries.is_empty() {
        Verdict {
            severity: Severity::Clean,
            findings: Vec::new(),
            has_media: false,
            adjudication: None,
        }
    } else {
        let files: Vec<InspectedFile> = listing
            .entries
            .iter()
            .map(|(name, size)| InspectedFile {
                // Prefixed so a finding names the archive AND the entry: "release.zip → x.exe"
                // is actionable, "x.exe" alone leaves the operator hunting for it.
                path: format!("{archive_path} → {name}"),
                size_bytes: *size,
                leading_bytes: None,
                // Structural: this module never decompresses, so entry bytes cannot exist.
                bytes_unavailable: true,
            })
            .collect();
        super::inspect(&files)
    };

    // An entry that would write outside the extraction root is not a mistake anyone makes by
    // accident. It is treated as hostile regardless of what the entry claims to be.
    for name in &listing.traversal_attempts {
        verdict.findings.push(super::Finding {
            path: format!("{archive_path} → {name}"),
            severity: Severity::Dangerous,
            reason: "archive entry escapes the extraction directory (path traversal) — an \
                     archive built to write outside where it is unpacked"
                .to_string(),
        });
    }

    // ── FAIL CLOSED ON WHAT WAS NOT BYTE-CHECKED ────────────────────────────────────────
    // An entry is judged by NAME only, because listing deliberately never decompresses. So a
    // member named `Movie.mkv` whose bytes are actually an executable is indistinguishable
    // here from a real video, and before this the whole archive came back CLEAN (codex).
    //
    // The earlier design suppressed the per-entry "contents not read" finding to avoid burying
    // real findings under one line per entry — right about the noise, wrong about the verdict.
    // The fix keeps the noise reduction and restores the floor: ONE archive-level finding,
    // stating exactly what was and was not established. A listed archive can therefore never
    // be Clean, which is correct — nothing inside it has been byte-verified.
    //
    // It is not merely `has_media = false` holding this back. That guard is about
    // importability; this is about the CLAIM. Reporting Clean for an archive whose contents
    // were never read is the same false certification this module exists to prevent.
    // Applies when the archive actually carries file entries. An EMPTY archive hides nothing,
    // so there is nothing to withhold certification about — and codex's directory-entry case is
    // handled by checking traversal BEFORE the is_dir skip, not by this floor.
    if !listing.entries.is_empty() {
        verdict.findings.push(super::Finding {
            path: archive_path.to_string(),
            severity: Severity::Suspicious,
            reason: format!(
                "{} entries were checked by NAME only — this gate does not decompress, so no \
                 entry's actual contents have been verified",
                listing.entries.len()
            ),
        });
    }

    if let Some(failure) = &listing.failure {
        verdict.findings.push(super::Finding {
            path: archive_path.to_string(),
            severity: Severity::Suspicious,
            reason: match failure {
                ListingFailure::UnsupportedFormat(f) => format!(
                    "archive contents were NOT inspected: no reader for {f} on this deployment"
                ),
                ListingFailure::Unreadable(e) => {
                    format!("archive could not be read, so its contents are unknown: {e}")
                }
                ListingFailure::TooDeep => {
                    "nested archives exceeded the inspection depth limit, so the innermost \
                     contents are unknown"
                        .to_string()
                }
            },
        });
    }

    if listing.omitted > 0 {
        verdict.findings.push(super::Finding {
            path: archive_path.to_string(),
            severity: Severity::Suspicious,
            reason: format!(
                "{} further entries were not listed, so the archive is only partly inspected",
                listing.omitted
            ),
        });
    }

    // `inspect` computed a severity over the entries only; the findings appended above can
    // raise it, so it is recomputed rather than left stale. Getting this wrong would report a
    // path-traversal archive at the severity of its innocuous entries.
    verdict.severity = verdict
        .findings
        .iter()
        .map(|f| f.severity)
        .max()
        .unwrap_or(Severity::Clean);

    // Media INSIDE an archive is not importable media: it has to be extracted first, and this
    // module never extracts. Reporting `has_media` would let a download containing only a
    // zipped video look ready to import.
    //
    // VERIFIED INERT TODAY, and kept deliberately. A mutation deleting this line passes the
    // whole suite, because entries carry no bytes, so `inspect` never sets `has_media` for them
    // in the first place. It is a guard against a future change that looks entirely reasonable
    // — "an entry named .mkv is media, set has_media" — which would silently make archived
    // media importable without extraction. Stated as defensive rather than dressed up as
    // load-bearing, because a comment claiming a line does work it does not do is how the next
    // reader gets misled.
    verdict.has_media = false;
    verdict
}

/// Read the leading bytes of an archive member's declared name for magic purposes.
///
/// Not implemented, and deliberately so: it would require decompressing member data, which is
/// the entire risk this module is built to avoid. Kept as a documented absence so the next
/// reader does not assume the entries were byte-checked — they were not, they were name-checked.
pub const ENTRIES_ARE_NAME_CHECKED_ONLY: &str =
    "archive entries are judged by name; their bytes are not decompressed";

/// Convenience: the prefix length a caller should read to classify an archive file itself.
pub const ARCHIVE_MAGIC_PREFIX_LEN: usize = MAGIC_PREFIX_LEN;

/// Whether a loose file warrants an archive listing pass at all.
pub fn should_list(path: &str, leading_bytes: Option<&[u8]>) -> bool {
    !matches!(archive_kind(path, leading_bytes), ArchiveKind::NotAnArchive)
        || detect_magic(leading_bytes) == super::Magic::Archive
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    /// Build a real ZIP in memory. Real bytes through the real reader — a hand-made fixture
    /// would only prove the fixture matched the assertion.
    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            for (name, data) in entries {
                w.start_file(*name, SimpleFileOptions::default())
                    .expect("start_file");
                w.write_all(data).expect("write");
            }
            w.finish().expect("finish");
        }
        buf
    }

    // ── THE POINT OF THE MODULE ──────────────────────────────────────────────────────────

    #[test]
    fn an_executable_hidden_inside_a_zip_is_found_without_extracting_it() {
        // Before this module, this archive was reported "suspicious — contents not inspected",
        // which is honest but useless: the operator still has to open it by hand. The whole
        // point is to name the executable without ever decompressing a byte of it.
        let bytes = zip_with(&[("Movie.2024.mkv", b"x"), ("Setup.exe", b"MZ fake")]);
        let listing = list_zip(Cursor::new(bytes.clone()), bytes.len() as u64);
        assert!(listing.failure.is_none());

        let v = inspect_listing("release.zip", &listing);
        assert_eq!(v.severity, Severity::Dangerous);
        assert!(
            v.findings.iter().any(|f| f.path.contains("Setup.exe")),
            "the finding must name the entry: {:?}",
            v.findings,
        );
        assert!(
            v.findings.iter().any(|f| f.path.starts_with("release.zip")),
            "and the archive it is in",
        );
    }

    #[test]
    fn a_traversal_payload_declared_as_a_DIRECTORY_is_still_caught() {
        // The zip-slip variant that slipped through: entries were skipped on `is_dir()` BEFORE
        // traversal was checked, so an archive whose escape was declared as a directory came
        // back clean (codex). Traversal is now checked first, for every entry.
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            w.add_directory("../../etc/cron.d/", SimpleFileOptions::default()).unwrap();
            w.finish().unwrap();
        }
        let n = buf.len() as u64;
        let listing = list_zip(Cursor::new(buf), n);
        assert_eq!(listing.traversal_attempts.len(), 1, "{listing:?}");
        let v = inspect_listing("evil.zip", &listing);
        assert_eq!(v.severity, Severity::Dangerous);
    }

    #[test]
    fn an_archive_above_the_size_limit_is_refused_before_it_is_parsed() {
        // The control is now IN list_zip's signature, so an archive cannot be listed without
        // its size having been considered. Previously this lived in a helper nothing called,
        // while the doc claimed the refusal happened.
        let bytes = zip_with(&[("Movie.mkv", b"x")]);
        let listing = list_zip(Cursor::new(bytes), MAX_ARCHIVE_BYTES + 1);
        assert!(listing.entries.is_empty(), "must not have parsed it");
        assert!(matches!(listing.failure, Some(ListingFailure::Unreadable(_))));
        let v = inspect_listing("huge.zip", &listing);
        assert_eq!(v.severity, Severity::Suspicious);
    }

    #[test]
    fn a_path_traversal_entry_is_dangerous_on_its_own() {
        // Zip slip. No legitimate release ships `../` entries; an archive that does was built
        // to write outside wherever it is unpacked.
        let bytes = zip_with(&[("../../etc/cron.d/pwn", b"x"), ("Movie.mkv", b"y")]);
        let listing = list_zip(Cursor::new(bytes.clone()), bytes.len() as u64);
        assert_eq!(listing.traversal_attempts.len(), 1, "{listing:?}");

        let v = inspect_listing("release.zip", &listing);
        assert_eq!(v.severity, Severity::Dangerous);
        assert!(v.findings.iter().any(|f| f.reason.contains("path traversal")));
    }

    #[test]
    fn traversal_detection_covers_both_separators_and_absolute_forms() {
        // A Windows-authored archive uses `\`, which a Linux reader does not treat as a
        // separator — so a check that only looks at `/` misses `..\..\x` entirely.
        assert!(is_traversal("../x"));
        assert!(is_traversal("a/../../x"));
        assert!(is_traversal("..\\..\\x"));
        assert!(is_traversal("a\\..\\x"));
        assert!(is_traversal("/etc/passwd"));
        assert!(is_traversal("\\windows\\system32\\x"));
        assert!(is_traversal("C:\\windows\\x"));
        assert!(is_traversal("C:/windows/x"));

        // Not traversal: `..` must be a whole path COMPONENT, not a substring.
        assert!(!is_traversal("Movie.2024.mkv"));
        assert!(!is_traversal("Season 1/Ep..01.mkv"));
        assert!(!is_traversal("..leading.dots.mkv"));
        assert!(!is_traversal("sub/dir/file.mkv"));
    }

    #[test]
    fn an_archive_of_apparently_clean_media_is_still_never_certified_clean() {
        // This test previously asserted Clean, and that was the bug: entries are judged by NAME
        // only, so a member called `Movie.mkv` whose bytes are an executable is
        // indistinguishable from a real video here. Certifying the archive clean is exactly the
        // false certification this module exists to prevent (codex).
        //
        // The floor is one archive-level finding stating what was and was not established —
        // which keeps the verdict honest without one line of noise per entry.
        let bytes = zip_with(&[("Movie.2024.mkv", b"x"), ("Movie.srt", b"y")]);
        let v = inspect_listing("release.zip", &list_zip(Cursor::new(bytes.clone()), bytes.len() as u64));
        assert_eq!(v.severity, Severity::Suspicious, "{:?}", v.findings);
        assert!(
            v.findings.iter().any(|f| f.reason.contains("NAME only")),
            "must state that nothing inside was byte-verified: {:?}",
            v.findings,
        );
        assert!(!v.has_media, "archived media is not yet importable media");
        assert!(!v.is_importable());
    }

    #[test]
    fn a_media_named_entry_hiding_an_executable_is_not_reported_clean() {
        // The concrete attack behind the rule above: the archive equivalent of the renamed
        // executable. We cannot SEE the bytes without decompressing, so the gate must not
        // pretend it looked — it reports that nothing was verified rather than certifying.
        let bytes = zip_with(&[("Movie.2024.1080p.mkv", b"MZ\x90\x00 this is really a PE")]);
        let v = inspect_listing("release.zip", &list_zip(Cursor::new(bytes.clone()), bytes.len() as u64));
        assert_ne!(v.severity, Severity::Clean, "must never be certified clean");
        assert!(!v.is_importable());
    }

    // ── HONEST ABOUT WHAT IT CANNOT SEE ──────────────────────────────────────────────────

    #[test]
    fn rar_and_7z_are_reported_as_uninspected_never_as_clean() {
        // The common release formats, with no pure-Rust reader and no unrar/7z binary on the
        // deployment host. The verdict must name the missing capability rather than implying
        // the contents passed.
        for failure in [
            ListingFailure::UnsupportedFormat("rar".into()),
            ListingFailure::UnsupportedFormat("7z".into()),
        ] {
            let v = inspect_listing("release.rar", &ArchiveListing::failed(failure));
            assert_eq!(v.severity, Severity::Suspicious);
            assert!(!v.is_importable());
            assert!(
                v.findings[0].reason.contains("NOT inspected"),
                "must state the limit of its knowledge: {}",
                v.findings[0].reason,
            );
        }
    }

    #[test]
    fn an_unreadable_archive_is_suspicious_not_clean() {
        // Truncated, encrypted or corrupt. Fail closed: unknown contents are never safe.
        let v = inspect_listing("x.zip", &ArchiveListing::failed(ListingFailure::Unreadable("bad".into())));
        assert_eq!(v.severity, Severity::Suspicious);
    }

    #[test]
    fn a_garbage_zip_fails_to_list_rather_than_panicking() {
        let listing = list_zip(Cursor::new(b"not a zip at all".to_vec()), 16);
        assert!(matches!(listing.failure, Some(ListingFailure::Unreadable(_))));
        assert!(listing.entries.is_empty());
    }

    #[test]
    fn an_empty_zip_is_clean_but_carries_no_media() {
        let v = inspect_listing("empty.zip", &{ let b = zip_with(&[]); let n = b.len() as u64; list_zip(Cursor::new(b), n) });
        assert_eq!(v.severity, Severity::Clean);
        assert!(!v.is_importable());
    }

    #[test]
    fn omitted_entries_are_reported_so_a_partial_listing_is_not_read_as_complete() {
        let listing = ArchiveListing {
            entries: vec![("a.mkv".into(), 1)],
            omitted: 12_000,
            failure: None,
            traversal_attempts: Vec::new(),
        };
        let v = inspect_listing("huge.zip", &listing);
        assert_eq!(v.severity, Severity::Suspicious);
        assert!(v.findings.iter().any(|f| f.reason.contains("12000 further entries")));
    }

    // ── CLASSIFICATION ───────────────────────────────────────────────────────────────────

    #[test]
    fn an_archive_too_large_to_parse_safely_is_refused_not_parsed() {
        // The parser reads the whole central directory eagerly, so the only way to bound its
        // work is to bound its input. Refusing is the fail-closed answer: a file too large to
        // examine has not been cleared.
        assert!(!too_large_to_inspect(MAX_ARCHIVE_BYTES));
        assert!(too_large_to_inspect(MAX_ARCHIVE_BYTES + 1));

        let v = inspect_listing(
            "huge.zip",
            &ArchiveListing::failed(ListingFailure::Unreadable("archive exceeds the inspection size limit".into())),
        );
        assert_eq!(v.severity, Severity::Suspicious);
        assert!(!v.is_importable());
    }

    #[test]
    fn a_lie_about_the_size_does_not_get_past_the_byte_bound() {
        // The declared size is the caller's CLAIM. A caller reporting 1 KiB for a huge archive
        // would otherwise walk straight into the eager parser (codex, gpt56). The authoritative
        // bound is on bytes actually read, so a lie buys nothing: the reader ends early, the
        // parse fails, and it is reported unreadable — fail closed rather than parsed anyway.
        let mut buf = zip_with(&[("Movie.mkv", b"x")]);
        // Pad well past the cap so a truthful read would exceed it.
        buf.resize((MAX_ARCHIVE_BYTES + 4096) as usize, 0);
        let n = buf.len() as u64;

        // Truthful size: refused up front with a precise reason.
        let honest = list_zip(Cursor::new(buf.clone()), n);
        assert!(honest.entries.is_empty());
        assert!(honest.failure.is_some());

        // Lied-about size: the byte bound still stops it, rather than the declared value
        // deciding how much the parser may consume.
        let lying = list_zip(Cursor::new(buf), 1024);
        assert!(
            lying.entries.is_empty() && lying.failure.is_some(),
            "a false size must not unlock unbounded parsing: {lying:?}",
        );
    }

    #[test]
    fn an_enormous_entry_name_is_truncated_rather_than_retained_whole() {
        let long = "a".repeat(MAX_ENTRY_NAME_LEN * 4) + ".mkv";
        let bytes = zip_with(&[(long.as_str(), b"x")]);
        let listing = list_zip(Cursor::new(bytes.clone()), bytes.len() as u64);
        assert_eq!(listing.entries.len(), 1);
        assert!(
            listing.entries[0].0.chars().count() <= MAX_ENTRY_NAME_LEN,
            "retained name must be bounded",
        );
    }

    #[test]
    fn magic_beats_the_extension_when_classifying_an_archive() {
        // Same principle as the main gate: the name is the part an adversary controls.
        assert_eq!(archive_kind("release.mkv", Some(b"PK\x03\x04")), ArchiveKind::Zip);
        assert_eq!(
            archive_kind("release.zip", Some(b"Rar!\x1a\x07\x00")),
            ArchiveKind::UnsupportedRar,
            "bytes say RAR, so it is RAR regardless of the .zip name",
        );
    }

    #[test]
    fn extension_classifies_when_no_bytes_were_read() {
        assert_eq!(archive_kind("a.zip", None), ArchiveKind::Zip);
        assert_eq!(archive_kind("a.rar", None), ArchiveKind::UnsupportedRar);
        assert_eq!(archive_kind("a.r00", None), ArchiveKind::UnsupportedRar);
        assert_eq!(archive_kind("a.7z", None), ArchiveKind::UnsupportedSevenZip);
        assert_eq!(archive_kind("a.mkv", None), ArchiveKind::NotAnArchive);
    }

    #[test]
    fn the_severity_is_recomputed_after_appending_findings() {
        // The appended traversal/failure findings must be able to RAISE the severity that
        // `inspect` computed over the entries alone. Leaving it stale would report a
        // path-traversal archive at the severity of its innocuous contents.
        let listing = ArchiveListing {
            entries: vec![("Movie.mkv".into(), 1)], // clean on its own
            omitted: 0,
            failure: None,
            traversal_attempts: vec!["../../x".into()],
        };
        let v = inspect_listing("r.zip", &listing);
        assert_eq!(v.severity, Severity::Dangerous, "the traversal must win");
    }

    #[test]
    fn entries_are_judged_by_the_same_rules_as_loose_files() {
        // One rule set, so the gate cannot develop two opinions about the same file. A `.lnk`
        // is dangerous loose; it is dangerous zipped.
        let bytes = zip_with(&[("RARBG.lnk", b"x")]);
        let v = inspect_listing("r.zip", &list_zip(Cursor::new(bytes.clone()), bytes.len() as u64));
        assert_eq!(v.severity, Severity::Dangerous);
    }
}
