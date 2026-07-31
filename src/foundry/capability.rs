//! MUSEF-02 — what media tooling actually exists on this host, right now.
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
//!   broken install, a non-zero exit).
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

use std::process::Command;

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

/// What Foundry can actually do on this host.
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

// --- The impure edge -------------------------------------------------------

/// Probe one tool by running it. The single thin layer that spawns anything in
/// this module.
///
/// `version_argv` is passed in rather than hardcoded because HandBrakeCLI
/// spells the flag `--version` while ffmpeg/ffprobe use `-version`.
fn detect_tool(tool: &'static str, bin: &str, version_argv: &[String]) -> ToolReport {
    let state = match Command::new(bin).args(version_argv).output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ToolState::Missing,
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
/// Called on demand rather than cached at startup, deliberately: on this fleet
/// ffmpeg is expected to *appear* during the lifetime of the process (the
/// operator installs it into a running container), and a capability snapshot
/// taken at boot would keep reporting "not installed" long after it was. The
/// cost is three `--version` spawns per call, which is nothing next to an
/// encode.
pub fn detect(ffprobe_bin: &str, ffmpeg_bin: &str, handbrake_bin: &str) -> Capabilities {
    let ffmpeg_style = version_args();
    Capabilities {
        ffprobe: detect_tool("ffprobe", ffprobe_bin, &ffmpeg_style),
        ffmpeg: detect_tool("ffmpeg", ffmpeg_bin, &ffmpeg_style),
        // HandBrakeCLI spells it with two dashes and rejects `-version`.
        handbrake: detect_tool(
            "HandBrakeCLI",
            handbrake_bin,
            &["--version".to_string()],
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
        let r = detect_tool("false-tool", "/bin/false", &version_args());
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
        let r = detect_tool("true-tool", "/bin/true", &version_args());
        assert!(
            !r.is_present(),
            "a silent success is not evidence the tool works, got {:?}",
            r.state
        );
        assert!(matches!(r.state, ToolState::Unusable { .. }));
    }
}
