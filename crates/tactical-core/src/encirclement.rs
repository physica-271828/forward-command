//! Encirclement detection — DESIGN §6.4 (progressive; current design merges
//! Semi/Flanked into a single Partial level and retires the flanking damage
//! bonus — multi-directional attacks are already rewarded by the Lanchester
//! concentration itself — and the attrition pace is tuned to the 10-minute
//! turn, §8.1).

use crate::grid::HexGrid;
use crate::hex::{HexCoord, HexDirection};
use crate::unit::{BattalionUnit, UnitState};

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

/// The pocket `target` belongs to: the connected set of same-side units
/// still holding ground (`Active` / `Retreating`, on the board), connected
/// through adjacent hexes. Returns indices into `units`; a lone unit's
/// pocket is itself, an empty Vec means the target no longer holds ground.
fn pocket_group(target: &BattalionUnit, units: &[BattalionUnit]) -> Vec<usize> {
    let holds = |u: &BattalionUnit| {
        u.side == target.side
            && matches!(u.state, UnitState::Active | UnitState::Retreating)
            && u.position != BattalionUnit::OFFBOARD
    };
    if !holds(target) {
        return Vec::new();
    }
    let mut group: Vec<usize> = Vec::new();
    let mut visited: Vec<HexCoord> = vec![target.position];
    let mut frontier: Vec<HexCoord> = vec![target.position];
    while let Some(pos) = frontier.pop() {
        for n in pos.neighbors() {
            if visited.contains(&n) {
                continue;
            }
            if let Some(i) = units.iter().position(|u| holds(u) && u.position == n) {
                visited.push(n);
                frontier.push(n);
                group.push(i);
            }
        }
    }
    if let Some(i) = units.iter().position(|u| u.id == target.id) {
        group.push(i);
    }
    group
}

/// The pocket's liberties (围棋气口): every passable hex adjacent to the
/// group's cells that is neither one of the group's own hexes nor held by a
/// combat-effective enemy. Zero liberties = the pocket is sealed — there is
/// no hex left to step into.
fn group_liberties(grid: &HexGrid, group: &[usize], units: &[BattalionUnit]) -> usize {
    let own: Vec<HexCoord> = group.iter().map(|&i| units[i].position).collect();
    let mut liberties: Vec<HexCoord> = Vec::new();
    for &i in group {
        for n in grid.passable_neighbors(units[i].position) {
            if own.contains(&n) || liberties.contains(&n) {
                continue;
            }
            let enemy_holds = units
                .iter()
                .any(|e| e.side != units[i].side && e.is_combat_effective() && e.position == n);
            if !enemy_holds {
                liberties.push(n);
            }
        }
    }
    liberties.len()
}

/// Does any cell of the pocket touch a combat-effective enemy? Kept as a
/// separate gate so terrain alone still never encircles anything.
fn group_has_adjacent_enemy(group: &[usize], units: &[BattalionUnit]) -> bool {
    group
        .iter()
        .any(|&i| !enemy_adjacency_sides(&units[i], units).is_empty())
}

/// §6.4 pocket rule — **Go-style liberties, judged on the whole group**:
/// the target's pocket is fully encircled when its group has no liberty at
/// all. This is the only way a group of two or more adjacent friends can be
/// encircled (the old per-unit "0 free edges" rule is this rule applied to
/// a one-unit pocket, so an isolated unit judges exactly as before).
///
/// §6.13: a pocket holding a **live HQ** never seals — the command element
/// is its morale anchor, so a surrounded force under one is simply not
/// encircled at all (kill the HQ and the pocket starts to collapse).
///
/// Below the pocket rule sit the unchanged per-unit pressure rules: an
/// ISOLATED unit (no adjacent combat-effective friend) with free ≤ 2 or an
/// opposite adjacent enemy pair is Partially encircled; an adjacent friend
/// covers against them (a contiguous line is not pocketed battalion by
/// battalion).
pub fn detect_encirclement(
    grid: &HexGrid,
    target: &BattalionUnit,
    units: &[BattalionUnit],
) -> EncirclementLevel {
    let group = pocket_group(target, units);
    if !group.is_empty()
        && group_has_adjacent_enemy(&group, units)
        && group_liberties(grid, &group, units) == 0
    {
        let hq_sustained = group
            .iter()
            .any(|&i| units[i].is_hq() && units[i].is_combat_effective());
        // A live HQ keeps the pocket an organized defense — it is not
        // surrounded at all, not even the light pressure tier. Kill the HQ
        // (org 0 or annihilated) and the very same pocket seals.
        return if hq_sustained {
            EncirclementLevel::None
        } else {
            EncirclementLevel::Full
        };
    }
    let sides = enemy_adjacency_sides(target, units);
    if sides.is_empty() || has_adjacent_friend(target, units) {
        return EncirclementLevel::None;
    }
    let free = free_edges(grid, target, units);
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

    /// Ring every passable neighbour of these pocket cells with attackers
    /// (ids from 100 up) so the pocket ends up with zero liberties.
    fn seal_pocket(g: &HexGrid, pocket: &[HexCoord], units: &mut Vec<BattalionUnit>) {
        let mut filled: Vec<HexCoord> = Vec::new();
        for c in pocket {
            for n in g.passable_neighbors(*c) {
                if pocket.contains(&n) || filled.contains(&n) {
                    continue;
                }
                filled.push(n);
                let id = 100 + units.len();
                units.push(u(id, Side::Attacker, n));
            }
        }
    }

    fn hq(id: usize, pos: HexCoord) -> BattalionUnit {
        BattalionUnit::new(
            id,
            format!("HQ{id}"),
            UnitType::Headquarters,
            Side::Defender,
            pos,
        )
    }

    #[test]
    fn a_group_without_an_hq_can_be_sealed() {
        // The reported hole: a CLUSTER of battalions used to be exempt from
        // encirclement entirely (the old rule required an isolated target),
        // so several battalions pocketed together never counted as
        // surrounded. With Go-style liberties the cluster IS the pocket.
        let g = grid();
        let a = u(0, Side::Defender, HexCoord::new(5, 5));
        let b = u(1, Side::Defender, HexCoord::new(5, 6));
        let mut units = vec![a.clone(), b.clone()];
        seal_pocket(&g, &[a.position, b.position], &mut units);
        assert_eq!(detect_encirclement(&g, &a, &units), EncirclementLevel::Full);
        assert_eq!(detect_encirclement(&g, &b, &units), EncirclementLevel::Full);
    }

    #[test]
    fn one_liberty_is_not_a_sealed_pocket() {
        // A cluster with one open hex left is not sealed — the ring must
        // close completely.
        let g = grid();
        let a = u(0, Side::Defender, HexCoord::new(5, 5));
        let b = u(1, Side::Defender, HexCoord::new(5, 6));
        let mut units = vec![a.clone(), b.clone()];
        seal_pocket(&g, &[a.position, b.position], &mut units);
        let ring_id = units
            .iter()
            .find(|x| x.side == Side::Attacker)
            .map(|x| x.id)
            .unwrap();
        units.retain(|x| x.id != ring_id);
        assert_eq!(
            detect_encirclement(&g, &a, &units),
            EncirclementLevel::None
        );
    }

    #[test]
    fn a_live_hq_keeps_the_pocket_from_sealing() {
        // §6.13: a pocket holding a LIVE HQ is not surrounded at all — the
        // command element is its morale anchor. Kill the HQ and the very
        // same pocket seals (the tactical point of hunting it).
        let g = grid();
        let a = u(0, Side::Defender, HexCoord::new(5, 5));
        let h = hq(1, HexCoord::new(5, 6));
        let mut units = vec![a.clone(), h];
        seal_pocket(&g, &[a.position, HexCoord::new(5, 6)], &mut units);
        assert_eq!(
            detect_encirclement(&g, &a, &units),
            EncirclementLevel::None
        );
        let idx = units.iter().position(|x| x.is_hq()).unwrap();
        units[idx].org = 0.0; // HQ broken — it can no longer sustain anyone
        assert_eq!(detect_encirclement(&g, &a, &units), EncirclementLevel::Full);
    }

    #[test]
    fn a_lone_hq_is_never_encircled() {
        let g = grid();
        let h = hq(0, HexCoord::new(5, 5));
        let mut units = vec![h.clone()];
        seal_pocket(&g, &[h.position], &mut units);
        assert_eq!(
            detect_encirclement(&g, &h, &units),
            EncirclementLevel::None
        );
    }
}
