//! Encirclement detection — DESIGN §6.4 (progressive; current design merges
//! Semi/Flanked into a single Partial level and retires the flanking damage
//! bonus — multi-directional attacks are already rewarded by the Lanchester
//! concentration itself — and the attrition pace is tuned to the 10-minute
//! turn, §8.1).

use crate::grid::HexGrid;
use crate::hex::HexDirection;
use crate::unit::BattalionUnit;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EncirclementLevel {
    None,
    /// Isolated and pressed — free ≤ 2, OR an opposite adjacent enemy pair.
    /// Either gate only applies to an ISOLATED target: an adjacent
    /// combat-effective enemy must be present and no combat-effective friend
    /// may be adjacent. -2.5% max org/turn. (§6.4's
    /// planned movement restriction stays unimplemented.)
    Partial,
    /// 0 free edges under the same enemy/no-friend gates: -5% max org/turn,
    /// org 0 ⇒ surrender (the unit never routs — it cannot retreat).
    Full,
}

/// A "free edge" is a passable neighbor hex not occupied by a
/// combat-effective enemy unit.
fn free_edges(grid: &HexGrid, unit: &BattalionUnit, units: &[BattalionUnit]) -> usize {
    grid.passable_neighbors(unit.position)
        .into_iter()
        .filter(|n| {
            !units
                .iter()
                .any(|u| u.side != unit.side && u.is_combat_effective() && u.position == *n)
        })
        .count()
}

/// Adjacent directions occupied by a combat-effective enemy.
fn enemy_adjacency_sides(unit: &BattalionUnit, units: &[BattalionUnit]) -> Vec<HexDirection> {
    HexDirection::ALL
        .into_iter()
        .filter(|d| {
            let n = unit.position.neighbor(*d);
            units
                .iter()
                .any(|u| u.side != unit.side && u.is_combat_effective() && u.position == n)
        })
        .collect()
}

/// An adjacent combat-effective FRIEND covers the unit:
/// a contiguous friendly line cannot be pocketed one battalion at a time.
fn has_adjacent_friend(target: &BattalionUnit, units: &[BattalionUnit]) -> bool {
    HexDirection::ALL.into_iter().any(|d| {
        let n = target.position.neighbor(d);
        units.iter().any(|u| {
            u.side == target.side && u.id != target.id && u.is_combat_effective() && u.position == n
        })
    })
}

fn has_opposite_pair(sides: &[HexDirection]) -> bool {
    sides
        .iter()
        .any(|a| sides.iter().any(|b| a.opposite() == *b))
}

pub fn detect_encirclement(
    grid: &HexGrid,
    target: &BattalionUnit,
    units: &[BattalionUnit],
) -> EncirclementLevel {
    // Encirclement only bites on isolated units. No adjacent enemy → terrain
    // alone never "encircles" (the old rule counted impassable hexes as
    // blocked edges, so a unit in a water corridor bled org with zero
    // contact); an adjacent friend covers.
    let sides = enemy_adjacency_sides(target, units);
    if sides.is_empty() || has_adjacent_friend(target, units) {
        return EncirclementLevel::None;
    }
    let free = free_edges(grid, target, units);
    if free == 0 {
        return EncirclementLevel::Full;
    }
    if free <= 2 || has_opposite_pair(&sides) {
        return EncirclementLevel::Partial;
    }
    EncirclementLevel::None
}

/// Org attrition per turn for a given level (§6.4). Applied by the combat
/// crate at the end of each side's turn.
pub fn org_attrition_fraction(
    level: EncirclementLevel,
    params: &crate::params::CombatParams,
) -> f32 {
    match level {
        EncirclementLevel::Partial => params.partial_encircle_org_attrition,
        EncirclementLevel::Full => params.full_encircle_org_attrition,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex::HexCoord;
    use crate::terrain::Terrain;
    use crate::unit::{Side, UnitType};

    fn grid() -> HexGrid {
        HexGrid::new(10, 10, Terrain::Plains)
    }

    fn u(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        BattalionUnit::new(id, format!("U{id}"), UnitType::Infantry, side, pos)
    }

    #[test]
    fn surrounded_is_full() {
        let g = grid();
        let target = u(0, Side::Defender, HexCoord::new(5, 5));
        let mut units = vec![target.clone()];
        for (i, n) in target.position.neighbors().into_iter().enumerate() {
            units.push(u(10 + i, Side::Attacker, n));
        }
        assert_eq!(
            detect_encirclement(&g, &target, &units),
            EncirclementLevel::Full
        );
    }

    #[test]
    fn opposite_pair_is_partial() {
        // The old Flanked case (opposite enemy pair, 4 free edges) is a
        // Partial encirclement now.
        let g = grid();
        let target = u(0, Side::Defender, HexCoord::new(5, 5));
        let units = vec![
            target.clone(),
            u(1, Side::Attacker, HexCoord::new(4, 5)), // W
            u(2, Side::Attacker, HexCoord::new(6, 5)), // E (opposite of W)
        ];
        assert_eq!(
            detect_encirclement(&g, &target, &units),
            EncirclementLevel::Partial
        );
    }

    #[test]
    fn adjacent_pair_not_opposite_is_none() {
        let g = grid();
        let target = u(0, Side::Defender, HexCoord::new(5, 5));
        let units = vec![
            target.clone(),
            u(1, Side::Attacker, HexCoord::new(6, 4)), // NE
            u(2, Side::Attacker, HexCoord::new(6, 5)), // E — adjacent bearings, not opposite
        ];
        assert_eq!(
            detect_encirclement(&g, &target, &units),
            EncirclementLevel::None
        );
    }

    #[test]
    fn two_free_edges_is_partial() {
        // The free-edge threshold dropped to ≤ 2. Pin the free-edge path
        // ALONE: two enemies on adjacent bearings (no opposite pair) plus
        // two water (impassable) hexes leave free = 2.
        let mut g = grid();
        let target = u(0, Side::Defender, HexCoord::new(5, 5));
        g.set_terrain(HexCoord::new(5, 4), Terrain::Water); // NW
        g.set_terrain(HexCoord::new(5, 6), Terrain::Water); // SE
        let units = vec![
            target.clone(),
            u(1, Side::Attacker, HexCoord::new(6, 4)), // NE
            u(2, Side::Attacker, HexCoord::new(6, 5)), // E
        ];
        assert_eq!(
            detect_encirclement(&g, &target, &units),
            EncirclementLevel::Partial
        );
    }

    #[test]
    fn three_enemies_three_free_is_none() {
        // Threshold tightening: 3 enemies on adjacent bearings (free = 3,
        // no opposite pair) used to be Semi — under the current rules it is
        // not an encirclement at all.
        let g = grid();
        let target = u(0, Side::Defender, HexCoord::new(5, 5));
        let units = vec![
            target.clone(),
            u(1, Side::Attacker, HexCoord::new(6, 4)), // NE
            u(2, Side::Attacker, HexCoord::new(6, 5)), // E
            u(3, Side::Attacker, HexCoord::new(5, 6)), // SE
        ];
        assert_eq!(
            detect_encirclement(&g, &target, &units),
            EncirclementLevel::None
        );
    }

    #[test]
    fn adjacent_friend_covers_against_partial_and_full() {
        // A contiguous friendly line cannot be pocketed — an adjacent
        // combat-effective friend grants cover, even against a
        // otherwise-full surround.
        let g = grid();
        let target = u(0, Side::Defender, HexCoord::new(5, 5));
        let mut units = vec![target.clone()];
        // Five enemies + one friend: free = 0, but covered.
        let neighbors = target.position.neighbors();
        for (i, n) in neighbors.into_iter().enumerate() {
            let side = if i == 0 {
                Side::Defender
            } else {
                Side::Attacker
            };
            units.push(u(10 + i, side, n));
        }
        assert_eq!(
            detect_encirclement(&g, &target, &units),
            EncirclementLevel::None
        );
    }

    #[test]
    fn terrain_alone_cannot_encircle() {
        // With no adjacent enemy, impassable hexes no longer bleed org (the
        // old rule's water-corridor false positive).
        let mut g = grid();
        let target = u(0, Side::Defender, HexCoord::new(5, 5));
        for n in target.position.neighbors().into_iter().take(5) {
            g.set_terrain(n, Terrain::Water);
        }
        let units = vec![target.clone()];
        assert_eq!(
            detect_encirclement(&g, &target, &units),
            EncirclementLevel::None
        );
    }
}
