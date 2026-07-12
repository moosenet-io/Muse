//! Named channel presets (spec S96-muse-foundation §4d-C): theme/genre/era/
//! mood filters + interstitial-cadence parameters, each resolving to a
//! [`ComposeOptions`] overlay. A caller supplies the parts a preset has no
//! opinion about (which shows, which account, when it starts, whether to
//! use the LLM) and layers a preset's cadence/theme/ordering defaults on
//! top via [`Preset::apply`].

use serde::{Deserialize, Serialize};

use crate::models::interstitial::InterstitialKind;

use super::compose::{ComposeOptions, EpisodeOrdering};

const HOUR_MS: i64 = 3_600_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetName {
    SaturdayMorning,
    PrestigeNight,
    NinetiesChaos,
    ComfortRewatch,
    Discover,
    Household,
}

/// A named channel preset: the composer options it fixes, plus a
/// human-readable label/description for a channel-picker UI (MUSE-27).
#[derive(Debug, Clone)]
pub struct Preset {
    pub name: PresetName,
    pub display_name: &'static str,
    pub description: &'static str,
    pub ordering: EpisodeOrdering,
    pub interstitial_kind: Option<InterstitialKind>,
    pub interstitial_decade: Option<i32>,
    pub interstitial_theme: Option<&'static str>,
    /// Insert one interstitial after every N content items (>= 1).
    pub interstitial_every_n_items: u32,
    /// Session-length default, used only when the caller's base options
    /// didn't already specify one (see [`Preset::apply`]).
    pub default_session_ms: i64,
}

/// The six MVP presets (spec §4d-C / §5 MUSE-24). Order here is the display
/// order for a picker UI, not a priority ranking.
pub fn list_presets() -> Vec<Preset> {
    vec![
        Preset {
            name: PresetName::SaturdayMorning,
            display_name: "Saturday Morning",
            description: "A rotating block of animated/kids shows with retro cartoon bumpers \
                and cereal-commercial energy — an interstitial after every episode.",
            ordering: EpisodeOrdering::NextUnwatched,
            interstitial_kind: Some(InterstitialKind::Bumper),
            interstitial_decade: Some(1990),
            interstitial_theme: Some("saturday_morning"),
            interstitial_every_n_items: 1,
            default_session_ms: 2 * HOUR_MS,
        },
        Preset {
            name: PresetName::PrestigeNight,
            display_name: "Prestige Drama Night",
            description: "Fewer, longer prestige-drama episodes, taste-ranked, with sparse \
                idents rather than commercial breaks.",
            ordering: EpisodeOrdering::TasteRanked,
            interstitial_kind: Some(InterstitialKind::Ident),
            interstitial_decade: None,
            interstitial_theme: None,
            interstitial_every_n_items: 3,
            default_session_ms: 3 * HOUR_MS,
        },
        Preset {
            name: PresetName::NinetiesChaos,
            display_name: "90s Chaos",
            description: "Rapid-fire 90s sitcoms/cartoons interleaved with era-matched \
                commercials — high-cadence, high-nostalgia.",
            ordering: EpisodeOrdering::NextUnwatched,
            interstitial_kind: Some(InterstitialKind::Commercial),
            interstitial_decade: Some(1990),
            interstitial_theme: None,
            interstitial_every_n_items: 1,
            default_session_ms: 2 * HOUR_MS,
        },
        Preset {
            name: PresetName::ComfortRewatch,
            display_name: "Comfort Rewatch",
            description: "Taste-ranked favorites you already love, light on interstitials, \
                tuned for background comfort viewing.",
            ordering: EpisodeOrdering::TasteRanked,
            interstitial_kind: Some(InterstitialKind::Bumper),
            interstitial_decade: None,
            interstitial_theme: None,
            interstitial_every_n_items: 4,
            default_session_ms: 2 * HOUR_MS,
        },
        Preset {
            name: PresetName::Discover,
            display_name: "Discover",
            description: "Things the taste model thinks you'd love but haven't started; \
                interstitials skew toward trailers for what's next.",
            ordering: EpisodeOrdering::TasteRanked,
            interstitial_kind: Some(InterstitialKind::Trailer),
            interstitial_decade: None,
            interstitial_theme: None,
            interstitial_every_n_items: 2,
            default_session_ms: (2.5 * HOUR_MS as f64) as i64,
        },
        Preset {
            name: PresetName::Household,
            display_name: "Household Movie Night",
            description: "One pick per household member's queue, with idents/trailers between \
                features.",
            ordering: EpisodeOrdering::NextUnwatched,
            interstitial_kind: Some(InterstitialKind::Trailer),
            interstitial_decade: None,
            interstitial_theme: None,
            interstitial_every_n_items: 1,
            default_session_ms: 4 * HOUR_MS,
        },
    ]
}

/// Look up a single preset by name.
pub fn resolve_preset(name: PresetName) -> Option<Preset> {
    list_presets().into_iter().find(|p| p.name == name)
}

impl Preset {
    /// Layer this preset's ordering/interstitial-cadence/theme defaults onto
    /// a caller-provided base `ComposeOptions` (which supplies
    /// `account_id`/`show_media_item_ids`/`start_at`/`use_llm` — things this
    /// preset has no opinion about). The base's `target_session_ms` wins if
    /// already set (`> 0`); otherwise the preset's own default is used, so a
    /// caller can request e.g. "Saturday Morning, but only 45 minutes" by
    /// setting `target_session_ms` before calling `apply`.
    pub fn apply(&self, mut base: ComposeOptions) -> ComposeOptions {
        base.ordering = self.ordering;
        base.interstitial_kind = self.interstitial_kind;
        base.interstitial_decade = self.interstitial_decade;
        base.interstitial_theme = self.interstitial_theme.map(|s| s.to_string());
        base.interstitial_every_n_items = self.interstitial_every_n_items;
        if base.target_session_ms <= 0 {
            base.target_session_ms = self.default_session_ms;
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_presets_with_unique_names() {
        let presets = list_presets();
        assert_eq!(presets.len(), 6);
        let mut names: Vec<PresetName> = presets.iter().map(|p| p.name).collect();
        names.sort_by_key(|n| format!("{n:?}"));
        names.dedup();
        assert_eq!(names.len(), 6, "preset names must be unique");
    }

    #[test]
    fn resolve_preset_finds_each_by_name() {
        for p in list_presets() {
            let found = resolve_preset(p.name).expect("preset should resolve");
            assert_eq!(found.display_name, p.display_name);
        }
    }

    #[test]
    fn resolve_preset_none_is_unreachable_for_listed_names() {
        // Every PresetName variant is covered by list_presets(); this test
        // documents that invariant rather than exercising a "missing" case
        // (there is no PresetName outside the enum to probe with).
        assert!(resolve_preset(PresetName::Discover).is_some());
    }

    #[test]
    fn apply_only_overrides_session_length_when_caller_left_it_unset() {
        let preset = resolve_preset(PresetName::SaturdayMorning).unwrap();

        let base_with_length = ComposeOptions {
            target_session_ms: 5_000,
            ..Default::default()
        };
        let applied = preset.clone().apply(base_with_length);
        assert_eq!(
            applied.target_session_ms, 5_000,
            "caller-supplied session length must win over the preset default"
        );

        let base_without_length = ComposeOptions {
            target_session_ms: 0,
            ..Default::default()
        };
        let applied2 = preset.apply(base_without_length);
        assert_eq!(applied2.target_session_ms, 2 * HOUR_MS);
    }

    #[test]
    fn apply_sets_cadence_and_theme_from_preset() {
        let applied = resolve_preset(PresetName::NinetiesChaos)
            .unwrap()
            .apply(ComposeOptions::default());
        assert_eq!(applied.interstitial_every_n_items, 1);
        assert_eq!(applied.interstitial_decade, Some(1990));
        assert_eq!(applied.interstitial_kind, Some(InterstitialKind::Commercial));
        assert_eq!(applied.ordering, EpisodeOrdering::NextUnwatched);
    }
}
