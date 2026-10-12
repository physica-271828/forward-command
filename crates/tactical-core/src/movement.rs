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
//!
//! Re-pointing (§6.2 step-progress memory): replacing an order whose
//! fresh route starts on a different first step banks the abandoned
//! investment into the unit's step memory instead of losing it —
//! pointing back onto that first step picks the hours back up (see
//! [`repoint_carry_hours`]).

use crate::grid::HexGrid;
use crate::hex::HexCoord;
use crate::params::CombatParams;
use crate::pathfinding::{control_field, find_path, step_hours, ControlField};
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
    /// Reached a waypoint in the middle of a Shift-click chain — the
    /// order survives and the NEXT leg becomes the standing segment.
    /// Emitted only for intermediate arrivals; the final destination
    /// emits [`MovementEvent::Arrived`] with the full end-of-order
    /// behaviour (log, manual-override release at refresh).
    WaypointReached { unit_id: usize },
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
/// Only the ACTIVE segment is judged here — queued chain legs stay queued
/// (their previews are display-only and never adopted); a chain between
/// legs (empty path) tries to activate its next leg instead.
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
        // No route on the standing segment: a finished order drops
        // ("already there"); a chain between legs retries the next leg's
        // plan (a route may have opened since the last attempt).
        if order.queue.is_empty() {
            unit.move_order = None;
            return true;
        }
        return plan_next_leg(grid, unit, view, params);
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
    // The queued chain legs ride along untouched.
    //
    // A first-step change can undercut the bank: hours invested toward an
    // expensive step exceed the new, cheaper step's cost. Clamp the carry
    // to that cost — the §6.2 invariant `hours < first-step cost` keeps
    // the surplus from refunding into the turn's march budget at advance
    // time (the per-turn cap is speed/6 km, not negotiable).
    let hours = if same_next {
        order.hours
    } else {
        let control = control_field(grid, view, unit.side.opponent());
        step_hours(grid, unit, &control, unit.position, path[0], params)
            .map(|c| order.hours.min(c))
            .unwrap_or(order.hours)
    };
    unit.move_order = Some(crate::unit::MoveOrder {
        path,
        hours,
        queue: order.queue,
    });
    true
}

/// Re-point a standing march order onto a fresh route (§6.2 step-progress
/// memory): returns the hours the new order starts with, updating the
/// unit's progress memory as a side effect. Shared by the player command
/// path and the AI apply path so both walk the same retention ladder:
///
/// 1. the fresh route shares the standing order's first step — the
///    invested hours ride along untouched (the unit is mid-step into that
///    very hex; nothing was abandoned and the memory stays as it is);
/// 2. the memory's hex IS the fresh route's first step and the unit still
///    stands next to it — the banked investment is picked back up,
///    clamped to the step's current cost (the same clamp as the
///    route-refresh carry), and the memory is consumed;
/// 3. otherwise the bar restarts from zero.
///
/// The restore is judged BEFORE the outgoing order banks: the A→B→A
/// change of mind must still find A's investment in the slot. The banking
/// then keeps the memory the MOST RECENTLY abandoned progress — on a
/// restore, the outgoing order's own investment re-arms the slot in the
/// same call.
///
/// A memory whose hex no longer neighbours the unit is stale — the unit
/// marched on since banking it — and lapses (dropped) the first time a
/// re-point asks for it. `control` prices the restore clamp from the
/// caller's pathing view (§6.5 slowdown included; fog-filtered for the
/// player, omniscient for the AI).
pub fn repoint_carry_hours(
    grid: &HexGrid,
    control: &ControlField,
    unit: &mut BattalionUnit,
    new_path: &[HexCoord],
    params: &CombatParams,
) -> f32 {
    let Some(&new_first) = new_path.first() else {
        return 0.0;
    };
    // Rung 1: same first step — the investment rides along; nothing
    // banks, nothing restores.
    if unit
        .move_order
        .as_ref()
        .is_some_and(|o| o.path.first() == Some(&new_first))
    {
        return unit.move_order.as_ref().map(|o| o.hours).unwrap_or(0.0);
    }
    // Rung 2: the banked memory matches the fresh first step — restore
    // BEFORE the outgoing order's own banking overwrites the slot.
    let mut restored = None;
    if let Some((hex, hours)) = unit.step_memory {
        if hex == new_first {
            if unit.position.distance(hex) == 1 {
                restored = Some(
                    step_hours(grid, unit, control, unit.position, hex, params)
                        .map(|cost| hours.min(cost))
                        .unwrap_or(hours),
                );
            }
            // Consumed on a hit, lapsed (stale geometry) on a miss — the
            // slot clears either way.
            unit.step_memory = None;
        }
    }
    // The outgoing order's first step genuinely differs: its investment
    // banks as the most recent abandonment (a no-progress order banks
    // nothing and leaves an older, mismatching memory in place).
    unit.bank_step_memory();
    restored.unwrap_or(0.0)
}

/// Activate the next leg of a Shift-click chain: the standing segment is
/// exhausted (empty path — the unit sits on the waypoint) and the queue
/// holds further destinations. The first leg is popped and RE-PLANNED
/// from the unit's current hex against `view` — a queued leg never
/// marches its stale preview; the fresh segment starts with zero invested
/// hours. Returns true when the unit now holds the planned leg as its
/// standing order; false when the queue is empty, no route exists right
/// now (the empty-segment wait state is kept and retried by later
/// refreshes/advances), or the destination is the hex the unit already
/// stands on (redundant leg dropped, the next one tried).
fn plan_next_leg(
    grid: &HexGrid,
    unit: &mut BattalionUnit,
    view: &[BattalionUnit],
    params: &CombatParams,
) -> bool {
    loop {
        let dest = match unit.move_order.as_ref().and_then(|o| o.queue.front()) {
            Some(leg) => leg.dest,
            None => return false,
        };
        let Some((path, _)) = find_path(grid, unit, view, dest, params) else {
            return false; // no route right now — keep waiting on the queue
        };
        if !path.is_empty() {
            let order = unit.move_order.take().expect("queue present");
            let mut queue = order.queue;
            queue.pop_front();
            unit.move_order = Some(crate::unit::MoveOrder {
                path,
                hours: 0.0,
                queue,
            });
            return true;
        }
        // The unit already stands on the leg's destination — drop the
        // redundant leg and try the next one.
        if let Some(order) = unit.move_order.as_mut() {
            order.queue.pop_front();
        }
    }
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

    // Chain-leg activation re-plans against the LIVE unit set (executor
    // omniscience — the same data interception and jams read; the
    // per-side-turn refresh heals the leg against the side's own view
    // before it ever marches). The view is cloned only while a chain
    // actually exists on the board (the clone is empty otherwise — no
    // chain branch can then run, so the empty view is never consulted).
    let chain_view = if units
        .iter()
        .any(|u| u.move_order.as_ref().is_some_and(|o| !o.queue.is_empty()))
    {
        units.clone()
    } else {
        Vec::new()
    };

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
                // No route on the standing segment: a chain between legs
                // (waiting for a route) retries its plan here; a finished
                // order arrives.
                if order.queue.is_empty() {
                    units[i].move_order = None;
                    events.push(MovementEvent::Arrived { unit_id: id });
                    break;
                }
                if plan_next_leg(grid, &mut units[i], &chain_view, params) {
                    continue; // fresh segment — march it with this budget
                }
                break; // still no route — wait quietly
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
            let control = control_field(grid, units, units[i].side.opponent());
            let Some(cost) = step_hours(grid, &units[i], &control, from, next, params) else {
                units[i].move_order = None; // path invalidated — drop quietly
                break;
            };

            if order.hours + remaining + 1e-6 >= cost {
                // Complete the step into `next`. The banked hours may
                // overpay the step (a refresh can carry them onto a
                // cheaper first step) — the surplus is shed, never
                // refunded into the turn's remaining budget (§6.2 caps
                // the end-of-turn advance at speed/6 km).
                remaining -= (cost - order.hours).max(0.0);
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
                    if order.queue.is_empty() {
                        units[i].move_order = None;
                        events.push(MovementEvent::Arrived { unit_id: id });
                        break;
                    }
                    // Intermediate waypoint of a chain: hand the standing
                    // order to the next queued leg (re-planned from this
                    // hex). No arrival log and no manual-override release —
                    // only the LAST leg ends the order (§6.2 chain
                    // contract).
                    units[i].move_order = Some(crate::unit::MoveOrder {
                        path: Vec::new(),
                        hours: 0.0,
                        queue: order.queue,
                    });
                    events.push(MovementEvent::WaypointReached { unit_id: id });
                    if !plan_next_leg(grid, &mut units[i], &chain_view, params) || remaining <= 1e-6
                    {
                        break;
                    }
                    continue; // walk the fresh segment on the remaining budget
                }
                units[i].move_order = Some(crate::unit::MoveOrder {
                    path: rest,
                    hours: 0.0,
                    queue: order.queue,
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
    let control = control_field(grid, units, unit.side.opponent());
    let cost = step_hours(grid, unit, &control, unit.position, next, params)?;
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
    let control = control_field(grid, units, unit.side.opponent());
    let mut total = -order.hours;
    let mut from = unit.position;
    for &next in &order.path {
        // An impassable leg costs f32::MAX, not 0: a 0-cost step would make
        // the (broken) old route look CHEAPER than a fresh detour and bias
        // the route-comparison callers.
        total += step_hours(grid, unit, &control, from, next, params).unwrap_or(f32::MAX);
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

/// Whole-chain travel time of a standing order (§9.2 ETA display): the
/// active segment (invested hours deducted) plus every queued leg,
/// estimated over its preview route. Previews are display data — each
/// leg is re-planned at activation — so the queued part is an estimate
/// until then. Legs without a preview are skipped (the player command
/// path always stores one). None when the unit has no order.
pub fn chain_eta_hours(
    grid: &HexGrid,
    unit: &BattalionUnit,
    units: &[BattalionUnit],
    params: &CombatParams,
) -> Option<f32> {
    let order = unit.move_order.as_ref()?;
    let mut total = order_eta_hours(grid, unit, units, params).unwrap_or(0.0);
    let control = control_field(grid, units, unit.side.opponent());
    for leg in &order.queue {
        let mut from = match leg.preview.first() {
            Some(&h) => h,
            None => continue, // no preview — nothing to estimate
        };
        for &next in leg.preview.iter().skip(1) {
            total += step_hours(grid, unit, &control, from, next, params).unwrap_or(f32::MAX);
            from = next;
        }
    }
    Some(total.max(0.0))
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
            queue: Default::default(),
        })
    }

    /// No enemy cover on the test map — an empty §6.5 control field.
    fn no_control() -> ControlField {
        ControlField::default()
    }

    #[test]
    fn infantry_accumulates_fractional_progress() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1)]);
        let mut units = vec![u];
        let p = CombatParams::default();
        // Turn 1: 1/6 h invested into a 1.5/4 = 0.375 h step — no hex change.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert!(ev.contains(&MovementEvent::Progress { unit_id: 0 }));
        // Turn 2: 2/6 = 0.333 < 0.375 — still short of the step.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert!(ev.contains(&MovementEvent::Progress { unit_id: 0 }));
        // Turn 3: 3/6 = 0.5 ≥ 0.375 — the step completes.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(2, 1));
        assert!(ev.contains(&MovementEvent::Arrived { unit_id: 0 }));
    }

    #[test]
    fn motorized_covers_two_plains_hexes_per_turn_on_road() {
        let mut g = grid(8, 8);
        // §4.2 road network: surfaced hexes cost the base 1.0 for motor —
        // 12 km/h × 1/6 h = 2 km of road per turn.
        for (q, r) in [(2, 1), (3, 1), (4, 1)] {
            g.cell_mut(HexCoord::new(q, r)).unwrap().road = true;
        }
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1), (4, 1)]);
        let mut units = vec![u];
        let p = CombatParams::default();
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        assert!(units[0].move_order.is_some());
        // Same path OFF-road: motor pays 1.75/hex, only one step fits.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = order(&[(2, 1), (3, 1), (4, 1)]);
        let mut units = vec![u];
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(2, 1));
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
        // Two off-road plains steps (2×1.75 km) need two 2 km budgets.
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
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
        // §6.5 control: the enemy covers the (3,1) step — a fresh infantry
        // battalion doubles its price (off-road motor 1.75 → 3.5 km against
        // the 2 km/turn budget), so contact fires on the THIRD turn.
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
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
        // Off-road infantry step = 1.5/4 = 0.375 h; half-invested → ≈ 0.5.
        u.move_order = order(&[(2, 1)]);
        u.move_order.as_mut().unwrap().hours = 0.1875;
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
            queue: Default::default(),
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
        // §6.5 control doubles the enemy-adjacent (3,1) step (1.75 → 3.5 km
        // against the 2 km/turn budget): contact fires on the third turn,
        // halting both sides.
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
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
    /// walled by a friend at (2,1) plus mountains at (1,0)/(2,0)/(3,0) and a
    /// second friend at (2,2), so the only detour runs 5 steps south —
    /// 2.5× the 2-step remaining march, decisively above the 1.5× cap.
    /// (Three mountains: with the 0.5/hex off-road surcharge a single
    /// mountain hop — 4.75 + 2×1.75 = 8.25 km — undercuts the 5-step plain
    /// detour at 8.75 km, so the wall must actually block.)
    fn walled_scenario() -> (HexGrid, BattalionUnit, BattalionUnit, BattalionUnit) {
        let mut g = grid(8, 8);
        g.set_terrain(HexCoord::new(1, 0), Terrain::Mountain);
        g.set_terrain(HexCoord::new(2, 0), Terrain::Mountain);
        g.set_terrain(HexCoord::new(3, 0), Terrain::Mountain);
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
            queue: Default::default(),
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
            queue: Default::default(),
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
        // 3 off-road plains steps: 3×1.75 km at 12 km/h = 0.4375 h
        // → ceil(0.4375×6) = 3 turns.
        assert!((h - 0.4375).abs() < 1e-4);
        assert_eq!(eta_turns(h, &p), 3);
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

    #[test]
    fn over_invested_step_never_refunds_into_the_turn_budget() {
        // Hours banked on an expensive step, carried by a refresh onto a
        // CHEAPER first step, complete that step — but the surplus must not
        // grow the turn's remaining budget (§6.2 caps the end-of-turn
        // advance at speed/6 km).
        let mut g = grid(8, 8);
        for (q, r) in [(2, 1), (3, 1)] {
            g.cell_mut(HexCoord::new(q, r)).unwrap().road = true;
        }
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        // 1/3 h banked (two turns into a 0.5 h forest step); the detour's
        // road steps cost 1.0/4 = 0.25 h — the bank overpays the step.
        u.move_order = Some(crate::unit::MoveOrder {
            path: vec![HexCoord::new(2, 1), HexCoord::new(3, 1)],
            hours: 1.0 / 3.0,
            queue: Default::default(),
        });
        let mut units = vec![u];
        let p = CombatParams::default();
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        // The carried hours cover the first step; the plain 1/6 h budget
        // only PARTLY pays the second — a refund would complete both road
        // steps (2 km in a 2/3 km turn).
        assert_eq!(units[0].position, HexCoord::new(2, 1));
        let o = units[0].move_order.as_ref().unwrap();
        assert!(
            (o.hours - 1.0 / 6.0).abs() < 1e-4,
            "the turn's budget is invested, never grown: {}",
            o.hours
        );
    }

    fn order_with_hours(path: &[(i32, i32)], hours: f32) -> Option<crate::unit::MoveOrder> {
        let mut o = order(path).unwrap();
        o.hours = hours;
        Some(o)
    }

    #[test]
    fn repoint_same_first_step_carries_and_leaves_memory_untouched() {
        // §6.2 retention ladder, rung 1: the fresh route shares the old
        // first step — the invested hours ride along and the memory is
        // neither written nor consumed (nothing was abandoned).
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        u.move_order = order_with_hours(&[(2, 1), (3, 1)], 0.12);
        u.step_memory = Some((HexCoord::new(1, 2), 0.07));
        let p = CombatParams::default();
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(4, 1)],
            &p,
        );
        assert!(
            (carry - 0.12).abs() < 1e-6,
            "same first step carries: {carry}"
        );
        assert_eq!(
            u.step_memory,
            Some((HexCoord::new(1, 2), 0.07)),
            "memory untouched"
        );
    }

    #[test]
    fn repoint_banks_abandoned_progress_and_restores_it_on_change_of_mind() {
        // A→B: B's first step differs — A's investment banks into the
        // step memory and B starts from zero. A→B→A: the banked hours
        // come back (the player changed her mind) and the memory is
        // consumed.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        let p = CombatParams::default();
        u.move_order = order_with_hours(&[(2, 1), (3, 1)], 0.12);
        let b_path = [HexCoord::new(1, 2), HexCoord::new(1, 3)];
        let carry = repoint_carry_hours(&g, &no_control(), &mut u, &b_path, &p);
        assert_eq!(carry, 0.0, "a different first step restarts the bar");
        assert_eq!(
            u.step_memory,
            Some((HexCoord::new(2, 1), 0.12)),
            "abandoned progress banked"
        );
        u.move_order = order_with_hours(&[(1, 2), (1, 3)], 0.0);
        let a_path = [HexCoord::new(2, 1), HexCoord::new(3, 1)];
        let carry = repoint_carry_hours(&g, &no_control(), &mut u, &a_path, &p);
        assert!(
            (carry - 0.12).abs() < 1e-6,
            "banked progress restored: {carry}"
        );
        assert_eq!(u.step_memory, None, "the restore consumes the memory");
    }

    #[test]
    fn repoint_memory_holds_only_the_latest_abandonment() {
        // A→B→C with three distinct first steps: B's abandonment
        // overwrites A's — the memory always holds the MOST RECENTLY
        // deserted investment. Pointing back at A restores nothing (a
        // mismatching memory survives the ask); pointing back at B
        // restores B's.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        let p = CombatParams::default();
        u.move_order = order_with_hours(&[(2, 1), (3, 1)], 0.12);
        repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(1, 2), HexCoord::new(1, 3)],
            &p,
        );
        u.move_order = order_with_hours(&[(1, 2), (1, 3)], 0.20);
        repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 0), HexCoord::new(3, 0)],
            &p,
        );
        assert_eq!(
            u.step_memory,
            Some((HexCoord::new(1, 2), 0.20)),
            "B's abandonment overwrote A's"
        );
        u.move_order = order_with_hours(&[(2, 0), (3, 0)], 0.0);
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(3, 1)],
            &p,
        );
        assert_eq!(
            carry, 0.0,
            "A's memory was overwritten — nothing to restore"
        );
        assert_eq!(u.step_memory, Some((HexCoord::new(1, 2), 0.20)));
        u.move_order = order_with_hours(&[(2, 1), (3, 1)], 0.0);
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(1, 2), HexCoord::new(1, 3)],
            &p,
        );
        assert!(
            (carry - 0.20).abs() < 1e-6,
            "the latest abandonment restores: {carry}"
        );
    }

    #[test]
    fn repoint_restore_clamps_to_the_current_step_cost() {
        // The banked hours outprice the step they return to (terrain or
        // ZOC changed since the banking): the restore clamps to the
        // step's current cost — the §6.2 invariant `hours < first-step
        // cost` holds (same clamp as the route-refresh carry).
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        let p = CombatParams::default();
        // A plains infantry step into (2,1) costs 1.5/4 = 0.375 h; the
        // memory banks 0.5 h (e.g. banked while a ZOC surcharge priced it).
        u.step_memory = Some((HexCoord::new(2, 1), 0.5));
        u.move_order = order_with_hours(&[(1, 2)], 0.0);
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(3, 1)],
            &p,
        );
        let cost = crate::pathfinding::step_hours(
            &g,
            &u,
            &no_control(),
            u.position,
            HexCoord::new(2, 1),
            &p,
        )
        .unwrap();
        assert!(
            (carry - cost).abs() < 1e-6,
            "clamped to the step cost {cost}: {carry}"
        );
        assert!(carry < 0.5, "the surplus over the step cost is shed");
    }

    #[test]
    fn repoint_memory_lapses_when_the_unit_marched_away() {
        // Lazy geometry check: the remembered hex no longer neighbours
        // the unit (it marched on after the banking) — the memory is
        // stale, restores nothing, and is dropped. (A freshly planned
        // route always starts adjacent; the guard exists for stale or
        // foreign paths.)
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 4, 4);
        let p = CombatParams::default();
        u.step_memory = Some((HexCoord::new(2, 1), 0.12));
        u.move_order = order_with_hours(&[(4, 3)], 0.0);
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(3, 1)],
            &p,
        );
        assert_eq!(carry, 0.0, "no neighbour, no restore");
        assert_eq!(u.step_memory, None, "a stale memory lapses");
    }

    #[test]
    fn repoint_memory_survives_full_turns_of_marching() {
        // The memory never ticks down on its own: the replacement order
        // marches out and arrives, turns pass — the banked investment is
        // still there for the change of mind, and still restores.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        let p = CombatParams::default();
        u.move_order = order_with_hours(&[(2, 1), (3, 1)], 0.12);
        repoint_carry_hours(&g, &no_control(), &mut u, &[HexCoord::new(1, 2)], &p);
        u.move_order = order_with_hours(&[(1, 2)], 0.0);
        let mut units = vec![u];
        // Three side-turns march the one-step order out (0.375 h step vs
        // the 1/6 h budget) — the unit arrives, the order completes.
        for _ in 0..3 {
            advance_move_orders(&g, &mut units, Side::Attacker, &p);
        }
        let u = &mut units[0];
        assert_eq!(
            u.position,
            HexCoord::new(1, 2),
            "the replacement march arrived"
        );
        assert!(u.move_order.is_none());
        assert_eq!(
            u.step_memory,
            Some((HexCoord::new(2, 1), 0.12)),
            "banked progress survives the turns"
        );
        // (1,2) still neighbours the banked (2,1) — the restore fires.
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            u,
            &[HexCoord::new(2, 1), HexCoord::new(2, 2)],
            &p,
        );
        assert!(
            (carry - 0.12).abs() < 1e-6,
            "restored after full turns: {carry}"
        );
    }

    #[test]
    fn repoint_consumed_memory_needs_new_progress_to_restore_again() {
        // Once consumed, a second re-point at the same first step finds
        // an empty memory — unless fresh progress was banked in the
        // meantime (another abandonment re-arms the restore).
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        let p = CombatParams::default();
        u.step_memory = Some((HexCoord::new(2, 1), 0.12));
        u.move_order = order_with_hours(&[(1, 2), (1, 3)], 0.0);
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(3, 1)],
            &p,
        );
        assert!((carry - 0.12).abs() < 1e-6, "first restore consumes");
        // The restored order runs to completion (arrival clears it).
        u.move_order = None;
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(4, 1)],
            &p,
        );
        assert_eq!(carry, 0.0, "no ghost restore from a consumed memory");
        // Re-armed: the restored order had been abandoned again with its
        // progress intact — the bank holds it anew.
        u.move_order = order_with_hours(&[(2, 1), (3, 1)], 0.12);
        repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(1, 2), HexCoord::new(1, 3)],
            &p,
        );
        u.move_order = order_with_hours(&[(1, 2), (1, 3)], 0.0);
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(3, 1)],
            &p,
        );
        assert!(
            (carry - 0.12).abs() < 1e-6,
            "re-banked progress restores again: {carry}"
        );
    }

    #[test]
    fn stand_by_banks_progress_and_reissue_restores_it() {
        // Stand-by (§7.4) cancels the march but banks its step progress:
        // re-issuing toward the same first step picks the investment up.
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        let p = CombatParams::default();
        u.move_order = order_with_hours(&[(2, 1), (3, 1)], 0.12);
        u.stand_by();
        assert!(u.move_order.is_none());
        assert_eq!(
            u.step_memory,
            Some((HexCoord::new(2, 1), 0.12)),
            "stand-by banks the cancelled march's progress"
        );
        let carry = repoint_carry_hours(
            &g,
            &no_control(),
            &mut u,
            &[HexCoord::new(2, 1), HexCoord::new(4, 1)],
            &p,
        );
        assert!(
            (carry - 0.12).abs() < 1e-6,
            "re-issue restores the stood-by progress: {carry}"
        );
        assert_eq!(u.step_memory, None);
    }

    #[test]
    fn refresh_clamps_carried_hours_to_the_new_first_step_cost() {
        // The detour's first step is CHEAPER than the hours banked on the
        // old one: the carry is capped at the new step's cost, restoring
        // the §6.2 invariant `hours < first-step cost`.
        let (g, mut u, blocker, wall) = walled_scenario();
        // (2,1) as a mountain makes the 0.333 h bank plausible (a 4.75/12
        // ≈ 0.396 h step, two budgets in); the southern detour runs on
        // road (1.0/12 ≈ 0.083 h per first step) — far under the bank.
        let mut g = g;
        g.set_terrain(HexCoord::new(2, 1), Terrain::Mountain);
        for r in 2..=3 {
            for q in 0..=4 {
                g.cell_mut(HexCoord::new(q, r)).unwrap().road = true;
            }
        }
        u.move_order.as_mut().unwrap().hours = 1.0 / 3.0;
        let p = CombatParams::default();
        let units_view = vec![u.clone(), blocker, wall];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed, "parked blocker → detour adopted");
        let o = u.move_order.as_ref().unwrap();
        let first = o.path[0];
        let cap =
            crate::pathfinding::step_hours(&g, &u, &no_control(), u.position, first, &p).unwrap();
        assert!(
            o.hours <= cap + 1e-6,
            "carried hours {} exceed the new first step's cost {}",
            o.hours,
            cap
        );
        assert!(o.hours > 0.0, "the carry itself is preserved");
    }
}

#[cfg(test)]
mod chain_tests {
    use super::*;
    use crate::terrain::Terrain;
    use crate::unit::{MoveLeg, MoveOrder, UnitType};

    fn grid(w: usize, h: usize) -> HexGrid {
        HexGrid::new(w, h, Terrain::Plains)
    }

    fn unit(id: usize, ty: UnitType, side: Side, q: i32, r: i32) -> BattalionUnit {
        BattalionUnit::new(id, format!("U{id}"), ty, side, HexCoord::new(q, r))
    }

    /// Chain order: one active segment to `path_end` plus queued legs
    /// (each described by its own polyline INCLUDING its start hex).
    fn chain(path: &[(i32, i32)], legs: &[&[(i32, i32)]]) -> MoveOrder {
        let queue = legs
            .iter()
            .map(|pts| {
                let hexes: Vec<HexCoord> = pts
                    .iter()
                    .map(|(q, r)| HexCoord::new(*q, *r))
                    .collect();
                MoveLeg {
                    dest: *hexes.last().unwrap(),
                    preview: hexes,
                }
            })
            .collect();
        MoveOrder {
            path: path.iter().map(|(q, r)| HexCoord::new(*q, *r)).collect(),
            hours: 0.0,
            queue,
        }
    }

    /// A chain leg rolls over WITHIN the same turn's budget: a motorized
    /// battalion (2 plains hexes per turn) reaches its first waypoint and
    /// keeps marching into the next leg in the same advance. Intermediate
    /// waypoints emit WaypointReached (no log-worthy Arrived), never
    /// release the order, and the fresh leg starts at zero hours.
    #[test]
    fn chain_leg_activates_and_continues_within_the_turn() {
        let mut g = grid(8, 8);
        // Road-surfaced path (§4.2): motor pays the base 1.0/hex, so the
        // 2 km budget covers two steps in the turn.
        for (q, r) in [(2, 1), (3, 1), (4, 1)] {
            g.cell_mut(HexCoord::new(q, r)).unwrap().road = true;
        }
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = Some(chain(&[(2, 1)], &[&[(2, 1), (3, 1), (4, 1)]]));
        let mut units = vec![u];
        let p = CombatParams::default();

        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        // First step: (1,1) → (2,1) completes the FIRST leg; the second
        // leg activates on the remaining budget and marches to (3,1).
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        let order = units[0].move_order.as_ref().unwrap();
        assert_eq!(order.path.last(), Some(&HexCoord::new(4, 1)));
        assert!(order.queue.is_empty(), "last leg now active");
        assert_eq!(order.hours, 0.0, "fresh leg starts at zero hours");
        assert!(
            ev.contains(&MovementEvent::WaypointReached { unit_id: 0 }),
            "intermediate waypoint announced as WaypointReached: {ev:?}"
        );
        assert!(
            !ev.contains(&MovementEvent::Arrived { unit_id: 0 }),
            "a chain with legs left never Arrives: {ev:?}"
        );

        // Next turn: the final leg completes → the order ends properly.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(4, 1));
        assert!(units[0].move_order.is_none());
        assert!(ev.contains(&MovementEvent::Arrived { unit_id: 0 }));
    }

    /// Budget exhausted exactly at a waypoint: the next leg is planned
    /// (activated) but not marched — the following turn walks it.
    #[test]
    fn chain_waits_for_the_next_turn_when_budget_runs_out() {
        let g = grid(8, 8);
        // Plain infantry off-road: 1.5/4 = 0.375 h per step against a 1/6 h
        // budget — three turns for the first hex, ~2.25 per hex after.
        let mut u = unit(0, UnitType::Infantry, Side::Attacker, 1, 1);
        u.move_order = Some(chain(&[(2, 1)], &[&[(2, 1), (3, 1)]]));
        let mut units = vec![u];
        let p = CombatParams::default();

        // Turn 1: 1/6 h invested — no hex crossed.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert!(!ev.contains(&MovementEvent::WaypointReached { unit_id: 0 }));

        // Turn 2: 2/6 = 0.333 < 0.375 — still short of the step.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        assert!(!ev.contains(&MovementEvent::WaypointReached { unit_id: 0 }));

        // Turn 3: 3/6 = 0.5 ≥ 0.375 → waypoint reached, next leg planned,
        // remaining budget (0.5 − 0.375 = 0.125) too small to start it.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(2, 1));
        assert!(ev.contains(&MovementEvent::WaypointReached { unit_id: 0 }));
        assert!(!ev.contains(&MovementEvent::Arrived { unit_id: 0 }));
        let order = units[0].move_order.as_ref().unwrap();
        assert_eq!(order.path.last(), Some(&HexCoord::new(3, 1)));
        assert!(order.queue.is_empty());

        // Turn 4: 0.125 + 1/6 = 0.292 < 0.375 — still waiting.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(2, 1));
        assert!(!ev.contains(&MovementEvent::Arrived { unit_id: 0 }));

        // Turn 5: 0.458 ≥ 0.375 → final arrival.
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        assert!(units[0].move_order.is_none());
        assert!(ev.contains(&MovementEvent::Arrived { unit_id: 0 }));
    }

    /// Walking into contact kills the WHOLE chain, not just the active
    /// leg — the queued legs ride the same single-order slot and die with
    /// it (§6.5).
    #[test]
    fn chain_cleared_entirely_by_contact() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = Some(chain(&[(2, 1)], &[&[(2, 1), (4, 1)]]));
        let enemy = unit(1, UnitType::Infantry, Side::Defender, 2, 2); // adjacent to (2,1)
        let mut units = vec![u, enemy];
        let p = CombatParams::default();

        // §6.5 control doubles the covered first step (1.75 → 3.5 km against
        // the 2 km/turn budget): the chain reaches (2,1) on the second turn
        // and the contact kills it — queued legs included.
        let _ = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(2, 1));
        assert!(
            units[0].move_order.is_none(),
            "contact consumes the chain INCLUDING its queued legs"
        );
        assert!(ev.contains(&MovementEvent::MadeContact { unit_id: 0 }));
    }

    /// Friendly congestion preserves the whole chain (progress AND the
    /// queued legs) — waiting is not an order change.
    #[test]
    fn chain_survives_blocked_wait() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = Some(chain(&[(2, 1)], &[&[(2, 1), (4, 1)]]));
        let friend = unit(1, UnitType::Infantry, Side::Attacker, 2, 1);
        let mut units = vec![u, friend];
        let p = CombatParams::default();

        let ev = advance_move_orders(&g, &mut units, Side::Attacker, &p);
        assert_eq!(units[0].position, HexCoord::new(1, 1));
        let order = units[0].move_order.as_ref().unwrap();
        assert_eq!(order.path.last(), Some(&HexCoord::new(2, 1)));
        assert_eq!(order.queue.len(), 1, "queued leg survives the wait");
        assert!(ev.contains(&MovementEvent::Blocked {
            unit_id: 0,
            blocker_id: 1
        }));
    }

    /// Refresh between legs: an empty standing path with queued legs
    /// activates the next leg against the refresh view instead of
    /// dropping the order ("already there" only applies to finished
    /// chains).
    #[test]
    fn refresh_activates_waiting_chain_leg() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        u.move_order = Some(chain(&[], &[&[(1, 1), (2, 1), (3, 1)]]));
        let p = CombatParams::default();
        let units_view = vec![u.clone()];

        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed, "the waiting leg was planned");
        let order = u.move_order.unwrap();
        assert_eq!(order.path.last(), Some(&HexCoord::new(3, 1)));
        assert!(order.queue.is_empty());
    }

    /// Refresh adoptions (detour rewrites of the ACTIVE segment) carry
    /// the queued legs untouched — only the route to the current
    /// destination is re-judged.
    #[test]
    fn refresh_adoption_preserves_queued_legs() {
        let g = grid(8, 8);
        let mut u = unit(0, UnitType::Motorized, Side::Attacker, 1, 1);
        // Active segment deliberately long (cheaper route exists later via
        // the friend?) — simplest: same-dest shorter route becomes
        // available after the friend parks; here we only assert that a
        // rewrite keeps the queue.
        u.move_order = Some(chain(&[(2, 1), (3, 1), (4, 1)], &[&[(4, 1), (5, 1)]]));
        let p = CombatParams::default();
        // Block (2,1) with a parked friend → a detour is adopted even
        // though it is not cheaper (parked escape).
        let mut blocked = unit(1, UnitType::Infantry, Side::Attacker, 2, 1);
        blocked.move_order = None;
        let units_view = vec![blocked, u.clone()];
        let changed = refresh_move_order(&g, &mut u, &units_view, &p);
        assert!(changed, "parked blocker forces a detour");
        let order = u.move_order.unwrap();
        assert_eq!(order.path.last(), Some(&HexCoord::new(4, 1)), "same active dest");
        assert_eq!(order.queue.len(), 1, "queued legs ride the rewrite");
    }
}
