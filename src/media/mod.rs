//! **The shared media core** — how Muse describes a media file, wherever the
//! question comes from.
//!
//! ## What this module is, and why it exists
//! [`probe`], [`capability`] and [`paths`] were built inside Foundry (S128
//! MUSEF-01/02) and were **promoted here verbatim** by S130-A `MPRB-01`.
//! Nothing in them is curation-specific: describing a file, detecting whether
//! `ffprobe` exists, and proving a path lies inside a configured root are
//! questions every media subsystem asks.
//!
//! The move was not cosmetic. Foundry is **default-deny and inert** unless an
//! operator sets `MUSE_FOUNDRY_ALLOWED_ROOTS`, so while the probe layer lived
//! inside Foundry, a stock deployment could not probe its own library at all.
//! Maestro (S130) needs to, and so does the library scanner.
//!
//! `crate::foundry` re-exports all three modules under their original paths as
//! a **permanent compatibility surface** — not a deprecation. Foundry
//! legitimately consumes the shared core forever.
//!
//! ## `MediaProbe` vs `MediaInfoDoc` — the naming decision, recorded
//! [`probe::MediaProbe`] keeps its name and will keep it. It is an **ephemeral
//! observation**: what `ffprobe` said about a file at one moment, with no
//! schema version and no persistence. The *stored, versioned library fact* is a
//! separate type, [`doc::MediaInfoDoc`] (S130-A `MPRB-05`), which wraps a
//! `MediaProbe` in a versioned envelope.
//!
//! **Correction, MPRB-05:** an earlier draft of this paragraph said the envelope
//! carries "a schema version, a probe state and a timestamp". It carries the
//! schema version (plus the flat GUI projection and the filename's extension);
//! the probe **state** and the **timestamp** are `media_files` COLUMNS, not
//! document fields — deliberately, because a probe that FAILED produces no
//! document at all and its state must still be recorded. Fixed here rather than
//! left to rot, since the whole point of `0113`'s `probe_state` column is that
//! the unhappy states live outside the document.
//!
//! Two names, deliberately. An earlier draft proposed renaming `MediaProbe` to
//! `MediaInfo` during this promotion; that was declined twice, for two separate
//! reasons. It would have cost a mechanical rename across `plan.rs`,
//! `forge.rs` and `policy.rs` for a synonym — and, more importantly, it would
//! have conflated an observation with its stored envelope, which is exactly the
//! confusion `MediaInfoDoc` exists to prevent. A probe that failed is still a
//! fact worth persisting; it is not a `MediaProbe`.
//!
//! ## Two guards, no shared state
//! Foundry builds a **mutation-capable** guard over `MUSE_FOUNDRY_ALLOWED_ROOTS`
//! (still gated by `MUSE_FOUNDRY_ENABLE_MUTATION`). [`MediaCore`] builds a
//! second, entirely independent, **read-only** guard over `MUSE_LIBRARY_ROOT`
//! with `enable_mutation: false` — permanently, not by configuration. Nothing
//! in the media core writes to the library, and the type system is what says
//! so: `resolve_for_mutation` on the library guard is refused unconditionally.
//! The two guards share no configuration and no state; each path resolves
//! through its own roots.

pub mod capability;
pub mod derive;
/// MPRB-05: the stored, versioned form of a probe — see the `MediaProbe` vs
/// `MediaInfoDoc` decision recorded above.
pub mod doc;
pub mod paths;
pub mod probe;
/// The `ffprobe` fixture scrubber (S130-A MPRB-04). Test-only: it exists to
/// mint and to guard `tests/golden/probe/`, and adding a second binary target
/// for it was declined — see the module docs.
#[cfg(test)]
pub mod probe_capture;
/// The golden probe corpus (S130-A MPRB-04).
#[cfg(test)]
pub mod probe_golden;

/// The default `ffprobe` binary when neither override is configured: resolved
/// via `PATH`, exactly as Foundry's own default is.
const DEFAULT_FFPROBE_BIN: &str = "ffprobe";

/// Bounds on `MUSE_PROBE_TIMEOUT_SECS`.
///
/// The floor is not decoration: `MUSE_PROBE_TIMEOUT_SECS=0` would time out
/// every probe instantly, and downstream reads a `Timeout` as "unreadable" — a
/// single mistyped env var would quietly mark a whole library unreadable while
/// looking like a routine config change. The ceiling exists because a deadline
/// of a week is indistinguishable from the unbounded wait this defence was
/// built to remove.
///
/// One second, not a comfortable minimum: this is a guard against a
/// meaningless value, not an opinion about how long a probe should get. An
/// operator with a reason to set 2s is not second-guessed; an operator who set
/// 0 by accident is. Out-of-range values CLAMP rather than fall back to the
/// default, so someone who asked for more time gets as much as is allowed
/// instead of silently getting the value they were trying to change.
const MIN_PROBE_TIMEOUT_SECS: u64 = 1;
const MAX_PROBE_TIMEOUT_SECS: u64 = 6 * 60 * 60;

/// Bounds on `MUSE_PROBE_MAX_OUTPUT_BYTES`.
///
/// The floor is above any real ffprobe document (the largest measured in this
/// library is ~71 KB), so a too-small cap cannot make every file report
/// `OutputTooLarge`. The ceiling keeps the per-child memory bound meaningful:
/// a cap of "as much as it sends" is not a cap.
const MIN_PROBE_MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PROBE_MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;

/// The shared media core: a resolved `ffprobe` binary, a **read-only** guard
/// over the library root, and a snapshot of host tool capability.
///
/// It never fails to construct — an absent library root or an absent `ffprobe`
/// degrades the core, it does not stop Muse from booting (Module Contract §2:
/// an absent backend capability leaves the module inert, never broken).
///
/// **Wiring, stated honestly.** MPRB-01 adds this type and its tests; it does
/// **not** yet add a consumer. It is built from `&Config` at the call site,
/// exactly as [`crate::foundry::Foundry::from_config`] is (Foundry is not held
/// in `AppState` either), and the first callers are the scan integration
/// (MPRB-06) and the backfill worker (MPRB-07). Saying "constructed once at
/// startup and held in app state" would describe a mechanism this item does not
/// ship.
#[derive(Clone)]
pub struct MediaCore {
    /// The `ffprobe` binary this core invokes. A `PATH` name or an absolute
    /// path; see [`MediaCore::from_config`] for the resolution precedence.
    ffprobe_bin: String,
    /// Read-only guard over `MUSE_LIBRARY_ROOT`. Inert when unset.
    library_guard: paths::PathGuard,
    /// Host tool detection, taken once when this value is constructed. See
    /// [`MediaCore::can_probe`] for what that does and does not mean.
    capabilities: capability::Capabilities,
    /// Deadline for one probe. Resolved once, from config, at construction.
    probe_timeout: std::time::Duration,
    /// Capture cap for one probe stream. Resolved once, from config.
    probe_max_output_bytes: usize,
}

// No `Debug` derive, for the same reason `FoundryConfig` has none: the
// contained `PathGuard` holds canonical library paths, and a derived `Debug`
// would print every one of them to anyone able to format this value,
// regardless of field visibility. This impl prints shape only.
impl std::fmt::Debug for MediaCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaCore")
            .field("library_roots", &self.library_guard.root_count())
            .field("library_guard_inert", &self.library_guard.is_inert())
            .field("mutation_enabled", &self.library_guard.mutation_enabled())
            .field("can_probe", &self.can_probe())
            .finish_non_exhaustive()
    }
}

impl MediaCore {
    /// Build the media core from the already-loaded crate config.
    ///
    /// Takes `&Config` rather than reading the environment, so `config.rs`
    /// stays the crate's single env door and this type is testable without
    /// touching process state. Every value it reads is **non-secret
    /// behavioural config** — a binary name and a filesystem root.
    ///
    /// ## `ffprobe` binary precedence (documented, in order)
    /// 1. `MUSE_PROBE_FFPROBE_BIN` — the media core's own override.
    /// 2. `MUSE_FOUNDRY_FFPROBE_BIN` — Foundry's existing setting.
    /// 3. `"ffprobe"`, resolved via `PATH`.
    ///
    /// Step 2 is the point of having a precedence at all: an operator who
    /// already pointed Foundry at a custom `ffprobe` build on this host should
    /// not have to configure the same binary twice, and silently ignoring their
    /// existing setting would hand them the system `ffprobe` without ever
    /// saying so. Blank values are treated as unset, so an accidentally-empty
    /// env var falls through rather than producing an unrunnable binary name.
    ///
    /// ## Never a startup failure
    /// With `MUSE_LIBRARY_ROOT` unset the guard is inert and refuses every path
    /// with [`paths::PathError::NoAllowedRoots`]; with `ffprobe` absent
    /// [`MediaCore::can_probe`] is false. Both are reported, neither is fatal.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let ffprobe_bin = first_non_blank(&[
            cfg.probe_ffprobe_bin.as_deref(),
            cfg.foundry_ffprobe_bin.as_deref(),
        ])
        .unwrap_or(DEFAULT_FFPROBE_BIN)
        .to_string();

        // The library root is a READ-ONLY mount (see `Config::library_root`).
        // `enable_mutation: false` is hardcoded, not configuration: there is no
        // env var that can open this gate, because nothing in the media core is
        // ever allowed to write to the library.
        let roots: Vec<&str> = cfg
            .library_root
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .into_iter()
            .collect();
        let library_guard = paths::PathGuard::new(roots, false);

        if library_guard.is_inert() {
            tracing::debug!(
                "media: no usable MUSE_LIBRARY_ROOT — the media core's library guard is inert"
            );
        }

        // Detected once, here at construction. Foundry deliberately detects on demand
        // instead, because an operator can `apt install ffmpeg` into a running
        // container and a boot-time snapshot would keep reporting "not
        // installed". That trade-off is made the other way here on purpose: the
        // consumers of `can_probe()` are per-file loops over a 16,000-item
        // library (MPRB-06/07), and paying three subprocess spawns per file to
        // re-answer a question that changes once a year is not defensible.
        // The cost is a stale `false` until restart, which is why this is a
        // snapshot for *degradation decisions* and Foundry's on-demand
        // `Foundry::capabilities()` remains the operator-facing status surface.
        //
        // CAPDET-01: bounded. Each of those three spawns runs under
        // `MUSE_CAPABILITY_TIMEOUT_SECS` (5s by default), because this call is
        // on the startup path and a version probe that never returns used to
        // take the whole process with it.
        let capabilities = capability::detect(
            &ffprobe_bin,
            &cfg.ffmpeg_path,
            cfg.foundry_handbrake_bin.as_deref().unwrap_or("HandBrakeCLI"),
            capability::resolve_timeout(cfg.capability_timeout_secs),
        );

        if !capabilities.ffprobe.is_present() {
            tracing::warn!(
                summary = %capabilities.ffprobe.summary(),
                "media: ffprobe is not usable on this host — probe consumers will degrade, \
                 not fail per-file"
            );
        }

        Self {
            ffprobe_bin,
            library_guard,
            capabilities,
            probe_timeout: std::time::Duration::from_secs(
                cfg.probe_timeout_secs
                    .map(|s| s.clamp(MIN_PROBE_TIMEOUT_SECS, MAX_PROBE_TIMEOUT_SECS))
                    .unwrap_or(probe::PROBE_TIMEOUT.as_secs()),
            ),
            probe_max_output_bytes: cfg
                .probe_max_output_bytes
                .map(|b| b.clamp(MIN_PROBE_MAX_OUTPUT_BYTES, MAX_PROBE_MAX_OUTPUT_BYTES))
                .unwrap_or(probe::MAX_CAPTURED_BYTES),
        }
    }

    /// The probe deadline in force, after clamping.
    ///
    /// **120 seconds by default, and that number was not chosen here.** The
    /// S130-A spec text says 30s. It is stale: 120s was set against observed
    /// behaviour — an NFS probe of a large MKV is seconds, a wedged one is
    /// forever, and the deadline is there to catch the second case without
    /// touching the first. Lowering it to 30s would start reporting slow-but-fine
    /// files as `Timeout`, i.e. as unreadable, which is precisely the false
    /// verdict `ProbeError::Timeout`'s own documentation says it must not
    /// produce. An operator who wants 30s can set `MUSE_PROBE_TIMEOUT_SECS=30`;
    /// nobody gets it by accident.
    pub fn probe_timeout(&self) -> std::time::Duration {
        self.probe_timeout
    }

    /// The per-stream capture cap in force, after clamping.
    pub fn probe_max_output_bytes(&self) -> usize {
        self.probe_max_output_bytes
    }

    /// Probe a library file **asynchronously**, with this core's configured
    /// limits.
    ///
    /// This is the method the async consumers (MPRB-06 scan integration,
    /// MPRB-07 backfill, Maestro) are meant to call, and it is the reason the
    /// two knobs above exist: a limit nothing reads is not a limit. Stated
    /// honestly, as MPRB-01 stated it for `MediaCore` itself — **this item adds
    /// the method and its tests, not a caller.** Those land with the consumers.
    ///
    /// Takes a [`paths::ResolvedPath`] rather than a `&Path`, so a caller
    /// cannot reach outside `MUSE_LIBRARY_ROOT` by forgetting to resolve.
    #[allow(dead_code)]
    pub(crate) async fn probe_async(
        &self,
        path: &paths::ResolvedPath,
    ) -> Result<probe::MediaProbe, probe::ProbeError> {
        probe::run_ffprobe_async(
            &self.ffprobe_bin,
            path,
            self.probe_timeout,
            self.probe_max_output_bytes,
        )
        .await
    }

    /// Whether this host can run `ffprobe` at all, as observed when this
    /// `MediaCore` was constructed.
    ///
    /// Consumers use this to degrade once, up front, rather than discovering
    /// the same missing binary 16,000 times. It is a snapshot taken at
    /// construction: a long-lived `MediaCore` on a host where an operator
    /// installs `ffmpeg` afterwards keeps reporting `false`. For a live answer,
    /// run [`capability::detect`] again, or rebuild the core.
    pub fn can_probe(&self) -> bool {
        self.capabilities.can_probe()
    }

    /// The startup capability snapshot, for status surfaces and diagnostics.
    pub fn capabilities(&self) -> &capability::Capabilities {
        &self.capabilities
    }

    /// The **read-only** library guard.
    ///
    /// Crate-internal: a `PathGuard` is a capability, and handing one to code
    /// outside the crate is the disclosure this module's guards are narrowed to
    /// prevent. `resolve_for_mutation` on this guard always fails.
    // No consumer until MPRB-06/07; the accessor exists so those items add a
    // caller rather than a capability. Annotated rather than left as warning
    // noise, and rather than widening visibility to silence it.
    #[allow(dead_code)]
    pub(crate) fn library_guard(&self) -> &paths::PathGuard {
        &self.library_guard
    }

    /// The resolved `ffprobe` binary. Crate-internal: it is an operator-set
    /// filesystem path in the general case.
    #[allow(dead_code)]
    pub(crate) fn ffprobe_bin(&self) -> &str {
        &self.ffprobe_bin
    }

    /// Whether the library guard can address anything at all — false when
    /// `MUSE_LIBRARY_ROOT` is unset or did not resolve.
    pub fn library_guard_is_inert(&self) -> bool {
        self.library_guard.is_inert()
    }
}

/// First entry that is `Some` and not blank after trimming.
///
/// Blank-is-unset, deliberately: an env var set to `""` or `"   "` is an
/// operator accident, and treating it as a configured binary name would produce
/// a spawn failure that names nothing.
fn first_non_blank<'a>(candidates: &[Option<&'a str>]) -> Option<&'a str> {
    candidates
        .iter()
        .flatten()
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// A binary name that cannot exist on any host.
    const ABSENT_BIN: &str = "muse-media-no-such-ffprobe-xyzzy";

    fn cfg_with(probe: Option<&str>, foundry: Option<&str>, root: Option<&str>) -> Config {
        Config {
            probe_ffprobe_bin: probe.map(str::to_string),
            foundry_ffprobe_bin: foundry.map(str::to_string),
            library_root: root.map(str::to_string),
            // Keep the capability detection from spawning a real encoder in
            // tests: these two are only reported, never driven, by MediaCore.
            ffmpeg_path: ABSENT_BIN.to_string(),
            foundry_handbrake_bin: Some(ABSENT_BIN.to_string()),
            ..Default::default()
        }
    }

    /// Unique temp dir per test; no `tempfile` dependency is in this crate's
    /// media path, and the guard needs a real, canonicalizable directory.
    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muse-mprb01-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    #[test]
    fn probe_bin_prefers_the_media_core_override() {
        let core = MediaCore::from_config(&cfg_with(
            Some("/opt/probe-a"),
            Some("/opt/probe-b"),
            None,
        ));
        assert_eq!(core.ffprobe_bin(), "/opt/probe-a");
    }

    #[test]
    fn probe_bin_falls_back_to_the_foundry_setting() {
        let core = MediaCore::from_config(&cfg_with(None, Some("/opt/probe-b"), None));
        assert_eq!(
            core.ffprobe_bin(),
            "/opt/probe-b",
            "an operator who already configured MUSE_FOUNDRY_FFPROBE_BIN must not \
             be silently handed the system ffprobe"
        );
    }

    #[test]
    fn probe_bin_falls_back_to_path_when_nothing_is_configured() {
        let core = MediaCore::from_config(&cfg_with(None, None, None));
        assert_eq!(core.ffprobe_bin(), "ffprobe");
    }

    #[test]
    fn a_blank_probe_bin_is_treated_as_unset() {
        let core = MediaCore::from_config(&cfg_with(Some("   "), Some("/opt/probe-b"), None));
        assert_eq!(core.ffprobe_bin(), "/opt/probe-b");
    }

    #[test]
    fn a_host_without_ffprobe_starts_cleanly_and_reports_it() {
        // Constructing must not panic or error; it must report the absence.
        let core = MediaCore::from_config(&cfg_with(Some(ABSENT_BIN), None, None));
        assert!(
            !core.can_probe(),
            "an absent ffprobe must be reported as absent, never assumed present"
        );
    }

    #[test]
    fn an_unset_library_root_leaves_the_guard_inert_not_broken() {
        let core = MediaCore::from_config(&cfg_with(Some(ABSENT_BIN), None, None));
        assert!(core.library_guard_is_inert());
        assert_eq!(
            core.library_guard().resolve("/etc/hostname"),
            Err(paths::PathError::NoAllowedRoots),
            "an inert guard must refuse everything, not fall open"
        );
    }

    #[test]
    fn the_library_guard_resolves_inside_its_root() {
        let root = temp_root("resolve");
        let file = root.join("a-file.mkv");
        std::fs::write(&file, b"x").expect("write probe target");

        let core = MediaCore::from_config(&cfg_with(
            Some(ABSENT_BIN),
            None,
            Some(&root.to_string_lossy()),
        ));
        assert!(!core.library_guard_is_inert());
        assert!(
            core.library_guard().resolve(&file).is_ok(),
            "a file inside MUSE_LIBRARY_ROOT must resolve"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The negative test the whole read-only posture rests on.
    #[test]
    fn the_library_guard_refuses_mutation_unconditionally() {
        let root = temp_root("readonly");
        let file = root.join("a-file.mkv");
        std::fs::write(&file, b"x").expect("write probe target");

        let core = MediaCore::from_config(&cfg_with(
            Some(ABSENT_BIN),
            None,
            Some(&root.to_string_lossy()),
        ));

        assert!(
            !core.library_guard().mutation_enabled(),
            "the library guard's mutation gate is hardcoded closed"
        );
        assert_eq!(
            core.library_guard().resolve_for_mutation(&file).unwrap_err(),
            paths::PathError::MutationDisabled,
            "the media core must never obtain a MutablePath into the library"
        );
        assert_eq!(
            core.library_guard()
                .resolve_new_for_mutation(root.join("new.mkv"))
                .unwrap_err(),
            paths::PathError::MutationDisabled,
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- S130-A MPRB-02: the operator-tunable probe limits ------------------

    /// The default is **120 seconds**, and this test exists to keep it there.
    ///
    /// The S130-A spec section for MPRB-02 says 30s. That text is stale — 120s
    /// was set against observed behaviour, and dropping to 30s would start
    /// reporting slow-but-fine files as `Timeout`, which downstream treats as
    /// unreadable. A number that was tuned and then silently lowered by a spec
    /// nobody re-checked is exactly the regression this asserts against.
    #[test]
    fn the_probe_deadline_defaults_to_the_tuned_two_minutes() {
        let core = MediaCore::from_config(&cfg_with(Some(ABSENT_BIN), None, None));
        assert_eq!(core.probe_timeout(), std::time::Duration::from_secs(120));
        assert_eq!(core.probe_max_output_bytes(), probe::MAX_CAPTURED_BYTES);
    }

    #[test]
    fn an_operator_value_is_honoured_for_both_limits() {
        let cfg = Config {
            probe_timeout_secs: Some(45),
            probe_max_output_bytes: Some(2 * 1024 * 1024),
            ..cfg_with(Some(ABSENT_BIN), None, None)
        };
        let core = MediaCore::from_config(&cfg);
        assert_eq!(core.probe_timeout(), std::time::Duration::from_secs(45));
        assert_eq!(core.probe_max_output_bytes(), 2 * 1024 * 1024);
    }

    /// A nonsense value is clamped into range, never taken literally and never
    /// silently swapped for the default.
    #[test]
    fn out_of_range_limits_clamp_at_both_ends() {
        let low = MediaCore::from_config(&Config {
            probe_timeout_secs: Some(0),
            probe_max_output_bytes: Some(1),
            ..cfg_with(Some(ABSENT_BIN), None, None)
        });
        assert_eq!(
            low.probe_timeout(),
            std::time::Duration::from_secs(MIN_PROBE_TIMEOUT_SECS),
            "a zero deadline would report every file as unreadable"
        );
        assert_eq!(low.probe_max_output_bytes(), MIN_PROBE_MAX_OUTPUT_BYTES);

        let high = MediaCore::from_config(&Config {
            probe_timeout_secs: Some(u64::MAX),
            probe_max_output_bytes: Some(usize::MAX),
            ..cfg_with(Some(ABSENT_BIN), None, None)
        });
        assert_eq!(
            high.probe_timeout(),
            std::time::Duration::from_secs(MAX_PROBE_TIMEOUT_SECS),
            "an effectively infinite deadline is the unbounded wait again"
        );
        assert_eq!(high.probe_max_output_bytes(), MAX_PROBE_MAX_OUTPUT_BYTES);
    }

    /// Writes an executable stub standing in for `ffprobe`. Same reason as in
    /// `probe.rs`: ffprobe is not installed on the dev box, where this suite
    /// runs. (It IS installed on <host> — see `probe.rs`'s correction — but the
    /// host that runs Muse is not the host that tests it, so a stub is still
    /// what a test here has to use.)
    ///
    /// The stub answers `-version` immediately, and that is not decoration.
    /// `MediaCore::from_config` runs `capability::detect`, which invokes the
    /// configured binary with `-version`. A stub that slept for every
    /// invocation therefore delayed CONSTRUCTION, and the test looked like it
    /// was measuring the probe deadline while actually measuring capability
    /// detection. Matching the real tool's contract for both call shapes is what
    /// keeps the stub an honest double.
    ///
    /// (That call used to be a bare `Command::output()` with no deadline at all,
    /// so the delay was UNBOUNDED and a wedged ffprobe stalled startup outright.
    /// CAPDET-01 fixed it: version probes now run under
    /// `MUSE_CAPABILITY_TIMEOUT_SECS` and a hung tool is reported `Unusable`.
    /// The stub still answers `-version` promptly, because a test about the
    /// PROBE deadline should not spend the capability deadline first.)
    fn stub_bin(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("stub-ffprobe");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"-version\" ]; then\n    \
                 echo 'ffprobe version 6.0-stub'; exit 0\n  fi\ndone\n{body}\n"
            ),
        )
        .expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        path.to_string_lossy().into_owned()
    }

    /// The configured CAP actually reaches the probe.
    ///
    /// A knob nothing reads is not a knob, and "it is passed through" is a
    /// claim about a call site that only a call can settle. The stub writes
    /// 4 MB: under the 8 MiB default it would succeed-then-fail-to-parse, so
    /// `OutputTooLarge { cap: 256 KiB }` can only come from the CONFIGURED
    /// value having been used.
    #[tokio::test]
    async fn the_configured_cap_reaches_the_probe() {
        let root = temp_root("cfg-cap");
        let bin = stub_bin(&root, "head -c 4000000 /dev/zero | tr '\\0' 'x'");
        let file = root.join("a.mkv");
        std::fs::write(&file, b"x").expect("write subject");

        let core = MediaCore::from_config(&Config {
            probe_max_output_bytes: Some(MIN_PROBE_MAX_OUTPUT_BYTES),
            ..cfg_with(Some(&bin), None, Some(&root.to_string_lossy()))
        });
        let resolved = core.library_guard().resolve(&file).expect("inside the root");

        match core.probe_async(&resolved).await {
            Err(probe::ProbeError::OutputTooLarge { cap }) => {
                assert_eq!(cap, MIN_PROBE_MAX_OUTPUT_BYTES);
            }
            other => panic!("expected the configured cap to fire, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The configured DEADLINE actually reaches the probe.
    ///
    /// The stub sleeps 30s. Under the 120s default this test would take 30
    /// seconds and pass a probe; the timeout arriving in about a second is only
    /// possible if the configured value was used.
    #[tokio::test]
    async fn the_configured_deadline_reaches_the_probe() {
        let root = temp_root("cfg-timeout");
        let bin = stub_bin(&root, "exec sleep 30");
        let file = root.join("a.mkv");
        std::fs::write(&file, b"x").expect("write subject");

        let core = MediaCore::from_config(&Config {
            probe_timeout_secs: Some(1),
            ..cfg_with(Some(&bin), None, Some(&root.to_string_lossy()))
        });
        let resolved = core.library_guard().resolve(&file).expect("inside the root");

        let start = std::time::Instant::now();
        let got = core.probe_async(&resolved).await;
        let waited = start.elapsed();

        assert!(
            matches!(got, Err(probe::ProbeError::Timeout { secs: 1 })),
            "expected the configured 1s deadline, got {got:?}"
        );
        assert!(
            waited < std::time::Duration::from_secs(10),
            "the 120s default must not be what fired: {waited:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- S130-A CAPDET-01: startup survives a wedged tool -------------------

    /// **The item's whole point, asserted as termination.**
    ///
    /// `MediaCore::from_config` runs `capability::detect` on the startup path.
    /// With the old bare `Command::output()`, a configured binary that hangs
    /// instead of exiting hung the process here: no health endpoint, no log past
    /// this line, and under a supervisor a restart loop that never converges.
    ///
    /// Constructed on its own thread and collected with `recv_timeout`, so a
    /// regression FAILS this test instead of hanging the suite forever — a test
    /// that can only hang is a test that cannot report. The stub sleeps 600s, so
    /// nothing but the deadline can end this call.
    #[test]
    fn a_wedged_ffprobe_lets_startup_finish_instead_of_hanging_the_process() {
        let root = temp_root("capdet-startup");
        let bin = wedged_bin(&root);

        let cfg = Config {
            capability_timeout_secs: Some(1),
            ..cfg_with(Some(&bin), None, None)
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(MediaCore::from_config(&cfg));
        });
        let core = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("startup must RETURN against a wedged tool, not wait on it");
        let waited = started.elapsed();

        assert!(
            waited < std::time::Duration::from_secs(20),
            "startup must be bounded by the version-probe deadline, took {waited:?}"
        );
        assert!(
            !core.can_probe(),
            "a tool that never answered is not evidence the host can probe"
        );
        match &core.capabilities().ffprobe.state {
            capability::ToolState::Unusable { reason } => {
                assert!(
                    reason.contains("timeout"),
                    "the reason must name the timeout; got {reason}"
                );
            }
            other => panic!("expected Unusable, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The version-probe deadline is its own knob with its own default, and it
    /// is NOT the probe deadline: 5 seconds versus 120. Conflating them would
    /// put two minutes of potential stall back on the startup path.
    #[test]
    fn the_version_probe_deadline_is_separate_from_the_probe_deadline() {
        assert_eq!(
            capability::resolve_timeout(None),
            std::time::Duration::from_secs(capability::DEFAULT_CAPABILITY_TIMEOUT_SECS)
        );
        assert!(
            capability::resolve_timeout(None)
                < MediaCore::from_config(&cfg_with(Some(ABSENT_BIN), None, None)).probe_timeout(),
            "a `-version` banner is milliseconds of work; it must not be given a \
             file-probe's budget on the startup path"
        );
    }

    /// A binary that never exits. `exec sleep`, so the process the deadline
    /// kills IS the sleeper rather than a shell wrapping one.
    fn wedged_bin(dir: &std::path::Path) -> String {
        let path = dir.join("wedged-ffprobe");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 600\n").expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        path.to_string_lossy().into_owned()
    }

    /// Foundry's guard and the media core's guard share no state: the media
    /// core stays read-only and library-rooted even when Foundry is configured
    /// with mutation open over a different root.
    #[test]
    fn the_two_guards_are_independent() {
        let lib = temp_root("indep-lib");
        let foundry_root = temp_root("indep-foundry");
        let lib_file = lib.join("in-library.mkv");
        std::fs::write(&lib_file, b"x").expect("write library file");
        let foundry_file = foundry_root.join("in-foundry.mkv");
        std::fs::write(&foundry_file, b"x").expect("write foundry file");

        let cfg = Config {
            foundry_allowed_roots: Some(foundry_root.to_string_lossy().to_string()),
            foundry_enable_mutation: true,
            ..cfg_with(Some(ABSENT_BIN), None, Some(&lib.to_string_lossy()))
        };
        let core = MediaCore::from_config(&cfg);

        assert!(core.library_guard().resolve(&lib_file).is_ok());
        assert!(
            core.library_guard().resolve(&foundry_file).is_err(),
            "Foundry's roots must not become addressable through the media guard"
        );
        assert!(
            !core.library_guard().mutation_enabled(),
            "Foundry's open mutation gate must not open the media core's"
        );

        let _ = std::fs::remove_dir_all(&lib);
        let _ = std::fs::remove_dir_all(&foundry_root);
    }
}
