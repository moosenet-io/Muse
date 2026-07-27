//! MUSEF-01 — Foundry's typed configuration.
//!
//! Every value here is a **non-secret behavioral setting** (paths, a boolean
//! gate, a retention window, binary names), so it is read through
//! [`crate::config::Config`] like the rest of the crate's behavioral config.
//! Foundry introduces no credentials at this item; the ones it will need later
//! (`OPENSUBTITLES_API_KEY` in MUSEF-16, `MUSE_FOUNDRY_NODE_TOKEN` in
//! MUSEF-11) are secret-shaped and will be added to `Config` wrapped in the
//! crate's redacting secret type, never as bare `String`.
//!
//! ## The two rails configured here
//! 1. **`allowed_roots`** — the default-deny allowlist. Empty means Foundry is
//!    inert and does not register (Module Contract §2).
//! 2. **`enable_mutation`** — the kill-switch, default **false**. With it
//!    closed, Foundry probes, plans and reports, but cannot modify a byte.
//!
//! Both default to the safe value: an operator who sets nothing gets a Foundry
//! that does not exist, not a Foundry pointed at their library.

use std::path::PathBuf;

use crate::foundry::paths::PathGuard;

/// Default retention for the Foundry recycle bin, in days.
///
/// The surveyed *arr fleet has `recycleBin: ""` on every instance — there is no
/// undo in the operator's current setup at all. Foundry supplies its own, and
/// two weeks is long enough to notice a bad transcode through normal viewing.
const DEFAULT_RETENTION_DAYS: u32 = 14;

/// Default probe/encode binary names, resolved via `PATH`.
const DEFAULT_FFPROBE_BIN: &str = "ffprobe";
const DEFAULT_HANDBRAKE_BIN: &str = "HandBrakeCLI";

/// Typed Foundry configuration.
///
/// The path fields are `pub(in crate::foundry)`: they are raw `PathBuf`s, and
/// handing them to outside code would let a caller append a child and call
/// `std::fs` directly, bypassing the guard. Outside Foundry, use the
/// capability-free diagnostics on [`super::Foundry`]
/// (`root_descriptions`, `root_count`, `mutation_enabled`, `retention_days`).
#[derive(Debug, Clone)]
pub struct FoundryConfig {
    /// Default-deny allowlist of roots Foundry may address. Empty ⇒ inert.
    pub(in crate::foundry) allowed_roots: Vec<PathBuf>,
    /// Scratch directory for transcode output, before verification and swap.
    /// Should be on a **different device** from any allowed root (rail 3);
    /// [`FoundryConfig::warnings`] reports it when it is not.
    pub(in crate::foundry) work_dir: Option<PathBuf>,
    /// The mutation kill-switch. Default false.
    pub(in crate::foundry) enable_mutation: bool,
    /// How long a superseded original is retained in the Foundry recycle bin.
    pub(in crate::foundry) retention_days: u32,
    /// `ffprobe` binary (name on `PATH`, or an absolute path).
    pub(in crate::foundry) ffprobe_bin: String,
    /// `HandBrakeCLI` binary (name on `PATH`, or an absolute path).
    pub(in crate::foundry) handbrake_bin: String,
}

impl FoundryConfig {
    /// Build from the already-loaded crate config.
    ///
    /// Takes `&Config` rather than reading the environment directly, so
    /// `config.rs` stays the crate's single env-reading door (see its module
    /// docs) and Foundry stays testable without touching process state.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            allowed_roots: parse_roots(cfg.foundry_allowed_roots.as_deref()),
            work_dir: cfg
                .foundry_work_dir
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            enable_mutation: cfg.foundry_enable_mutation,
            retention_days: cfg.foundry_retention_days.unwrap_or(DEFAULT_RETENTION_DAYS),
            ffprobe_bin: non_empty_or(cfg.foundry_ffprobe_bin.as_deref(), DEFAULT_FFPROBE_BIN),
            handbrake_bin: non_empty_or(
                cfg.foundry_handbrake_bin.as_deref(),
                DEFAULT_HANDBRAKE_BIN,
            ),
        }
    }

    /// Build the [`PathGuard`] every Foundry operation resolves paths through.
    ///
    /// Two protections, both added after the S128 MUSEF-01 review pointed out
    /// that a public `guard()` on a struct with public fields let any caller
    /// assemble `FoundryConfig { enable_mutation: true, allowed_roots: vec![…] }`
    /// and mint an operational guard — bypassing [`super::Foundry::from_config`]
    /// and every validation in it, including the fatal rail-3 checks. That
    /// contradicted this module's own claim that only a *registered* Foundry
    /// yields a guard.
    ///
    /// 1. `pub(crate)`, so nothing outside the crate can call it at all.
    /// 2. **Fail-closed on its own validation**: it returns `None` when
    ///    [`FoundryConfig::fatal_errors`] is non-empty, so even in-crate code
    ///    that skips `from_config` cannot obtain a mutation-capable guard for a
    ///    configuration that would have been refused at registration. The
    ///    validation lives with the capability rather than only at the call
    ///    site that happens to remember it.
    pub(crate) fn guard(&self) -> Option<PathGuard> {
        if !self.fatal_errors().is_empty() {
            return None;
        }
        Some(PathGuard::new(self.guard_roots(), self.enable_mutation))
    }

    /// Every root the guard allows: the **library roots** plus the **work
    /// root**.
    ///
    /// Including `work_dir` is not a loosening — it is required for the guard
    /// to be usable at all, and its absence was a real gap (S128 round-13
    /// review). Foundry must address its own scratch and staging area:
    /// MUSEF-08 stages an encode there, and MUSEF-12 has the server resolve a
    /// distributed worker's output there. With only the library roots
    /// allowlisted there is no authority for those paths, which would force
    /// the very guard bypass this type exists to prevent.
    ///
    /// Rail 3 constrains the *relationship* between the two — the work root
    /// must sit outside every library root, on a different filesystem, and
    /// `rail3_problems` enforces that — but it does not remove the work root
    /// from the set of addressable paths.
    pub(crate) fn guard_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.allowed_roots.clone();
        if let Some(work) = &self.work_dir {
            roots.push(work.clone());
        }
        roots
    }

    /// True when no roots are configured at all — Foundry must not register
    /// its surface (Module Contract §2). Note this is the *pre*-canonicalization
    /// check; [`PathGuard::is_inert`] is the authoritative post-resolution one,
    /// since a configured-but-unmounted root also yields an inert guard.
    pub fn is_unconfigured(&self) -> bool {
        self.allowed_roots.is_empty()
    }

    /// Configuration problems that are **fatal once the mutation gate is
    /// open**.
    ///
    /// The distinction matters and is load-bearing (it was a real
    /// contradiction in an early draft of the S128 spec: MUSEF-01 warned where
    /// MUSEF-08 said "required"). A `work_dir` on the same filesystem as the
    /// library breaks safety rail 3 — but only if anything is ever actually
    /// staged there. While `enable_mutation` is false the setting is inert, so
    /// it is a warning; the moment mutation is enabled the same layout is a
    /// startup refusal. Warn when it cannot bite, refuse when it can.
    ///
    /// Empty ⇒ safe to start.
    pub fn fatal_errors(&self) -> Vec<String> {
        if !self.enable_mutation {
            return Vec::new();
        }
        let mut out = Vec::new();

        // Mutation with nowhere to stage is not a warning — there is no safe
        // way to honour rail 3, so the gate must not open (S128 MUSEF-01
        // review: this previously only warned, and Foundry registered anyway).
        if self.work_dir.is_none() {
            out.push(
                "MUSE_FOUNDRY_ENABLE_MUTATION is set but MUSE_FOUNDRY_WORK_DIR is not — \
                 there is no location outside the library to stage output, so the \
                 never-in-place rail cannot be honoured"
                    .to_string(),
            );
        }

        // Every rail-3 problem is fatal once mutation is possible.
        out.extend(self.rail3_problems());
        out
    }

    /// Non-fatal configuration problems worth surfacing at startup and on the
    /// status endpoint. Returning them (rather than logging inline) keeps this
    /// type pure and lets the status surface show the operator the same list.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();

        // When mutation is enabled these are reported by `fatal_errors`
        // instead, and reporting them twice would be noise.
        if !self.enable_mutation {
            out.extend(self.rail3_problems());
        }

        if self.retention_days == 0 {
            out.push(
                "MUSE_FOUNDRY_RETENTION_DAYS is 0 — superseded originals would be \
                 unrecoverable immediately after a swap"
                    .to_string(),
            );
        }

        out
    }

    /// Layout problems that violate safety rail 3 ("never in-place": staged
    /// output must live on a different filesystem from the library).
    ///
    /// Reported as warnings while mutation is closed and as fatal errors once
    /// it is open — see [`FoundryConfig::fatal_errors`].
    fn rail3_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Some(work) = &self.work_dir else {
            return out;
        };

        // A scratch dir very often does not exist yet at startup. Statting it
        // directly would return None and silently skip the whole check (S128
        // MUSEF-01 review), so resolve the nearest EXISTING ancestor instead —
        // whatever filesystem that sits on is the one the work dir will be
        // created on. Canonicalizing also means the containment test below
        // compares real paths, not lexical ones, so a symlinked work dir
        // cannot evade it.
        // `projected_path`, not just the anchor: the anchor alone discards the
        // unresolved components, which a `..` in the configured value can
        // exploit to evade both checks below. See its doc comment.
        let work_projected = projected_path(work);
        let work_anchor = work_projected
            .as_deref()
            .and_then(nearest_existing_ancestor)
            .or_else(|| nearest_existing_ancestor(work));

        match work_anchor.as_deref().and_then(device_of) {
            Some(work_dev) => {
                for root in &self.allowed_roots {
                    let root_anchor = projected_path(root)
                        .as_deref()
                        .and_then(nearest_existing_ancestor)
                        .or_else(|| nearest_existing_ancestor(root));
                    if root_anchor.as_deref().and_then(device_of) == Some(work_dev) {
                        out.push(format!(
                            "MUSE_FOUNDRY_WORK_DIR ({}) is on the same filesystem as the \
                             allowed root {} — a failed swap could consume the library's \
                             own free space; put the work dir on a different device",
                            work.display(),
                            root.display()
                        ));
                    }
                }
            }
            None => {
                // Fail closed: if we cannot determine the device at all, we
                // cannot assert rail 3 holds, and this list is consulted as
                // fatal_errors() when mutation is enabled.
                out.push(format!(
                    "MUSE_FOUNDRY_WORK_DIR ({}) has no resolvable existing ancestor, so \
                     its filesystem cannot be determined and the never-in-place rail \
                     cannot be verified",
                    work.display()
                ));
            }
        }

        // A work dir *inside* an allowed root is worse than same-device: the
        // library scan would see Foundry's own scratch output. Compare
        // canonicalized anchors so a symlinked or `..`-laden work dir cannot
        // evade the check lexically.
        for root in &self.allowed_roots {
            let root_projected = projected_path(root);
            let inside = match (&work_projected, &root_projected) {
                // Compare the paths each value will actually denote, so a
                // `..`-laden or partially-missing work dir cannot evade this.
                (Some(w), Some(r)) => w.starts_with(r),
                // Fall back to the lexical comparison only when projection is
                // impossible; better a false positive than a miss.
                _ => work.starts_with(root),
            };
            if inside {
                out.push(format!(
                    "MUSE_FOUNDRY_WORK_DIR ({}) is inside the allowed root {} — \
                     scratch output would appear in the library",
                    work.display(),
                    root.display()
                ));
            }
        }

        out
    }
}

/// Split the configured roots. Accepts `:`-separated (PATH-style) values, since
/// a media path can plausibly contain a comma but never a colon on this fleet.
fn parse_roots(raw: Option<&str>) -> Vec<PathBuf> {
    raw.unwrap_or("")
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn non_empty_or(v: Option<&str>, default: &str) -> String {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
}

/// The device id a path lives on, or `None` if it cannot be stat'ed.
fn device_of(p: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.dev())
}

/// The canonicalized nearest existing ancestor of `p` (possibly `p` itself).
///
/// A configured scratch or library directory may not exist yet at startup.
/// Statting it directly yields `None` and silently skips every filesystem
/// check that depends on it; walking up to the first existing ancestor gives
/// the filesystem the path *will* be created on, which is the thing rail 3
/// actually cares about. Canonicalizing on the way out means callers compare
/// real paths rather than lexical ones.
fn nearest_existing_ancestor(p: &std::path::Path) -> Option<PathBuf> {
    let mut cur = Some(p);
    while let Some(c) = cur {
        if let Ok(canon) = std::fs::canonicalize(c) {
            return Some(canon);
        }
        cur = c.parent();
    }
    None
}

/// The path `p` will resolve to once its missing directories exist.
///
/// Walking to the nearest existing ancestor is not enough on its own, and the
/// gap was a real hole (found by the S128 MUSEF-01 review): the walk
/// **discards the unresolved components**, so for a root `/mnt/media` on one
/// device and a `work_dir` of `/mnt/not-created/../media/.work`, the anchor
/// comes back as `/mnt` — a different device, and not inside `/mnt/media`. Both
/// rail-3 checks pass. Then `not-created` gets created, the path resolves to
/// `/mnt/media/.work`, and the work dir is inside the library on the library's
/// own filesystem, exactly what rail 3 forbids.
///
/// So the unresolved remainder is **replayed** onto the canonical anchor, with
/// `..` applied lexically (safe here: the anchor is already canonical, so there
/// are no symlinks left in it to be confused by) and `.` dropped. The result is
/// the path the configured value will actually denote.
fn projected_path(p: &std::path::Path) -> Option<PathBuf> {
    use std::path::Component;

    // Find the deepest existing ancestor and remember how far down it was, so
    // the remaining components can be replayed onto its canonical form.
    let mut prefix_len = p.components().count();
    let mut anchor = None;
    let mut cur = Some(p);
    while let Some(c) = cur {
        if let Ok(canon) = std::fs::canonicalize(c) {
            anchor = Some(canon);
            prefix_len = c.components().count();
            break;
        }
        cur = c.parent();
        if cur.is_some() {
            continue;
        }
    }

    let mut out = anchor?;
    for comp in p.components().skip(prefix_len) {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Lexical pop is sound: `out` starts canonical and every
                // component appended below is a plain name.
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            // A rooted/prefix component cannot appear after the first, and the
            // caller only ever passes absolute paths.
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_parse_from_a_colon_separated_list() {
        let r = parse_roots(Some("/srv/a:/srv/b"));
        assert_eq!(r, vec![PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]);
    }

    #[test]
    fn roots_tolerate_whitespace_and_empty_segments() {
        let r = parse_roots(Some(" /srv/a : : /srv/b "));
        assert_eq!(r, vec![PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]);
    }

    #[test]
    fn unset_or_empty_roots_yield_an_unconfigured_foundry() {
        assert!(parse_roots(None).is_empty());
        assert!(parse_roots(Some("")).is_empty());
        assert!(parse_roots(Some("   ")).is_empty());
    }

    fn cfg_with(roots: Option<&str>, mutation: bool) -> FoundryConfig {
        FoundryConfig {
            allowed_roots: parse_roots(roots),
            work_dir: None,
            enable_mutation: mutation,
            retention_days: DEFAULT_RETENTION_DAYS,
            ffprobe_bin: DEFAULT_FFPROBE_BIN.to_string(),
            handbrake_bin: DEFAULT_HANDBRAKE_BIN.to_string(),
        }
    }

    #[test]
    fn no_roots_means_unconfigured() {
        assert!(cfg_with(None, false).is_unconfigured());
        assert!(!cfg_with(Some("/srv/a"), false).is_unconfigured());
    }

    #[test]
    fn mutation_disabled_without_a_work_dir_does_not_warn() {
        let c = cfg_with(Some("/srv/a"), false);
        assert!(c.warnings().is_empty(), "got {:?}", c.warnings());
    }

    #[test]
    fn zero_retention_warns() {
        let mut c = cfg_with(Some("/srv/a"), false);
        c.retention_days = 0;
        assert!(c.warnings().iter().any(|w| w.contains("RETENTION_DAYS is 0")));
    }

    #[test]
    fn a_work_dir_inside_an_allowed_root_warns() {
        let mut c = cfg_with(Some("/srv/media"), false);
        c.work_dir = Some(PathBuf::from("/srv/media/.foundry-work"));
        assert!(
            c.warnings().iter().any(|w| w.contains("is inside the allowed root")),
            "got {:?}",
            c.warnings()
        );
    }

    #[test]
    fn a_rail3_violation_warns_while_mutation_is_closed_but_is_fatal_once_open() {
        // The whole point: warn when it cannot bite, refuse when it can.
        let mut c = cfg_with(Some("/srv/media"), false);
        c.work_dir = Some(PathBuf::from("/srv/media/.foundry-work"));

        assert!(
            c.fatal_errors().is_empty(),
            "inert while mutation is closed, got {:?}",
            c.fatal_errors()
        );
        assert!(
            c.warnings().iter().any(|w| w.contains("is inside the allowed root")),
            "should still warn, got {:?}",
            c.warnings()
        );

        c.enable_mutation = true;
        assert!(
            c.fatal_errors().iter().any(|w| w.contains("is inside the allowed root")),
            "must be fatal once mutation is open, got {:?}",
            c.fatal_errors()
        );
    }

    #[test]
    fn a_work_dir_outside_every_root_does_not_trip_the_containment_check() {
        // Isolates containment from the device comparison: this asserts only
        // that a work dir which is not inside any root produces no
        // "inside the allowed root" diagnostic. It deliberately does NOT
        // assert fatal_errors() is empty — whether the two land on the same
        // filesystem depends on the host's mount topology, and the
        // same-filesystem check is exercised separately.
        let mut c = cfg_with(Some("/srv/media"), true);
        c.work_dir = Some(PathBuf::from("/var/tmp/muse-foundry-work"));
        assert!(
            !c.fatal_errors().iter().any(|e| e.contains("is inside the allowed root")),
            "got {:?}",
            c.fatal_errors()
        );
    }

    #[test]
    fn mutation_without_a_work_dir_is_fatal_not_merely_a_warning() {
        // There is no safe way to honour rail 3 with nowhere to stage, so the
        // gate must not open at all (S128 MUSEF-01 review).
        let c = cfg_with(Some("/srv/a"), true);
        assert!(
            c.fatal_errors().iter().any(|e| e.contains("MUSE_FOUNDRY_WORK_DIR is not")),
            "expected a fatal error, got {:?}",
            c.fatal_errors()
        );
    }

    #[test]
    fn a_nonexistent_work_dir_still_gets_its_filesystem_checked() {
        // The scratch dir usually does not exist yet at startup. Statting it
        // directly returns None and would silently skip the whole rail-3
        // check; the nearest-existing-ancestor walk is what prevents that.
        let base = std::env::temp_dir();
        let mut c = cfg_with(None, true);
        c.allowed_roots = vec![base.clone()];
        c.work_dir = Some(base.join("muse-foundry-not-created-yet").join("deep"));

        let fatal = c.fatal_errors();
        assert!(
            fatal.iter().any(|e| e.contains("same filesystem") || e.contains("inside the allowed root")),
            "a nonexistent work dir under the root must still be caught, got {fatal:?}"
        );
    }

    #[test]
    fn an_undeterminable_work_dir_filesystem_fails_closed() {
        // If we cannot resolve any existing ancestor we cannot assert rail 3,
        // so this must be reported rather than silently passing.
        let mut c = cfg_with(Some("/srv/a"), true);
        c.work_dir = Some(PathBuf::from("/nonexistent-root-xyzzy/work"));
        let fatal = c.fatal_errors();
        assert!(
            !fatal.is_empty(),
            "an unresolvable work dir must not silently pass"
        );
    }

    #[test]
    fn warnings_do_not_duplicate_fatal_errors() {
        let c = cfg_with(Some("/srv/a"), true);
        assert!(
            !c.warnings().iter().any(|w| w.contains("MUSE_FOUNDRY_WORK_DIR is not")),
            "the missing-work-dir message belongs to fatal_errors when mutating"
        );
    }

    #[test]
    fn a_parent_dir_component_cannot_smuggle_the_work_dir_into_a_root() {
        // THE regression test for the S128 MUSEF-01 review finding. Walking to
        // the nearest existing ancestor discards the unresolved components, so
        // `<tmp>/not-created/../lib/.work` anchored to `<tmp>` — outside the
        // root and (potentially) a different device — and both rail-3 checks
        // passed. Once `not-created` exists the path denotes `<tmp>/lib/.work`,
        // inside the library. Replaying the remainder is what closes it.
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join("muse-foundry-rail3-lib");
        std::fs::create_dir_all(&root).unwrap();

        let mut c = cfg_with(None, true);
        c.allowed_roots = vec![root.clone()];
        c.work_dir = Some(
            base.join("muse-foundry-not-created")
                .join("..")
                .join("muse-foundry-rail3-lib")
                .join(".work"),
        );

        let fatal = c.fatal_errors();
        assert!(
            fatal.iter().any(|e| e.contains("is inside the allowed root")),
            "a `..`-laden work dir that lands inside the root must be caught, got {fatal:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn projected_path_replays_the_unresolved_remainder() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        // `<base>/a/../b` must project to `<base>/b` even though neither
        // `a` nor `b` exists.
        let p = base.join("muse-fnd-a").join("..").join("muse-fnd-b");
        assert_eq!(projected_path(&p), Some(base.join("muse-fnd-b")));
    }

    #[test]
    fn projected_path_is_identity_for_an_existing_canonical_path() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert_eq!(projected_path(&base), Some(base.clone()));
    }

    #[test]
    fn the_work_root_is_addressable_but_still_outside_the_library() {
        // Both halves matter. The guard must allow the work root (otherwise
        // MUSEF-08 cannot stage and MUSEF-12 cannot resolve worker output),
        // while rail 3 still requires it to live outside every library root.
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let lib = base.join("muse-fnd-lib-addr");
        let work = base.join("muse-fnd-work-addr");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&work).unwrap();

        let mut c = cfg_with(None, false);
        c.allowed_roots = vec![lib.clone()];
        c.work_dir = Some(work.clone());

        let roots = c.guard_roots();
        assert!(roots.contains(&lib), "library root must be allowed");
        assert!(roots.contains(&work), "work root must be allowed");

        // And a real path under the work root resolves through the guard.
        let g = c.guard().expect("valid read-only config yields a guard");
        let staged = work.join("job-1.tmp");
        std::fs::write(&staged, b"x").unwrap();
        assert!(
            g.resolve(&staged).is_ok(),
            "the server must be able to address its own staging area"
        );

        let _ = std::fs::remove_dir_all(&lib);
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn binary_names_fall_back_to_path_lookups() {
        assert_eq!(non_empty_or(None, DEFAULT_FFPROBE_BIN), "ffprobe");
        assert_eq!(non_empty_or(Some("  "), DEFAULT_HANDBRAKE_BIN), "HandBrakeCLI");
        assert_eq!(non_empty_or(Some("/opt/bin/ffprobe"), DEFAULT_FFPROBE_BIN), "/opt/bin/ffprobe");
    }
}
