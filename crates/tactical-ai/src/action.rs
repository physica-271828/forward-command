//! AI decision output — proposed commands for the game loop to execute.
//!
//! These are *proposals only* (§7.1 layer 3): `tactical-ai` never mutates unit
//! state. The caller replays the list against `tactical-combat`, which enforces
//! the §6.2 one-action-per-turn rule and resolves combat (§6.3). A proposed
//! action may fail there (e.g. an assault target already eliminated earlier in
//! the list); the caller must skip such actions.

use tactical_core::HexCoord;

/// One proposed command, in execution order (§6.12: the AI side completes all
/// of its actions before the opponent acts).
#[derive(Debug, Clone, PartialEq)]
pub enum AiAction {
    /// Issue a standing movement order along a path of adjacent hexes
    /// (excluding the start hex, ending at the destination). The unit then
    /// marches on its own at its speed (§6.2) — the AI does not teleport it.
    MoveUnit { unit_id: usize, path: Vec<HexCoord> },
    /// Close combat against an adjacent enemy battalion (§6.3 Assault).
    /// Consumes the unit's action for the turn (§6.2).
    Assault {
        attacker_id: usize,
        target_id: usize,
    },
    /// Ranged fire on a hex within the firing envelope (§6.3 Fire Support).
    /// The caller decides precise vs. area (§6.3 gesture rule):
    /// precise = the aim hex holds an enemy currently visible to the
    /// acting side (full damage on that hex); everything else resolves as
    /// area fire (weighted 4/10-1/10 zone; rocket artillery always area).
    /// Consumes the unit's action for the turn (§6.2).
    FireSupport {
        attacker_id: usize,
        target_hex: HexCoord,
    },
    /// Toggle/keep the Hold stance (§6.8): free action, +25% defense.
    Hold { unit_id: usize },
    /// Emplace a towed gun (§6.3): consumes the unit's action for the turn;
    /// required before it may provide fire support.
    Emplace { unit_id: usize },
    /// Limber an emplaced towed gun back to marching order (§6.3): consumes
    /// the unit's action for the turn.
    Limber { unit_id: usize },
    /// Manual retreat toward the own deployment zone (§6.8): the unit is in
    /// trouble and disengages; the caller applies the §6.8 retreat rules
    /// (entrenchment loss, -20% org, path to own edge).
    Retreat { unit_id: usize },
    /// Marker emitted once as the last element of every `plan_turn` result —
    /// the AI side is done (§6.12).
    EndTurn,
}
