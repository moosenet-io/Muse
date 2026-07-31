//! SAFE-01 (MUSE #110): the download threat gate.
//!
//! Judges the file list of a completed download BEFORE anything can move it into the library.
//! This is the piece standing between a public tracker and the operator's media, and it is the
//! stated reason the platform exists: not having to hand-clear malicious payloads out of
//! library-manager queues every day.
//!
//! ## Why this cannot be an extension check
//!
//! The payload this defends against is built to look like media. The common shapes are:
//!
//!   - `Movie.2024.1080p.mkv` whose bytes actually begin `MZ` — a Windows executable wearing a
//!     video extension. An extension check passes it. It is the single most common hostile
//!     shape on public trackers.
//!   - A real video beside `Setup.exe` / `password.txt` / `RARBG.lnk`.
//!   - An archive containing the executable, so nothing on the surface looks wrong at all.
//!
//! `crate::library::scan::MEDIA_EXTENSIONS` exists but is the wrong tool: it is an INCLUSION
//! filter over an already-imported, already-trusted library, and it never reads a byte. This
//! module judges UNTRUSTED input, so it reads both the name and the leading bytes, and it
//! treats disagreement between them as its own, worse signal.
//!
//! ## Fail closed
//!
//! Every prior gap of this shape in this codebase — MUSE #106, #108, #109 — turned an unknown
//! into a false negative: a wrong URL became "no results", an outage became "nothing matched",
//! an unparsed record became an empty title. Each was invisible precisely because the failure
//! looked like an ordinary empty answer.
//!
//! A security gate cannot afford that posture, so the shape here is inverted. There is no
//! denylist of bad extensions to keep up to date; there is an ALLOWLIST of file types known to
//! be safe to import, and everything else escalates. A type nobody has classified is
//! `Suspicious` by construction, not `Clean` by omission.
//!
//! ## What this module does NOT do
//!
//! It does not open archives. Inspecting a `.rar` means an extraction dependency parsing
//! hostile input inside the trust path, which is a separate decision with its own risk budget.
//! An archive is therefore reported as UNINSPECTED — never certified clean — and the reason
//! says so, so the verdict is honest about the limit of its own knowledge.
//!
//! It also performs no I/O. Input is a plain list of names, sizes and leading bytes; the caller
//! reads those (SAFE-02). Keeping this pure is what makes the rules exhaustively testable, and
//! this is the one module in the system where that matters most.

pub mod archive;
pub mod llm;

use std::fmt;

/// How many leading bytes the caller should read per file for magic detection.
///
/// 264 covers every signature below, including TAR's at offset 257. Reading more would not
/// improve detection and means holding more untrusted bytes in memory per file.
pub const MAGIC_PREFIX_LEN: usize = 264;

/// Container/media extensions that may be imported. ALLOWLIST — see the module doc on why this
/// is not a denylist. Mirrors `library::scan::MEDIA_EXTENSIONS` and adds the containers a
/// release realistically ships in.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "m4v", "mov", "wmv", "ts", "webm", "mpg", "mpeg", "flv", "m2ts",
];

/// Non-media files that are safe and expected beside a release: subtitles, artwork, metadata.
/// Allowed to exist, never sufficient on their own to make a download importable.
pub const SIDECAR_EXTENSIONS: &[&str] = &[
    "srt", "sub", "idx", "ass", "ssa", "vtt", "nfo", "jpg", "jpeg", "png", "txt", "sfv", "md5",
];

/// Extensions that are executable or script-bearing on some platform. Used for the NAME check
/// only — the byte check below is what catches the same content under a different name.
///
/// This list existing does not make the design a denylist: anything not in `MEDIA_EXTENSIONS`
/// or `SIDECAR_EXTENSIONS` already escalates. The list only sharpens the REASON, so an operator
/// is told "this is an executable" rather than "this is unrecognized".
pub const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "exe", "scr", "com", "bat", "cmd", "pif", "vbs", "vbe", "js", "jse", "wsf", "wsh", "ps1",
    "msi", "msp", "hta", "cpl", "jar", "lnk", "url", "reg", "dll", "sys", "app", "dmg", "pkg",
    "deb", "rpm", "sh", "bash", "run", "bin", "elf", "so", "apk",
];

/// Archive containers. Not hostile in themselves — plenty of legitimate releases ship `.rar`
/// parts — but opaque, and an opaque file cannot be certified clean.
pub const ARCHIVE_EXTENSIONS: &[&str] =
    &["rar", "zip", "7z", "gz", "bz2", "xz", "tar", "tgz", "z", "cab", "iso", "r00", "001"];

/// What the leading bytes say a file actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Magic {
    /// Windows PE — `.exe`, `.dll`, `.scr`. The renamed-executable case.
    WindowsExecutable,
    /// Linux/BSD ELF binary.
    ElfExecutable,
    /// macOS Mach-O binary (32/64, both endiannesses, and the fat/universal wrapper).
    MachExecutable,
    /// `#!` — a script that will be executed by whatever interpreter it names.
    Shebang,
    /// An archive container. Opaque, not inspected.
    Archive,
    /// A recognized media container.
    Media,
    /// Recognized, benign, non-media (images, subtitles-as-text are not detectable by magic).
    BenignData,
    /// Bytes were supplied but match no known signature.
    Unknown,
    /// No bytes were supplied — the caller could not or did not read them.
    NotProvided,
}

impl Magic {
    /// Whether these bytes are directly executable content.
    pub fn is_executable(self) -> bool {
        matches!(
            self,
            Magic::WindowsExecutable | Magic::ElfExecutable | Magic::MachExecutable | Magic::Shebang
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Magic::WindowsExecutable => "windows executable (MZ)",
            Magic::ElfExecutable => "elf executable",
            Magic::MachExecutable => "mach-o executable",
            Magic::Shebang => "script (#!)",
            Magic::Archive => "archive",
            Magic::Media => "media container",
            Magic::BenignData => "benign data",
            Magic::Unknown => "unrecognized",
            Magic::NotProvided => "not read",
        }
    }
}

/// Identify a file from its leading bytes.
///
/// Hand-written rather than pulling in `infer`. The set that matters for this gate is small,
/// fixed and security-critical, and a hand-written table is auditable in one screen — an
/// operator (or a reviewer) can see exactly what is and is not detected. A general-purpose
/// crate would recognize hundreds of formats this module has no opinion about, and its
/// classification of an unknown type would still have to be mapped back onto these verdicts by
/// hand. The dependency would add surface without removing the judgement.
pub fn detect_magic(bytes: Option<&[u8]>) -> Magic {
    let Some(b) = bytes else {
        return Magic::NotProvided;
    };
    if b.is_empty() {
        return Magic::NotProvided;
    }

    // ---- executables: checked FIRST, so a file that is both plausibly an archive and
    // plausibly an executable is reported as the more dangerous of the two.
    if b.starts_with(b"MZ") {
        return Magic::WindowsExecutable;
    }
    if b.starts_with(b"\x7fELF") {
        return Magic::ElfExecutable;
    }
    // Mach-O: 32/64-bit, both endiannesses, plus the fat/universal header.
    const MACH: [&[u8]; 5] = [
        &[0xFE, 0xED, 0xFA, 0xCE],
        &[0xFE, 0xED, 0xFA, 0xCF],
        &[0xCE, 0xFA, 0xED, 0xFE],
        &[0xCF, 0xFA, 0xED, 0xFE],
        &[0xCA, 0xFE, 0xBA, 0xBE],
    ];
    if MACH.iter().any(|m| b.starts_with(m)) {
        return Magic::MachExecutable;
    }
    if b.starts_with(b"#!") {
        return Magic::Shebang;
    }

    // ---- archives. ZIP is checked here and NOT treated as benign: `.jar`, `.apk` and Office
    // documents are all ZIPs, so "it is a ZIP" says nothing about whether it is safe.
    if b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") || b.starts_with(b"PK\x07\x08")
    {
        return Magic::Archive;
    }
    if b.starts_with(b"Rar!\x1a\x07") {
        return Magic::Archive;
    }
    if b.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Magic::Archive;
    }
    if b.starts_with(&[0x1f, 0x8b]) {
        return Magic::Archive; // gzip
    }
    if b.starts_with(b"BZh") {
        return Magic::Archive;
    }
    if b.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        return Magic::Archive; // xz
    }
    // TAR's magic sits at offset 257, which is why MAGIC_PREFIX_LEN is 264.
    if b.len() >= 262 && (&b[257..262] == b"ustar") {
        return Magic::Archive;
    }
    if b.starts_with(b"MSCF") {
        return Magic::Archive; // cab
    }

    // ---- media containers.
    if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Magic::Media; // Matroska/WebM (EBML)
    }
    // ISO-BMFF (mp4/m4v/mov): 4-byte size then "ftyp".
    if b.len() >= 8 && &b[4..8] == b"ftyp" {
        return Magic::Media;
    }
    if b.starts_with(b"RIFF") && b.len() >= 12 && &b[8..12] == b"AVI " {
        return Magic::Media;
    }
    if b.starts_with(&[0x30, 0x26, 0xB2, 0x75]) {
        return Magic::Media; // ASF/WMV
    }
    if b.starts_with(b"FLV\x01") {
        return Magic::Media;
    }
    // MPEG program/transport stream.
    if b.starts_with(&[0x00, 0x00, 0x01, 0xBA]) || b.first() == Some(&0x47) {
        return Magic::Media;
    }

    // ---- benign, recognizable non-media.
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Magic::BenignData; // jpeg
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Magic::BenignData;
    }

    Magic::Unknown
}

/// One file as presented to the gate. The caller supplies the leading bytes; this module never
/// touches the filesystem.
#[derive(Debug, Clone)]
pub struct InspectedFile {
    /// Path as reported by the download client, relative to the torrent root.
    pub path: String,
    pub size_bytes: u64,
    /// First [`MAGIC_PREFIX_LEN`] bytes, where the caller could read them. `None` means NOT
    /// READ — which is never treated as clean, unless `bytes_unavailable` explains why.
    pub leading_bytes: Option<Vec<u8>>,
    /// True when bytes are STRUCTURALLY unavailable rather than merely unread — the entry of an
    /// archive that this gate deliberately does not decompress (SAFE-03).
    ///
    /// The distinction matters because the two deserve different reporting. A caller that
    /// FAILED to read a loose file is a per-file problem worth naming per file. An archive
    /// entry can never be byte-checked without decompressing, so repeating "unverified" for
    /// every entry would bury the real findings under noise that says nothing an operator can
    /// act on — the archive is reported ONCE as name-checked-only, at the archive level.
    ///
    /// It does NOT weaken any dangerous rule: an entry named `.exe`, or one attempting path
    /// traversal, is judged exactly as it would be loose.
    pub bytes_unavailable: bool,
}

/// Severity of what was found. Ordered: `Clean < Suspicious < Dangerous`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Clean,
    Suspicious,
    Dangerous,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Clean => "clean",
            Severity::Suspicious => "suspicious",
            Severity::Dangerous => "dangerous",
        })
    }
}

/// A single finding against a single file. Always names the file and states WHY — a verdict an
/// operator cannot act on is not useful, and a blocklist entry with no stated reason is
/// unauditable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    pub severity: Severity,
    pub reason: String,
}

/// The gate's answer for one download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub severity: Severity,
    pub findings: Vec<Finding>,
    /// True when at least one file is an importable media container. A download with no media
    /// at all is not importable regardless of whether anything in it is hostile.
    pub has_media: bool,
    /// Whether LLM adjudication ran, and what it contributed.
    ///
    /// This field exists because the claim was made before it was true. `llm`'s module doc
    /// said the verdict "RECORDS that adjudication did not run", and it did not — `apply()`
    /// copied the severity and dropped the status, so a clean verdict was indistinguishable
    /// from one that had received scrutiny it never got. Both reviewers caught it. An audit
    /// claim that is only in a comment is not an audit claim.
    pub adjudication: Option<AdjudicationRecord>,
}

/// What the LLM stage contributed to a verdict, including having been unable to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjudicationRecord {
    /// Human-readable status: `completed`, `not_configured`, `unavailable: …`, `unparseable: …`.
    pub status: String,
    /// True only when a model actually answered and the answer parsed.
    pub ran: bool,
    /// Severity the model raised the verdict to, if any.
    pub escalated_to: Option<Severity>,
    pub concerns: Vec<String>,
    pub model: String,
}

impl Verdict {
    /// Whether this download may proceed toward the library.
    ///
    /// Deliberately strict: ONLY `Clean` passes. `Suspicious` does not, because every
    /// suspicious category here means "this gate could not establish that the file is safe",
    /// and importing on an unestablished claim is the exact failure mode this module exists to
    /// prevent.
    pub fn is_importable(&self) -> bool {
        self.severity == Severity::Clean && self.has_media
    }
}

fn extension_of(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 12 {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn in_list(ext: &str, list: &[&str]) -> bool {
    list.iter().any(|e| *e == ext)
}

/// Judge one download's file list.
///
/// The rules, in the order they are applied per file:
///
///  1. **Bytes say executable** → Dangerous, always. This outranks the extension entirely: it
///     is the renamed-executable case and the name is exactly what cannot be trusted.
///  2. **Extension says executable** → Dangerous, even if the bytes were unreadable. A file
///     named `.exe` is not given the benefit of the doubt.
///  3. **Extension and magic DISAGREE** → Dangerous, with its own reason. A `.mkv` whose bytes
///     are an archive is not a video, and the mismatch itself is the signal.
///  4. **Archive** → Suspicious, stating that contents were not inspected.
///  5. **Media extension with unreadable or unrecognized bytes** → Suspicious, not Clean.
///  6. **Sidecar** → Clean.
///  7. **Anything else** → Suspicious. The allowlist's default, not an oversight.
pub fn inspect(files: &[InspectedFile]) -> Verdict {
    let mut findings: Vec<Finding> = Vec::new();
    let mut has_media = false;

    for file in files {
        let ext = extension_of(&file.path);
        let ext_ref = ext.as_deref();
        let magic = detect_magic(file.leading_bytes.as_deref());

        let is_media_ext = ext_ref.is_some_and(|e| in_list(e, MEDIA_EXTENSIONS));
        let is_sidecar_ext = ext_ref.is_some_and(|e| in_list(e, SIDECAR_EXTENSIONS));
        let is_exec_ext = ext_ref.is_some_and(|e| in_list(e, EXECUTABLE_EXTENSIONS));
        let is_archive_ext = ext_ref.is_some_and(|e| in_list(e, ARCHIVE_EXTENSIONS));

        // 1. Bytes win over name, always.
        if magic.is_executable() {
            findings.push(Finding {
                path: file.path.clone(),
                severity: Severity::Dangerous,
                reason: if is_media_ext {
                    // The headline case, called out specifically so the operator sees the
                    // deception rather than a generic "executable found".
                    format!(
                        "declared a media file (.{}) but its contents are a {} — a renamed executable",
                        ext_ref.unwrap_or("?"),
                        magic.as_str()
                    )
                } else {
                    format!("contents are a {}", magic.as_str())
                },
            });
            continue;
        }

        // 2. Named as an executable. No benefit of the doubt, bytes or not.
        if is_exec_ext {
            findings.push(Finding {
                path: file.path.clone(),
                severity: Severity::Dangerous,
                reason: format!(
                    "executable or script by extension (.{})",
                    ext_ref.unwrap_or("?")
                ),
            });
            continue;
        }

        // 3. Name and bytes disagree about what this is.
        let mismatch = match (is_media_ext, magic) {
            (true, Magic::Archive) => Some("an archive"),
            (true, Magic::BenignData) => Some("an image or other non-media data"),
            (false, _) if is_sidecar_ext && magic == Magic::Media => Some("a media container"),
            _ => None,
        };
        if let Some(actually) = mismatch {
            findings.push(Finding {
                path: file.path.clone(),
                severity: Severity::Dangerous,
                reason: format!(
                    "extension .{} disagrees with its contents, which are {actually}",
                    ext_ref.unwrap_or("?")
                ),
            });
            continue;
        }

        // 4. Opaque container.
        if is_archive_ext || magic == Magic::Archive {
            findings.push(Finding {
                path: file.path.clone(),
                severity: Severity::Suspicious,
                reason: "archive — contents were not inspected, so nothing inside is certified"
                    .to_string(),
            });
            continue;
        }

        // 5. Looks like media by name.
        if is_media_ext {
            match magic {
                Magic::Media => {
                    has_media = true;
                }
                // Structurally unavailable (an archive entry) is reported once at the archive
                // level, not once per entry — see `InspectedFile::bytes_unavailable`.
                Magic::NotProvided if file.bytes_unavailable => {}
                Magic::NotProvided => findings.push(Finding {
                    path: file.path.clone(),
                    severity: Severity::Suspicious,
                    reason: "media extension but its contents were not read, so the file is unverified"
                        .to_string(),
                }),
                _ => findings.push(Finding {
                    path: file.path.clone(),
                    severity: Severity::Suspicious,
                    reason: format!(
                        "media extension but its contents are {} — not a recognized container",
                        magic.as_str()
                    ),
                }),
            }
            continue;
        }

        // 6. Expected companion file.
        if is_sidecar_ext {
            continue;
        }

        // 7. The allowlist default. Not an oversight — see the module doc on failing closed.
        findings.push(Finding {
            path: file.path.clone(),
            severity: Severity::Suspicious,
            reason: match ext_ref {
                Some(e) => format!("unrecognized file type (.{e})"),
                None => "unrecognized file with no extension".to_string(),
            },
        });
    }

    let severity = findings
        .iter()
        .map(|f| f.severity)
        .max()
        .unwrap_or(Severity::Clean);

    Verdict {
        severity,
        findings,
        has_media,
        // Set by `llm::apply`. `None` means the LLM stage has not been consulted at all — a
        // different statement from "it ran and added nothing".
        adjudication: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real leading bytes, not hand-waved placeholders — the point of the byte check is that it
    /// matches what a real file starts with.
    const MZ: &[u8] = b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00\xff\xff\x00\x00";
    const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    const MKV: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x00, 0x00, 0x00];
    const MP4: &[u8] = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00";
    const RAR: &[u8] = b"Rar!\x1a\x07\x00\xcf\x90\x73\x00\x00";
    const ZIP: &[u8] = b"PK\x03\x04\x14\x00\x00\x00\x08\x00";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

    fn f(path: &str, bytes: Option<&[u8]>) -> InspectedFile {
        InspectedFile {
            path: path.to_string(),
            size_bytes: 1_000_000,
            leading_bytes: bytes.map(<[u8]>::to_vec),
            bytes_unavailable: false,
        }
    }

    // ── THE CASE THIS MODULE EXISTS FOR ──────────────────────────────────────────────────

    #[test]
    fn a_windows_executable_wearing_a_video_extension_is_dangerous() {
        // The single most common hostile shape on public trackers, and the one an extension
        // check passes without complaint. If this test ever goes green while the gate lets the
        // file through, the module has failed at its only real job.
        let v = inspect(&[f("Movie.2024.1080p.BluRay.mkv", Some(MZ))]);
        assert_eq!(v.severity, Severity::Dangerous);
        assert!(!v.is_importable());
        assert!(
            v.findings[0].reason.contains("renamed executable"),
            "the operator must be told it is a DECEPTION, not just an executable: {}",
            v.findings[0].reason,
        );
    }

    #[test]
    fn an_executable_beside_a_real_video_condemns_the_whole_download() {
        // Severity is the MAXIMUM across files, not an average or a majority. One hostile file
        // in an otherwise perfect release is still a hostile release.
        let v = inspect(&[
            f("Movie.2024.1080p.mkv", Some(MKV)),
            f("Setup.exe", Some(MZ)),
        ]);
        assert_eq!(v.severity, Severity::Dangerous);
        assert!(v.has_media, "the real video is still media");
        assert!(!v.is_importable(), "but the download must not import");
        assert_eq!(v.findings.len(), 1);
        assert_eq!(v.findings[0].path, "Setup.exe");
    }

    #[test]
    fn the_worst_finding_decides_the_verdict_not_the_mildest() {
        // Pins that severity is the MAXIMUM across findings. My first pass at this claim was
        // only covered by a download producing ONE finding, where max and min are identical —
        // so a mutation swapping max() for min() SURVIVED. It needs findings of two different
        // severities to bind, and the consequence of getting it wrong is a dangerous download
        // reported as merely suspicious.
        let v = inspect(&[
            f("Movie.mkv", Some(MKV)),
            f("mystery.dat", Some(b"\x00\x01")), // Suspicious
            f("Setup.exe", Some(MZ)),              // Dangerous
        ]);
        assert_eq!(v.findings.len(), 2, "one suspicious + one dangerous");
        assert_eq!(v.severity, Severity::Dangerous, "the WORST finding must decide");
        assert!(!v.is_importable());
    }

    #[test]
    fn a_shortcut_or_script_is_dangerous_even_with_no_bytes_read() {
        // .lnk/.url are the RARBG-style payloads. Bytes may be unreadable; the name alone is
        // enough, and this must not depend on the caller managing to read the file.
        for name in ["RARBG.lnk", "download.url", "install.bat", "run.vbs", "x.scr"] {
            let v = inspect(&[f(name, None)]);
            assert_eq!(v.severity, Severity::Dangerous, "{name} must be dangerous");
        }
    }

    #[test]
    fn an_elf_binary_is_caught_too() {
        let v = inspect(&[f("readme", Some(ELF))]);
        assert_eq!(v.severity, Severity::Dangerous);
    }

    #[test]
    fn a_shebang_script_is_executable_content() {
        let v = inspect(&[f("notes.txt", Some(b"#!/bin/sh\nrm -rf /"))]);
        assert_eq!(v.severity, Severity::Dangerous);
    }

    // ── FAIL CLOSED ──────────────────────────────────────────────────────────────────────

    #[test]
    fn a_media_file_whose_bytes_were_not_read_is_not_clean() {
        // The fail-closed rule. Unread is UNVERIFIED, and unverified is not safe. Every earlier
        // defect in this codebase (MUSE #106/#108/#109) came from treating an unknown as a
        // benign empty answer; a security gate must not.
        let v = inspect(&[f("Movie.mkv", None)]);
        assert_eq!(v.severity, Severity::Suspicious);
        assert!(!v.is_importable());
    }

    #[test]
    fn an_unrecognized_extension_is_suspicious_not_clean() {
        // The allowlist default. A type nobody classified is suspicious BY CONSTRUCTION rather
        // than clean by omission — which is what makes this a fail-closed design rather than a
        // denylist that must be kept exhaustive.
        let v = inspect(&[f("payload.xyz", Some(b"\x00\x01\x02\x03random"))]);
        assert_eq!(v.severity, Severity::Suspicious);
    }

    #[test]
    fn a_media_extension_with_unrecognized_bytes_is_suspicious() {
        let v = inspect(&[f("Movie.mkv", Some(b"\x00\x01\x02\x03not a container"))]);
        assert_eq!(v.severity, Severity::Suspicious);
        assert!(!v.has_media);
    }

    #[test]
    fn an_archive_is_suspicious_and_says_it_was_not_inspected() {
        // Archives are not condemned — legitimate releases ship .rar parts — but they are
        // opaque, and the verdict must be honest that nothing inside was checked rather than
        // implying the contents passed.
        for (name, bytes) in [("release.rar", RAR), ("release.zip", ZIP)] {
            let v = inspect(&[f(name, Some(bytes))]);
            assert_eq!(v.severity, Severity::Suspicious, "{name}");
            assert!(
                v.findings[0].reason.contains("not inspected"),
                "must state the limit of its own knowledge: {}",
                v.findings[0].reason,
            );
        }
    }

    #[test]
    fn a_zip_is_never_treated_as_benign_data() {
        // .jar, .apk and Office documents are all ZIPs. "It is a ZIP" says nothing about
        // whether it is safe, so ZIP must classify as an opaque archive, never as benign.
        assert_eq!(detect_magic(Some(ZIP)), Magic::Archive);
    }

    // ── MISMATCH IS ITS OWN SIGNAL ───────────────────────────────────────────────────────

    #[test]
    fn a_video_extension_over_archive_bytes_is_a_mismatch_not_merely_an_archive() {
        // Distinct from the plain-archive case: a `.mkv` that is really a `.rar` is a lie about
        // what the file is, and lying is worse than being opaque.
        let v = inspect(&[f("Movie.2024.mkv", Some(RAR))]);
        assert_eq!(v.severity, Severity::Dangerous);
        assert!(v.findings[0].reason.contains("disagrees"));
    }

    #[test]
    fn a_video_extension_over_image_bytes_is_a_mismatch() {
        let v = inspect(&[f("Movie.mkv", Some(JPEG))]);
        assert_eq!(v.severity, Severity::Dangerous);
    }

    // ── THE CLEAN PATH ───────────────────────────────────────────────────────────────────

    #[test]
    fn an_ordinary_release_passes() {
        let v = inspect(&[
            f("Movie.2024.1080p.BluRay.x264.mkv", Some(MKV)),
            f("Movie.2024.1080p.BluRay.x264.srt", Some(b"1\n00:00:01,000 --> ")),
            f("poster.jpg", Some(JPEG)),
            f("Movie.nfo", Some(b"<movie><title>x</title></movie>")),
        ]);
        assert_eq!(v.severity, Severity::Clean, "findings: {:?}", v.findings);
        assert!(v.has_media);
        assert!(v.is_importable());
    }

    #[test]
    fn mp4_is_recognized_by_its_ftyp_box() {
        let v = inspect(&[f("Movie.mp4", Some(MP4))]);
        assert_eq!(v.severity, Severity::Clean);
        assert!(v.is_importable());
    }

    #[test]
    fn subtitles_alone_are_clean_but_not_importable() {
        // No media => nothing to import, even though nothing is wrong. `is_importable` is the
        // conjunction of "safe" AND "actually contains media"; a subtitle-only download must
        // not be treated as a successful import.
        let v = inspect(&[f("Movie.srt", Some(b"1\n00:00"))]);
        assert_eq!(v.severity, Severity::Clean);
        assert!(!v.has_media);
        assert!(!v.is_importable());
    }

    #[test]
    fn an_empty_file_list_is_not_importable() {
        let v = inspect(&[]);
        assert_eq!(v.severity, Severity::Clean);
        assert!(!v.is_importable(), "nothing to import is not a successful import");
    }

    #[test]
    fn only_clean_passes_the_gate() {
        // Pinning the strictness: Suspicious does NOT import. Every suspicious category means
        // "could not establish this is safe", and importing on an unestablished claim is the
        // failure this module exists to prevent.
        let suspicious = inspect(&[f("Movie.mkv", Some(MKV)), f("mystery.dat", Some(b"\x00\x01"))]);
        assert_eq!(suspicious.severity, Severity::Suspicious);
        assert!(suspicious.has_media);
        assert!(!suspicious.is_importable());
    }

    // ── DETAIL ───────────────────────────────────────────────────────────────────────────

    // ── ORCHESTRATION: the parts actually talking to each other ─────────────────────────

    #[test]
    fn a_download_is_as_dangerous_as_its_worst_part_loose_or_archived() {
        // The gap a reviewer found: three correct modules wired to nothing. `inspect` treated
        // an archive as merely opaque and the listing logic had no production caller, so the
        // archive-level executable detection did not actually happen anywhere.
        use std::io::{Cursor, Write};
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            w.start_file("Setup.exe", zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(b"MZ").unwrap();
            w.finish().unwrap();
        }
        let size = buf.len() as u64;

        let v = inspect_download(vec![
            DownloadFile {
                path: "Movie.2024.mkv".into(),
                size_bytes: 1_000_000,
                leading_bytes: Some(vec![0x1A, 0x45, 0xDF, 0xA3]),
                archive_reader: None::<Cursor<Vec<u8>>>,
            },
            DownloadFile {
                path: "extras.zip".into(),
                size_bytes: size,
                leading_bytes: Some(b"PK\x03\x04".to_vec()),
                archive_reader: Some(Cursor::new(buf)),
            },
        ]);

        assert_eq!(v.severity, Severity::Dangerous, "{:?}", v.findings);
        assert!(
            v.findings.iter().any(|f| f.path.contains("Setup.exe")),
            "the executable INSIDE the zip must be named: {:?}",
            v.findings,
        );
        assert!(!v.is_importable());
    }

    #[test]
    fn an_archive_the_caller_cannot_open_is_uninspected_not_clean() {
        use std::io::Cursor;
        let v = inspect_download(vec![DownloadFile {
            path: "release.rar".into(),
            size_bytes: 5_000_000,
            leading_bytes: Some(b"Rar!\x1a\x07\x00".to_vec()),
            archive_reader: None::<Cursor<Vec<u8>>>,
        }]);
        assert_eq!(v.severity, Severity::Suspicious);
        assert!(!v.is_importable());
        assert!(v.findings.iter().any(|f| f.reason.contains("NOT inspected")));
    }

    #[test]
    fn extensions_are_matched_case_insensitively_and_by_the_last_dot() {
        assert_eq!(extension_of("A.Movie.2024.MKV").as_deref(), Some("mkv"));
        assert_eq!(extension_of("dir/sub/Setup.EXE").as_deref(), Some("exe"));
        assert_eq!(extension_of("windows\\path\\x.Exe").as_deref(), Some("exe"));
        assert_eq!(extension_of("no_extension"), None);
        // A trailing dot yields no usable extension rather than an empty one.
        assert_eq!(extension_of("weird."), None);
        // Guards against a "extension" that is really the rest of a long filename.
        assert_eq!(extension_of("a.b_very_long_not_an_extension"), None);
    }

    #[test]
    fn an_uppercase_executable_extension_is_still_dangerous() {
        let v = inspect(&[f("SETUP.EXE", None)]);
        assert_eq!(v.severity, Severity::Dangerous);
    }

    #[test]
    fn tar_is_detected_at_its_offset_257_magic() {
        let mut tar = vec![0u8; MAGIC_PREFIX_LEN];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(detect_magic(Some(&tar)), Magic::Archive);
    }

    #[test]
    fn empty_or_absent_bytes_are_reported_as_not_read_not_as_unknown() {
        // These mean different things: NotProvided is "we did not look", Unknown is "we looked
        // and did not recognize it". Collapsing them would hide a caller that silently failed
        // to read anything.
        assert_eq!(detect_magic(None), Magic::NotProvided);
        assert_eq!(detect_magic(Some(&[])), Magic::NotProvided);
        assert_eq!(detect_magic(Some(b"\x00\x01\x02\x03")), Magic::Unknown);
    }

    #[test]
    fn every_finding_names_its_file_and_gives_a_reason() {
        // A verdict an operator cannot act on is not useful, and a blocklist entry with no
        // stated reason is unauditable.
        let v = inspect(&[f("Setup.exe", Some(MZ)), f("odd.dat", Some(b"\x01\x02"))]);
        for finding in &v.findings {
            assert!(!finding.path.is_empty());
            assert!(finding.reason.len() > 10, "reason too thin: {}", finding.reason);
        }
    }
}

// ===========================================================================
// Orchestration
// ===========================================================================

/// One file of a download, as the caller can present it.
///
/// Separate from [`InspectedFile`] because a caller that can open an archive has more to offer
/// than one that can only stat a name: `archive_reader` lets the gate look INSIDE without this
/// module doing any I/O of its own.
pub struct DownloadFile<R> {
    pub path: String,
    pub size_bytes: u64,
    pub leading_bytes: Option<Vec<u8>>,
    /// A seekable reader, supplied only for files the caller believes are archives. `None`
    /// means the archive is not openable here — which is reported as uninspected, not clean.
    pub archive_reader: Option<R>,
}

/// Judge a whole download: loose files, then anything inside its archives.
///
/// THIS FUNCTION EXISTS BECAUSE THE PARTS DID NOT TALK TO EACH OTHER. `inspect`, `archive` and
/// `llm` were each built and tested in isolation, and a reviewer pointed out that nothing
/// called across them — so `inspect` still treated an archive as merely opaque, the listing
/// logic had no production caller, and the LLM stage was never reached. Three correct modules
/// wired to nothing is not a gate, and describing it as one would have been the largest
/// unsupported claim in the whole feature.
///
/// The severities compose by MAXIMUM: a download is as dangerous as its worst part, whether
/// that part is loose on disk or inside a `.zip`.
pub fn inspect_download<R: std::io::Read + std::io::Seek>(
    files: Vec<DownloadFile<R>>,
) -> Verdict {
    let loose: Vec<InspectedFile> = files
        .iter()
        .map(|f| InspectedFile {
            path: f.path.clone(),
            size_bytes: f.size_bytes,
            leading_bytes: f.leading_bytes.clone(),
            bytes_unavailable: false,
        })
        .collect();
    let mut verdict = inspect(&loose);

    for file in files {
        if !archive::should_list(&file.path, file.leading_bytes.as_deref()) {
            continue;
        }
        let kind = archive::archive_kind(&file.path, file.leading_bytes.as_deref());
        let listing = match (kind, file.archive_reader) {
            (archive::ArchiveKind::Zip, Some(reader)) => archive::list_zip(reader, file.size_bytes),
            // A format with no reader on this deployment, or an archive the caller could not
            // open. Either way its contents are unknown, and unknown is not clean.
            (kind, _) => archive::ArchiveListing::unsupported(format!("{kind:?}")),
        };
        let sub = archive::inspect_listing(&file.path, &listing);
        verdict.severity = verdict.severity.max(sub.severity);
        verdict.findings.extend(sub.findings);
    }

    verdict
}
