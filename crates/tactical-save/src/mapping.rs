//! HOI4 subunit token ↔ [`UnitType`] × [`Chassis`] mapping and per-type
//! fallback data (DESIGN.md §5.3; a battalion class = weapon
//! type ⊕ chassis ⊕ token-level extra flags, mirroring HOI4's own
//! `type = { motorized, artillery }` multi-labels).
//!
//! Token spellings follow the real keys of `data/unit_templates.json`
//! (e.g. support companies are `signal_company`, `logistics_company`,
//! `artillery`, `anti_tank`, `anti_air`), with common aliases included.

use tactical_core::{Attrs, Chassis, UnitType};

/// A fully-resolved battalion class: weapon type, what carries
/// it, and token-level flags the type ⊕ chassis derivation cannot express
/// (amphibious, flame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitClass {
    pub unit_type: UnitType,
    pub chassis: Chassis,
    pub extra_attrs: Attrs,
}

impl Default for UnitClass {
    /// Unknown-token fallback (§5.3): plain foot infantry.
    fn default() -> Self {
        UnitClass {
            unit_type: UnitType::Infantry,
            chassis: Chassis::None,
            extra_attrs: Attrs::NONE,
        }
    }
}

/// Map a HOI4 subunit token to a full [`UnitClass`]. Returns `None` for
/// unknown tokens; callers decide whether to skip or fall back to the
/// [`Default`] (infantry) class.
pub fn map_unit_class(token: &str) -> Option<UnitClass> {
    let unit_type = map_unit_type(token)?;
    let chassis = map_chassis(token).unwrap_or_else(|| unit_type.default_chassis());
    Some(UnitClass {
        unit_type,
        chassis,
        extra_attrs: map_extra_attrs(token),
    })
}

/// Map a HOI4 subunit token to a [`UnitType`]. Returns `None` for unknown
/// tokens; callers decide whether to skip or fall back to Infantry (§5.3).
pub fn map_unit_type(token: &str) -> Option<UnitType> {
    Some(match token {
        "infantry" | "irregular_infantry" | "militia" | "penal_battalion" | "hq_infantry"
        | "sturmtruppe_battalion" | "blackshirt_assault_battalion" | "hq_support_company" => UnitType::Infantry,
        "marine" | "marine_commando" => UnitType::Marine,
        "mountaineers" | "ranger_battalion" => UnitType::Mountaineer,
        "paratrooper" | "hq_paratrooper" => UnitType::Paratrooper,
        "cavalry" | "camelry" | "elephantry" => UnitType::Cavalry,
        "bicycle_battalion" | "bicycle" => UnitType::Bicycle,
        "motorized" | "bus" | "hq_motorized" => UnitType::Motorized,
        "mechanized" | "amphibious_mechanized" => UnitType::Mechanized,
        "light_armor" | "amphibious_light_armor" | "hq_light_armor" | "airborne_light_armor"
        | "light_flame_tank" => UnitType::LightArmor,
        "medium_armor" | "amphibious_medium_armor" | "hq_medium_armor" | "medium_flame_tank" => {
            UnitType::MediumArmor
        }
        "heavy_armor" | "amphibious_heavy_armor" | "hq_heavy_armor" | "heavy_flame_tank" => {
            UnitType::HeavyArmor
        }
        "super_heavy_armor" | "land_cruiser" => UnitType::SuperHeavyArmor,
        "modern_armor" => UnitType::ModernArmor,
        "amphibious_armor" => UnitType::AmphibiousArmor,
        "artillery_brigade" | "artillery" | "mot_artillery_brigade" | "field_guns"
        | "super_heavy_artillery" | "self_propelled_super_heavy_artillery"
        | "light_sp_artillery_brigade" | "medium_sp_artillery_brigade"
        | "heavy_sp_artillery_brigade" | "super_heavy_sp_artillery_brigade"
        | "modern_sp_artillery_brigade" | "fire_support"
        | "mot_fire_support" => UnitType::ArtilleryBrigade,
        "rocket_artillery_brigade" | "rocket_artillery" | "rocket_battery" => {
            UnitType::RocketArtillery
        }
        "motorized_rocket_brigade" | "motorized_rocket" | "mot_rocket_artillery_brigade" => {
            UnitType::MotRocketArtillery
        }
        "anti_tank_brigade" | "anti_tank" | "anti_tank_battery" | "mot_anti_tank_brigade"
        | "light_tank_destroyer_brigade" | "medium_tank_destroyer_brigade"
        | "heavy_tank_destroyer_brigade" | "super_heavy_tank_destroyer_brigade"
        | "modern_tank_destroyer_brigade"
        | "light_tank_destroyer_support" | "medium_tank_destroyer_support"
        | "heavy_tank_destroyer_support" | "modern_tank_destroyer_support" => {
            UnitType::AntiTankBrigade
        }
        "anti_air_brigade" | "anti_air" | "anti_air_battery" | "mot_anti_air_brigade"
        | "light_sp_anti_air_brigade" | "medium_sp_anti_air_brigade" | "heavy_sp_anti_air_brigade"
        | "super_heavy_sp_anti_air_brigade" | "modern_sp_anti_air_brigade"
        | "light_sp_anti_air_support" | "medium_sp_anti_air_support" | "heavy_sp_anti_air_support"
        | "modern_sp_anti_air_support" => UnitType::AntiAirBrigade,
        "engineer" | "armored_engineer" | "assault_engineer" | "hq_engineer"
        | "pioneer_support" | "jungle_pioneers_support" => UnitType::Engineer,
        "recon" | "light_armor_recon" | "light_tank_recon" | "mot_recon" | "armored_car_recon"
        | "armored_car" | "hq_recon" | "helicopter_recon" | "long_range_patrol_support"
        | "northern_territory_recon_support" | "rangers_support"
        // Tentative: air-cavalry / specops-liaison companies read as scouts.
        | "helicopter_brigade" | "hq_specops" => UnitType::Recon,
        "signal" | "signal_company" | "armored_signal" | "hq_signal"
        | "hq_naval_liaison" | "hq_air_liaison" => UnitType::Signal,
        "logistics" | "logistics_company" | "hq_logistics" | "helicopter_transport"
        | "winter_logistics_support" => UnitType::Logistics,
        "maintenance" | "maintenance_company" | "armored_maintenance" | "hq_maintenance" => {
            UnitType::Maintenance
        }
        "field_hospital" | "helicopter_field_hospital" | "hq_field_hospital" => {
            UnitType::FieldHospital
        }
        "military_police" | "motorized_military_police" | "hq_military_police" => {
            UnitType::MilitaryPolice
        }
        _ => return None,
    })
}

/// Non-default chassis named by the token: self-propelled
/// guns/TD/SPAA ride the weight-class chassis of their gun, truck-towed
/// pieces (`mot_*`) get trucks, the Katyusha is a wheeled self-propelled
/// launcher. `None` → the type's [`UnitType::default_chassis`].
pub fn map_chassis(token: &str) -> Option<Chassis> {
    use Chassis::*;
    Some(match token {
        "light_sp_artillery_brigade"
        | "light_tank_destroyer_brigade"
        | "light_tank_destroyer_support"
        | "light_sp_anti_air_brigade"
        | "light_sp_anti_air_support" => Light,
        "medium_sp_artillery_brigade"
        | "medium_tank_destroyer_brigade"
        | "medium_tank_destroyer_support"
        | "medium_sp_anti_air_brigade"
        | "medium_sp_anti_air_support" => Medium,
        "heavy_sp_artillery_brigade"
        | "heavy_tank_destroyer_brigade"
        | "heavy_tank_destroyer_support"
        | "heavy_sp_anti_air_brigade"
        | "heavy_sp_anti_air_support" => Heavy,
        "super_heavy_sp_artillery_brigade"
        | "self_propelled_super_heavy_artillery"
        | "super_heavy_tank_destroyer_brigade"
        | "super_heavy_sp_anti_air_brigade"
        | "land_cruiser" => SuperHeavy,
        "modern_sp_artillery_brigade"
        | "modern_tank_destroyer_brigade"
        | "modern_tank_destroyer_support"
        | "modern_sp_anti_air_brigade"
        | "modern_sp_anti_air_support" => Modern,
        // Truck-towed guns (type = { motorized, artillery } in HOI4): road
        // speed 12 but the crew still emplaces before firing.
        "mot_artillery_brigade"
        | "mot_anti_tank_brigade"
        | "mot_anti_air_brigade"
        | "mot_rocket_artillery_brigade"
        | "mot_fire_support" => TruckTowed,
        // Katyusha-style self-propelled rocket launcher (no emplacement).
        "motorized_rocket_brigade" | "motorized_rocket" => Wheeled,
        // NB: `None` here is Option's — Chassis::None is shadowed by the
        // glob import above, so spell the Option path out.
        _ => return Option::None,
    })
}

/// Token-level attribute extras the type ⊕ chassis derivation cannot
/// express (rules for these land in later passes).
pub fn map_extra_attrs(token: &str) -> Attrs {
    match token {
        "marine"
        | "marine_commando"
        | "amphibious_armor"
        | "amphibious_light_armor"
        | "amphibious_medium_armor"
        | "amphibious_heavy_armor"
        | "amphibious_mechanized" => Attrs::AMPHIBIOUS,
        "light_flame_tank" | "medium_flame_tank" | "heavy_flame_tank" => Attrs::FLAME,
        _ => Attrs::NONE,
    }
}

/// Short label used for generated battalion names ("1. Inf", "2. Inf", ...).
pub fn unit_type_abbrev(ut: UnitType) -> &'static str {
    match ut {
        UnitType::Infantry => "Inf",
        UnitType::Marine => "Mar",
        UnitType::Mountaineer => "Mtn",
        UnitType::Paratrooper => "Para",
        UnitType::Cavalry => "Cav",
        UnitType::Bicycle => "Bic",
        UnitType::Motorized => "Mot",
        UnitType::Mechanized => "Mech",
        UnitType::LightArmor => "L Arm",
        UnitType::MediumArmor => "M Arm",
        UnitType::HeavyArmor => "H Arm",
        UnitType::SuperHeavyArmor => "SH Arm",
        UnitType::ModernArmor => "Mod Arm",
        UnitType::AmphibiousArmor => "Amph Arm",
        UnitType::ArtilleryBrigade => "Art",
        UnitType::RocketArtillery => "R Art",
        UnitType::MotRocketArtillery => "MR Art",
        UnitType::AntiTankBrigade => "AT",
        UnitType::AntiAirBrigade => "AA",
        UnitType::Engineer => "Eng",
        UnitType::Recon => "Rec",
        UnitType::Signal => "Sig",
        UnitType::Logistics => "Log",
        UnitType::Maintenance => "Mnt",
        UnitType::FieldHospital => "FH",
        UnitType::MilitaryPolice => "MP",
        // §6.13: HQ units are synthesized, never token-parsed —
        // this arm only keeps the match exhaustive.
        UnitType::Headquarters => "HQ",
    }
}

/// Display-name table baked into generated battalion names + support tags.
/// `Default` carries the historical English abbreviations
/// ("1. Inf", "AT", "HQ"); tactical3d-bin overlays the session language from
/// the `unit_abbrev.*` / `support_abbrev.*` locale keys so a Chinese session
/// fields "1. 步兵营" / "指挥部" (the zh labels carry the proper
/// echelon suffix — 营 for line-capable types; the 7 support-company-only
/// types (engineer…military_police, HOI4_UNITS.md: they never field as line
/// battalions) and the support attachments take 连). The table is plain data
/// so this crate stays locale-free (§2.2 layering).
#[derive(Clone)]
pub struct UnitNaming {
    /// Per-type abbreviation, incl. the synthesized HQ ("HQ"/"指挥部").
    types: std::collections::HashMap<UnitType, String>,
    /// Token overrides: several tokens fold into one UnitType
    /// (militia → Infantry, …) and the type label alone made a blackshirt
    /// militia battalion indistinguishable from line infantry in the OOB.
    tokens: std::collections::HashMap<&'static str, String>,
    /// Per-support-kind attachment tag (OOB / hover card).
    support: std::collections::HashMap<tactical_core::SupportKind, String>,
    /// Per-token support-company names for the companies that attach with
    /// a no-bonus kind (their vanilla effect is not modelled — §5.3): the
    /// kind tag would mislabel a flame-tank company as MPs.
    support_names: std::collections::HashMap<&'static str, String>,
}

impl Default for UnitNaming {
    /// English abbreviations — the originally hardcoded strings.
    fn default() -> Self {
        let types = UnitType::ALL
            .into_iter()
            .map(|ut| (ut, unit_type_abbrev(ut).to_string()))
            .collect();
        // The army-HQ family (§6.13) is labelled as staff for the same
        // reason the folded types are: an HQ formation fields those
        // battalions normally, and the plain type label made the general's
        // guard indistinguishable from a line battalion in the OOB.
        let tokens = [
            ("militia", "Mil"),
            ("irregular_infantry", "Irr"),
            ("penal_battalion", "Pen"),
            ("hq_infantry", "HQ Inf"),
            ("hq_paratrooper", "HQ Para"),
            ("hq_motorized", "HQ Mot"),
            ("hq_light_armor", "HQ L Arm"),
            ("hq_medium_armor", "HQ M Arm"),
            ("hq_heavy_armor", "HQ H Arm"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect();
        use tactical_core::SupportKind as K;
        let support = [
            (K::AntiTank, "AT"),
            (K::AntiAir, "AA"),
            (K::Artillery, "ART"),
            (K::Engineer, "ENG"),
            (K::Recon, "REC"),
            (K::FieldHospital, "HOS"),
            (K::Signal, "SIG"),
            (K::Maintenance, "MNT"),
            (K::Logistics, "LOG"),
            (K::MilitaryPolice, "MP"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect();
        // English short names for the no-modelled-effect companies (the
        // `support_name.*` locale keys carry the same strings for English).
        let support_names = [
            ("light_flame_tank", "L. Flame"),
            ("medium_flame_tank", "M. Flame"),
            ("heavy_flame_tank", "H. Flame"),
            ("super_heavy_armor", "SH Tank"),
            ("land_cruiser", "Land Cruiser"),
            ("helicopter_brigade", "Helicopter"),
            ("blackshirt_assault_battalion", "Blackshirt"),
            ("sturmtruppe_battalion", "Stosstruppen"),
            ("hq_support_company", "HQ Staff"),
            ("hq_specops", "SpecOps"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect();
        UnitNaming {
            types,
            tokens,
            support,
            support_names,
        }
    }
}

impl UnitNaming {
    /// Override one type's label (localized pass in tactical3d-bin).
    pub fn set_type(&mut self, ut: UnitType, label: String) {
        self.types.insert(ut, label);
    }
    /// Override one special-token label (localized pass).
    pub fn set_token(&mut self, token: &'static str, label: String) {
        self.tokens.insert(token, label);
    }
    /// Override one support-kind tag (localized pass).
    pub fn set_support(&mut self, kind: tactical_core::SupportKind, label: String) {
        self.support.insert(kind, label);
    }
    /// Override one no-modelled-effect company's display name (localized
    /// pass) — see the `support_names` field doc.
    pub fn set_support_name(&mut self, token: &'static str, label: String) {
        self.support_names.insert(token, label);
    }

    /// The label piece after the running number ("1. Inf" → "Inf"): distinct
    /// tokens keep their distinct label, everything else uses the type's.
    pub fn subunit_abbrev(&self, token: &str, ut: UnitType) -> String {
        if let Some(t) = self.tokens.get(token) {
            return t.clone();
        }
        self.types
            .get(&ut)
            .cloned()
            .unwrap_or_else(|| unit_type_abbrev(ut).to_string())
    }

    /// Attachment tag for a support kind (OOB / hover card).
    pub fn support_tag(&self, kind: tactical_core::SupportKind) -> String {
        self.support.get(&kind).cloned().unwrap_or_default()
    }

    /// Display name for one support company: the token's own name when it
    /// carries one (the no-modelled-effect companies), else its kind tag.
    pub fn support_name(&self, token: &str, kind: tactical_core::SupportKind) -> String {
        if let Some(t) = self.support_names.get(token) {
            return t.clone();
        }
        self.support_tag(kind)
    }

    /// Synthesized-HQ label (§6.13) — the Headquarters type entry.
    pub fn hq(&self) -> String {
        self.types
            .get(&UnitType::Headquarters)
            .cloned()
            .unwrap_or_else(|| "HQ".to_string())
    }
}

/// Canonical `unit_templates.json` key for a unit type — fallback when the
/// save's original token is not itself a table key (§5.3).
pub(crate) fn canonical_template_key(ut: UnitType) -> &'static str {
    match ut {
        UnitType::Infantry => "infantry",
        UnitType::Marine => "marine",
        UnitType::Mountaineer => "mountaineers",
        UnitType::Paratrooper => "paratrooper",
        UnitType::Cavalry => "cavalry",
        UnitType::Bicycle => "bicycle_battalion",
        UnitType::Motorized => "motorized",
        UnitType::Mechanized => "mechanized",
        UnitType::LightArmor => "light_armor",
        UnitType::MediumArmor => "medium_armor",
        UnitType::HeavyArmor => "heavy_armor",
        UnitType::SuperHeavyArmor => "super_heavy_armor",
        UnitType::ModernArmor => "modern_armor",
        UnitType::AmphibiousArmor => "amphibious_armor",
        UnitType::ArtilleryBrigade => "artillery_brigade",
        UnitType::RocketArtillery => "rocket_artillery_brigade",
        UnitType::MotRocketArtillery => "motorized_rocket_brigade",
        UnitType::AntiTankBrigade => "anti_tank_brigade",
        UnitType::AntiAirBrigade => "anti_air_brigade",
        UnitType::Engineer => "engineer",
        UnitType::Recon => "recon",
        UnitType::Signal => "signal_company",
        UnitType::Logistics => "logistics_company",
        UnitType::Maintenance => "maintenance_company",
        UnitType::FieldHospital => "field_hospital",
        UnitType::MilitaryPolice => "military_police",
        // §6.13: synthesized, never resolves a template.
        UnitType::Headquarters => "infantry",
    }
}

/// Map a HOI4 support-company token to its attachment kind.
/// Only support companies reach here — line-battalion artillery/AT/AA (the
/// `*_brigade` forms) stay independent map units. Tokens outside
/// [`support_kind`]'s table (modded content) fall back to MilitaryPolice,
/// which carries no bonuses (a safe no-op placeholder); the assembly logs
/// them so new vanilla companies surface instead of silently attaching.
pub fn map_support_kind(token: &str) -> tactical_core::SupportKind {
    support_kind(token).unwrap_or(tactical_core::SupportKind::MilitaryPolice)
}

/// The explicit token → kind table behind [`map_support_kind`]: every
/// support company of the vanilla land roster (§5.4 `support_companies`
/// bucket), including the regimental-support column and the army-HQ `hq_*`
/// companies. The companies whose vanilla effect FC does not model (flame
/// tanks, super-heavy armor, land cruiser, the helicopter/specops/assault
/// companies, the HQ staff company) still list here with the no-bonus
/// MilitaryPolice kind — the honest zero-effect semantic — and keep their
/// own display name through [`UnitNaming::support_name`]. Returns `None`
/// for tokens outside the table.
pub fn support_kind(token: &str) -> Option<tactical_core::SupportKind> {
    use tactical_core::SupportKind as K;
    Some(match token {
        "artillery"
        | "rocket_artillery"
        | "super_heavy_artillery"
        | "self_propelled_super_heavy_artillery"
        | "super_heavy_sp_artillery_brigade"
        | "fire_support"
        | "mot_fire_support"
        | "field_guns"
        | "rocket_battery" => K::Artillery,
        "anti_tank"
        | "anti_tank_battery"
        | "super_heavy_tank_destroyer_brigade"
        | "light_tank_destroyer_support"
        | "medium_tank_destroyer_support"
        | "heavy_tank_destroyer_support"
        | "modern_tank_destroyer_support" => K::AntiTank,
        "anti_air"
        | "anti_air_battery"
        | "super_heavy_sp_anti_air_brigade"
        | "light_sp_anti_air_support"
        | "medium_sp_anti_air_support"
        | "heavy_sp_anti_air_support"
        | "modern_sp_anti_air_support" => K::AntiAir,
        "engineer"
        | "pioneer_support"
        | "jungle_pioneers_support"
        | "assault_engineer"
        | "armored_engineer"
        | "hq_engineer" => K::Engineer,
        "recon"
        | "mot_recon"
        | "armored_car_recon"
        | "light_tank_recon"
        | "airborne_light_armor"
        | "rangers_support"
        | "northern_territory_recon_support"
        | "long_range_patrol_support"
        | "helicopter_recon"
        | "hq_recon" => K::Recon,
        "field_hospital" | "helicopter_field_hospital" | "hq_field_hospital" => K::FieldHospital,
        "signal_company" | "armored_signal" | "hq_signal" | "hq_naval_liaison"
        | "hq_air_liaison" => K::Signal,
        "maintenance_company" | "armored_maintenance" | "hq_maintenance" => K::Maintenance,
        "logistics_company"
        | "winter_logistics_support"
        | "helicopter_transport"
        | "hq_logistics" => K::Logistics,
        "military_police" | "motorized_military_police" | "hq_military_police" => K::MilitaryPolice,
        // No modelled vanilla effect — attached with the no-bonus kind,
        // distinguished by their display name (§5.3).
        "light_flame_tank"
        | "medium_flame_tank"
        | "heavy_flame_tank"
        | "super_heavy_armor"
        | "land_cruiser"
        | "helicopter_brigade"
        | "blackshirt_assault_battalion"
        | "sturmtruppe_battalion"
        | "hq_support_company"
        | "hq_specops" => K::MilitaryPolice,
        _ => return None,
    })
}

/// Equipment archetype a unit type draws from (HOI4 data-name quirk: tank
/// archetypes are `*_tank_chassis`; ALL support companies draw
/// `support_equipment`). Used only when the unit template table has no entry.
pub(crate) fn fallback_equipment_archetype(ut: UnitType) -> &'static str {
    match ut {
        UnitType::Infantry
        | UnitType::Marine
        | UnitType::Mountaineer
        | UnitType::Paratrooper
        | UnitType::Cavalry
        | UnitType::Bicycle => "infantry_equipment",
        UnitType::Motorized => "motorized_equipment",
        UnitType::Mechanized => "mechanized_equipment",
        UnitType::LightArmor => "light_tank_chassis",
        UnitType::MediumArmor => "medium_tank_chassis",
        UnitType::HeavyArmor => "heavy_tank_chassis",
        UnitType::SuperHeavyArmor => "super_heavy_tank_chassis",
        UnitType::ModernArmor => "modern_tank_chassis",
        UnitType::AmphibiousArmor => "amphibious_tank_chassis",
        UnitType::ArtilleryBrigade => "artillery_equipment",
        UnitType::RocketArtillery => "rocket_artillery_equipment",
        UnitType::MotRocketArtillery => "motorized_rocket_equipment",
        UnitType::AntiTankBrigade => "anti_tank_equipment",
        UnitType::AntiAirBrigade => "anti_air_equipment",
        UnitType::Engineer
        | UnitType::Recon
        | UnitType::Signal
        | UnitType::Logistics
        | UnitType::Maintenance
        | UnitType::FieldHospital
        | UnitType::MilitaryPolice => "support_equipment",
        // §6.13: synthesized, never draws equipment.
        UnitType::Headquarters => "support_equipment",
    }
}

/// Rough per-battalion equipment complement, used only when the unit template
/// table has no `needs` entry (approximations of `common/units/*.txt`).
pub(crate) fn fallback_equipment_target(ut: UnitType) -> f32 {
    match ut {
        UnitType::Infantry
        | UnitType::Marine
        | UnitType::Mountaineer
        | UnitType::Paratrooper
        | UnitType::Cavalry
        | UnitType::Bicycle => 100.0,
        UnitType::Motorized | UnitType::Mechanized => 50.0,
        UnitType::LightArmor
        | UnitType::MediumArmor
        | UnitType::HeavyArmor
        | UnitType::SuperHeavyArmor
        | UnitType::ModernArmor
        | UnitType::AmphibiousArmor => 60.0,
        UnitType::ArtilleryBrigade | UnitType::RocketArtillery | UnitType::MotRocketArtillery => {
            36.0
        }
        UnitType::AntiTankBrigade | UnitType::AntiAirBrigade => 30.0,
        UnitType::Engineer
        | UnitType::Recon
        | UnitType::Signal
        | UnitType::Logistics
        | UnitType::Maintenance
        | UnitType::FieldHospital
        | UnitType::MilitaryPolice => 30.0,
        // §6.13: synthesized, never draws equipment.
        UnitType::Headquarters => 30.0,
    }
}

/// Equipment archetype a chassis draws its MOBILITY from (§5.3 mobility
/// shortfall): trucks for the wheeled / truck-towed classes, halftracks
/// for mechanized infantry, the tank chassis archetype for armored weight
/// classes. Foot and horse-towed classes have no mobility equipment —
/// they never slow for want of it.
pub(crate) fn mobility_equipment_archetype(chassis: Chassis, ut: UnitType) -> Option<&'static str> {
    match chassis {
        Chassis::None | Chassis::Towed => None,
        Chassis::TruckTowed | Chassis::Wheeled => Some("motorized_equipment"),
        Chassis::Halftrack => Some("mechanized_equipment"),
        Chassis::Light
        | Chassis::Medium
        | Chassis::Heavy
        | Chassis::SuperHeavy
        | Chassis::Modern => Some(fallback_equipment_archetype(ut)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_maps_to_expected_unit_type() {
        assert_eq!(map_unit_type("infantry"), Some(UnitType::Infantry));
        assert_eq!(map_unit_type("marine"), Some(UnitType::Marine));
        assert_eq!(map_unit_type("mountaineers"), Some(UnitType::Mountaineer));
        assert_eq!(map_unit_type("light_armor"), Some(UnitType::LightArmor));
        assert_eq!(map_unit_type("medium_armor"), Some(UnitType::MediumArmor));
        assert_eq!(
            map_unit_type("artillery_brigade"),
            Some(UnitType::ArtilleryBrigade)
        );
        assert_eq!(map_unit_type("artillery"), Some(UnitType::ArtilleryBrigade));
        assert_eq!(
            map_unit_type("rocket_artillery"),
            Some(UnitType::RocketArtillery)
        );
        assert_eq!(
            map_unit_type("motorized_rocket"),
            Some(UnitType::MotRocketArtillery)
        );
        assert_eq!(map_unit_type("anti_tank"), Some(UnitType::AntiTankBrigade));
        assert_eq!(map_unit_type("anti_air"), Some(UnitType::AntiAirBrigade));
        assert_eq!(map_unit_type("engineer"), Some(UnitType::Engineer));
        assert_eq!(map_unit_type("recon"), Some(UnitType::Recon));
        // Real unit_templates.json spellings for support companies:
        assert_eq!(map_unit_type("signal_company"), Some(UnitType::Signal));
        assert_eq!(
            map_unit_type("logistics_company"),
            Some(UnitType::Logistics)
        );
        assert_eq!(
            map_unit_type("maintenance_company"),
            Some(UnitType::Maintenance)
        );
        assert_eq!(
            map_unit_type("field_hospital"),
            Some(UnitType::FieldHospital)
        );
        assert_eq!(
            map_unit_type("military_police"),
            Some(UnitType::MilitaryPolice)
        );
        // Recon aliases:
        assert_eq!(map_unit_type("light_armor_recon"), Some(UnitType::Recon));
        assert_eq!(map_unit_type("armored_car_recon"), Some(UnitType::Recon));
    }

    #[test]
    fn unknown_token_returns_none() {
        assert_eq!(map_unit_type("warpack"), None);
        assert_eq!(map_unit_type(""), None);
    }

    #[test]
    fn unit_class_resolves_chassis_and_extras() {
        // SP guns ride weight-class chassis (no emplacement),
        // truck-towed pieces get trucks, foot types keep the type default.
        let c = map_unit_class("medium_sp_artillery_brigade").unwrap();
        assert_eq!(
            (c.unit_type, c.chassis),
            (UnitType::ArtilleryBrigade, Chassis::Medium)
        );
        let c = map_unit_class("heavy_tank_destroyer_brigade").unwrap();
        assert_eq!(
            (c.unit_type, c.chassis),
            (UnitType::AntiTankBrigade, Chassis::Heavy)
        );
        let c = map_unit_class("modern_sp_anti_air_brigade").unwrap();
        assert_eq!(
            (c.unit_type, c.chassis),
            (UnitType::AntiAirBrigade, Chassis::Modern)
        );
        let c = map_unit_class("mot_artillery_brigade").unwrap();
        assert_eq!(c.chassis, Chassis::TruckTowed);
        let c = map_unit_class("mot_rocket_artillery_brigade").unwrap();
        // Truck-towed rockets: emplaces despite the "Mot" in the token.
        assert_eq!(
            (c.unit_type, c.chassis),
            (UnitType::MotRocketArtillery, Chassis::TruckTowed)
        );
        let c = map_unit_class("motorized_rocket_brigade").unwrap();
        assert_eq!(
            (c.unit_type, c.chassis),
            (UnitType::MotRocketArtillery, Chassis::Wheeled)
        );
        // Plain towed gun: no chassis override → type default (Towed).
        let c = map_unit_class("artillery_brigade").unwrap();
        assert_eq!(c.chassis, Chassis::Towed);
        // Extra flags.
        assert_eq!(
            map_unit_class("marine").unwrap().extra_attrs,
            Attrs::AMPHIBIOUS
        );
        assert_eq!(
            map_unit_class("medium_flame_tank").unwrap().extra_attrs,
            Attrs::FLAME
        );
        assert_eq!(map_unit_class("infantry").unwrap().extra_attrs, Attrs::NONE);
        // Previously-unmapped tokens now covered.
        assert!(map_unit_class("super_heavy_sp_artillery_brigade").is_some());
        assert!(map_unit_class("super_heavy_tank_destroyer_brigade").is_some());
        assert!(map_unit_class("modern_sp_anti_air_support").is_some());
        assert_eq!(
            map_unit_class("land_cruiser").map(|c| (c.unit_type, c.chassis)),
            Some((UnitType::SuperHeavyArmor, Chassis::SuperHeavy))
        );
        assert_eq!(
            map_unit_class("winter_logistics_support").map(|c| c.unit_type),
            Some(UnitType::Logistics)
        );
        assert_eq!(
            map_unit_class("helicopter_transport").map(|c| c.unit_type),
            Some(UnitType::Logistics)
        );
        // Unknown-token fallback is plain foot infantry.
        assert_eq!(
            map_unit_class("warpack").unwrap_or_default(),
            UnitClass {
                unit_type: UnitType::Infantry,
                chassis: Chassis::None,
                extra_attrs: Attrs::NONE
            }
        );
    }

    #[test]
    fn abbreviations_are_short_labels() {
        assert_eq!(unit_type_abbrev(UnitType::Infantry), "Inf");
        assert_eq!(unit_type_abbrev(UnitType::ArtilleryBrigade), "Art");
        assert_eq!(unit_type_abbrev(UnitType::Engineer), "Eng");
    }

    #[test]
    fn army_hq_tokens_carry_staff_labels() {
        // §6.13: the army-HQ formation's battalions are staff, not line —
        // every `hq_*` subunit token carries its own label, or the OOB
        // reads the general's guard as just another combat battalion.
        let naming = UnitNaming::default();
        for (token, ut, label) in [
            ("hq_infantry", UnitType::Infantry, "HQ Inf"),
            ("hq_paratrooper", UnitType::Paratrooper, "HQ Para"),
            ("hq_motorized", UnitType::Motorized, "HQ Mot"),
            ("hq_light_armor", UnitType::LightArmor, "HQ L Arm"),
            ("hq_medium_armor", UnitType::MediumArmor, "HQ M Arm"),
            ("hq_heavy_armor", UnitType::HeavyArmor, "HQ H Arm"),
        ] {
            assert_eq!(naming.subunit_abbrev(token, ut), label, "token {token}");
        }
    }
}
