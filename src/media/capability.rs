//! What media tooling actually exists on this host, right now.
//!
//! Built as `foundry::capability` (S128 MUSEF-02) and **promoted unchanged** to
//! `crate::media::capability` by S130-A MPRB-01. Foundry still consumes it
//! through the permanent re-export shim in [`crate::foundry`];
//! [`crate::media::MediaCore`] runs [`detect`] once when it is constructed, so
//! probe consumers can degrade once on a host without `ffprobe` instead of
//! failing per file.
//!
//! ## Why this module exists at all
//! **None of `ffprobe`, `ffmpeg` or `HandBrakeCLI` is installed on <host>**, the
//! container Muse runs in (verified 2026-07-31), nor on the <host> dev box. That
//! is not a temporary embarrassment to be coded around — it is the state the
//! code has to be correct in, and it will change without any code change when
//! the operator installs ffmpeg.
//!
//! So capability is **detected, never assumed**: every tool is probed by
//! execution at the moment it is needed (and on demand for the status surface),
//! and the result is one of three states, not a bool. The three-way split is
//! the whole point:
//!
//! - [`ToolState::Present`] — it ran, and we have its version string.
//! - [`ToolState::Missing`] — the binary does not exist on this host.
//! - [`ToolState::Unusable`] — it exists but did not work (permissions, a
//!   broken install, a non-zero exit, **or a version probe that never
//!   returned**).
//!
//! A bool would collapse the last two, and "installed but broken" reported as
//! "not installed" sends the operator to `apt install` for a problem that is
//! not a missing package.
//!
//! ## What this module must never do
//! Report absence as sufficiency. Nothing here ever answers "no work needed"
//! — it answers "this tool is not here", and the caller
//! ([`crate::foundry::forge`]) turns that into an explicit skip with a stated
//! reason. Silently treating a missing encoder as "the file was fine" is the
//! precise false claim Foundry is built to avoid.
//!
//! Nor may it **wait forever**. See [`detect_tool`]: every probe here runs
//! under a deadline, because this module's one impure call happens at process
//! startup.

use std::time::Duration;

/// Whether one external tool is usable, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// The binary ran and reported a version.
    Present { version: String },
    /// The binary does not exist on this host (spawn failed with `NotFound`).
    /// The expected state on this fleet today.
    Missing,
    /// The binary exists but could not be used. `reason` is operator-facing.
    Unusable { reason: String },
}

impl ToolState {
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }

    /// The version string, or `None` for any non-present state.
    ///
    /// Deliberately `Option` rather than a `""` fallback: an empty version
    /// string in a status panel reads as "present, version unknown", which is
    /// a different claim from "not present".
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Present { version } => Some(version),
            _ => None,
        }
    }
}

/// One tool's detection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReport {
    /// Stable identifier for the tool, for logs and the status surface.
    pub tool: &'static str,
    /// The binary name or path that was actually probed. Included because
    /// "ffprobe is missing" is much less useful than "the configured
    /// `/opt/bin/ffprobe` is missing" when the operator has overridden it.
    /// A binary name is a non-secret behavioral setting, like every other
    /// value in [`crate::foundry::config`].
    pub configured_bin: String,
    pub state: ToolState,
}

impl ToolReport {
    pub fn is_present(&self) -> bool {
        self.state.is_present()
    }

    /// A single operator-facing line.
    pub fn summary(&self) -> String {
        match &self.state {
            ToolState::Present { version } => {
                format!("{} ({}): {}", self.tool, self.configured_bin, version)
            }
            ToolState::Missing => format!(
                "{} ({}): NOT INSTALLED on this host",
                self.tool, self.configured_bin
            ),
            ToolState::Unusable { reason } => format!(
                "{} ({}): present but unusable — {}",
                self.tool, self.configured_bin, reason
            ),
        }
    }
}

/// What this host can actually do with media files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub ffprobe: ToolReport,
    pub ffmpeg: ToolReport,
    pub handbrake: ToolReport,
}

impl Capabilities {
    /// Whether files can be described at all. Everything downstream — planning
    /// included — depends on this, because a plan is a function of a probe.
    pub fn can_probe(&self) -> bool {
        self.ffprobe.is_present()
    }

    /// Whether a transcode can be executed **and verified**.
    ///
    /// Both binaries, not just the encoder. ffprobe is not an optional extra
    /// here: the output of every encode is re-probed before anything is
    /// replaced (see [`crate::foundry::forge::verify_output`]), so without
    /// ffprobe a transcode could be *run* but never *verified* — and an
    /// unverified success is a failure by Foundry's own rule. Requiring both
    /// is what stops "the encoder is installed" from being mistaken for "we
    /// can safely swap files".
    pub fn can_transcode(&self) -> bool {
        self.ffmpeg.is_present() && self.ffprobe.is_present()
    }

    /// Every tool report, for a status endpoint or a startup log.
    pub fn reports(&self) -> [&ToolReport; 3] {
        [&self.ffprobe, &self.ffmpeg, &self.handbrake]
    }

    /// The tools that are not usable, by name. Empty ⇒ fully capable.
    pub fn unavailable(&self) -> Vec<&'static str> {
        self.reports()
            .into_iter()
            .filter(|r| !r.is_present())
            .map(|r| r.tool)
            .collect()
    }
}

/// The `--version` argv for a tool. Trivial, but kept as a named function so
/// the invocation layer has no literals of its own.
fn version_args() -> Vec<String> {
    vec!["-version".to_string()]
}

/// Extract a one-line version banner from a tool's output.
///
/// Pure, and the reason it is pure is that it is the only part of detection
/// that can be tested here: no host in this fleet has any of these binaries, so
/// the *invocation* cannot be exercised, but the parsing of real captured
/// banners can be.
///
/// Returns `None` for output with no non-empty line — which the caller turns
/// into [`ToolState::Unusable`], not into `Present { version: "" }`. A tool
/// that produced nothing has not been observed to work.
pub fn parse_version_banner(output: &str) -> Option<String> {
    const MAX: usize = 200;
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    // ffmpeg's banner runs to a full configure line; a status panel wants the
    // first clause, and an unbounded string in a log field is a hazard.
    let truncated: String = line.chars().take(MAX).collect();
    Some(truncated)
}

// --- The deadline ----------------------------------------------------------

/// The compiled ceiling on ONE version probe, when
/// `MUSE_CAPABILITY_TIMEOUT_SECS` is unset.
///
/// Five seconds is already generous by three orders of magnitude: `-version`
/// formats a banner and exits, which is single-digit milliseconds on any host
/// that is working at all. This is not a budget for slow work — there is no
/// slow work here — it is the line past which "the binary is wedged" is the
/// only remaining explanation.
pub const DEFAULT_CAPABILITY_TIMEOUT_SECS: u64 = 5;

/// Bounds on `MUSE_CAPABILITY_TIMEOUT_SECS`.
///
/// The floor exists for the same reason [`crate::media`]'s does: `0` would
/// report every tool as timed out, so a single mistyped env var would make a
/// fully-provisioned host look like it had no ffmpeg at all. The ceiling is
/// low on purpose and is NOT the probe deadline's six hours — this call is on
/// the startup path, and a startup that is allowed to stall for an hour is the
/// hang this item removes wearing a number.
///
/// Out-of-range values CLAMP rather than fall back, so an operator who asked
/// for more time gets as much as is allowed instead of silently getting the
/// value they were trying to change.
const MIN_CAPABILITY_TIMEOUT_SECS: u64 = 1;
const MAX_CAPABILITY_TIMEOUT_SECS: u64 = 60;

/// The version-probe deadline actually in force, from the crate config's
/// optional seconds value.
///
/// One home for the resolution, called by both entry points that detect —
/// [`crate::media::MediaCore::from_config`] and
/// [`crate::foundry::forge::detect_capabilities`] — so the two cannot drift
/// into disagreeing about how long a wedged tool is waited on.
pub fn resolve_timeout(configured_secs: Option<u64>) -> Duration {
    Duration::from_secs(
        configured_secs
            .map(|s| s.clamp(MIN_CAPABILITY_TIMEOUT_SECS, MAX_CAPABILITY_TIMEOUT_SECS))
            .unwrap_or(DEFAULT_CAPABILITY_TIMEOUT_SECS),
    )
}

// --- The impure edge -------------------------------------------------------

/// Probe one tool by running it, **under a deadline**. The single thin layer
/// that spawns anything in this module.
///
/// `version_argv` is passed in rather than hardcoded because HandBrakeCLI
/// spells the flag `--version` while ffmpeg/ffprobe use `-version`.
///
/// ## Why the deadline is not optional here
/// This runs at `MediaCore` construction, i.e. at process startup. The
/// original of this function used a bare `Command::output()`, which waits for
/// the child with no bound at all — so a configured binary that hangs instead
/// of exiting hung STARTUP: no health endpoint, no log past this line, no
/// serve, and under a supervisor a restart loop that never converges. The
/// trigger is not exotic. `ffprobe` living on, or reading from, a network
/// mount that stalls leaves it in uninterruptible D-state; that is not a
/// hypothesis, it is the fault [`crate::media::probe`] was built after — a
/// single stalled read once held an entire validation run forever.
///
/// The deadline is [`crate::media::probe::spawn_with_timeout`], **called, not
/// restated**. It already does spawn → drain → poll → kill → reap, including
/// the parts that are easy to get wrong (draining both pipes on their own
/// threads so a chatty child cannot deadlock, and `try_wait` rather than
/// `wait` after the kill so the timeout path cannot itself hang on a D-state
/// child). A second copy of that logic here would be a second thing to keep
/// correct, and copies drift.
///
/// A tool that blows the deadline is [`ToolState::Unusable`], never a startup
/// failure and never a panic: "present but not answering" is a normal,
/// reportable host state, and this module exists precisely to express it.
/// [`Capabilities::can_probe`] then returns false and every consumer degrades
/// exactly as it does for a missing binary.
fn detect_tool(
    tool: &'static str,
    bin: &str,
    version_argv: &[String],
    timeout: Duration,
) -> ToolReport {
    use crate::media::probe::ProbeError;

    let state = match crate::media::probe::spawn_with_timeout(bin, version_argv, timeout) {
        Err(ProbeError::ToolMissing { .. }) => ToolState::Missing,
        Err(ProbeError::Timeout { secs }) => ToolState::Unusable {
            reason: format!(
                "`{bin} {}` did not answer within the {secs}s version-probe timeout and was \
                 killed — the binary is present but wedged (a stalled network mount will do \
                 this); raise MUSE_CAPABILITY_TIMEOUT_SECS only if that is genuinely too short",
                version_argv.join(" ")
            ),
        },
        Err(e) => ToolState::Unusable {
            reason: format!("could not be executed: {e}"),
        },
        Ok(out) => {
            if !out.status.success() {
                ToolState::Unusable {
                    reason: format!(
                        "`{bin} {}` exited with {}",
                        version_argv.join(" "),
                        out.status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "a signal".into())
                    ),
                }
            } else {
                // Some tools write the banner to stderr rather than stdout;
                // read both rather than reporting a working tool as broken
                // because it chose the other stream.
                let stdout = String::from_utf8_lossy(&out.stdout);
                let combined = if parse_version_banner(&stdout).is_some() {
                    stdout.into_owned()
                } else {
                    String::from_utf8_lossy(&out.stderr).into_owned()
                };
                match parse_version_banner(&combined) {
                    Some(version) => ToolState::Present { version },
                    None => ToolState::Unusable {
                        reason: "ran successfully but printed no version banner".to_string(),
                    },
                }
            }
        }
    };

    ToolReport {
        tool,
        configured_bin: bin.to_string(),
        state,
    }
}

/// Detect every tool Foundry may use.
///
/// Called on demand by Foundry rather than cached, deliberately: on this fleet
/// ffmpeg is expected to *appear* during the lifetime of the process (the
/// operator installs it into a running container), and a capability snapshot
/// taken at boot would keep reporting "not installed" long after it was. The
/// cost is three `--version` spawns per call, which is nothing next to an
/// encode. [`crate::media::MediaCore`] makes the opposite trade and snapshots
/// once; both go through here.
///
/// `timeout` bounds EACH tool's probe, so the worst case for the whole call is
/// three times it — still bounded, which is the only property startup needs.
/// Resolve it with [`resolve_timeout`] rather than inventing a value.
pub fn detect(
    ffprobe_bin: &str,
    ffmpeg_bin: &str,
    handbrake_bin: &str,
    timeout: Duration,
) -> Capabilities {
    let ffmpeg_style = version_args();
    Capabilities {
        ffprobe: detect_tool("ffprobe", ffprobe_bin, &ffmpeg_style, timeout),
        ffmpeg: detect_tool("ffmpeg", ffmpeg_bin, &ffmpeg_style, timeout),
        // HandBrakeCLI spells it with two dashes and rejects `-version`.
        handbrake: detect_tool(
            "HandBrakeCLI",
            handbrake_bin,
            &["--version".to_string()],
            timeout,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(tool: &'static str, state: ToolState) -> ToolReport {
        ToolReport {
            tool,
            configured_bin: tool.to_string(),
            state,
        }
    }

    fn caps(ffprobe: ToolState, ffmpeg: ToolState, handbrake: ToolState) -> Capabilities {
        Capabilities {
            ffprobe: report("ffprobe", ffprobe),
            ffmpeg: report("ffmpeg", ffmpeg),
            handbrake: report("HandBrakeCLI", handbrake),
        }
    }

    fn present(v: &str) -> ToolState {
        ToolState::Present {
            version: v.to_string(),
        }
    }

    /// The deadline for tests that are NOT about the deadline.
    ///
    /// Deliberately generous, so a loaded CI box cannot turn an assertion about
    /// classification into a flaky timeout. The CAPDET-01 tests below pass
    /// their own short value.
    fn generous() -> Duration {
        Duration::from_secs(30)
    }

    #[test]
    fn a_real_ffmpeg_banner_yields_its_first_line() {
        // Captured from `ffmpeg -version` on Debian bookworm.
        let banner = "ffmpeg version 5.1.6-0+deb12u1 Copyright (c) 2000-2024 the FFmpeg developers\n\
                      built with gcc 12 (Debian 12.2.0-14)\n\
                      configuration: --prefix=/usr --extra-version=0+deb12u1 --toolchain=hardened\n";
        assert_eq!(
            parse_version_banner(banner).unwrap(),
            "ffmpeg version 5.1.6-0+deb12u1 Copyright (c) 2000-2024 the FFmpeg developers"
        );
    }

    #[test]
    fn a_real_handbrake_banner_yields_its_first_line() {
        let banner = "HandBrake 1.6.1\n\n";
        assert_eq!(parse_version_banner(banner).unwrap(), "HandBrake 1.6.1");
    }

    #[test]
    fn leading_blank_lines_are_skipped_rather_than_returned_as_the_version() {
        assert_eq!(
            parse_version_banner("\n\n   \nffprobe version 5.1.6\n").unwrap(),
            "ffprobe version 5.1.6"
        );
    }

    #[test]
    fn no_banner_is_none_never_an_empty_version_string() {
        // An empty version in a status panel reads as "present, version
        // unknown" — a different and false claim.
        assert_eq!(parse_version_banner(""), None);
        assert_eq!(parse_version_banner("   \n\t\n  "), None);
    }

    #[test]
    fn an_absurdly_long_banner_is_truncated() {
        let long = format!("ffmpeg {}", "x".repeat(10_000));
        let v = parse_version_banner(&long).unwrap();
        assert!(v.chars().count() <= 200, "len {}", v.chars().count());
    }

    #[test]
    fn missing_is_distinct_from_unusable_in_every_observable_way() {
        // Collapsing these sends the operator to `apt install` for a problem
        // that is not a missing package.
        let missing = ToolState::Missing;
        let unusable = ToolState::Unusable {
            reason: "exited with 127".to_string(),
        };
        assert_ne!(missing, unusable);
        assert!(!missing.is_present() && !unusable.is_present());
        assert_eq!(missing.version(), None);
        assert_eq!(unusable.version(), None);

        assert!(report("ffmpeg", missing).summary().contains("NOT INSTALLED"));
        assert!(report("ffmpeg", unusable).summary().contains("unusable"));
    }

    #[test]
    fn a_present_tool_reports_its_version_in_the_summary() {
        let r = ToolReport {
            tool: "ffprobe",
            configured_bin: "/opt/bin/ffprobe".to_string(),
            state: present("ffprobe version 6.1"),
        };
        let s = r.summary();
        assert!(s.contains("ffprobe version 6.1"), "got {s}");
        assert!(
            s.contains("/opt/bin/ffprobe"),
            "the summary must name the binary that was actually probed, got {s}"
        );
    }

    #[test]
    fn transcoding_requires_ffprobe_as_well_as_ffmpeg() {
        // THE capability rule. Without ffprobe an encode can be run but its
        // output can never be verified, and an unverified success is a failure
        // — so "the encoder is installed" must not read as "we can safely
        // replace files".
        let c = caps(ToolState::Missing, present("ffmpeg version 6.1"), ToolState::Missing);
        assert!(c.ffmpeg.is_present());
        assert!(!c.can_probe());
        assert!(
            !c.can_transcode(),
            "an encoder without a verifier must not count as transcode capability"
        );

        let c = caps(present("ffprobe 6.1"), present("ffmpeg 6.1"), ToolState::Missing);
        assert!(c.can_probe() && c.can_transcode());
    }

    #[test]
    fn handbrake_is_not_required_for_either_capability() {
        // Foundry drives ffmpeg directly (the argv is built in
        // `plan::build_transcode_args`); HandBrakeCLI is detected and reported
        // because the config carries it, but nothing here depends on it.
        let c = caps(present("ffprobe 6.1"), present("ffmpeg 6.1"), ToolState::Missing);
        assert!(c.can_transcode());
        assert_eq!(c.unavailable(), vec!["HandBrakeCLI"]);
    }

    #[test]
    fn the_live_fleet_state_reports_everything_as_unavailable() {
        // <host> and the <host> dev box have none of these installed
        // (verified 2026-07-31). This is what Foundry must say about that
        // host: three names, explicitly unavailable — never "nothing to do".
        let c = caps(ToolState::Missing, ToolState::Missing, ToolState::Missing);
        assert!(!c.can_probe());
        assert!(!c.can_transcode());
        assert_eq!(c.unavailable(), vec!["ffprobe", "ffmpeg", "HandBrakeCLI"]);
        for r in c.reports() {
            assert!(r.summary().contains("NOT INSTALLED"), "got {}", r.summary());
        }
    }

    #[test]
    fn a_fully_capable_host_reports_nothing_unavailable() {
        let c = caps(present("a"), present("b"), present("c"));
        assert!(c.unavailable().is_empty());
    }

    #[test]
    fn detecting_a_binary_that_does_not_exist_reports_missing_not_a_crash() {
        // The one test here that really does spawn — against a name that is
        // guaranteed absent, so it asserts the `NotFound` classification on
        // any host, including one where ffmpeg *is* installed.
        let r = detect_tool(
            "nonexistent",
            "muse-foundry-no-such-binary-xyzzy",
            &version_args(),
            generous(),
        );
        assert_eq!(r.state, ToolState::Missing);
        assert!(!r.is_present());
    }

    #[test]
    fn detect_names_every_tool_and_echoes_the_configured_binary() {
        // Runs against deliberately absent binaries so the assertion holds
        // regardless of what the host has installed.
        let c = detect(
            "muse-foundry-absent-ffprobe",
            "muse-foundry-absent-ffmpeg",
            "muse-foundry-absent-handbrake",
            generous(),
        );
        assert_eq!(c.ffprobe.tool, "ffprobe");
        assert_eq!(c.ffmpeg.tool, "ffmpeg");
        assert_eq!(c.handbrake.tool, "HandBrakeCLI");
        assert_eq!(c.ffprobe.configured_bin, "muse-foundry-absent-ffprobe");
        assert_eq!(c.handbrake.configured_bin, "muse-foundry-absent-handbrake");
        assert_eq!(c.unavailable().len(), 3);
    }

    #[test]
    fn a_tool_that_exits_nonzero_is_unusable_not_missing() {
        // `false` exists on every host and always exits 1: an installed but
        // non-cooperating binary. It must not be reported as absent.
        let r = detect_tool("false-tool", "/bin/false", &version_args(), generous());
        match r.state {
            ToolState::Unusable { ref reason } => {
                assert!(reason.contains("exited"), "got {reason}")
            }
            other => panic!("expected Unusable, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_that_succeeds_but_prints_nothing_is_unusable_not_present() {
        // `true` exits 0 and prints nothing. Reporting it Present with an
        // empty version would be claiming a working tool we never observed
        // working.
        let r = detect_tool("true-tool", "/bin/true", &version_args(), generous());
        assert!(
            !r.is_present(),
            "a silent success is not evidence the tool works, got {:?}",
            r.state
        );
        assert!(matches!(r.state, ToolState::Unusable { .. }));
    }

    // --- S130-A CAPDET-01: the version-probe deadline -----------------------

    /// Write an executable that never exits, standing in for a wedged tool.
    ///
    /// `exec sleep` rather than a bare `sleep`, deliberately: the deadline path
    /// kills the process it spawned, and with a plain `sleep` that is the shell
    /// — leaving the real sleeper orphaned and running for its full duration
    /// after the test ends. `exec` makes the killed process the sleeper itself.
    ///
    /// A stub is the ONLY way to exercise this. Neither the dev box nor <host>
    /// has ffprobe installed, and no real tool can be made to hang on demand.
    fn hanging_stub(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muse-capdet01-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create stub dir");
        let path = dir.join("wedged-tool");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 600\n").expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        path
    }

    /// The bug CAPDET-01 fixes, at the level of the one function that spawns.
    ///
    /// Before the fix this call was a bare `Command::output()` and this test
    /// would not fail — it would never return at all.
    #[test]
    fn a_tool_that_never_exits_is_unusable_rather_than_an_unbounded_wait() {
        let stub = hanging_stub("detect-tool");
        let started = std::time::Instant::now();
        let r = detect_tool(
            "wedged",
            &stub.to_string_lossy(),
            &version_args(),
            Duration::from_secs(1),
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "the probe must be bounded by its deadline, took {elapsed:?}"
        );
        match r.state {
            ToolState::Unusable { ref reason } => {
                assert!(
                    reason.contains("timeout"),
                    "the reason must name the timeout, so the operator is not sent \
                     to `apt install` for a wedged mount; got {reason}"
                );
                assert!(reason.contains("1s"), "and the deadline that fired; got {reason}");
            }
            other => panic!("expected Unusable, got {other:?}"),
        }
        assert!(!r.is_present(), "a tool we never observed working is not present");

        let _ = std::fs::remove_dir_all(stub.parent().expect("stub dir"));
    }

    /// A wedged tool must be `Unusable`, NOT `Missing`. The two send the
    /// operator at opposite problems — one at `apt install`, one at the mount.
    #[test]
    fn a_wedged_tool_is_never_reported_as_not_installed() {
        let stub = hanging_stub("not-missing");
        let r = detect_tool(
            "wedged",
            &stub.to_string_lossy(),
            &version_args(),
            Duration::from_secs(1),
        );
        assert_ne!(r.state, ToolState::Missing);
        assert!(r.summary().contains("unusable"), "got {}", r.summary());
        assert!(
            !r.summary().contains("NOT INSTALLED"),
            "the binary is right there; got {}",
            r.summary()
        );
        let _ = std::fs::remove_dir_all(stub.parent().expect("stub dir"));
    }

    /// The whole snapshot stays bounded, and one wedged tool does not stop the
    /// other two from being reported.
    #[test]
    fn one_wedged_tool_does_not_prevent_the_others_being_detected() {
        let stub = hanging_stub("detect-all");
        let started = std::time::Instant::now();
        let c = detect(
            &stub.to_string_lossy(),
            "muse-capdet-absent-ffmpeg",
            "muse-capdet-absent-handbrake",
            Duration::from_secs(1),
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "three probes, each bounded, is still bounded"
        );
        assert!(!c.can_probe());
        assert!(matches!(c.ffprobe.state, ToolState::Unusable { .. }));
        assert_eq!(
            c.ffmpeg.state,
            ToolState::Missing,
            "the wedged ffprobe must not turn the other reports into guesses"
        );
        assert_eq!(c.unavailable(), vec!["ffprobe", "ffmpeg", "HandBrakeCLI"]);
        let _ = std::fs::remove_dir_all(stub.parent().expect("stub dir"));
    }

    #[test]
    fn the_deadline_defaults_to_the_compiled_value_and_clamps_nonsense() {
        assert_eq!(
            resolve_timeout(None),
            Duration::from_secs(DEFAULT_CAPABILITY_TIMEOUT_SECS)
        );
        assert_eq!(resolve_timeout(Some(3)), Duration::from_secs(3));
        // Zero would report every tool on a healthy host as timed out.
        assert_eq!(
            resolve_timeout(Some(0)),
            Duration::from_secs(MIN_CAPABILITY_TIMEOUT_SECS)
        );
        // And an effectively infinite value is the startup hang again.
        assert_eq!(
            resolve_timeout(Some(u64::MAX)),
            Duration::from_secs(MAX_CAPABILITY_TIMEOUT_SECS)
        );
    }

    /// The deadline is CALLED, not restated.
    ///
    /// A second copy of the spawn/poll/kill/reap logic would be a second thing
    /// to keep correct, and this epic has already paid 20x for a restated rule
    /// (`predicted_deletion_refusals`). This asserts the call site exists in the
    /// non-test body — the same shape `forge.rs` and `subtitles/sync.rs` use to
    /// pin their own use of it.
    #[test]
    fn detection_calls_the_shared_spawn_timeout_rather_than_reimplementing_it() {
        let body = include_str!("capability.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a non-test body");
        assert!(
            body.contains("probe::spawn_with_timeout"),
            "the deadline must come from media::probe, not a local copy"
        );
        assert!(
            !body.contains("Command::new"),
            "a local spawn here means the shared deadline was re-implemented"
        );
    }
}
