//! MUSEX-18 (Plane TERM #394): the Constellation GUI control + tuning
//! panel — the persisted, server-side source of truth for experience-layer
//! behavior.
//!
//! ## What this module owns
//! [`ExperienceSettings`] is the single settings document
//! `migrations/0102_experience_settings.sql` persists (via
//! `crate::repo::settings`) and `crate::web::settings` serves over HTTP
//! (GET/PUT, secrets masked, sensitive toggles confirmation-gated). It
//! carries:
//! - a MASTER on/off switch ([`ExperienceSettings::master_enabled`]) plus a
//!   per-subsystem toggle for `channel_director`, `watch_together`,
//!   `adaptation_loop`, `discord_bot`, `whats_hot`, `kg_viz`;
//! - the VARIABLE tunables the panel exposes: adaptation aggressiveness,
//!   serendipity %, question frequency (incl. silent-mode), persona
//!   definitions, Discord promotion cadence + trusted-friends roster,
//!   trend-source weighting, per-user sharing granularity, KG-viz opt-in.
//!
//! ## A deliberate seam: this panel does not rewire every subsystem
//! Several of these tunables already exist as their OWN typed, independently
//! tested domain values elsewhere in this crate —
//! `crate::adaptation::Aggressiveness`, `crate::assistant::AskFrequency`,
//! `crate::channels::serendipity::SerendipityRange` — none of which derive
//! `serde`/persist anywhere today (they're plain call-site parameters, see
//! `crate::config`'s doc comment on this exact gap). Rather than retrofit
//! `serde` derives onto those production types (out of scope, and risky to
//! do unreviewed), this module defines its OWN GUI-facing mirrors
//! ([`QuestionFrequency`], the raw `f32`/`f64` tunables below) with `From`
//! conversions into the real types where one exists
//! ([`QuestionFrequency::into_ask_frequency`],
//! [`AdaptationLoopSettings::aggressiveness`]). Wiring every subsystem's
//! internals to READ this panel on every call is real follow-on work; what
//! this item guarantees is that the panel is the authoritative PERSISTED
//! surface, and that the master/per-subsystem gate is REAL and enforced at
//! at least one concrete entry point end-to-end (see
//! `crate::promotion::run_promotion_dispatch`, the load-bearing inertness
//! test named in the AC).
//!
//! ## Secrets: never in this document
//! Nothing in [`ExperienceSettings`] holds a raw secret. The Discord bot
//! token stays exactly where `crate::discord::client::RealDiscordClient`
//! already reads it from — `Config::discord_bot_token`, <secret-manager>-
//! materialized env at runtime (S7) — and is never accepted by the PUT
//! DTO, never written into this JSONB document, and never returned by GET.
//! [`mask_discord_token`] turns "is one configured" into a display-only
//! placeholder for the GET response; see `crate::web::settings`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// --- top-level document -----------------------------------------------------

/// The full persisted settings document — one row, one JSONB blob (see
/// `migrations/0102_experience_settings.sql`). `#[serde(default)]` at every
/// level means a partially-shaped stored document (e.g. one written before
/// a later MUSEX-18 revision added a field) still deserializes cleanly,
/// filling in the new field's default rather than failing to load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperienceSettings {
    /// The master switch. `false` makes every accessor below report
    /// disabled regardless of its own per-subsystem flag — see
    /// [`Self::is_channel_director_enabled`] and friends, all of which AND
    /// this in.
    pub master_enabled: bool,
    pub channel_director: ChannelDirectorSettings,
    pub watch_together: SubsystemToggle,
    pub adaptation_loop: AdaptationLoopSettings,
    pub discord_bot: DiscordBotSettings,
    pub whats_hot: WhatsHotSettings,
    pub kg_viz: KgVizSettings,
    pub question_frequency: QuestionFrequencySettings,
    /// Editable persona definitions/labels the GUI panel manages. Not the
    /// same thing as `crate::models::persona::Persona` (a computed taste
    /// centroid) — these are operator-authored descriptive metadata a
    /// future persona-editing surface can bind display copy to.
    pub personas: Vec<PersonaDefinition>,
    pub sharing: SharingSettings,
}

impl Default for ExperienceSettings {
    fn default() -> Self {
        Self {
            master_enabled: true,
            channel_director: ChannelDirectorSettings::default(),
            watch_together: SubsystemToggle { enabled: true },
            adaptation_loop: AdaptationLoopSettings::default(),
            discord_bot: DiscordBotSettings::default(),
            whats_hot: WhatsHotSettings::default(),
            kg_viz: KgVizSettings::default(),
            question_frequency: QuestionFrequencySettings::default(),
            personas: Vec::new(),
            sharing: SharingSettings::default(),
        }
    }
}

impl ExperienceSettings {
    /// `true` only when both the master switch AND `channel_director.enabled`
    /// hold — the exact "AND" gate every per-subsystem accessor below
    /// repeats, so a subsystem can never read itself as enabled while the
    /// master switch is off.
    pub fn is_channel_director_enabled(&self) -> bool {
        self.master_enabled && self.channel_director.enabled
    }

    pub fn is_watch_together_enabled(&self) -> bool {
        self.master_enabled && self.watch_together.enabled
    }

    pub fn is_adaptation_loop_enabled(&self) -> bool {
        self.master_enabled && self.adaptation_loop.enabled
    }

    /// Gates BOTH the bot's own responsiveness (`crate::discord::bot`) and
    /// promotion dispatch (`crate::promotion::run_promotion_dispatch`,
    /// the AC's load-bearing negative test).
    pub fn is_discord_bot_enabled(&self) -> bool {
        self.master_enabled && self.discord_bot.enabled
    }

    pub fn is_whats_hot_enabled(&self) -> bool {
        self.master_enabled && self.whats_hot.enabled
    }

    pub fn is_kg_viz_enabled(&self) -> bool {
        self.master_enabled && self.kg_viz.enabled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubsystemToggle {
    pub enabled: bool,
}

impl Default for SubsystemToggle {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// --- channel director --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelDirectorSettings {
    pub enabled: bool,
    /// `[0.0, 100.0]`, the GUI-facing percent unit
    /// `crate::channels::serendipity::SerendipityRange::from_percent`
    /// already accepts — convert via [`Self::serendipity_fraction`] before
    /// handing it to `director::DirectorConstraints::serendipity_budget`.
    pub serendipity_percent: f64,
}

/// Mirrors `channels::director::DEFAULT`-shaped values documented in
/// `crate::channels::serendipity` (a moderate, clearly-nonzero exploration
/// budget) — this module doesn't import that constant directly since it's
/// private to that module's own tuning, but 20% is the same order of
/// magnitude the module doc's worked examples use.
const DEFAULT_SERENDIPITY_PERCENT: f64 = 20.0;

impl Default for ChannelDirectorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            serendipity_percent: DEFAULT_SERENDIPITY_PERCENT,
        }
    }
}

impl ChannelDirectorSettings {
    /// `serendipity_percent` clamped to `[0.0, 100.0]` and converted to the
    /// `[0.0, 1.0]` fraction `DirectorConstraints::serendipity_budget`
    /// consumes.
    pub fn serendipity_fraction(&self) -> f64 {
        self.serendipity_percent.clamp(0.0, 100.0) / 100.0
    }
}

// --- adaptation loop ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdaptationLoopSettings {
    pub enabled: bool,
    /// `[0.0, 1.0]` — the same scale `crate::adaptation::Aggressiveness`
    /// clamps to. See [`Self::aggressiveness`] for the conversion.
    pub aggressiveness: f32,
}

/// Mirrors `crate::adaptation::Aggressiveness::STANDARD` (0.5) — kept as a
/// plain literal here rather than importing that type's private inner
/// value, since `Aggressiveness` intentionally exposes no `const fn` giving
/// back the raw `f32` outside its own `value()` accessor on a constructed
/// instance.
const DEFAULT_AGGRESSIVENESS: f32 = 0.5;

impl Default for AdaptationLoopSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            aggressiveness: DEFAULT_AGGRESSIVENESS,
        }
    }
}

impl AdaptationLoopSettings {
    /// Convert into the real `crate::adaptation::Aggressiveness` newtype —
    /// clamps the same way `Aggressiveness::new` does, so an out-of-range
    /// stored value (e.g. from an older panel revision) degrades safely
    /// rather than panicking or silently overshooting.
    pub fn aggressiveness(&self) -> crate::adaptation::Aggressiveness {
        crate::adaptation::Aggressiveness::new(self.aggressiveness)
    }
}

// --- discord bot: cadence + trusted-friends roster + the sensitive toggle ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordBotSettings {
    /// SENSITIVE (per the AC): flipping `false -> true` requires
    /// `confirm_sensitive` on the PUT request — see
    /// `crate::web::settings::evaluate_update`. Defaults `false`: the bot
    /// surface stays off until an operator explicitly, confirmedly turns
    /// it on, same "default-private" posture
    /// `crate::discord::identity::FriendIdentity` already established for
    /// per-friend consent.
    pub enabled: bool,
    /// Mirrors `Config::promotion_cadence_secs`'s default (21_600s / 6h).
    pub promotion_cadence_secs: u64,
    /// Mirrors `Config::promotion_match_threshold`'s default (0.55).
    pub promotion_match_threshold: f64,
    /// The roster of allowlisted Discord identities the panel manages.
    /// Deliberately NOT the same thing as consent: adding an entry here
    /// only makes a friend addressable (mirrors
    /// `crate::discord::identity::TrustedFriends::upsert`'s allowlist
    /// step) — it can never itself grant `taste_opt_in`, since
    /// `FriendIdentity`'s consent field stays private and settable only
    /// through `FriendIdentity::opt_in` (see that module's doc). A caller
    /// turning a [`TrustedFriendEntry`] into a real
    /// `crate::discord::identity::FriendIdentity` gets a NOT-opted-in
    /// identity every time, by construction — same seam
    /// `crate::web::graph::FriendInput::into_identity` already documents.
    pub trusted_friends: Vec<TrustedFriendEntry>,
}

impl Default for DiscordBotSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            promotion_cadence_secs: 21_600,
            promotion_match_threshold: 0.55,
            trusted_friends: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustedFriendEntry {
    pub discord_user_id: String,
    pub display_name: String,
}

// --- what's-hot / trending source weighting -----------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhatsHotSettings {
    pub enabled: bool,
    /// Source name (e.g. `"trakt"`) -> relative weight. `BTreeMap` (not
    /// `HashMap`) so a round-trip through JSON is deterministically
    /// ordered — matters for the panel's own round-trip test, and for a
    /// human reading the raw stored JSONB.
    pub source_weights: BTreeMap<String, f64>,
}

impl Default for WhatsHotSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            source_weights: BTreeMap::new(),
        }
    }
}

// --- KG visualization opt-in ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KgVizSettings {
    /// Opt-in, defaults `false` — same default-private posture as
    /// `FriendIdentity::taste_opt_in`, since a KG viz is inherently a
    /// multi-person watch-data surface (`crate::kg::assemble`).
    pub enabled: bool,
    /// Mirrors `Config::kg_viz_watch_history_limit`'s default (200).
    pub watch_history_limit: u64,
    /// Mirrors `Config::kg_taste_neighbor_threshold`'s default (0.5).
    pub taste_neighbor_threshold: f32,
}

impl Default for KgVizSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            watch_history_limit: 200,
            taste_neighbor_threshold: 0.5,
        }
    }
}

// --- question frequency (incl. silent-mode) -----------------------------------

/// GUI-facing mirror of `crate::assistant::AskFrequency`'s two
/// non-`Never` variants — silent mode is modeled as its own explicit
/// [`QuestionFrequencySettings::silent_mode`] flag rather than a third enum
/// variant, so the panel exposes "how often" and "whether at all" as two
/// separately toggleable controls (matching the AC's "question freq incl
/// silent-mode" phrasing literally).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QuestionFrequency {
    #[default]
    Standard,
    Reduced,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuestionFrequencySettings {
    pub frequency: QuestionFrequency,
    /// When `true`, overrides `frequency` entirely — the effective mode is
    /// always `AskFrequency::Never`. See [`Self::into_ask_frequency`].
    pub silent_mode: bool,
}

impl Default for QuestionFrequencySettings {
    fn default() -> Self {
        Self {
            frequency: QuestionFrequency::default(),
            silent_mode: false,
        }
    }
}

impl QuestionFrequencySettings {
    /// Convert into the real `crate::assistant::AskFrequency` a subsystem
    /// consumes — `silent_mode` wins unconditionally over `frequency`,
    /// exactly like `AskFrequency::Never`'s own doc describes for itself.
    pub fn into_ask_frequency(self) -> crate::assistant::AskFrequency {
        if self.silent_mode {
            return crate::assistant::AskFrequency::Never;
        }
        match self.frequency {
            QuestionFrequency::Standard => crate::assistant::AskFrequency::Standard,
            QuestionFrequency::Reduced => crate::assistant::AskFrequency::Reduced,
        }
    }
}

// --- persona definitions -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonaDefinition {
    pub name: String,
    pub kind: String,
    pub description: String,
}

// --- sharing granularity (sensitive: widening requires confirmation) ----------

/// Ordered narrowest -> widest. [`SharingGranularity::rank`] backs the
/// "widening" test `crate::web::settings::evaluate_update` uses to decide
/// whether a PUT needs `confirm_sensitive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SharingGranularity {
    #[default]
    Private,
    HouseholdOnly,
    TrustedFriendsOnly,
    Public,
}

impl SharingGranularity {
    fn rank(self) -> u8 {
        match self {
            SharingGranularity::Private => 0,
            SharingGranularity::HouseholdOnly => 1,
            SharingGranularity::TrustedFriendsOnly => 2,
            SharingGranularity::Public => 3,
        }
    }

    /// `true` when `self` shares with a strictly larger audience than
    /// `previous` — the SENSITIVE transition the AC requires confirmation
    /// for. Narrowing (or staying the same) is never sensitive.
    pub fn widens(self, previous: SharingGranularity) -> bool {
        self.rank() > previous.rank()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharingSettings {
    pub granularity: SharingGranularity,
}

impl Default for SharingSettings {
    fn default() -> Self {
        Self {
            granularity: SharingGranularity::default(),
        }
    }
}

// --- secret masking ------------------------------------------------------------

/// Placeholder returned by GET in place of any secret value — never a
/// substring of the real secret, never even its length. Shared by
/// `crate::web::settings`'s response DTO.
pub const MASKED_SECRET_PLACEHOLDER: &str = "***configured***";

/// Turn "is a Discord bot token currently configured" (never the token
/// itself) into the GET-response-safe display value: the placeholder when
/// `Some`, `None` when unconfigured. The caller passes only a `bool`
/// derived from `Config::discord_bot_token.is_some()` — this function's
/// signature makes it structurally impossible to pass the real token in by
/// accident (there's no `&str`/`String` parameter to misuse).
pub fn mask_discord_token(configured: bool) -> Option<&'static str> {
    configured.then_some(MASKED_SECRET_PLACEHOLDER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_have_the_master_switch_on() {
        let settings = ExperienceSettings::default();
        assert!(settings.master_enabled);
        assert!(settings.is_channel_director_enabled());
        assert!(settings.is_watch_together_enabled());
        assert!(settings.is_adaptation_loop_enabled());
        assert!(settings.is_whats_hot_enabled());
    }

    #[test]
    fn discord_bot_and_kg_viz_default_off_sensitive_and_opt_in() {
        let settings = ExperienceSettings::default();
        assert!(!settings.discord_bot.enabled);
        assert!(!settings.is_discord_bot_enabled());
        assert!(!settings.kg_viz.enabled);
        assert!(!settings.is_kg_viz_enabled());
    }

    #[test]
    fn master_switch_off_disables_every_subsystem_regardless_of_its_own_flag() {
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = false;
        settings.discord_bot.enabled = true;
        settings.kg_viz.enabled = true;

        assert!(!settings.is_channel_director_enabled());
        assert!(!settings.is_watch_together_enabled());
        assert!(!settings.is_adaptation_loop_enabled());
        assert!(!settings.is_discord_bot_enabled());
        assert!(!settings.is_whats_hot_enabled());
        assert!(!settings.is_kg_viz_enabled());
    }

    #[test]
    fn per_subsystem_toggle_off_disables_only_that_subsystem() {
        let mut settings = ExperienceSettings::default();
        settings.channel_director.enabled = false;

        assert!(!settings.is_channel_director_enabled());
        // everything else stays on
        assert!(settings.is_watch_together_enabled());
        assert!(settings.is_adaptation_loop_enabled());
        assert!(settings.is_whats_hot_enabled());
    }

    #[test]
    fn serendipity_fraction_clamps_and_scales() {
        let s = ChannelDirectorSettings {
            enabled: true,
            serendipity_percent: 250.0,
        };
        assert!((s.serendipity_fraction() - 1.0).abs() < 1e-9);

        let s = ChannelDirectorSettings {
            enabled: true,
            serendipity_percent: -10.0,
        };
        assert!((s.serendipity_fraction() - 0.0).abs() < 1e-9);

        let s = ChannelDirectorSettings {
            enabled: true,
            serendipity_percent: 20.0,
        };
        assert!((s.serendipity_fraction() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn aggressiveness_conversion_clamps_via_the_real_newtype() {
        let s = AdaptationLoopSettings {
            enabled: true,
            aggressiveness: 5.0,
        };
        assert!((s.aggressiveness().value() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn silent_mode_overrides_frequency() {
        let s = QuestionFrequencySettings {
            frequency: QuestionFrequency::Standard,
            silent_mode: true,
        };
        assert_eq!(
            s.into_ask_frequency(),
            crate::assistant::AskFrequency::Never
        );
    }

    #[test]
    fn frequency_without_silent_mode_maps_through() {
        let s = QuestionFrequencySettings {
            frequency: QuestionFrequency::Reduced,
            silent_mode: false,
        };
        assert_eq!(
            s.into_ask_frequency(),
            crate::assistant::AskFrequency::Reduced
        );
    }

    #[test]
    fn sharing_granularity_widening_is_detected_correctly() {
        assert!(SharingGranularity::Public.widens(SharingGranularity::Private));
        assert!(SharingGranularity::HouseholdOnly.widens(SharingGranularity::Private));
        assert!(!SharingGranularity::Private.widens(SharingGranularity::Public));
        assert!(!SharingGranularity::Private.widens(SharingGranularity::Private));
    }

    #[test]
    fn mask_discord_token_never_carries_the_real_value() {
        assert_eq!(mask_discord_token(true), Some(MASKED_SECRET_PLACEHOLDER));
        assert_eq!(mask_discord_token(false), None);
    }

    #[test]
    fn settings_round_trip_through_json_field_by_field() {
        // Not a DB round trip (that's `repo::settings`'s `db_gated` test) --
        // this proves the serde shape itself is lossless, since
        // `repo::settings::save`/`load` go through exactly this
        // `serde_json::Value` conversion.
        let mut settings = ExperienceSettings::default();
        settings.discord_bot.enabled = true;
        settings
            .discord_bot
            .trusted_friends
            .push(TrustedFriendEntry {
                discord_user_id: "discord-1".to_string(),
                display_name: "Alex".to_string(),
            });
        settings
            .whats_hot
            .source_weights
            .insert("trakt".to_string(), 0.75);
        settings.sharing.granularity = SharingGranularity::TrustedFriendsOnly;

        let json = serde_json::to_value(&settings).expect("serialize");
        let round_tripped: ExperienceSettings = serde_json::from_value(json).expect("deserialize");

        assert_eq!(settings, round_tripped);
    }
}
