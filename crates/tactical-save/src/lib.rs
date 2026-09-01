//! tactical-save — HOI4 save parsing → tactical battalions (DESIGN.md §5).
//!
//! Pipeline (§5.1): read a Clausewitz-text `.hoi4` save, extract countries,
//! divisions and division templates (§5.2), then merge with the pre-extracted
//! JSON data tables (§5.4) to produce per-battalion [`tactical_core::BattalionUnit`]
//! stats (§5.3).
//!
//! Only **text** saves are supported: HOI4 must run with `save_as_binary=no`
//! (§11.2). Binary saves are detected up-front and rejected with a clear,
//! actionable error.
//!
//! Known save-format quirks handled here (see project debug guide):
//! - Save files start with a `HOI4txt` header line that must be stripped.
//! - Real saves keep `division_templates` at ROOT level, not under the country
//!   (country-level `division_template` is still parsed as a fallback).
//! - Tank equipment archetypes are `light_tank_chassis` etc., and all support
//!   companies draw `support_equipment`; equipment JSON has no bare archetype
//!   keys for most types, so archetype → latest-variant resolution is built in.

mod mapping;
mod model;
mod names;
mod parser;
mod tables;
mod units;

pub use mapping::{map_unit_class, map_unit_type, unit_type_abbrev, UnitClass, UnitNaming};
pub use model::{
    ArmyData, BattalionInfo, CountryData, DivisionData, LandCombatData, LandCombatSideData,
    LeaderData, SaveGame, TemplateData, COMBAT_TACTIC_IDS,
};
pub use names::{NameGroup, NameGroups};
pub use parser::{find_divisions_in_province, SaveParser};
pub use tables::{
    CountryColorTable, DoctrineFactors, DoctrineTable, EquipmentStats, EquipmentTable,
    IdeaModifiers, ModifierTable, UnitTemplateStats, UnitTemplateTable,
};
pub use units::{
    create_tactical_units, create_tactical_units_named, division_maxima, experience_attack_factor,
    leader_bonus, CountryCombatModifier, CountryOrgModifier, LeaderBonus,
    DEFAULT_DEGRADATION_FACTOR,
};

use std::path::PathBuf;

/// Errors produced by save parsing and data-table loading.
///
/// Every variant is descriptive; no code path in this crate panics on
/// malformed input (§11.3 error recovery).
#[derive(Debug)]
pub enum SaveError {
    /// A file (save or data table) could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The save is in HOI4's binary format; only text saves are supported.
    Binary,
    /// The Clausewitz text could not be tokenized/parsed.
    Parse(String),
    /// A pre-extracted JSON data table (§5.4) is malformed.
    Data { path: PathBuf, message: String },
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Io { path, source } => {
                write!(f, "failed to read file {}: {}", path.display(), source)
            }
            SaveError::Binary => write!(
                f,
                "binary HOI4 save detected; tactical-save only reads text saves — \
                 set `save_as_binary=no` in HOI4 settings (settings.txt) and save again"
            ),
            SaveError::Parse(msg) => write!(f, "failed to parse save text: {}", msg),
            SaveError::Data { path, message } => {
                write!(
                    f,
                    "failed to load data table {}: {}",
                    path.display(),
                    message
                )
            }
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SaveError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
