//! MUSEF-01 — the path safety primitive every Foundry operation goes through.
//!
//! ## Why this type exists
//! Foundry is the first Muse subsystem that *writes to* and *deletes from* the
//! media library. Everything else in this crate (`library::scan`,
//! `matching::stills`, `metadata::resolve`) is read-only by construction, so a
//! path bug there costs a wrong row. Here it costs a file — on a library the
//! operator cannot re-download.
//!
//! So the safety property is enforced in the **type system**, not by
//! convention: [`ResolvedPath`] has a private field and no public constructor,
//! and the only way to obtain one is [`PathGuard::resolve`] (or
//! [`PathGuard::resolve_new`]). A later Foundry item that takes a
//! `ResolvedPath` therefore *cannot* be handed an unvalidated path — the
//! unsafe call is unrepresentable rather than merely discouraged. This is the
//! same posture `library::scan` documents for "never create a metadata row",
//! but promoted from a doc comment to a compiler-checked invariant, because
//! the blast radius is larger.
//!
//! ## What "safe" means here
//! A path is safe iff, **after full symlink resolution**, it lies inside one
//! of the configured allowed roots. Resolution-then-check (not check-then-use)
//! is the load-bearing ordering: a `..` component or a symlink pointing out of
//! the library would otherwise pass a textual prefix test and then escape at
//! open(2) time.
//!
//! Note this is deliberately NOT a TOCTOU-proof guarantee — an attacker who
//! can swap a directory for a symlink between our `canonicalize` and a later
//! `open` could still escape. That is out of scope: Foundry's threat model is
//! *our own bugs* and *operator misconfiguration* on a single-tenant home
//! fleet, not a hostile local user racing us. Documented so a future reader
//! does not mistake this for a sandbox.

use std::path::{Component, Path, PathBuf};

/// A filesystem path that has been proven to lie inside a configured Foundry
/// allowed root, with all symlinks resolved.
///
/// Constructible only via [`PathGuard`] — see the module docs for why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolvedPath(PathBuf);

// The raw-path accessors have no consumers yet: MUSEF-01 is deliberately the
// foundation, built and reviewed before any code exists that could misuse it.
// MUSEF-02 (probe) is the first reader and MUSEF-08 (verify-then-swap) the
// first writer. Annotated rather than left as warning noise, and rather than
// widening visibility just to silence it.
#[allow(dead_code)]
impl ResolvedPath {
    /// The underlying canonical path.
    ///
    /// Restricted to the `foundry` module. A reviewer observed that a public
    /// accessor makes the type boundary porous: any caller could take the path
    /// out of a read-only `ResolvedPath` and hand it to `std::fs::write` while
    /// the mutation gate was closed. Narrowing the accessor means nothing
    /// *outside* Foundry can extract a raw path at all.
    ///
    /// **What this does and does not guarantee, stated plainly.** Inside
    /// Foundry the boundary is a strong convention, not a sandbox: Rust cannot
    /// stop a module that holds a `&Path` from calling `std::fs::remove_file`
    /// on it, and read operations legitimately need the path (ffprobe, ffmpeg,
    /// `File::open`). So the guarantee is: *outside* Foundry, no raw path;
    /// *inside* Foundry, a function that takes [`MutablePath`] documents and
    /// type-checks its intent to mutate, and any `fs` mutation reached from a
    /// plain `ResolvedPath` is a reviewable defect. Making that unrepresentable
    /// would need every filesystem call to route through a Foundry IO façade
    /// that only accepts `MutablePath` — a worthwhile design, scoped as a
    /// follow-up rather than claimed here.
    pub(in crate::foundry) fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the owned canonical [`PathBuf`]. Foundry-internal, for the
    /// same reason as [`ResolvedPath::as_path`].
    pub(in crate::foundry) fn into_path_buf(self) -> PathBuf {
        self.0
    }

    /// A lossy display form, safe to hand to logs and error messages outside
    /// Foundry — it cannot be used to open a file.
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl std::fmt::Display for ResolvedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Lossy for non-UTF8, which is correct for a human-facing message; the
        // stored path stays exact. Display leaks no capability — a formatted
        // string cannot be used to open a file without re-resolving it.
        write!(f, "{}", self.0.display())
    }
}

/// A [`ResolvedPath`] that has *also* passed the mutation gate.
///
/// The point of a separate type (S128 MUSEF-01 review): `ResolvedPath` alone
/// proves only "this path is inside the allowlist" — it says nothing about
/// whether mutation is permitted, and `as_path()` would happily hand it to
/// `std::fs::remove_file`. Later Foundry items that modify, move or delete
/// take a `MutablePath`, which can only be obtained through
/// [`PathGuard::resolve_for_mutation`] / [`PathGuard::resolve_new_for_mutation`]
/// — both of which call [`PathGuard::require_mutation`] first.
///
/// So the gate is enforced by the *type* a function accepts, exactly as
/// confinement is, rather than by remembering to call a checker. A read-only
/// operation keeps taking `ResolvedPath` and is unaffected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MutablePath(ResolvedPath);

// Same as ResolvedPath above: no consumers until MUSEF-08.
#[allow(dead_code)]
impl MutablePath {
    /// The underlying canonical path. Foundry-internal, as for
    /// [`ResolvedPath::as_path`].
    pub(in crate::foundry) fn as_path(&self) -> &Path {
        self.0.as_path()
    }

    /// Consume into the owned canonical [`PathBuf`].
    pub(in crate::foundry) fn into_path_buf(self) -> PathBuf {
        self.0.into_path_buf()
    }

    /// Downgrade to a plain [`ResolvedPath`] for a read-only call.
    pub(in crate::foundry) fn as_resolved(&self) -> &ResolvedPath {
        &self.0
    }

    /// A lossy display form, safe outside Foundry.
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl std::fmt::Display for MutablePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why a path was refused, or why a mutation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The input was not absolute. Foundry never resolves a relative path:
    /// there is no correct base to resolve it against, and guessing one is
    /// how a sandbox escape starts.
    NotAbsolute(PathBuf),
    /// The path (or, for a not-yet-existing path, its parent) does not exist.
    NotFound(PathBuf),
    /// The path resolved successfully but lands outside every allowed root.
    /// Covers `..` traversal and symlink escape alike, since both are only
    /// visible after canonicalization.
    OutsideAllowedRoots { path: PathBuf, resolved: PathBuf },
    /// No allowed roots are configured at all, so nothing is addressable.
    NoAllowedRoots,
    /// A mutating operation was attempted while the mutation gate is closed.
    MutationDisabled,
    /// The OS refused to resolve the path (permissions, I/O, stale NFS handle).
    Io { path: PathBuf, message: String },
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute(p) => write!(
                f,
                "path is not absolute: {} (Foundry never resolves relative paths)",
                p.display()
            ),
            Self::NotFound(p) => write!(f, "path does not exist: {}", p.display()),
            Self::OutsideAllowedRoots { path, resolved } => write!(
                f,
                "path {} resolves to {}, which is outside every configured \
                 Foundry allowed root (MUSE_FOUNDRY_ALLOWED_ROOTS)",
                path.display(),
                resolved.display()
            ),
            Self::NoAllowedRoots => write!(
                f,
                "no Foundry allowed roots are configured \
                 (MUSE_FOUNDRY_ALLOWED_ROOTS is empty) — Foundry is inert"
            ),
            Self::MutationDisabled => write!(
                f,
                "Foundry mutation is disabled (MUSE_FOUNDRY_ENABLE_MUTATION is \
                 not set) — this operation would modify the library"
            ),
            Self::Io { path, message } => {
                write!(f, "could not resolve {}: {}", path.display(), message)
            }
        }
    }
}

impl std::error::Error for PathError {}

/// Validates paths against the configured allowed roots and gates mutation.
///
/// Cheap to clone; hold one per `AppState` rather than rebuilding it per call.
#[derive(Debug, Clone)]
pub struct PathGuard {
    /// Canonicalized allowed roots. Non-existent configured roots are dropped
    /// at construction (and logged) rather than failing startup — a library
    /// mount that is temporarily absent should degrade Foundry, not crash Muse.
    roots: Vec<PathBuf>,
    enable_mutation: bool,
}

impl PathGuard {
    /// Build a guard from raw configured roots.
    ///
    /// `pub(in crate::foundry)` deliberately, and narrowed twice under review.
    /// A guard is a *capability*: code that can mint its own with arbitrary
    /// roots and `enable_mutation: true` has bypassed the configuration
    /// entirely. `pub(crate)` was not enough — any current or future module
    /// anywhere in the crate could still call it — so construction is now
    /// restricted to the `foundry` module itself, where
    /// [`crate::foundry::FoundryConfig::guard`] is the only caller and applies
    /// the full validation first.
    ///
    /// Each root is canonicalized; roots that do not exist or cannot be
    /// resolved are dropped with a warning. If *every* root drops out, the
    /// guard is still constructed but refuses all paths with
    /// [`PathError::NoAllowedRoots`] — callers that want "Foundry is not
    /// configured at all" semantics should check [`PathGuard::is_inert`].
    pub(in crate::foundry) fn new<I, P>(roots: I, enable_mutation: bool) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut resolved = Vec::new();
        for r in roots {
            let raw = r.as_ref();
            if !raw.is_absolute() {
                tracing::warn!(
                    root = %raw.display(),
                    "foundry: ignoring non-absolute allowed root"
                );
                continue;
            }
            match std::fs::canonicalize(raw) {
                Ok(c) => {
                    if resolved.contains(&c) {
                        tracing::debug!(root = %c.display(), "foundry: duplicate allowed root ignored");
                    } else {
                        resolved.push(c);
                    }
                }
                Err(e) => {
                    // Deliberately non-fatal: an unmounted NFS library at boot
                    // must not prevent Muse from starting.
                    tracing::warn!(
                        root = %raw.display(),
                        error = %e,
                        "foundry: allowed root is unreachable and has been dropped"
                    );
                }
            }
        }
        Self {
            roots: resolved,
            enable_mutation,
        }
    }

    /// True when no usable allowed root survived construction — Foundry should
    /// not register its surface at all (Module Contract §2: absent backend
    /// capability ⇒ inert, never broken).
    pub fn is_inert(&self) -> bool {
        self.roots.is_empty()
    }

    /// The canonical allowed roots, for diagnostics and the status endpoint.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether the mutation gate is open.
    pub fn mutation_enabled(&self) -> bool {
        self.enable_mutation
    }

    /// Gate a mutating operation.
    ///
    /// Every Foundry call site that would modify, move, or delete a file calls
    /// this *first*. Returning a `Result` (rather than exposing the bool) is
    /// what makes the gate hard to forget: `?` at the top of a function is the
    /// natural way to write it, and a missing call is visible in review as a
    /// mutating function that never mentions the guard.
    pub fn require_mutation(&self) -> Result<(), PathError> {
        if self.enable_mutation {
            Ok(())
        } else {
            Err(PathError::MutationDisabled)
        }
    }

    /// Resolve an **existing** path and prove it is inside an allowed root.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<ResolvedPath, PathError> {
        let raw = path.as_ref();
        self.precheck(raw)?;
        let canonical = std::fs::canonicalize(raw).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => PathError::NotFound(raw.to_path_buf()),
            _ => PathError::Io {
                path: raw.to_path_buf(),
                message: e.to_string(),
            },
        })?;
        self.confine(raw, canonical)
    }

    /// Resolve a path that does **not** exist yet (a transcode output, a
    /// subtitle sidecar, an organizer move target).
    ///
    /// The parent directory must exist and be inside an allowed root; the final
    /// component is appended to the canonical parent. The final component is
    /// required to be a plain name — a `..` or a nested path there would let a
    /// caller step back out of the parent we just proved safe.
    pub fn resolve_new(&self, path: impl AsRef<Path>) -> Result<ResolvedPath, PathError> {
        let raw = path.as_ref();
        self.precheck(raw)?;

        let parent = raw.parent().ok_or_else(|| PathError::NotAbsolute(raw.to_path_buf()))?;
        let name = match raw.file_name() {
            Some(n) => n,
            // No final component at all (e.g. `/`) — nothing to create.
            None => return Err(PathError::NotAbsolute(raw.to_path_buf())),
        };

        let canonical_parent = std::fs::canonicalize(parent).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => PathError::NotFound(parent.to_path_buf()),
            _ => PathError::Io {
                path: parent.to_path_buf(),
                message: e.to_string(),
            },
        })?;

        // Prove the parent is confined *before* appending, then append a single
        // plain component so the result cannot escape it.
        let confined_parent = self.confine(parent, canonical_parent)?;
        let candidate = confined_parent.0.join(name);

        // CRITICAL (S128 MUSEF-01 review, found independently by two
        // reviewers): a confined parent is NOT sufficient on its own.
        // `resolve_new` is also used to address a file that may already exist
        // (replacing a subtitle sidecar, overwriting a stale staged temp), and
        // if that existing final component is a SYMLINK pointing outside the
        // allowlist, returning the unresolved `candidate` hands the caller a
        // path that escapes the moment anything writes through it. The parent
        // check cannot see this, because the escape lives in the leaf.
        //
        // So: if the final component exists at all, fall through to the full
        // existing-path resolution, which canonicalizes (following the
        // symlink) and re-confines. Use `symlink_metadata` so a dangling or
        // escaping symlink is still detected as "exists" — `metadata` follows
        // the link and would report NotFound for a dangling one, letting it
        // slip through as a fresh path.
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => self.resolve(&candidate),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Genuinely new: the parent is confined and the leaf is a
                // plain name, so the result cannot escape.
                Ok(ResolvedPath(candidate))
            }
            Err(e) => Err(PathError::Io {
                path: candidate,
                message: e.to_string(),
            }),
        }
    }

    /// Resolve an existing path **and** pass the mutation gate.
    ///
    /// The only way to obtain a [`MutablePath`] for an existing file. Gate
    /// first, then resolve: a caller with the gate closed learns that before
    /// any filesystem work happens, and cannot distinguish which paths exist.
    pub fn resolve_for_mutation(&self, path: impl AsRef<Path>) -> Result<MutablePath, PathError> {
        self.require_mutation()?;
        Ok(MutablePath(self.resolve(path)?))
    }

    /// Resolve a prospective path **and** pass the mutation gate.
    ///
    /// The only way to obtain a [`MutablePath`] for a file to be created.
    pub fn resolve_new_for_mutation(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<MutablePath, PathError> {
        self.require_mutation()?;
        Ok(MutablePath(self.resolve_new(path)?))
    }

    /// Cheap structural checks shared by both resolve paths.
    fn precheck(&self, raw: &Path) -> Result<(), PathError> {
        if self.roots.is_empty() {
            return Err(PathError::NoAllowedRoots);
        }
        if !raw.is_absolute() {
            return Err(PathError::NotAbsolute(raw.to_path_buf()));
        }
        // A `..` in the *input* is not itself fatal — canonicalization resolves
        // it and the confinement check below is authoritative. But a `..` as
        // the final component of a `resolve_new` target is meaningless, and
        // rejecting the pattern early gives a clearer error than "outside root".
        if raw.components().next_back() == Some(Component::ParentDir) {
            return Err(PathError::NotAbsolute(raw.to_path_buf()));
        }
        Ok(())
    }

    /// The confinement check: is `canonical` inside any allowed root?
    ///
    /// Compares by path *component*, not string prefix — a textual
    /// `starts_with` would wrongly accept `/srv/media-evil` for the root
    /// `/srv/media`. `Path::starts_with` is component-wise, which is why it is
    /// used here rather than `str::starts_with`.
    fn confine(&self, original: &Path, canonical: PathBuf) -> Result<ResolvedPath, PathError> {
        if self.roots.iter().any(|r| canonical.starts_with(r)) {
            Ok(ResolvedPath(canonical))
        } else {
            Err(PathError::OutsideAllowedRoots {
                path: original.to_path_buf(),
                resolved: canonical,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A throwaway directory tree, removed on drop.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "muse-foundry-paths-{}-{}-{:?}",
                tag,
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("create tmp root");
            // Resolve through any symlinked temp dir (macOS /tmp, some CI) so
            // test expectations compare canonical-to-canonical.
            let p = fs::canonicalize(&p).expect("canonicalize tmp root");
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn guard_for(root: &Path, mutation: bool) -> PathGuard {
        PathGuard::new([root], mutation)
    }

    #[test]
    fn resolves_a_path_inside_an_allowed_root() {
        let tmp = Tmp::new("inside");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("movie.mkv");
        fs::write(&file, b"x").unwrap();

        let g = guard_for(&root, false);
        let r = g.resolve(&file).expect("inside root must resolve");
        assert_eq!(r.as_path(), file.as_path());
    }

    #[test]
    fn rejects_a_path_outside_every_allowed_root() {
        let tmp = Tmp::new("outside");
        let root = tmp.path().join("lib");
        let other = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let file = other.join("secret.txt");
        fs::write(&file, b"x").unwrap();

        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve(&file),
            Err(PathError::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn rejects_parent_traversal_out_of_the_root() {
        let tmp = Tmp::new("traverse");
        let root = tmp.path().join("lib");
        let other = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let target = other.join("secret.txt");
        fs::write(&target, b"x").unwrap();

        // Textually rooted under `root`, but `..` escapes it.
        let sneaky = root.join("..").join("elsewhere").join("secret.txt");
        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve(&sneaky),
            Err(PathError::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn rejects_a_symlink_pointing_outside_the_root() {
        let tmp = Tmp::new("symlink");
        let root = tmp.path().join("lib");
        let other = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let target = other.join("secret.txt");
        fs::write(&target, b"x").unwrap();

        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // The link *is* inside the root textually; only canonicalization
        // reveals the escape. This is the check-then-use bug, prevented.
        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve(&link),
            Err(PathError::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn accepts_a_symlink_that_stays_inside_the_root() {
        let tmp = Tmp::new("symlink-inside");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("real.mkv");
        fs::write(&target, b"x").unwrap();
        let link = root.join("link.mkv");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let g = guard_for(&root, false);
        let r = g.resolve(&link).expect("symlink inside root is fine");
        assert_eq!(r.as_path(), target.as_path(), "resolves to the link target");
    }

    #[test]
    fn rejects_a_relative_path() {
        let tmp = Tmp::new("relative");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve("some/relative/path.mkv"),
            Err(PathError::NotAbsolute(_))
        ));
    }

    #[test]
    fn reports_not_found_for_a_missing_path() {
        let tmp = Tmp::new("missing");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve(root.join("nope.mkv")),
            Err(PathError::NotFound(_))
        ));
    }

    #[test]
    fn a_sibling_root_with_a_shared_name_prefix_is_not_inside() {
        // `/…/lib-evil` must not be accepted for the root `/…/lib`.
        // A `str::starts_with` implementation fails this test.
        let tmp = Tmp::new("prefix");
        let root = tmp.path().join("lib");
        let evil = tmp.path().join("lib-evil");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&evil).unwrap();
        let file = evil.join("x.mkv");
        fs::write(&file, b"x").unwrap();

        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve(&file),
            Err(PathError::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn resolve_new_accepts_a_nonexistent_child_of_an_allowed_parent() {
        let tmp = Tmp::new("new-ok");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let g = guard_for(&root, false);
        let out = g
            .resolve_new(root.join("output.mkv"))
            .expect("new file under an allowed root");
        assert_eq!(out.as_path(), root.join("output.mkv").as_path());
        assert!(!out.as_path().exists(), "resolve_new must not create anything");
    }

    #[test]
    fn resolve_new_rejects_a_target_whose_parent_escapes() {
        let tmp = Tmp::new("new-escape");
        let root = tmp.path().join("lib");
        let other = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve_new(other.join("output.mkv")),
            Err(PathError::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn resolve_new_rejects_a_missing_parent() {
        let tmp = Tmp::new("new-noparent");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let g = guard_for(&root, false);
        assert!(matches!(
            g.resolve_new(root.join("no-such-dir").join("out.mkv")),
            Err(PathError::NotFound(_))
        ));
    }

    #[test]
    fn resolve_new_rejects_an_existing_leaf_symlink_that_escapes() {
        // THE regression test for the S128 MUSEF-01 review finding. The parent
        // is legitimately inside the root, so the parent-confinement check
        // passes; the escape is entirely in the leaf. Returning the unresolved
        // candidate here would hand the caller a path that escapes the instant
        // anything writes through it.
        let tmp = Tmp::new("new-leaf-symlink");
        let root = tmp.path().join("lib");
        let other = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let outside = other.join("victim.srt");
        fs::write(&outside, b"x").unwrap();

        let link = root.join("subtitle.srt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let g = guard_for(&root, false);
        assert!(
            matches!(g.resolve_new(&link), Err(PathError::OutsideAllowedRoots { .. })),
            "an existing leaf symlink pointing outside must be refused"
        );
    }

    #[test]
    fn resolve_new_rejects_a_dangling_leaf_symlink_that_escapes() {
        // `metadata` follows the link and reports NotFound for a dangling one,
        // which would let it through as a "fresh" path; `symlink_metadata` is
        // what makes this case detectable.
        let tmp = Tmp::new("new-dangling");
        let root = tmp.path().join("lib");
        let other = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();

        let link = root.join("subtitle.srt");
        std::os::unix::fs::symlink(other.join("does-not-exist.srt"), &link).unwrap();

        let g = guard_for(&root, false);
        assert!(
            g.resolve_new(&link).is_err(),
            "a dangling leaf symlink pointing outside must not resolve as a fresh path"
        );
    }

    #[test]
    fn resolve_new_accepts_an_existing_leaf_that_stays_inside() {
        // Replacing an existing sidecar is a legitimate use of resolve_new.
        let tmp = Tmp::new("new-existing-ok");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let existing = root.join("subtitle.srt");
        fs::write(&existing, b"x").unwrap();

        let g = guard_for(&root, false);
        let r = g.resolve_new(&existing).expect("existing leaf inside the root is fine");
        assert_eq!(r.as_path(), existing.as_path());
    }

    #[test]
    fn a_mutable_path_cannot_be_obtained_while_the_gate_is_closed() {
        let tmp = Tmp::new("cap");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let f = root.join("a.mkv");
        fs::write(&f, b"x").unwrap();

        let closed = guard_for(&root, false);
        assert!(matches!(
            closed.resolve_for_mutation(&f),
            Err(PathError::MutationDisabled)
        ));
        assert!(matches!(
            closed.resolve_new_for_mutation(root.join("new.mkv")),
            Err(PathError::MutationDisabled)
        ));
        // ...but a read-only resolve still works, which is the point.
        assert!(closed.resolve(&f).is_ok());

        let open = guard_for(&root, true);
        assert!(open.resolve_for_mutation(&f).is_ok());
        assert!(open.resolve_new_for_mutation(root.join("new.mkv")).is_ok());
    }

    #[test]
    fn a_mutable_path_still_cannot_escape_the_allowlist() {
        // The gate and the confinement are independent: opening the gate must
        // not weaken containment.
        let tmp = Tmp::new("cap-escape");
        let root = tmp.path().join("lib");
        let other = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let outside = other.join("x.mkv");
        fs::write(&outside, b"x").unwrap();

        let open = guard_for(&root, true);
        assert!(matches!(
            open.resolve_for_mutation(&outside),
            Err(PathError::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn mutation_gate_is_closed_by_default_and_opens_explicitly() {
        let tmp = Tmp::new("gate");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();

        let closed = guard_for(&root, false);
        assert!(!closed.mutation_enabled());
        assert!(matches!(
            closed.require_mutation(),
            Err(PathError::MutationDisabled)
        ));

        let open = guard_for(&root, true);
        assert!(open.mutation_enabled());
        assert!(open.require_mutation().is_ok());
    }

    #[test]
    fn an_unreachable_root_is_dropped_not_fatal() {
        let tmp = Tmp::new("droproot");
        let good = tmp.path().join("lib");
        fs::create_dir_all(&good).unwrap();
        let missing = tmp.path().join("not-mounted");

        let g = PathGuard::new([missing.clone(), good.clone()], false);
        assert_eq!(g.roots().len(), 1, "only the reachable root survives");
        assert!(!g.is_inert());
        // The surviving root still works.
        let f = good.join("a.mkv");
        fs::write(&f, b"x").unwrap();
        assert!(g.resolve(&f).is_ok());
    }

    #[test]
    fn no_usable_roots_makes_the_guard_inert_and_refuses_everything() {
        let tmp = Tmp::new("inert");
        let missing = tmp.path().join("not-mounted");
        let g = PathGuard::new([missing], false);
        assert!(g.is_inert());
        assert!(matches!(
            g.resolve("/etc/hostname"),
            Err(PathError::NoAllowedRoots)
        ));
    }

    #[test]
    fn a_non_absolute_configured_root_is_ignored() {
        let g = PathGuard::new(["relative/root"], false);
        assert!(g.is_inert());
    }

    #[test]
    fn duplicate_roots_are_collapsed() {
        let tmp = Tmp::new("dupe");
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let g = PathGuard::new([root.clone(), root.clone()], false);
        assert_eq!(g.roots().len(), 1);
    }

    #[test]
    fn multiple_roots_are_each_honored() {
        let tmp = Tmp::new("multi");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let fa = a.join("x.mkv");
        let fb = b.join("y.mkv");
        fs::write(&fa, b"x").unwrap();
        fs::write(&fb, b"y").unwrap();

        let g = PathGuard::new([a, b], false);
        assert!(g.resolve(&fa).is_ok());
        assert!(g.resolve(&fb).is_ok());
    }
}
