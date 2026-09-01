//! Continuous movement-order execution (§6.2).
//!
//! Time model: 1 hex = 1 km and `turns_per_strategic_hour` turns make one
//! strategic hour (default 6), so at the end of each side's turn every
//! un-acted, un-emplaced unit of that side with a [`MoveOrder`] advances
//! `speed_kmh / turns_per_strategic_hour` kilometres along its path.
//! Fractional progress accumulates inside the order, so e.g. 4 km/h
//! infantry covers one plains hex every two turns.
//!
//! Contact rule (§6.5): a unit that steps into a hex adjacent to an enemy
//! makes contact — the order is consumed and the unit stops there, ready
//! for the player/AI to decide on an assault next turn.

use crate::grid::HexGrid;
use crate::hex::HexCoord;
use crate::params::CombatParams;
use crate::pathfinding::{find_path, step_hours, zoc_hexes};
use crate::unit::{BattalionUnit, Side};

/// One side-turn's movement budget in hours (§6.2/§8.1).
pub fn turn_budget_hours(params: &CombatParams) -> f32 {
    1.0 / params.turns_per_strategic_hour.max(1) as f32
}

/// What happened to one unit during [`advance_move_orders`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MovementEvent {
    /// Moved one hex (fired per hex crossed).
    Advanced {
        unit_id: usize,
        from: HexCoord,
        to: HexCoord,
    },
    /// Invested its whole budget mid-hex; no hex change yet.
    Progress { unit_id: usize },
    /// Next hex is occupied by a FRIEND — the unit waits (progress kept;
    /// congestion — the game layer may try a detour next turn).
    /// `blocker_id` identifies the friend: the game layer distinguishes
    /// following a MOVING convoy (quiet) from a PARKED blocker (congestion
    /// notice + detour escape).
    Blocked { unit_id: usize, blocker_id: usize },
    /// Next hex is held by an ENEMY (hidden, or it marched onto our route):
    /// the unit stops in front of it, both sides' orders are spent
    /// (interception — the simple ambush rule).
    Intercepted { unit_id: usize, enemy_id: usize },
    /// Entered a hex adjacent to an enemy — order consumed (§6.5 contact);
    /// adjacent enemies marching through halt too (symmetric stop).
    MadeContact { unit_id: usize },
    /// Reached the destination — order completed.
    Arrived { unit_id: usize },
}

/// Recompute a unit's standing-order route against the *current* situation
/// (detours must heal once the blocker leaves).
/// Called once per side-turn before marching; `view` is the pathing view of
/// that side (fog-filtered for the player, omniscient for the AI).
///
/// Adoption rules (guard against oscillation and wasted progress):
/// - never adopt an equal-cost different path (no route flip-flopping);
/// - if the fresh path shares the first step, invested `hours` are kept;
/// - a strictly cheaper route is always adopted;
/// - if the old next step is currently occupied, a detour costing at most
///   1.5× the remaining march is adopted even though it is not cheaper —
///   UNLESS the blocker has no standing order of its own (parked): a parked
///   unit never permanently pins anyone, any detour is accepted on the next
///   refresh.
///
/// Returns true when the order was rewritten.
pub fn refresh_move_order(
    grid: &HexGrid,
    unit: &mut BattalionUnit,
    view: &[BattalionUnit],
    params: &CombatParams,
) -> bool {
    let Some(order) = unit.move_order.clone() else {
        return false;
    };
    let Some(&dest) = order.path.last() else {
        unit.move_order = None;
        return true;
    };
    let Some((path, cost)) = find_path(grid, unit, view, dest, params) else {
        return false; // no route at all right now — keep waiting on the old one
    };
    if path.is_empty() {
        unit.move_order = None; // already there
        return true;
    }
    let old_remaining = order_eta_hours(grid, unit, view, params).unwrap_or(f32::MAX);
    let same_next = path.first() == order.path.first();
    let blocker = order.path.first().and_then(|h| {
        view.iter()
            .find(|o| o.id != unit.id && o.is_combat_effective() && o.position == *h)
    });
    // Parked-escape: the 1.5× detour cap only applies while the
    // blocker is itself moving (a convoy clears on its own); a parked
    // blocker (no standing order) must never pin us — accept any detour.
    let blocker_parked = blocker.is_some_and(|b| b.move_order.is_none());
    let cheaper = cost < old_remaining - 1e-6;
    let affordable_detour =
        blocker.is_some() && (blocker_parked || cost <= old_remaining * 1.5 + 1e-6);
    let adopt = if same_next {
        cheaper
    } else {
        cheaper || affordable_detour
    };
    if !adopt || path == order.path {
        return false;
    }
    // Invested hours ALWAYS carry. Refresh only ever recomputes the route
    // to the SAME destination, so the march investment is destination-
    // bound, not route-bound. Resetting it on a first-step change (a
    // crowd detour) pinned the progress at one turn's budget — the bar
    // sat frozen while the route flip-flopped (the "has order, bar not
    // growing" report) — reintroducing through this side door the very
    // pinning family the apply layer killed (§7.2 order persistence).
    unit.move_order = Some(crate::unit::MoveOrder {
        path,
        hours: order.hours,
    });
    true
}

/// Advance every eligible unit of `side` along its standing move order by
/// one side-turn's budget. Skips units that acted this turn, are emplaced,
/// or are not combat-effective (§6.2). Moving resets entrenchment and drops
/// the take-cover stance (§6.8).
pub fn advance_move_orders(
    grid: &HexGrid,
    units: &mut Vec<BattalionUnit>,
    side: Side,
    params: &CombatParams,
) -> Vec<MovementEvent> {
    let budget = turn_budget_hours(params);
    let ids: Vec<usize> = units
        .iter()
        .filter(|u| {
            u.side == side
                && u.is_combat_effective()
                && !u.acted
                && !u.is_emplaced
                && u.move_order.is_some()
        })
        .map(|u| u.id)
        .collect();

    let mut events = Vec::new();
    for id in ids {
        let mut remaining = budget;
        loop {
            let Some(i) = units.iter().position(|u| u.id == id) else {
                break;
            };
            let Some(order) = units[i].move_order.clone() else {
                break;
            };
            let Some(&next) = order.path.first() else {
                units[i].move_order = None;
                events.push(MovementEvent::Arrived { unit_id: id });
                break;
            };
            // One battalion per hex (§6.9): the next hex is occupied.
            if let Some(oi) = units
                .iter()
                .position(|o| o.id != id && o.is_combat_effective() && o.position == next)
            {
                if units[oi].side != side {
                    // Interception: enemy holds the next hex (hidden
                    // or marched onto our route) — halt in front of it; BOTH
                    // sides' orders are spent (simple ambush rule).
                    let enemy_id = units[oi].id;
                    units[i].move_order = None;
                    units[oi].move_order = None;
                    events.push(MovementEvent::Intercepted {
                        unit_id: id,
                        enemy_id,
                    });
                } else {
                    // Friendly congestion — wait (progress invested is kept).
                    events.push(MovementEvent::Blocked {
                        unit_id: id,
                        blocker_id: units[oi].id,
                    });
                }
                break;
            }
            let from = units[i].position;
            let enemy_zoc = zoc_hexes(grid, units, units[i].side.opponent());
            let Some(cost) = step_hours(grid, &units[i], &enemy_zoc, from, next, params) else {
                units[i].move_order = None; // path invalidated — drop quietly
                break;
            };

            if order.hours + remaining + 1e-6 >= cost {
                // Complete the step into `next`.
                remaining -= cost - order.hours;
                units[i].position = next;
                units[i].entrenchment = 0;
                // Moving also drops the take-cover stance.
                units[i].is_holding = false;
                events.push(MovementEvent::Advanced {
                    unit_id: id,
                    from,
                    to: next,
                });
                let mut rest = order.path;
                rest.remove(0);
                let contact = units.iter().any(|e| {
                    e.side != units[i].side
                        && e.is_combat_effective()
                        && e.position.distance(next) == 1
                });
                if contact {
                    units[i].move_order = None;
                    // Symmetric halt: enemies marching through the
                    // contact point stop too ("同时停下").
                    let my_side = units[i].side;
                    for e in units.iter_mut().filter(|e| {
                        e.side != my_side
                            && e.is_combat_effective()
                            && e.position.distance(next) == 1
                    }) {
                        e.move_order = None;
                    }
                    events.push(MovementEvent::MadeContact { unit_id: id });
                    break;
                }
                if rest.is_empty() {
                    units[i].move_order = None;
                    events.push(MovementEvent::Arrived { unit_id: id });
                    break;
                }
                units[i].move_order = Some(crate::unit::MoveOrder {
                    path: rest,
                    hours: 0.0,
                });
                if remaining <= 1e-6 {
                    events.push(MovementEvent::Progress { unit_id: id });
                    break;
                }
                // else: keep marching on the remaining budget.
            } else {
                if let Some(o) = units[i].move_order.as_mut() {
                    o.hours += remaining;
                }
                events.push(MovementEvent::Progress { unit_id: id });
                break;
            }
        }
    }
    events
}

/// Fraction (0..=1) of the CURRENT next step already invested: the
/// standing order's accumulated `hours` against the next hex's step
/// cost (§6.2). Drives the per-step progress mark on the route ribbon —
/// the mark reaches the next hex exactly when the marching step
/// completes. None when there is no next step (no order / empty path /
/// uncomputable cost).
pub fn next_step_progress(
    grid: &HexGrid,
    units: &[BattalionUnit],
    unit: &BattalionUnit,
    params: &CombatParams,
) -> Option<f32> {
    let order = unit.move_order.as_ref()?;
    let &next = order.path.first()?;
    let enemy_zoc = zoc_hexes(grid, units, unit.side.opponent());
    let cost = step_hours(grid, unit, &enemy_zoc, unit.position, next, params)?;
    Some((order.hours / cost.max(1e-6)).clamp(0.0, 1.0))
}

/// Remaining travel time of a standing order, in hours (§9.2 ETA display).
pub fn order_eta_hours(
    grid: &HexGrid,
    unit: &BattalionUnit,
    units: &[BattalionUnit],
    params: &CombatParams,
) -> Option<f32> {
    let order = unit.move_order.as_ref()?;
    let enemy_zoc = zoc_hexes(grid, units, unit.side.opponent());
    let mut total = -order.hours;
    let mut from = unit.position;
    for &next in &order.path {
        // An impassable leg costs f32::MAX, not 0: a 0-cost step would make
        // the (broken) old route look CHEAPER than a fresh detour and bias
        // the route-comparison callers.
        total += step_hours(grid, unit, &enemy_zoc, from, next, params).unwrap_or(f32::MAX);
        from = next;
    }
    Some(total.max(0.0))
}

/// Whole turns (rounded up) until arrival, for the ETA display (§9.2).
pub fn eta_turns(hours: f32, params: &CombatParams) -> u32 {
    (hours * params.turns_per_strategic_hour.max(1) as f32)
        .ceil()
        .max(0.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Terrain;
    use crate::unit::UnitType;

    fn grid(w: usize, h: usize) -> HexGrid {
        HexGrid::new(w, h, Terrain::Plains)
    }

    fn unit(id: usize, ty: UnitType, side: Side, q: i32, r: i32) -> BattalionUnit {
        BattalionUnit::new(id, format!("U{id}"), ty, side, HexCoord::new(q, r))
    }

    fn order(path: &[(i32, i32)]) -> Option<crate::unit::MoveOrder> {
        Some(crate::unit::MoveOrder {
            path: path.iter().map(|(q, r)| HexCoord::new(*q, *r)).collect(),
            hours: 0.0,
        })
    }

    #[test]
    fn infantry_accumulates_fractional_progress() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1)]);
        let mut units = vec![u];
        let p = CombatParams::default();
        // Turn 1: 1/6 h invested into a 1/4 h step — no hex change.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert!(ev.contains(&MovementEvent::Progress { unit_id: 0 }));
        // Turn 2: 2/6 ≥ 1/4 — the step completes.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(2, 1));
        assert!(ev.contains(&MovementEvent::Arrived { unit_id: 0 }));
    }

    #[test]
    fn motorized_covers_two_plains_hexes_per_turn() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1), (4, 1)]);
        let mut units = vec![u];
        let p = CombatParams::default();
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        // 12 km/h × 1/6 h = 2 km of plains per turn.
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        assert!(units[0].move_order.is_some());
    }

    #[test]
    fn moving_drops_cover_stance() {
        // The take-cover stance drops with the first step (the
        // +25% only protects a stationary unit) — alongside entrenchment.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.is_holding = true;
        u.entrenchment = 2;
        u.move_order = order(&[(2, 1), (3, 1)]);
        let mut units = vec![u];
        let p = CombatParams::default();
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        assert!(!units[0].is_holding);
        assert_eq!(units[0].entrenchment, 0);
    }

    #[test]
    fn contact_consumes_the_order() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1), (4, 1), (5, 1)]);
        let enemy = unit(1, UnitType::Infantry, Side::Defender, 3, 2); // adjacent to (3,1)
        let mut units = vec![u, enemy];
        let p = CombatParams::default();
        // ZOC surcharges are zeroed in the defaults, so the 2 km budget
        // covers both plains steps in one turn: entering the enemy-adjacent
        // (3,1) triggers contact and the order is spent.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        assert!(units[0].move_order.is_none());
        assert!(ev.contains(&MovementEvent::MadeContact { unit_id: 0 }));
    }

    #[test]
    fn next_step_progress_tracks_invested_hours() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        // No order → nothing in progress.
        assert!(next_step_progress(&g, &[u.clone()], &u, &CombatParams::default()).is_none());
        // Plain infantry step = 0.25 h; half-invested hours → ≈ 0.5.
        u.move_order = order(&[(2, 1)]);
        u.move_order.as_mut().unwrap().hours = 0.125;
        let p = next_step_progress(&g, &[u.clone()], &u, &CombatParams::default()).unwrap();
        assert!((p - 0.5).abs() < 0.05, "half-invested step ≈ 0.5: {p}");
        // Fresh order (hours 0) → no mark yet.
        let mut fresh = u.clone();
        fresh.move_order.as_mut().unwrap().hours = 0.0;
        let p0 = next_step_progress(&g, &[fresh.clone()], &fresh, &CombatParams::default()).unwrap();
        assert!(p0 < 0.01, "no progress at hours 0: {p0}");
        // Arrived (empty path) → none.
        let mut done = u.clone();
        done.move_order = Some(crate::unit::MoveOrder {
            path: vec![],
            hours: 0.0,
        });
        assert!(next_step_progress(&g, &[done.clone()], &done, &CombatParams::default()).is_none());
    }

    #[test]
    fn occupied_next_hex_waits() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1)]);
        let friend = unit(1, UnitType::Infantry, Side::Attacker, 2, 1);
        let mut units = vec![u, friend];
        let p = CombatParams::default();
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert!(units[0].move_order.is_some());
        assert!(ev.contains(&MovementEvent::Blocked {
            unit_id: 0,
            blocker_id: 1
        }));
    }

    #[test]
    fn enemy_on_next_hex_intercepts_and_both_halt() {
        // The next hex is enemy-held (hidden or it marched onto the
        // route) — the mover halts in front, both orders are spent.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1)]);
        let mut e = unit(1, UnitType::Infantry, Side::Defender, 2, 1);
        e.move_order = order(&[(2, 2)]);
        let mut units = vec![u, e];
        let p = CombatParams::default();
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert!(units[0].move_order.is_none(), "mover halted");
        assert!(
            units[1].move_order.is_none(),
            "intercepted enemy halted too"
        );
        assert!(ev.contains(&MovementEvent::Intercepted {
            unit_id: 0,
            enemy_id: 1
        }));
    }

    #[test]
    fn contact_halts_marching_enemy_too() {
        // Symmetric stop: an enemy marching through the contact
        // point has its own order consumed as well.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1)]);
        let mut e = unit(1, UnitType::Infantry, Side::Defender, 3, 2);
        e.move_order = order(&[(4, 2)]);
        let mut units = vec![u, e];
        let p = CombatParams::default();
        // ZOC off in the defaults: the 2 km budget reaches (3,1) in the
        // first turn — contact fires immediately, halting both sides.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert!(ev.contains(&MovementEvent::MadeContact { unit_id: 0 }));
        assert!(units[0].move_order.is_none());
        assert!(units[1].move_order.is_none(), "contacted enemy halted too");
    }

    #[test]
    fn refresh_heals_stale_detour_once_blocker_leaves() {
        // A detour forced by a friendly blocker must revert to the
        // shorter direct route once the blocker moves on.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        // Detour: friend stood at (2,1), so the order went around south —
        // 4 steps where the direct route east takes 3.
        u.move_order = order(&[(1, 2), (2, 2), (3, 2), (4, 1)]);
        let p = CombatParams::default();
        // Friend has left — no one else on the map.
        let units_view = vec![u.clone()];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed);
        let path = u.move_order.unwrap().path;
        assert_eq!(path.len(), 3, "back to a shortest route: {path:?}");
        assert_eq!(
            path.last().copied(),
            Some(HexCoord::new(4, 1)),
            "destination kept"
        );
    }

    #[test]
    fn refresh_keeps_equal_cost_path_no_oscillation() {
        // Guard: an alternative of equal cost must NOT be adopted (no route
        // flip-flopping between turns).
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1)]); // one of two equal 2-step routes
        let p = CombatParams::default();
        let units_view = vec![u.clone()];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(!changed, "equal-cost alternative must not be adopted");
        assert_eq!(
            u.move_order.unwrap().path.first().copied(),
            Some(HexCoord::new(2, 1))
        );
    }

    #[test]
    fn refresh_adopts_affordable_detour_when_blocked() {
        // Friend blocks the old next step; a detour at ≤1.5× is adopted even
        // though it costs more than the (impassable-right-now) old route.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1)]);
        let friend = unit(1, UnitType::Infantry, Side::Attacker, 2, 1);
        let p = CombatParams::default();
        let units_view = vec![u.clone(), friend];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed);
        let path = u.move_order.unwrap().path;
        assert!(
            !path.contains(&HexCoord::new(2, 1)),
            "detour avoids the blocker: {path:?}"
        );
        assert_eq!(
            path.last().copied(),
            Some(HexCoord::new(3, 1)),
            "destination kept"
        );
    }

    /// Geometry for the parked/moving escape tests: the direct route east is
    /// walled by a friend at (2,1) plus mountains at (1,0)/(2,0) and a
    /// second friend at (2,2), so the only detour runs 5 steps south —
    /// 2.5× the 2-step remaining march, decisively above the 1.5× cap.
    fn walled_scenario() -> (HexGrid, BattalionUnit, BattalionUnit, BattalionUnit) {
        let mut g = grid(8, 8);
        g.set_terrain(HexCoord::new(1, 0), Terrain::Mountain);
        g.set_terrain(HexCoord::new(2, 0), Terrain::Mountain);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1)]);
        let blocker = unit(1, UnitType::Infantry, Side::Attacker, 2, 1);
        let wall = unit(2, UnitType::Infantry, Side::Attacker, 2, 2);
        (g, u, blocker, wall)
    }

    #[test]
    fn refresh_adopts_any_detour_when_blocker_parked() {
        // A parked blocker (no standing order) must never pin us —
        // even a 2.5× detour is adopted on the next refresh.
        let (g, mut u, blocker, wall) = walled_scenario();
        let p = CombatParams::default();
        let units_view = vec![u.clone(), blocker, wall];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed, "parked blocker → detour adopted");
        let path = u.move_order.unwrap().path;
        assert!(
            !path.contains(&HexCoord::new(2, 1)),
            "detour avoids the parked blocker: {path:?}"
        );
        assert_eq!(path.len(), 5, "5-step detour (2.5×) accepted: {path:?}");
        assert_eq!(
            path.last().copied(),
            Some(HexCoord::new(3, 1)),
            "destination kept"
        );
    }

    #[test]
    fn refresh_keeps_cap_when_blocker_moving() {
        // The blocker has its own standing order (a convoy): the 1.5× cap
        // still applies — the 2.5× detour is NOT adopted, we just follow.
        let (g, mut u, mut blocker, wall) = walled_scenario();
        blocker.move_order = order(&[(3, 1), (4, 1)]);
        let p = CombatParams::default();
        let units_view = vec![u.clone(), blocker, wall];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(
            !changed,
            "moving blocker: expensive detour rejected, convoy follows"
        );
        assert_eq!(
            u.move_order.unwrap().path.first().copied(),
            Some(HexCoord::new(2, 1))
        );
    }

    #[test]
    fn refresh_keeps_invested_hours_when_first_step_unchanged() {
        // Same first step: the fresh tail may be adopted, but invested
        // progress into the current step is preserved.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = Some(crate::unit::MoveOrder {
            path: vec![HexCoord::new(2, 1), HexCoord::new(3, 1)],
            hours: 0.05,
        });
        let p = CombatParams::default();
        let units_view = vec![u.clone()];
        let _ = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!((u.move_order.unwrap().hours - 0.05).abs() < 1e-6);
    }

    #[test]
    fn refresh_keeps_invested_hours_across_detour() {
        // A crowd detour must NOT wipe the invested progress: refresh only
        // ever recomputes to the SAME destination, so the hours are
        // destination-bound. (The frozen-bar report: resetting on every
        // detour adoption pinned the progress mark at one turn's budget.)
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = Some(crate::unit::MoveOrder {
            path: vec![HexCoord::new(2, 1), HexCoord::new(3, 1)],
            hours: 0.05,
        });
        let friend = unit(1, UnitType::Infantry, Side::Attacker, 2, 1);
        let p = CombatParams::default();
        let units_view = vec![u.clone(), friend];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed, "detour adopted around the friend");
        let o = u.move_order.unwrap();
        assert!(
            !o.path.contains(&HexCoord::new(2, 1)),
            "detour avoids the blocker: {:?}",
            o.path
        );
        assert!(
            (o.hours - 0.05).abs() < 1e-6,
            "invested hours carry across the detour: {}",
            o.hours
        );
    }

    #[test]
    fn acted_and_emplaced_do_not_march() {
        let g = grid(8, 8);
        let mut a = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        a.move_order = order(&[(2, 1)]);
        a.acted = true;
        let mut b = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 1, 3);
        b.move_order = order(&[(2, 3)]);
        b.is_emplaced = true;
        let mut units = vec![a, b];
        let p = CombatParams::default();
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert!(ev.is_empty());
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert_eq!(units[1].position, HexCoord::new(1, 3));
    }

    #[test]
    fn eta_counts_remaining_hours_in_turns() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1), (4, 1)]);
        let units = vec![u];
        let p = CombatParams::default();
        let h = order_eta_hours(&g, &units[0], &units, &p).unwrap();
        // 3 plains km at 12 km/h = 0.25 h → ceil(0.25×6) = 2 turns.
        assert!((h - 0.25).abs() < 1e-4);
        assert_eq!(eta_turns(h, &p), 2);
    }

    #[test]
    fn broken_route_eta_is_huge_and_detour_wins() {
        // An impassable leg in the STANDING order prices at f32::MAX,
        // not 0 — a 0 made the broken route look cheaper than any real
        // detour, so refresh_move_order never healed it.
        let mut g = grid(8, 8);
        g.set_terrain(HexCoord::new(2, 1), Terrain::Water);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1)]);
        let units_view = vec![u.clone()];
        let p = CombatParams::default();
        let eta = order_eta_hours(&g, &u, &units_view, &p).unwrap();
        assert!(eta > 1e6, "an impassable leg must price huge, got {eta}");
        // refresh_move_order: any finite detour beats the drowned route.
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed, "the finite detour must be adopted");
        let path = u.move_order.unwrap().path;
        assert!(
            !path.contains(&HexCoord::new(2, 1)),
            "detour avoids the water: {path:?}"
        );
        assert_eq!(
            path.last().copied(),
            Some(HexCoord::new(3, 1)),
            "destination kept"
        );
    }
}
