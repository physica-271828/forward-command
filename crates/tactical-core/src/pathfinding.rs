//! A* pathfinding with continuous movement time (§6.2/§6.5): costs are in
//! **hours** — effective kilometres per hex (terrain × mobility class, the
//! relief price of a surfaced hex, river crossing surcharge) divided by the
//! mover's speed in km/h. Enemy CONTROL (控制区, §6.5) scales the step by
//! the pressure of the entered hex's covering units; lane soft-penalties
//! and yield rules live in movement.rs.

use std::collections::BinaryHeap;
use std::collections::HashMap;

use crate::grid::{GridCell, HexGrid};
use crate::hex::HexCoord;
use crate::params::CombatParams;
use crate::terrain::Terrain;
use crate::unit::{BattalionUnit, MobilityClass, Side, UnitState};

/// Motor-class units pay this multiple of the terrain's excess-over-baseline
/// cost (§6.6): forest 1.5 km leg → 1.75 km motor, mountain
/// 3.0 → 4.0.
pub const MOTOR_TERRAIN_MULT: f32 = 1.5;

/// Movement cost of a bridge-deck hex (§4.2 bridges): flat effective
/// kilometres for every mobility class — movement-only, no combat modifiers.
pub const BRIDGE_MOVE_COST_KM: f32 = 1.0;

/// §6.5 控制区 (control zone): the hexes covered by `side`'s fighting
/// units, each carrying the CONTROL PRESSURE that slows enemy movement —
/// Σ `org_ratio × strength_ratio` over the covering units, so a spent
/// battalion throttles far less than a fresh one. Only units that can
/// hold ground project control: routing or ineffective units, HQs and
/// indirect artillery (tube + rocket) are skipped — a rocket battery
/// cannot close a line. Impassable terrain breaks the cover (only
/// passable neighbours are projected).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ControlField {
    /// `(hex, pressure)` sorted by `(q, r)` — binary-search lookup.
    entries: Vec<(HexCoord, f32)>,
}

impl ControlField {
    /// Total control pressure over `h`; 0.0 when the hex is uncovered.
    pub fn pressure(&self, h: HexCoord) -> f32 {
        self.entries
            .binary_search_by_key(&(h.q, h.r), |&(x, _)| (x.q, x.r))
            .map(|i| self.entries[i].1)
            .unwrap_or(0.0)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build the control field `side`'s units project (§6.5).
pub fn control_field(grid: &HexGrid, units: &[BattalionUnit], side: Side) -> ControlField {
    let mut acc: HashMap<HexCoord, f32> = HashMap::new();
    for u in units.iter().filter(|u| {
        u.side == side && u.is_combat_effective() && !u.is_hq() && !u.is_indirect_artillery()
    }) {
        let pressure = u.org_ratio() * u.strength_ratio();
        for n in grid.passable_neighbors(u.position) {
            *acc.entry(n).or_insert(0.0) += pressure;
        }
    }
    let mut entries: Vec<(HexCoord, f32)> = acc.into_iter().collect();
    entries.sort_by_key(|&(h, _)| (h.q, h.r));
    ControlField { entries }
}

/// §6.6 price of ENTERING `cell` for one mobility class — the single place
/// the road / urban / off-road branches live, so the A*, the movement
/// executor and the hover card can never drift apart.
///
/// A surfaced hex (road or rail, non-river) costs the RELIEF price whatever
/// the ground under it says — pavement flattens the terrain type, and the
/// motor excess is waived, so vehicles belong on roads. A FORD (road across
/// River without a deck) carries no bonus — it prices like open river.
/// Urban streets are roads by definition; every other hex pays the off-road
/// surcharge, legs flat, motor amplified through its excess multiplier.
/// `terrain_mult` is the mover's engineer terrain craft (§6.3/§6.6): it
/// scales the off-road penalty only — road and street prices never move.
fn enter_move_km(
    cell: &GridCell,
    class: MobilityClass,
    params: &CombatParams,
    terrain_mult: f32,
) -> f32 {
    if cell.terrain == Terrain::River && cell.bridge {
        BRIDGE_MOVE_COST_KM
    } else if cell.has_road() && cell.terrain != Terrain::River {
        road_move_km(cell, params)
    } else if cell.terrain == Terrain::Urban {
        cell.terrain.movement_cost_for(class)
    } else {
        off_road_move_km(cell.terrain, class, params, terrain_mult)
    }
}

/// §6.6 road network: `road_base_km` on flat ground plus `road_elev_step_km`
/// per POSITIVE elevation level — hill roads 1.3/1.6, mountain roads
/// 1.9/2.2 at the shipped defaults. Marsh and riverbeds sit at −1 and clamp
/// to the base price (a causeway is as good as the plain).
fn road_move_km(cell: &GridCell, params: &CombatParams) -> f32 {
    params.road_base_km + params.road_elev_step_km * cell.elevation.max(0) as f32
}

/// §6.6 off-road: the terrain row plus the flat `off_road_surcharge_km` for
/// legs; motor rides the same sum through its excess multiplier.
/// `terrain_mult` (engineer terrain craft, §6.3) scales the EXCESS over the
/// 1 km baseline — the terrain penalty shrinks, the baseline kilometre
/// itself never gets cheaper.
fn off_road_move_km(
    terrain: Terrain,
    class: MobilityClass,
    params: &CombatParams,
    terrain_mult: f32,
) -> f32 {
    let base = terrain.movement_cost() + params.off_road_surcharge_km;
    match class {
        MobilityClass::Leg => 1.0 + (base - 1.0) * terrain_mult,
        MobilityClass::Motor => 1.0 + (base - 1.0) * MOTOR_TERRAIN_MULT * terrain_mult,
    }
}

/// The hover card's generic LEG row for entering `cell` — lockstep with
/// [`step_km`] through [`enter_move_km`] for an attachment-free mover
/// (engineer terrain craft is per-battalion and never shown here).
pub fn cell_move_km(cell: &GridCell, params: &CombatParams) -> f32 {
    enter_move_km(cell, MobilityClass::Leg, params, 1.0)
}

/// Effective kilometres for `mover` to step from `from` into `to`: the
/// entered hex's price (terrain × mobility class, road network §6.6), river
/// surcharge, out-of-bounds leaving fee, and the §6.5 CONTROL slowdown —
/// entering a hex covered by the enemy's fighting units scales the step by
/// `1 + control_slow_k × pressure`, pressure being the covering units'
/// Σ org_ratio × strength_ratio.
pub fn step_km(
    grid: &HexGrid,
    mover: &BattalionUnit,
    control: &ControlField,
    from: HexCoord,
    to: HexCoord,
    params: &CombatParams,
) -> Option<f32> {
    let cell = grid.cell(to)?;
    if !cell.is_passable {
        return None;
    }
    let class = mover.unit_type.mobility_class();
    let mut cost = enter_move_km(cell, class, params, mover.terrain_cost_mult);
    // §6.14: out-of-bounds land is passable at its terrain cost
    // ("界外陆地格行动力消耗与界内一致") but carries a hefty soft surcharge
    // so PLANNED routes (AI approaches, player standing orders) detour
    // around the ring instead of wandering off the map. Routs ignore costs
    // (BFS), so a broken unit still slips away over the boundary.
    if cell.out_of_bounds {
        cost += params.oob_step_penalty_km;
    }
    // River edge crossing surcharge (§6.6 river row): only when the step
    // actually crosses a river edge — previously charged for entering any
    // cell that merely touches a river on any side. Uniform for every
    // river: a wide band is a wider stretch of the same ford rule, not a
    // pricier one. A bridged edge (§4.2) waives the surcharge.
    //
    // Either endpoint may be the one marking the shared edge: the map
    // generator writes the mirrored bits only for in-province↔in-province
    // edges, so a step between the province and the §6.14 out-of-province
    // ring carries the bit on the in-province side alone. Reading both
    // directions keeps leaving and re-entering a border river at the same
    // price.
    if let (Some(from_cell), Some(dir)) = (grid.cell(from), from.direction_to(to)) {
        let back = dir.opposite();
        let (to_river, to_bridge) = grid
            .cell(to)
            .map(|c| (c.river_edges, c.bridge_edges))
            .unwrap_or((0, 0));
        let marked = from_cell.river_edges & dir.bit() != 0 || to_river & back.bit() != 0;
        let bridged = from_cell.bridge_edges & dir.bit() != 0 || to_bridge & back.bit() != 0;
        if marked && !bridged {
            cost += params.river_edge_surcharge_km;
        }
    }
    // §6.5 control slowdown: entering a covered hex scales the step by the
    // covering units' org × strength pressure — a fresh infantry battalion
    // roughly halves the pace, a spent one barely dents it. A ROUT is
    // exempt: a retreating unit is neither slowed nor covered (§6.8), so
    // disengaging stays cheap and routs keep their BFS escape.
    if mover.state != UnitState::Retreating {
        let pressure = control.pressure(to);
        if pressure > 0.0 {
            cost *= 1.0 + params.control_slow_k * pressure;
        }
    }
    Some(cost)
}

/// Hours for `mover` to step from `from` into `to` (§6.2: 1 hex = 1 km).
pub fn step_hours(
    grid: &HexGrid,
    mover: &BattalionUnit,
    control: &ControlField,
    from: HexCoord,
    to: HexCoord,
    params: &CombatParams,
) -> Option<f32> {
    Some(step_km(grid, mover, control, from, to, params)? / mover.speed_kmh.max(0.1))
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
    let control = control_field(grid, units, mover.side.opponent());
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
            let Some(step) = step_hours(grid, mover, &control, pos, next, params) else {
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

/// A* heuristic for one step toward `target`: distance × the cheapest
/// possible step. The cheapest step is the flat road price
/// ([`CombatParams::road_base_km`]) — every `enter_move_km` branch starts
/// from that baseline and surcharges only ever add to it, so a shorter
/// estimate would make the search return non-optimal paths. `road_base_km`
/// is a live tuning knob, so the baseline is read from the params instead
/// of being hardcoded.
fn heuristic_hours(
    mover: &BattalionUnit,
    from: HexCoord,
    target: HexCoord,
    params: &CombatParams,
) -> f32 {
    from.distance(target) as f32 * (params.road_base_km / mover.speed_kmh.max(0.1))
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
    let control = control_field(grid, units, mover.side.opponent());
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
            let Some(mut step) = step_hours(grid, mover, &control, pos, next, params) else {
                continue;
            };
            if next != target {
                step += lane_penalty.get(&next).copied().unwrap_or(0.0) / mover.speed_kmh.max(0.1);
            }
            // Admissible heuristic: distance × the cheapest possible step
            // (`road_base_km` bounds every `enter_move_km` branch from
            // below — terrain rows and surcharges only ever add to it).
            let h = heuristic_hours(mover, next, target, params);
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
    use crate::unit::{Attrs, UnitType};

    /// 6 km/h unit: 1 plains km costs 1/6 h ≈ 0.1667.
    fn mk_unit(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        BattalionUnit::new(id, format!("U{id}"), UnitType::Cavalry, side, pos)
    }

    /// No enemy cover on the test map — an empty §6.5 control field.
    fn no_control() -> ControlField {
        ControlField::default()
    }

    #[test]
    fn plains_step_pays_off_road_surcharge() {
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let reach = reachable_within(&g, &mover, &[mover.clone()], &CombatParams::default(), 1.0);
        // Off-road legs: 1.0 base + 0.5 surcharge (§4.2 road network).
        assert!(reach
            .iter()
            .any(|(h, c)| *h == HexCoord::new(3, 2) && (*c - 1.5 / 6.0).abs() < 1e-4));
    }

    #[test]
    fn forest_costs_base_plus_off_road_surcharge() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::Forest);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let reach = reachable_within(&g, &mover, &[mover.clone()], &CombatParams::default(), 1.0);
        let (_, c) = reach
            .iter()
            .find(|(h, _)| *h == HexCoord::new(3, 2))
            .unwrap();
        assert!((*c - 2.0 / 6.0).abs() < 1e-4);
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
        // Forest motor: 1 + (1.5+0.5−1)×1.5 = 2.5 km at 12 km/h — the
        // off-road surcharge rides inside the motor excess multiplier.
        assert!((*c - 2.5 / 12.0).abs() < 1e-4, "cost {c}");
    }

    #[test]
    fn control_pressure_scales_with_org_and_strength() {
        // §6.5 控制区: a fresh battalion covers its neighbours with
        // pressure = org_ratio × str_ratio = 1.0; a spent one throttles far
        // less (both ratios halved → 0.25).
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mut enemy = mk_unit(1, Side::Defender, HexCoord::new(4, 2));
        let field = control_field(&g, &[enemy.clone()], Side::Defender);
        assert!((field.pressure(HexCoord::new(3, 2)) - 1.0).abs() < 1e-4);
        assert_eq!(field.pressure(HexCoord::new(2, 2)), 0.0, "two hexes away");
        enemy.org *= 0.5;
        enemy.strength *= 0.5;
        let field = control_field(&g, &[enemy], Side::Defender);
        assert!((field.pressure(HexCoord::new(3, 2)) - 0.25).abs() < 1e-4);
    }

    #[test]
    fn indirect_artillery_and_hqs_project_no_control() {
        // Tube + rocket batteries cannot close a line, and an HQ is not a
        // fighting unit: neither covers ground.
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let gun = BattalionUnit::new(
            1,
            "G",
            UnitType::ArtilleryBrigade,
            Side::Defender,
            HexCoord::new(4, 2),
        );
        let rockets = BattalionUnit::new(
            2,
            "R",
            UnitType::RocketArtillery,
            Side::Defender,
            HexCoord::new(4, 4),
        );
        let mut hq = mk_unit(3, Side::Defender, HexCoord::new(4, 6));
        hq.attrs |= Attrs::HQ;
        let field = control_field(&g, &[gun, rockets, hq], Side::Defender);
        assert!(field.is_empty(), "guns and HQs cover nothing");
    }

    #[test]
    fn retreating_units_are_exempt_both_ways() {
        // §6.8: a routing unit neither projects control nor suffers it —
        // the rout escape keeps its BFS pricing.
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mut runner = mk_unit(1, Side::Defender, HexCoord::new(4, 2));
        runner.state = UnitState::Retreating;
        assert!(control_field(&g, &[runner], Side::Defender).is_empty());
        let mut mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        mover.state = UnitState::Retreating;
        let cover = control_field(
            &g,
            &[mk_unit(1, Side::Defender, HexCoord::new(4, 2))],
            Side::Defender,
        );
        let p = CombatParams::default();
        let open = step_km(
            &g,
            &mover,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        let covered = step_km(
            &g,
            &mover,
            &cover,
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((open - covered).abs() < 1e-6, "a rout is never slowed");
    }

    #[test]
    fn a_fresh_battalion_halves_the_pace_inside_its_cover() {
        // Entering (3,2), covered at pressure 1.0 with k = 1: the 1.5 km
        // off-road leg is scaled to 3.0 km. Stepping back OUT is priced
        // normally — disengaging is never punished.
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let field = control_field(
            &g,
            &[mk_unit(1, Side::Defender, HexCoord::new(4, 2))],
            Side::Defender,
        );
        let p = CombatParams::default();
        let covered = step_km(
            &g,
            &mover,
            &field,
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((covered - 3.0).abs() < 1e-4, "covered step {covered}");
        let escape = step_km(
            &g,
            &mover,
            &field,
            HexCoord::new(3, 2),
            HexCoord::new(2, 2),
            &p,
        )
        .unwrap();
        assert!((escape - 1.5).abs() < 1e-4, "escape step {escape}");
    }

    #[test]
    fn control_scaling_reaches_the_reachability_probe() {
        // reachable_within pays the same control-scaled prices — the hover
        // ring and the AI's one-hex probes cannot drift from the executor.
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let units = vec![
            mover.clone(),
            mk_unit(1, Side::Defender, HexCoord::new(4, 2)),
        ];
        let reach = reachable_within(&g, &mover, &units, &CombatParams::default(), 1.0);
        let (_, c) = reach
            .iter()
            .find(|(h, _)| *h == HexCoord::new(3, 2))
            .unwrap();
        // 1.5 km off-road × (1 + 1.0) = 3.0 km at 6 km/h.
        assert!((*c - 3.0 / 6.0).abs() < 1e-4, "cost {c}");
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
        let zoc = ControlField::default();
        // Crossing the river edge: 1.5 plains off-road + 2.0 surcharge.
        let c = step_km(
            &g,
            &mover,
            &zoc,
            HexCoord::new(3, 2),
            HexCoord::new(4, 2),
            &p,
        )
        .unwrap();
        assert!((c - 3.5).abs() < 1e-4, "crossed {c}");
        let c = step_km(
            &g,
            &mover,
            &zoc,
            HexCoord::new(4, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 3.5).abs() < 1e-4, "crossed back {c}");
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
        assert!((c - 1.5).abs() < 1e-4, "dry entry {c}");
    }

    #[test]
    fn river_terrain_is_costly_ford() {
        // Full-hex water is passable at 3× plus the off-road surcharge — a
        // ford, not a road; pathfinding should strongly prefer dry ground.
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::River);
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &mover,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &CombatParams::default(),
        )
        .unwrap();
        assert!((c - 3.5).abs() < 1e-4, "ford {c}");
    }

    #[test]
    fn river_band_never_changes_the_ford_rows() {
        use crate::hex::HexDirection;
        // Width bands are pure geometry: a Medium/Large river
        // is a WIDER strip of the same ford, never a pricier one — the
        // per-hex cost must not depend on the band.
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let p = CombatParams::default();
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::River);
        for band in [
            crate::terrain::RiverBand::Small,
            crate::terrain::RiverBand::Medium,
            crate::terrain::RiverBand::Large,
        ] {
            g.cell_mut(HexCoord::new(3, 2)).unwrap().river_band = band;
            let c = step_km(
                &g,
                &mover,
                &no_control(),
                HexCoord::new(2, 2),
                HexCoord::new(3, 2),
                &p,
            )
            .unwrap();
            assert!((c - 3.5).abs() < 1e-4, "{band:?} ford {c}");
        }
        // Edge crossing: same uniform surcharge for every band.
        let mut g2 = HexGrid::new(10, 10, Terrain::Plains);
        g2.cell_mut(HexCoord::new(3, 2)).unwrap().river_edges = HexDirection::E.bit();
        g2.cell_mut(HexCoord::new(4, 2)).unwrap().river_edges = HexDirection::W.bit();
        g2.cell_mut(HexCoord::new(3, 2)).unwrap().river_band = crate::terrain::RiverBand::Large;
        let c = step_km(
            &g2,
            &mover,
            &no_control(),
            HexCoord::new(3, 2),
            HexCoord::new(4, 2),
            &p,
        )
        .unwrap();
        assert!((c - 3.5).abs() < 1e-4, "large-band edge {c}");
    }

    /// 12 km/h motor unit for the mobility-class tier checks.
    fn mk_motor(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        BattalionUnit::new(id, format!("M{id}"), UnitType::Motorized, side, pos)
    }

    #[test]
    fn road_and_rail_price_by_relief_not_terrain() {
        // §6.6 road network: a surfaced hex costs the RELIEF price for every
        // class whatever the ground under it says — pavement flattens the
        // terrain type (forest road = plains road = 1.0 here), the motor
        // excess is waived, and relief ≤ 0 (marsh/riverbed) clamps to base.
        let p = CombatParams::default();
        for (flag, terrain) in [
            ("road", Terrain::Forest),
            ("rail", Terrain::Forest),
            ("road", Terrain::Desert),
            ("road", Terrain::Jungle),
            ("road", Terrain::Marsh),
        ] {
            let mut g = HexGrid::new(10, 10, Terrain::Plains);
            g.set_terrain(HexCoord::new(3, 2), terrain);
            let cell = g.cell_mut(HexCoord::new(3, 2)).unwrap();
            if flag == "road" {
                cell.road = true;
            } else {
                cell.rail = true;
            }
            let leg = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
            let c = step_km(
                &g,
                &leg,
                &no_control(),
                HexCoord::new(2, 2),
                HexCoord::new(3, 2),
                &p,
            )
            .unwrap();
            assert!((c - 1.0).abs() < 1e-4, "{flag} on {terrain:?} leg {c}");
            let mot = mk_motor(1, Side::Attacker, HexCoord::new(2, 2));
            let c = step_km(
                &g,
                &mot,
                &no_control(),
                HexCoord::new(2, 2),
                HexCoord::new(3, 2),
                &p,
            )
            .unwrap();
            assert!((c - 1.0).abs() < 1e-4, "{flag} on {terrain:?} motor {c}");
        }
    }

    #[test]
    fn road_price_rises_with_positive_elevation_only() {
        // The only road surcharge left is relief: road_base_km + step ×
        // max(0, elevation) — hill roads 1.3/1.6, mountain roads 1.9/2.2.
        let p = CombatParams::default();
        for (terrain, elev, want) in [
            (Terrain::Hills, 1, 1.3),
            (Terrain::Hills, 2, 1.6),
            (Terrain::Mountain, 3, 1.9),
            (Terrain::Mountain, 4, 2.2),
        ] {
            let mut g = HexGrid::new(10, 10, Terrain::Plains);
            g.set_terrain(HexCoord::new(3, 2), terrain);
            let cell = g.cell_mut(HexCoord::new(3, 2)).unwrap();
            cell.elevation = elev;
            cell.road = true;
            let leg = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
            let c = step_km(
                &g,
                &leg,
                &no_control(),
                HexCoord::new(2, 2),
                HexCoord::new(3, 2),
                &p,
            )
            .unwrap();
            assert!((c - want).abs() < 1e-4, "{terrain:?} elev {elev} leg {c}");
            let mot = mk_motor(1, Side::Attacker, HexCoord::new(2, 2));
            let c = step_km(
                &g,
                &mot,
                &no_control(),
                HexCoord::new(2, 2),
                HexCoord::new(3, 2),
                &p,
            )
            .unwrap();
            assert!((c - want).abs() < 1e-4, "{terrain:?} elev {elev} motor {c}");
        }
    }

    #[test]
    fn hover_row_matches_the_leg_step_price() {
        // The hover card shows `cell_move_km` — the generic LEG row. It must
        // equal step_km for a leg mover on every branch: bridge deck, road
        // (flat + relief), ford, urban street, open off-road, marsh road.
        fn put(g: &mut HexGrid, h: HexCoord, terrain: Terrain, elev: i32) {
            g.set_terrain(h, terrain);
            g.cell_mut(h).unwrap().elevation = elev;
        }
        let p = CombatParams::default();
        let mut g = HexGrid::new(12, 2, Terrain::Plains);
        put(&mut g, HexCoord::new(3, 1), Terrain::River, -1);
        g.cell_mut(HexCoord::new(3, 1)).unwrap().bridge = true;
        put(&mut g, HexCoord::new(4, 1), Terrain::Forest, 0);
        put(&mut g, HexCoord::new(5, 1), Terrain::Hills, 2);
        put(&mut g, HexCoord::new(6, 1), Terrain::River, -1);
        put(&mut g, HexCoord::new(7, 1), Terrain::Urban, 0);
        put(&mut g, HexCoord::new(8, 1), Terrain::Forest, 0);
        put(&mut g, HexCoord::new(9, 1), Terrain::Marsh, -1);
        // Flat road, hill road, marsh road — and a ford (road, no deck).
        for h in [
            HexCoord::new(4, 1),
            HexCoord::new(5, 1),
            HexCoord::new(6, 1),
            HexCoord::new(9, 1),
        ] {
            g.cell_mut(h).unwrap().road = true;
        }
        let leg = mk_unit(0, Side::Attacker, HexCoord::new(2, 1));
        for q in 3..=9 {
            let to = HexCoord::new(q, 1);
            let step = step_km(&g, &leg, &no_control(), HexCoord::new(q - 1, 1), to, &p).unwrap();
            let hover = cell_move_km(g.cell(to).unwrap(), &p);
            assert!(
                (step - hover).abs() < 1e-4,
                "hover {hover} != step {step} at {to:?}"
            );
        }
    }

    #[test]
    fn motor_off_road_tiers_by_terrain() {
        // Frozen tier table: legs pay +0.5 flat; motor pays the surcharge
        // amplified through the excess multiplier — rougher ground, more pain.
        let p = CombatParams::default();
        let mot = mk_motor(0, Side::Attacker, HexCoord::new(2, 2));
        // Plains motor: 1 + (1.0+0.5−1)×1.5 = 1.75.
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let c = step_km(
            &g,
            &mot,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.75).abs() < 1e-4, "plains motor {c}");
        // Mountain motor: 1 + (3.0+0.5−1)×1.5 = 4.75.
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::Mountain);
        let c = step_km(
            &g,
            &mot,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 4.75).abs() < 1e-4, "mountain motor {c}");
    }

    #[test]
    fn urban_streets_are_roads_by_definition() {
        // Urban never pays the off-road surcharge (the city IS road), and
        // the motor excess stays exactly as the legacy terrain table.
        let p = CombatParams::default();
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::Urban);
        let leg = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &leg,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.2).abs() < 1e-4, "urban leg {c}");
        let mot = mk_motor(1, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &mot,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.3).abs() < 1e-4, "urban motor {c}");
    }

    #[test]
    fn engineer_host_shaves_the_off_road_penalty() {
        // §6.3/§6.6 engineer terrain craft: the EXCESS over the 1 km
        // baseline scales ×0.75 — forest leg 2.0 → 1.75, forest motor
        // 2.5 → 2.125 — while the paved price over the same ground and
        // an engineer-free mover never move.
        let p = CombatParams::default();
        let engineers = || crate::unit::SupportAttachment {
            kind: crate::unit::SupportKind::Engineer,
            name: "Eng".to_string(),
            token: "engineer".to_string(),
        };
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::Forest);
        let mut leg = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        leg.attach(engineers());
        let c = step_km(
            &g,
            &leg,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.75).abs() < 1e-4, "forest leg with engineers {c}");
        let mut mot = mk_motor(1, Side::Attacker, HexCoord::new(2, 2));
        mot.attach(engineers());
        let c = step_km(
            &g,
            &mot,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 2.125).abs() < 1e-4, "forest motor with engineers {c}");
        // Detach restores the full price.
        mot.detach(0);
        let c = step_km(
            &g,
            &mot,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 2.5).abs() < 1e-4, "forest motor after detach {c}");
        // A road over the same forest prices the pavement, engineers or not.
        g.cell_mut(HexCoord::new(3, 2)).unwrap().road = true;
        let c = step_km(
            &g,
            &leg,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.0).abs() < 1e-4, "forest road with engineers {c}");
    }

    #[test]
    fn bridge_deck_is_flat_one_km_for_every_class() {
        let p = CombatParams::default();
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::River);
        g.cell_mut(HexCoord::new(3, 2)).unwrap().bridge = true;
        let leg = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &leg,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.0).abs() < 1e-4, "bridge leg {c}");
        let mot = mk_motor(1, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &mot,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.0).abs() < 1e-4, "bridge motor {c}");
    }

    #[test]
    fn ford_carries_no_road_bonus() {
        // A ford (road across River, no deck) prices like open river:
        // leg 3.0 + 0.5 surcharge, motor amplified through the excess.
        let p = CombatParams::default();
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 2), Terrain::River);
        g.cell_mut(HexCoord::new(3, 2)).unwrap().road = true;
        let leg = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &leg,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 3.5).abs() < 1e-4, "ford leg {c}");
        let mot = mk_motor(1, Side::Attacker, HexCoord::new(2, 2));
        let c = step_km(
            &g,
            &mot,
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!((c - 4.75).abs() < 1e-4, "ford motor {c}");
    }

    #[test]
    fn heuristic_stays_below_every_real_step() {
        // The A* estimate must never exceed the cheapest step actually
        // taken, whatever the terrain / mobility class / surcharge knobs
        // say — otherwise the search can return a non-optimal path.
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let mot = mk_motor(1, Side::Attacker, HexCoord::new(2, 2));
        let mut params = CombatParams::default();
        for base in [0.5f32, 1.0, 1.5] {
            params.road_base_km = base;
            for terrain in [
                Terrain::Plains,
                Terrain::Forest,
                Terrain::Hills,
                Terrain::Mountain,
                Terrain::Marsh,
                Terrain::Jungle,
            ] {
                for mover in [&mover, &mot] {
                    let mut g = HexGrid::new(10, 10, Terrain::Plains);
                    g.set_terrain(HexCoord::new(3, 2), terrain);
                    let real = step_hours(
                        &g,
                        mover,
                        &no_control(),
                        HexCoord::new(2, 2),
                        HexCoord::new(3, 2),
                        &params,
                    )
                    .unwrap();
                    let h =
                        heuristic_hours(mover, HexCoord::new(2, 2), HexCoord::new(3, 2), &params);
                    assert!(
                        h <= real + 1e-6,
                        "heuristic {h} above the real step {real} ({terrain:?}, base {base})"
                    );
                }
            }
        }
    }

    #[test]
    fn heuristic_scales_with_the_road_baseline() {
        // Tuning `road_base_km` down must move the estimate with it: the
        // old hardcoded 1.0 would over-estimate and could prune the
        // optimal path.
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let mut params = CombatParams::default();
        let full = heuristic_hours(&mover, HexCoord::new(2, 2), HexCoord::new(5, 2), &params);
        params.road_base_km = 0.5;
        let half = heuristic_hours(&mover, HexCoord::new(2, 2), HexCoord::new(5, 2), &params);
        assert!((half - full * 0.5).abs() < 1e-6, "{half} vs {full}");
    }

    #[test]
    fn border_river_charges_the_same_both_ways() {
        use crate::hex::HexDirection;
        // The generator writes `river_edges` only for in-province cells, so
        // a border river between the province and the §6.14 out-of-province
        // ring carries the bit on the province side alone. Leaving over it
        // and stepping back in must cost the same.
        //
        // q ≥ 9 is the province; q ≤ 8 is the ring. The province cell marks
        // the shared edge itself, the ring hex carries no bits.
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        for r in 0..10 {
            for q in 0..9 {
                g.cell_mut(HexCoord::new(q, r)).unwrap().out_of_bounds = true;
            }
        }
        g.cell_mut(HexCoord::new(9, 2)).unwrap().river_edges = HexDirection::W.bit();
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(9, 2));
        let p = CombatParams::default();
        let zoc = ControlField::default();
        // Ring-bound step: plains row + river surcharge + the out-of-bounds
        // leaving fee.
        let ring_bound = 1.5 + p.river_edge_surcharge_km + p.oob_step_penalty_km;
        let leave = step_km(
            &g,
            &mover,
            &zoc,
            HexCoord::new(9, 2),
            HexCoord::new(8, 2),
            &p,
        )
        .unwrap();
        assert!((leave - ring_bound).abs() < 1e-4, "leave {leave}");
        // Province-bound step: plains row + the same river surcharge, read
        // from the ring side's lack of a mirrored bit (the fix).
        let reenter = step_km(
            &g,
            &mover,
            &zoc,
            HexCoord::new(8, 2),
            HexCoord::new(9, 2),
            &p,
        )
        .unwrap();
        assert!((reenter - 3.5).abs() < 1e-4, "re-enter {reenter}");
        // A ring hex that marks the shared edge itself (the mirrored
        // layout) prices the same crossing the same way.
        let mut mirrored = HexGrid::new(10, 10, Terrain::Plains);
        for r in 0..10 {
            for q in 0..9 {
                mirrored
                    .cell_mut(HexCoord::new(q, r))
                    .unwrap()
                    .out_of_bounds = true;
            }
        }
        mirrored.cell_mut(HexCoord::new(8, 2)).unwrap().river_edges = HexDirection::E.bit();
        let out = step_km(
            &mirrored,
            &mover,
            &zoc,
            HexCoord::new(9, 2),
            HexCoord::new(8, 2),
            &p,
        )
        .unwrap();
        assert!((out - ring_bound).abs() < 1e-4, "into ring {out}");
        // …and the step back is charged from the ring side's own bit.
        let back = step_km(
            &mirrored,
            &mover,
            &zoc,
            HexCoord::new(8, 2),
            HexCoord::new(9, 2),
            &p,
        )
        .unwrap();
        assert!((back - 3.5).abs() < 1e-4, "out of ring {back}");
    }

    #[test]
    fn bridged_edge_waives_river_surcharge() {
        use crate::hex::HexDirection;
        let p = CombatParams::default();
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.cell_mut(HexCoord::new(3, 2)).unwrap().river_edges = HexDirection::E.bit();
        g.cell_mut(HexCoord::new(4, 2)).unwrap().river_edges = HexDirection::W.bit();
        // Control: unbridged crossing = 1.5 + 2.0.
        let c = step_km(
            &g,
            &mover,
            &no_control(),
            HexCoord::new(3, 2),
            HexCoord::new(4, 2),
            &p,
        )
        .unwrap();
        assert!((c - 3.5).abs() < 1e-4, "unbridged {c}");
        // Bridged edge: surcharge waived, only the off-road terrain row.
        g.cell_mut(HexCoord::new(3, 2)).unwrap().bridge_edges = HexDirection::E.bit();
        g.cell_mut(HexCoord::new(4, 2)).unwrap().bridge_edges = HexDirection::W.bit();
        let c = step_km(
            &g,
            &mover,
            &no_control(),
            HexCoord::new(3, 2),
            HexCoord::new(4, 2),
            &p,
        )
        .unwrap();
        assert!((c - 1.5).abs() < 1e-4, "bridged {c}");
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
        let g = HexGrid::new(12, 12, Terrain::Plains);
        let p = CombatParams::default();
        let mover = mk_unit(0, Side::Attacker, HexCoord::new(2, 2));
        let mut leader = mk_unit(1, Side::Attacker, HexCoord::new(2, 3));
        let target = HexCoord::new(8, 2);

        // Control: no standing order → the direct row-2 route wins.
        let units = vec![mover.clone(), leader.clone()];
        let (path, _) = find_path(&g, &mover, &units, target, &p).unwrap();
        assert_eq!(
            path.first().copied(),
            Some(HexCoord::new(3, 2)),
            "direct: {path:?}"
        );
        assert_eq!(path.len(), 6);

        // Leader ordered along row 2 (through (3,2)…(7,2) → dest (8,2)):
        // the follower fans out into the parallel lane instead of planning
        // through the leader's track. The surcharge accrues per penalized
        // hex, so a track long enough beats the one extra lane step (with
        // the shipped 0.5/hex penalty the break-even sits at 4 hexes; a
        // three-hex track is an exact tie and may follow).
        leader.move_order = Some(crate::unit::MoveOrder {
            path: vec![
                HexCoord::new(3, 2),
                HexCoord::new(4, 2),
                HexCoord::new(5, 2),
                HexCoord::new(6, 2),
                HexCoord::new(7, 2),
                HexCoord::new(8, 2),
            ],
            hours: 0.0,
            queue: Default::default(),
        });
        let units = vec![mover.clone(), leader];
        let (path, _) = find_path(&g, &mover, &units, target, &p).unwrap();
        assert_ne!(
            path.first().copied(),
            Some(HexCoord::new(3, 2)),
            "fanned out: {path:?}"
        );
        assert_eq!(path.last().copied(), Some(target), "destination kept");
        let track = [
            HexCoord::new(3, 2),
            HexCoord::new(4, 2),
            HexCoord::new(5, 2),
            HexCoord::new(6, 2),
            HexCoord::new(7, 2),
        ];
        let shared = path.iter().filter(|h| track.contains(h)).count();
        assert!(
            shared <= 1,
            "the lane only merges at the destination: {path:?}"
        );
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
            &no_control(),
            HexCoord::new(2, 2),
            HexCoord::new(3, 2),
            &p,
        )
        .unwrap();
        assert!(
            (c - (1.5 + p.oob_step_penalty_km)).abs() < 1e-4,
            "oob step = off-road terrain + surcharge, got {c}"
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
