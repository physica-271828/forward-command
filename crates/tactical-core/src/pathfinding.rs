//! A* pathfinding with continuous movement time (§6.2/§6.5): costs are in
//! **hours** — effective kilometres per hex (terrain × mobility class, river
//! crossing surcharge) divided by the mover's speed in km/h. ZOC delay is
//! set to 0 in the shipped defaults (a movement-feel decision); lane
//! soft-penalties and yield rules live in movement.rs.

use std::collections::BinaryHeap;
use std::collections::HashMap;

use crate::grid::HexGrid;
use crate::hex::HexCoord;
use crate::params::CombatParams;
use crate::unit::{BattalionUnit, Side};

/// Motor-class units pay this multiple of the terrain's excess-over-baseline
/// cost (§6.6): forest 1.5 km leg → 1.75 km motor, mountain
/// 3.0 → 4.0.
pub const MOTOR_TERRAIN_MULT: f32 = 1.5;

/// Hexes projected as ZOC by all combat-effective units of `side` (§6.5).
/// ZOC does not extend through impassable terrain.
pub fn zoc_hexes(grid: &HexGrid, units: &[BattalionUnit], side: Side) -> Vec<HexCoord> {
    let mut out = Vec::new();
    for u in units
        .iter()
        .filter(|u| u.side == side && u.is_combat_effective())
    {
        for n in grid.passable_neighbors(u.position) {
            out.push(n);
        }
    }
    out.sort_by_key(|h| (h.q, h.r));
    out.dedup();
    out
}

fn in_zoc(zoc: &[HexCoord], h: HexCoord) -> bool {
    zoc.binary_search_by_key(&(h.q, h.r), |x| (x.q, x.r))
        .is_ok()
}

/// Effective kilometres for `mover` to step from `from` into `to`: terrain
/// (mobility-class adjusted), river surcharge, ZOC delay (the legacy
/// `zoc_*_ap_cost` params are now read as extra effective km — they delay
/// fast units less and slow units more, which is the intended "迟滞").
pub fn step_km(
    grid: &HexGrid,
    mover: &BattalionUnit,
    enemy_zoc: &[HexCoord],
    from: HexCoord,
    to: HexCoord,
    params: &CombatParams,
) -> Option<f32> {
    let cell = grid.cell(to)?;
    if !cell.is_passable {
        return None;
    }
    let mut cost = cell
        .terrain
        .movement_cost_for(mover.unit_type.mobility_class());
    // §6.14: out-of-bounds land is passable at its terrain cost
    // ("界外陆地格行动力消耗与界内一致") but carries a hefty soft surcharge
    // so PLANNED routes (AI approaches, player standing orders) detour
    // around the ring instead of wandering off the map. Routs ignore costs
    // (BFS), so a broken unit still slips away over the boundary.
    if cell.out_of_bounds {
        cost += params.oob_step_penalty_km;
    }
    // River edge crossing surcharge (§6.6 river row: +2): only when the step
    // actually crosses a river edge — previously charged for entering any
    // cell that merely touches a river on any side.
    if let (Some(from_cell), Some(dir)) = (grid.cell(from), from.direction_to(to)) {
        if from_cell.river_edges & dir.bit() != 0 {
            cost += 2.0;
        }
    }
    let from_zoc = in_zoc(enemy_zoc, from);
    let to_zoc = in_zoc(enemy_zoc, to);
    if from_zoc && to_zoc {
        cost += params.zoc_to_zoc_ap_cost;
    } else if to_zoc {
        cost += params.zoc_entry_ap_cost;
    }
    Some(cost)
}

/// Hours for `mover` to step from `from` into `to` (§6.2: 1 hex = 1 km).
pub fn step_hours(
    grid: &HexGrid,
    mover: &BattalionUnit,
    enemy_zoc: &[HexCoord],
    from: HexCoord,
    to: HexCoord,
    params: &CombatParams,
) -> Option<f32> {
    Some(step_km(grid, mover, enemy_zoc, from, to, params)? / mover.speed_kmh.max(0.1))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct State {
    /// Heap ordering key (f = g + heuristic).
    f: f32,
    /// Actual path cost so far (g).
    g: f32,
    pos: HexCoord,
}

impl Eq for State {}

impl Ord for State {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap by f
        other
            .f
            .partial_cmp(&self.f)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for State {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Dijkstra flood: every hex reachable within `hours_budget` of travel, with
/// its cost in hours. Occupied hexes — friendly or enemy — block routing
/// entirely (§6.9 stacking: paths must go *around* units; blocking
/// is only judged at execution time).
pub fn reachable_within(
    grid: &HexGrid,
    mover: &BattalionUnit,
    units: &[BattalionUnit],
    params: &CombatParams,
    hours_budget: f32,
) -> Vec<(HexCoord, f32)> {
    let enemy_zoc = zoc_hexes(grid, units, mover.side.opponent());
    let occupied: Vec<HexCoord> = units
        .iter()
        .filter(|u| u.id != mover.id && u.is_combat_effective())
        .map(|u| u.position)
        .collect();

    let mut dist: HashMap<HexCoord, f32> = HashMap::new();
    let mut heap = BinaryHeap::new();
    dist.insert(mover.position, 0.0);
    heap.push(State {
        f: 0.0,
        g: 0.0,
        pos: mover.position,
    });

    while let Some(State { g, pos, .. }) = heap.pop() {
        if g > *dist.get(&pos).unwrap_or(&f32::MAX) + 1e-6 {
            continue;
        }
        for next in grid.passable_neighbors(pos) {
            if occupied.contains(&next) {
                continue;
            }
            let Some(step) = step_hours(grid, mover, &enemy_zoc, pos, next, params) else {
                continue;
            };
            let next_cost = g + step;
            if next_cost > hours_budget + 1e-6 {
                continue;
            }
            if next_cost + 1e-6 < *dist.get(&next).unwrap_or(&f32::MAX) {
                dist.insert(next, next_cost);
                heap.push(State {
                    f: next_cost,
                    g: next_cost,
                    pos: next,
                });
            }
        }
    }

    dist.remove(&mover.position);
    let mut out: Vec<(HexCoord, f32)> = dist.into_iter().collect();
    out.sort_by_key(|(h, _)| (h.q, h.r));
    out
}

/// A* shortest path (terrain + ZOC costs). Returns (path without start,
/// total cost in hours). Occupied hexes block routing — but the *target*
/// itself may be occupied (the occupant may march away before we
/// arrive; blocking is judged at execution, not at order time).
/// Lane spreading: hexes on another FRIENDLY standing order's
/// route pay a soft surcharge (path hex `friendly_path_penalty_km`, the
/// order's destination `friendly_dest_penalty_km`), so sequentially issued
/// orders fan out into parallel lanes instead of an accordion column. Soft
/// — a single-file corridor still works. The target hex itself is exempt
/// (same-destination orders stay legal, per the same
/// execution-time-blocking philosophy).
pub fn find_path(
    grid: &HexGrid,
    mover: &BattalionUnit,
    units: &[BattalionUnit],
    target: HexCoord,
    params: &CombatParams,
) -> Option<(Vec<HexCoord>, f32)> {
    if mover.position == target {
        return Some((Vec::new(), 0.0));
    }
    let enemy_zoc = zoc_hexes(grid, units, mover.side.opponent());
    let blocked: Vec<HexCoord> = units
        .iter()
        .filter(|u| u.id != mover.id && u.is_combat_effective())
        .map(|u| u.position)
        .collect();
    // Lane spreading: collect friendly standing-order routes as soft obstacles.
    let mut lane_penalty: HashMap<HexCoord, f32> = HashMap::new();
    for u in units
        .iter()
        .filter(|u| u.id != mover.id && u.side == mover.side && u.is_combat_effective())
    {
        if let Some(o) = &u.move_order {
            for (i, h) in o.path.iter().enumerate() {
                let p = if i + 1 == o.path.len() {
                    params.friendly_dest_penalty_km
                } else {
                    params.friendly_path_penalty_km
                };
                let e = lane_penalty.entry(*h).or_insert(0.0);
                if p > *e {
                    *e = p;
                }
            }
        }
    }

    let mut dist: HashMap<HexCoord, f32> = HashMap::new();
    let mut came: HashMap<HexCoord, HexCoord> = HashMap::new();
    let mut heap = BinaryHeap::new();
    dist.insert(mover.position, 0.0);
    heap.push(State {
        f: 0.0,
        g: 0.0,
        pos: mover.position,
    });

    while let Some(State { g, pos, .. }) = heap.pop() {
        if pos == target {
            // Reconstruct
            let mut path = vec![target];
            let mut cur = target;
            while let Some(&prev) = came.get(&cur) {
                path.push(prev);
                cur = prev;
            }
            path.reverse();
            path.remove(0); // drop start
            return Some((path, g));
        }
        if g > *dist.get(&pos).unwrap_or(&f32::MAX) + 1e-6 {
            continue;
        }
        for next in grid.passable_neighbors(pos) {
            if blocked.contains(&next) && next != target {
                continue;
            }
            let Some(mut step) = step_hours(grid, mover, &enemy_zoc, pos, next, params) else {
                continue;
            };
            if next != target {
                step += lane_penalty.get(&next).copied().unwrap_or(0.0) / mover.speed_kmh.max(0.1);
            }
            // Admissible heuristic: distance × cheapest possible step.
            let h = next.distance(target) as f32 * (1.0 / mover.speed_kmh.max(0.1));
            let next_cost = g + step;
            if next_cost + 1e-6 < *dist.get(&next).unwrap_or(&f32::MAX) {
                dist.insert(next, next_cost);
                came.insert(next, pos);
                heap.push(State {
                    f: next_cost + h,
                    g: next_cost,
                    pos: next,
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Terrain;
    use crate::unit::UnitType;

    /// 6 km/h unit: 1 plains km costs 1/6 h ≈ 0.1667.
    fn mk_unit(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        BattalionUnit::new(id, format!("U{id}"), UnitType::Cavalry, side, pos)
    }

    #[test]
    fn plains_step_is_one_km_over_speed() {
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let reach = reachable_within(&g, &mover, &[mover.clone()], &CombatParams::default(), 1.0);
        assert!(reach
            .iter()
            .any(|(h, c)| *h == HexCoord::new(3, 2) && (*c - 1.0 / 6.0).abs() < 1e-4));
    }

    #[test]
    fn forest_costs_one_and_half_km() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::Forest);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let reach = reachable_within(&g, &mover, &[mover.clone()], &CombatParams::default(), 1.0);
        let (_, c) = reach
            .iter()
            .find(|(h, _)| *h == HexCoord::new(3, 2))
            .unwrap();
        assert!((*c - 1.5 / 6.0).abs() < 1e-4);
    }

    #[test]
    fn motor_pays_extra_offroad() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::Forest);
        let mover = BattalionUnit::new(
            0,
            "M",
            UnitType::Motorized,
            Side::Attacker,
            HexCoord::new(2, 2),
        );
        let reach = reachable_within(&g, &mover, &[mover.clone()], &CombatParams::default(), 1.0);
        let (_, c) = reach
            .iter()
            .find(|(h, _)| *h == HexCoord::new(3, 2))
            .unwrap();
        // Forest motor: 1 + (1.5-1)×1.5 = 1.75 km at 12 km/h.
        assert!((*c - 1.75 / 12.0).abs() < 1e-4, "cost {c}");
    }

    #[test]
    fn zoc_entry_adds_delay_km() {
        // ZOC surcharges are zeroed in the shipped defaults (a movement-feel
        // decision); this test enables them explicitly to cover the mechanism.
        let p = CombatParams {
            zoc_entry_ap_cost: 1.0,
            ..Default::default()
        };
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let enemy = mk_unit(1, Side::Defender, HexCoord::new(4, 2));
        let units = vec![mover.clone(), enemy];
        let reach = reachable_within(&g, &mover, &units, &p, 1.0);
        // (3,2) is adjacent to the enemy -> inside ZOC: 1.0 terrain + 1.0 entry
        let (_, c) = reach
            .iter()
            .find(|(h, _)| *h == HexCoord::new(3, 2))
            .unwrap();
        assert!((*c - 2.0 / 6.0).abs() < 1e-4);
    }

    #[test]
    fn enemy_hex_is_impassable() {
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let enemy = mk_unit(1, Side::Defender, HexCoord::new(3, 2));
        let units = vec![mover.clone(), enemy];
        let reach = reachable_within(&g, &mover, &units, &CombatParams::default(), 1.0);
        assert!(!reach.iter().any(|(h, _)| *h == HexCoord::new(3, 2)));
    }

    #[test]
    fn path_avoids_mountain_detour() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        // Wall of mountains across q=3 except at r=9
        for r in 0..9 {
            g.set_terrain(HexCoord::new(3, r), Terrain::Mountain);
        }
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 4));
        let (path, cost) = find_path(
            &g,
            &mover,
            &[mover.clone()],
            HexCoord::new(4, 4),
            &CombatParams::default(),
        )
        .unwrap();
        assert!(!path.is_empty());
        assert_eq!(*path.last().unwrap(), HexCoord::new(4, 4));
        assert!(cost > 2.0 / 6.0); // paid for the mountain
    }

    #[test]
    fn river_edge_surcharge_only_when_crossed() {
        use crate::hex::HexDirection;
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        // River runs along the (3,2)-(4,2) shared edge: mirrored bits as the
        // generator writes them.
        g.cell_mut(HexCoord::new(3, 2)).unwrap().river_edges = HexDirection::E.bit();
        g.cell_mut(HexCoord::new(4, 2)).unwrap().river_edges = HexDirection::W.bit();
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let p = CombatParams::default();
        let zoc = [];
        // Crossing the river edge: 1.0 plains + 2.0 surcharge.
        let c = step_km(
            &g,
            &mover,
            &zoc,
            HexCoord::new(3, 2),
            HexCoord::new(4, 2),
            &p,
        )
        .unwrap();
        assert!((c - 3.0).abs() < 1e-4, "crossed {c}");
        let c = step_km(
            &g,
            &mover,
            &zoc,
            HexCoord::new(4, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 3.0).abs() < 1e-4, "crossed back {c}");
        // Entering the same river-edged cell from a dry side: no surcharge
        // (used to charge +2 for any entry into a river-adjacent hex).
        let c = step_km(
            &g,
            &mover,
            &zoc,
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.0).abs() < 1e-4, "dry entry {c}");
    }

    #[test]
    fn river_terrain_is_costly_ford() {
        // Full-hex water is passable at 3× — a ford,
        // not a road; pathfinding should strongly prefer dry ground.
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::River);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &mover,
            &[],
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &CombatParams::default(),
        )
        .unwrap();
        assert!((c - 3.0).abs() < 1e-4, "ford {c}");
    }

    #[test]
    fn friendly_hex_reroutes_but_target_may_be_occupied() {
        // Paths route AROUND friendly units, but an order onto a
        // friendly-occupied hex is legal — the occupant may leave before we
        // arrive (blocking is judged at execution).
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let friend = mk_unit(1, Side::Attacker, HexCoord::new(3, 2));
        let units = vec![mover.clone(), friend];
        let p = CombatParams::default();
        // Detour around the friend.
        let (path, _) = find_path(&g, &mover, &units, HexCoord::new(4, 2), &p).unwrap();
        assert!(!path.contains(&HexCoord::new(3, 2)), "path {path:?}");
        assert_eq!(*path.last().unwrap(), HexCoord::new(4, 2));
        // Order onto the friend's own hex is allowed.
        let (path, _) = find_path(&g, &mover, &units, HexCoord::new(3, 2), &p).unwrap();
        assert_eq!(*path.last().unwrap(), HexCoord::new(3, 2));
    }

    #[test]
    fn friendly_order_lanes_spread() {
        // A friendly standing order's route is a soft obstacle —
        // the follower fans out into the parallel lane instead of planning
        // through the leader's track.
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let p = CombatParams::default();
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let mut leader = mk_unit(1, Side::Attacker, HexCoord::new(2, 3));
        let target = HexCoord::new(6, 2);

        // Control: no standing order → the direct row-2 route wins.
        let units = vec![mover.clone(), leader.clone()];
        let (path, _) = find_path(&g, &mover, &units, target, &p).unwrap();
        assert_eq!(
            path.first().copied(),
            Some(HexCoord::new(3, 2)),
            "direct: {path:?}"
        );
        assert_eq!(path.len(), 4);

        // Leader ordered along row 2 (through (3,2),(4,2),(5,2) → dest (6,2)):
        // the follower must take the 5-step parallel lane instead (three
        // penalized path hexes outweigh one extra step).
        leader.move_order = Some(crate::unit::MoveOrder {
            path: vec![
                HexCoord::new(3, 2),
                HexCoord::new(4, 2),
                HexCoord::new(5, 2),
                HexCoord::new(6, 2),
            ],
            hours: 0.0,
        });
        let units = vec![mover.clone(), leader];
        let (path, _) = find_path(&g, &mover, &units, target, &p).unwrap();
        assert_ne!(
            path.first().copied(),
            Some(HexCoord::new(3, 2)),
            "fanned out: {path:?}"
        );
        assert_eq!(path.len(), 5, "parallel lane costs one extra hex: {path:?}");
        assert_eq!(path.last().copied(), Some(target), "destination kept");
    }

    #[test]
    fn reachable_excludes_and_reroutes_friendly() {
        // reachable_within aligns with find_path — friendly hexes
        // are neither end points nor pass-throughs.
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let friend = mk_unit(1, Side::Attacker, HexCoord::new(3, 2));
        let units = vec![mover.clone(), friend];
        let reach = reachable_within(&g, &mover, &units, &CombatParams::default(), 1.0);
        assert!(!reach.iter().any(|(h, _)| *h == HexCoord::new(3, 2)));
        // (4,2) is still reachable via the detour around the friend.
        assert!(reach.iter().any(|(h, _)| *h == HexCoord::new(4, 2)));
    }

    #[test]
    fn oob_step_carries_leaving_surcharge_but_stays_routable() {
        // §6.14: out-of-bounds land is passable at its terrain
        // cost PLUS the soft leaving surcharge ("界外陆地格行动力消耗与界内
        // 一致" for execution; the surcharge only steers PLANNING), so
        // planned routes detour around the ring instead of cutting through.
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.cell_mut(HexCoord::new(3, 2)).unwrap().out_of_bounds = true;
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let p = CombatParams::default();
        let c = step_km(
            &g,
            &mover,
            &[],
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!(
            (c - (1.0 + p.oob_step_penalty_km)).abs() < 1e-4,
            "oob step = terrain + surcharge, got {c}"
        );
        // Soft, not a wall: the hex is still reachable…
        let reach = reachable_within(&g, &mover, &[mover.clone()], &p, 100.0);
        assert!(reach.iter().any(|(h, _)| *h == HexCoord::new(3, 2)));
        // …but A* plans around it for an in-bounds destination.
        let (path, _) = find_path(&g, &mover, &[mover.clone()], HexCoord::new(4, 2), &p).unwrap();
        assert!(
            !path.contains(&HexCoord::new(3, 2)),
            "detours around the ring: {path:?}"
        );
    }
}
