//! **Foundry** — Muse's media-formatting subsystem (spec S128, `MUSEF-*`).
//!
//! Foundry is the half of the media lifecycle Muse did not previously own: the
//! gap between "a file landed in the library" and "every client plays it
//! natively". It replaces two containers in the operator's ARR suite that were
//! deployed but never configured (see `docs/ARR-SUITE-GRAPH.md`): the transcode
//! orchestrator and the subtitle matcher.
//!
//! Four subsystems, delivered across six phases:
//!
//! | Subsystem | What it does | Phase |
//! |---|---|---|
//! | core (`probe`, `policy`, `plan`) | describe a file, judge it against client profiles, decide what to do | 1 |
//! | `forge` | execute a plan: encode, verify, atomically swap | 2 |
//! | `fabric` | distribute jobs to `muse-node` workers across the network | 3 |
//! | `lexicon` | find, score, sync-verify and write subtitles | 4 |
//! | `archivist` | plan and apply library folder/naming layout | 5 |
//!
//! This item (**MUSEF-01**) ships only the foundation both of those rest on:
//! typed [`config::FoundryConfig`] and the [`paths::PathGuard`] /
//! [`paths::ResolvedPath`] safety primitive.
//!
//! ## Why the safety primitive comes first
//! Every other Muse subsystem is read-only by construction. Foundry is the
//! first that deletes and overwrites, against a library the operator cannot
//! re-acquire. So the ordering is deliberate: the guard exists, with its tests,
//! before any code that could use it wrongly. Later items accept a
//! [`paths::ResolvedPath`] rather than a `Path`, which makes "I forgot to
//! validate this" a compile error instead of a code-review catch.
//!
//! ## Capability gating (Module Contract §2)
//! Foundry registers only when it is actually configured. With
//! `MUSE_FOUNDRY_ALLOWED_ROOTS` unset — the default — [`Foundry::from_config`]
//! returns `None` and no Foundry surface exists. An absent backend capability
//! leaves the module inert, never broken.

pub mod config;
pub mod paths;

pub use config::FoundryConfig;
pub use paths::{PathError, PathGuard, ResolvedPath};

/// A configured, usable Foundry.
///
/// Obtained from [`Foundry::from_config`], which returns `None` when Foundry is
/// not configured — so holding this type is itself proof the subsystem is live.
// No `Debug` derive: it would print the contained FoundryConfig and PathGuard,
// disclosing every configured path regardless of field visibility.
#[derive(Clone)]
pub struct Foundry {
    config: FoundryConfig,
    guard: PathGuard,
}

impl Foundry {
    /// Build Foundry from the crate config, or `None` when unconfigured.
    ///
    /// Returns `None` in two distinct cases, both of which mean "do not
    /// register the surface":
    /// - no roots configured at all (the default), and
    /// - roots configured but none of them currently resolve (e.g. the library
    ///   mount is absent) — an inert guard cannot address anything, so a
    ///   registered surface would only produce confusing errors.
    ///
    /// Both are logged so the operator can tell them apart.
    pub fn from_config(cfg: &crate::config::Config) -> Option<Self> {
        let config = FoundryConfig::from_config(cfg);

        if config.is_unconfigured() {
            tracing::debug!(
                "foundry: MUSE_FOUNDRY_ALLOWED_ROOTS is unset — Foundry is not registered"
            );
            return None;
        }

        // Fatal errors are the rail-3 violations that only bite once the
        // mutation gate is open (see FoundryConfig::fatal_errors). Refusing to
        // register is the right response: a Foundry that can mutate but stages
        // output onto the library's own filesystem is more dangerous than no
        // Foundry at all, so it must not come up half-configured. Reported
        // here for the operator; `config.guard()` independently refuses to
        // mint a guard in this state, so the check is not the only line of
        // defence.
        let fatal = config.fatal_errors();
        if !fatal.is_empty() {
            for e in &fatal {
                tracing::error!(error = %e, "foundry: fatal configuration error");
            }
            tracing::error!(
                "foundry: the layout violates safety rail 3 while mutation is \
                 enabled — refusing to register. Fix the configuration, or unset \
                 MUSE_FOUNDRY_ENABLE_MUTATION to run read-only."
            );
            return None;
        }

        let guard = config.guard()?;
        if guard.is_inert() {
            tracing::warn!(
                configured = config.allowed_roots.len(),
                "foundry: allowed roots are configured but none could be resolved \
                 (unmounted library?) — Foundry is not registered"
            );
            return None;
        }

        for w in config.warnings() {
            tracing::warn!(warning = %w, "foundry: configuration warning");
        }

        tracing::info!(
            roots = guard.roots().len(),
            mutation_enabled = guard.mutation_enabled(),
            retention_days = config.retention_days,
            "foundry: registered"
        );

        Some(Self { config, guard })
    }

    // Not yet consumed: MUSEF-02 (probe) is the first caller of `guard()`, and
    // MUSEF-08 the first of `config()`. Same reasoning as the accessors in
    // paths.rs — this item is deliberately the foundation, reviewed before any
    // consumer exists. Annotated rather than widened to silence a warning.
    #[allow(dead_code)]
    /// The typed configuration. **Foundry-internal.**
    ///
    /// `FoundryConfig` carries raw `PathBuf`s, so a public accessor would hand
    /// out the configured roots to any caller — who could append a child and
    /// call `std::fs` directly, bypassing both confinement and the gate. That
    /// is the same leak the narrowed path accessors closed, one level up
    /// (S128 MUSEF-01 review, round 6).
    pub(in crate::foundry) fn config(&self) -> &FoundryConfig {
        &self.config
    }

    /// The path guard every Foundry operation resolves through.
    /// **Foundry-internal**, and this is the load-bearing one: a `&PathGuard`
    /// *is* the capability. Handing it out publicly would let outside code
    /// call `resolve_for_mutation` directly, which defeats the entire
    /// MutablePath design. Narrowing the leaf accessors while leaving this
    /// public achieved nothing — both reviewers caught it.
    #[allow(dead_code)]
    pub(in crate::foundry) fn guard(&self) -> &PathGuard {
        &self.guard
    }

    // --- Public, capability-free diagnostics -------------------------------
    // Everything a status endpoint or log line legitimately needs, without
    // leaking a path or a capability.

    /// How many roots the guard allows. Useful for status; leaks nothing.
    pub fn root_count(&self) -> usize {
        self.guard.root_count()
    }

    /// Whether the mutation gate is open.
    pub fn mutation_enabled(&self) -> bool {
        self.guard.mutation_enabled()
    }

    /// Configured recycle-bin retention, in days.
    pub fn retention_days(&self) -> u32 {
        self.config.retention_days
    }

}

impl std::fmt::Debug for Foundry {
    /// Path-free, for the same reason as [`FoundryConfig`]'s impl.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Foundry")
            .field("roots", &self.guard.root_count())
            .field("mutation_enabled", &self.guard.mutation_enabled())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Config::from_env` reads process env, so these tests build the Foundry
    /// config struct directly rather than mutating global state — which also
    /// keeps them safe under `cargo test`'s default parallelism (see the
    /// MUSE-TEST flaky-gate issue).
    fn foundry_from(config: FoundryConfig) -> Option<Foundry> {
        if config.is_unconfigured() {
            return None;
        }
        let guard = config.guard()?;
        if guard.is_inert() {
            return None;
        }
        Some(Foundry { config, guard })
    }

    fn cfg(roots: Vec<std::path::PathBuf>) -> FoundryConfig {
        FoundryConfig {
            allowed_roots: roots,
            work_dir: None,
            enable_mutation: false,
            retention_days: 14,
            ffprobe_bin: "ffprobe".into(),
            handbrake_bin: "HandBrakeCLI".into(),
        }
    }

    #[test]
    fn unconfigured_foundry_does_not_register() {
        assert!(foundry_from(cfg(vec![])).is_none());
    }

    #[test]
    fn configured_but_unresolvable_roots_do_not_register() {
        let missing = std::env::temp_dir().join("muse-foundry-definitely-not-mounted");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(foundry_from(cfg(vec![missing])).is_none());
    }

    #[test]
    fn a_rail3_violation_refuses_to_register_once_mutation_is_enabled() {
        // A Foundry that can mutate but stages onto the library's own
        // filesystem is more dangerous than no Foundry, so it must not come up.
        let root = std::env::temp_dir();
        let mut c = cfg(vec![root.clone()]);
        c.work_dir = Some(root.join("foundry-work-inside"));

        c.enable_mutation = false;
        assert!(
            foundry_from(c.clone()).is_some(),
            "read-only Foundry with an inert work_dir still registers"
        );

        c.enable_mutation = true;
        assert!(
            foundry_from(c).is_none(),
            "the same layout must refuse to register once mutation is enabled"
        );
    }

    #[test]
    fn a_config_that_would_be_refused_cannot_mint_a_guard_at_all() {
        // The capability boundary, not just the registration path: even code
        // that skips Foundry::from_config must not be able to obtain a
        // mutation-capable guard for a configuration that registration would
        // have rejected (S128 MUSEF-01 review).
        let root = std::env::temp_dir();
        let mut c = cfg(vec![root]);
        c.enable_mutation = true; // ...with no work_dir at all
        assert!(!c.fatal_errors().is_empty(), "precondition: this config is invalid");
        assert!(
            c.guard().is_none(),
            "an invalid mutating config must not yield a guard"
        );
    }

    #[test]
    fn a_resolvable_root_registers_with_the_mutation_gate_closed() {
        let root = std::env::temp_dir();
        let f = foundry_from(cfg(vec![root])).expect("temp dir always resolves");
        assert!(!f.guard().mutation_enabled(), "mutation must default closed");
        assert_eq!(f.guard().roots().len(), 1);
    }
}
