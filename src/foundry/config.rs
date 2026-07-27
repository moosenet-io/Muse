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
#[derive(Debug, Clone)]
pub struct FoundryConfig {
    /// Default-deny allowlist of roots Foundry may address. Empty ⇒ inert.
    pub allowed_roots: Vec<PathBuf>,
    /// Scratch directory for transcode output, before verification and swap.
    /// Should be on a **different device** from any allowed root (rail 3);
    /// [`FoundryConfig::warnings`] reports it when it is not.
    pub work_dir: Option<PathBuf>,
    /// The mutation kill-switch. Default false.
    pub enable_mutation: bool,
    /// How long a superseded original is retained in the Foundry recycle bin.
    pub retention_days: u32,
    /// `ffprobe` binary (name on `PATH`, or an absolute path).
    pub ffprobe_bin: String,
    /// `HandBrakeCLI` binary (name on `PATH`, or an absolute path).
    pub handbrake_bin: String,
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
    pub fn guard(&self) -> PathGuard {
        PathGuard::new(&self.allowed_roots, self.enable_mutation)
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
        // Every rail-3 problem is fatal once mutation is possible.
        self.rail3_problems()
    }

    /// Non-fatal configuration problems worth surfacing at startup and on the
    /// status endpoint. Returning them (rather than logging inline) keeps this
    /// type pure and lets the status surface show the operator the same list.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();

        if self.enable_mutation && self.work_dir.is_none() {
            out.push(
                "MUSE_FOUNDRY_ENABLE_MUTATION is set but MUSE_FOUNDRY_WORK_DIR is not — \
                 Foundry has nowhere to stage output outside the library, which defeats \
                 the never-in-place rail"
                    .to_string(),
            );
        }

        out.extend(self.rail3_problems());

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

        if let Some(dev) = device_of(work) {
            for root in &self.allowed_roots {
                if device_of(root) == Some(dev) {
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

        // A work dir *inside* an allowed root is worse than same-device: the
        // library scan would see Foundry's own scratch output.
        for root in &self.allowed_roots {
            if work.starts_with(root) {
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
    fn mutation_without_a_work_dir_warns() {
        let c = cfg_with(Some("/srv/a"), true);
        assert!(
            c.warnings().iter().any(|w| w.contains("MUSE_FOUNDRY_WORK_DIR is not")),
            "expected a missing-work-dir warning, got {:?}",
            c.warnings()
        );
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
    fn a_sound_layout_is_never_fatal() {
        let mut c = cfg_with(Some("/srv/media"), true);
        c.work_dir = Some(PathBuf::from("/var/tmp/muse-foundry-work"));
        // Neither path exists in the test env, so the device comparison is a
        // no-op; what matters is that containment does not fire and nothing
        // is reported fatal.
        assert!(c.fatal_errors().is_empty(), "got {:?}", c.fatal_errors());
    }

    #[test]
    fn binary_names_fall_back_to_path_lookups() {
        assert_eq!(non_empty_or(None, DEFAULT_FFPROBE_BIN), "ffprobe");
        assert_eq!(non_empty_or(Some("  "), DEFAULT_HANDBRAKE_BIN), "HandBrakeCLI");
        assert_eq!(non_empty_or(Some("/opt/bin/ffprobe"), DEFAULT_FFPROBE_BIN), "/opt/bin/ffprobe");
    }
}
