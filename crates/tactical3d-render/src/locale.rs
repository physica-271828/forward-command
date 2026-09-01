//! `LocaleRes` Bevy resource + enum display-name lookups routed through the
//! localisation files (DESIGN.md §15).
//!
//! Enum names historically leaked to the UI via `{:?}` Debug formatting
//! (`MediumArmor`, `Attacker`, …) or lived as hardcoded English in
//! `tactical-ai` (`CombatTactic::name()` etc.). They now resolve through
//! `30-names_l_*.yml` keys derived from the variant's Debug name
//! (`camel_to_snake`), so every UI surface shows the player's language. The
//! old Rust `name()`/`Display` impls stay as the English fallback used by
//! tests and CLI output.

use bevy::prelude::*;
use std::borrow::Cow;
use std::ops::{Deref, DerefMut};
use tactical_ai::CombatTactic;
use tactical_core::terrain::Terrain;
use tactical_core::unit::{Side, UnitState, UnitType};
use tactical_locale::{Language, Locale};
use tactical_sync::BattlePhase;

/// The active string table. Inserted by the binary from `settings.json`
/// (`AppSettings::language()`); `TacticalUiPlugin` registers an English
/// default so the resource always exists (insert-after-init wins).
#[derive(Resource, Clone)]
pub struct LocaleRes(pub Locale);

impl Default for LocaleRes {
    fn default() -> Self {
        Self(Locale::load(Language::English))
    }
}

impl Deref for LocaleRes {
    type Target = Locale;
    fn deref(&self) -> &Locale {
        &self.0
    }
}

impl DerefMut for LocaleRes {
    fn deref_mut(&mut self) -> &mut Locale {
        &mut self.0
    }
}

/// `"MotRocketArtillery"` → `"mot_rocket_artillery"` — enum Debug names map
/// onto localisation key suffixes without a hand-written match per variant.
pub fn camel_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn key_of(prefix: &str, debug_name: String) -> String {
    format!("{prefix}.{}", camel_to_snake(&debug_name))
}

impl LocaleRes {
    pub fn unit_type_name(&self, t: UnitType) -> Cow<'_, str> {
        self.tr(&key_of("unit_type", format!("{t:?}")))
    }
    pub fn terrain_name(&self, t: Terrain) -> Cow<'_, str> {
        self.tr(&key_of("terrain", format!("{t:?}")))
    }
    pub fn side_name(&self, s: Side) -> Cow<'_, str> {
        self.tr(&key_of("side", format!("{s:?}")))
    }
    pub fn unit_state_name(&self, s: UnitState) -> Cow<'_, str> {
        self.tr(&key_of("state", format!("{s:?}")))
    }
    pub fn phase_name(&self, p: BattlePhase) -> Cow<'_, str> {
        self.tr(&key_of("phase", format!("{p:?}")))
    }
    pub fn tactic_name(&self, t: CombatTactic) -> Cow<'_, str> {
        self.tr(&format!(
            "tactic.{}.name",
            camel_to_snake(&format!("{t:?}"))
        ))
    }
    pub fn tactic_desc(&self, t: CombatTactic) -> Cow<'_, str> {
        self.tr(&format!(
            "tactic.{}.desc",
            camel_to_snake(&format!("{t:?}"))
        ))
    }
    pub fn tactic_hint(&self, t: CombatTactic) -> Cow<'_, str> {
        self.tr(&format!(
            "tactic.{}.hint",
            camel_to_snake(&format!("{t:?}"))
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_conversion() {
        assert_eq!(camel_to_snake("MotRocketArtillery"), "mot_rocket_artillery");
        assert_eq!(camel_to_snake("Infantry"), "infantry");
        assert_eq!(camel_to_snake("AA"), "a_a");
    }

    /// Every enum variant must resolve to a real key in the embedded files —
    /// a variant added without a locale entry fails here, not on screen.
    #[test]
    fn all_variants_have_names() {
        for lang in Language::ALL {
            let loc = LocaleRes(Locale::load(*lang));
            for t in [
                UnitType::Infantry,
                UnitType::Marine,
                UnitType::Mountaineer,
                UnitType::Paratrooper,
                UnitType::Cavalry,
                UnitType::Bicycle,
                UnitType::Motorized,
                UnitType::Mechanized,
                UnitType::LightArmor,
                UnitType::MediumArmor,
                UnitType::HeavyArmor,
                UnitType::SuperHeavyArmor,
                UnitType::ModernArmor,
                UnitType::AmphibiousArmor,
                UnitType::ArtilleryBrigade,
                UnitType::RocketArtillery,
                UnitType::MotRocketArtillery,
                UnitType::AntiTankBrigade,
                UnitType::AntiAirBrigade,
                UnitType::Engineer,
                UnitType::Recon,
                UnitType::Signal,
                UnitType::Logistics,
                UnitType::Maintenance,
                UnitType::FieldHospital,
                UnitType::MilitaryPolice,
                UnitType::Headquarters,
            ] {
                let name = loc.unit_type_name(t);
                assert!(
                    !name.starts_with("unit_type."),
                    "{lang:?} missing {t:?}: {name}"
                );
            }
            // The counter-name abbreviations must resolve too
            // (tactical3d-bin's UnitNaming overlay reads these keys).
            for t in UnitType::ALL {
                let key = format!("unit_abbrev.{}", camel_to_snake(&format!("{t:?}")));
                assert!(
                    !loc.tr(&key).starts_with("unit_abbrev."),
                    "{lang:?} missing {key}"
                );
            }
            for t in Terrain::ALL {
                let name = loc.terrain_name(t);
                assert!(
                    !name.starts_with("terrain."),
                    "{lang:?} missing {t:?}: {name}"
                );
            }
            for t in [
                CombatTactic::Blitz,
                CombatTactic::ElasticDefense,
                CombatTactic::OverwhelmingFire,
                CombatTactic::InfiltrationAssault,
                CombatTactic::MassCharge,
                CombatTactic::GuerrillaTactics,
                CombatTactic::TacticalWithdrawal,
                CombatTactic::Encirclement,
                CombatTactic::Default,
                CombatTactic::Counterattack,
                CombatTactic::Ambush,
                CombatTactic::RiverDefense,
                CombatTactic::UrbanDefense,
                CombatTactic::Delay,
                CombatTactic::Assault,
                CombatTactic::RiverAssault,
            ] {
                for suffix in ["name", "desc", "hint"] {
                    let key = format!("tactic.{}.{suffix}", camel_to_snake(&format!("{t:?}")));
                    assert!(
                        !loc.tr(&key).starts_with("tactic."),
                        "{lang:?} missing {key}"
                    );
                }
            }
        }
    }
}
