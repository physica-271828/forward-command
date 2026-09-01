//! tactical-core — pure game-logic foundation, rewritten per DESIGN.md.
//!
//! Modules: hex math (axial pointy-top), terrain table (§6.6), battalion units
//! (§6.1/§6.2), A* pathfinding with ZOC (§6.5), line-of-sight, fog of war,
//! encirclement detection (§6.4), combat parameters (§12), deterministic PRNG.

pub mod command;
pub mod damage;
pub mod encirclement;
pub mod flag;
pub mod fog;
pub mod grid;
pub mod hex;
pub mod los;
pub mod movement;
pub mod noise;
pub mod params;
pub mod pathfinding;
pub mod rng;
pub mod terrain;
pub mod unit;

pub use command::{
    aura_radius_of, compute_command_links, hq_chassis_for_division, in_command, synthesize_hqs,
    CommandLink,
};
pub use flag::{
    collapse_side, derive_flag_state, update_flag_progress, FlagKind, FlagState, FlagTick, FlagZone,
};
pub use grid::{GridCell, HexGrid};
pub use hex::{CubeCoord, HexCoord, HexDirection};
pub use params::CombatParams;
pub use rng::XorShift64;
pub use terrain::Terrain;
pub use unit::{
    Attrs, BattalionUnit, Chassis, MobilityClass, MoveOrder, Side, SupportAttachment, SupportKind,
    TerrainAdjusters, UnitState, UnitType,
};
