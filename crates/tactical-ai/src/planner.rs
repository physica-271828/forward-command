//! Three-layer decision planner (DESIGN §7.1).
//!
//! ```text
//! Layer 1: select_objective  — tactic → StrategicObjective (§7.2), tempered
//!                                by the global force ratio (§7.1).
//! Layer 2: assign_role       — battalion type + condition → UnitRole (§7.1,
//!                                §7.3 damaged-unit rule).
//! Layer 3: execute_unit      — per-unit command using tactical-core
//!                                pathfinding/LoS under the §7.3 constraints.
//! ```
//!
//! Planning is pure: input slices are never mutated (see `AiAction` docs).
//! All randomness flows through `XorShift64`, so a fixed seed reproduces a
//! turn exactly.

use std::cmp::Ordering;
use std::collections::HashSet;

use tactical_core::flag::FlagState;
use tactical_core::hex::HexCoord;
use tactical_core::pathfinding::find_path;
use tactical_core::{
    Attrs, BattalionUnit, CombatParams, HexGrid, Side, Terrain, UnitState, UnitType, XorShift64,
};

use crate::action::AiAction;
use crate::deploy::river_between;
use crate::tactic::CombatTactic;

/// §7.3: units below 30% org are withdrawn from the frontline when possible.
const DAMAGED_ORG_RATIO: f32 = 0.30;
/// §7.3: refuse assault when locally outnumbered 3:1 (and §7.1: go globally
/// defensive when the whole force is outnumbered 3:1).
const REFUSE_ODDS: f32 = 3.0;
/// §7.3: artillery stays 1–2 hexes behind the frontline, i.e. at least this
/// many hexes away from the nearest enemy.
const ARTILLERY_MIN_STANDOFF: i32 = 2;
/// §6.11: how close a hold-role attacker must be to a cleared flag zone to
/// walk in and raise the flag. Kept small so only the local shoulder line
/// occupies — a line-wide surge toward the city is what sealed the vanguard
/// in friendly-packed hexes in the headless Warsaw trace.
const FLAG_OCCUPY_REACH: i32 = 6;

/// Destination hysteresis radius (§7.2 order persistence): a standing
/// order's destination is kept while the freshly computed goal stays
/// within this distance. Position-dependent goals (the intel ring, a
/// moving enemy) would otherwise re-base the destination every turn and
/// wipe the invested march hours at the apply layer — the progress bar
/// sat frozen at one turn's budget while the order was re-issued
/// ("has order, bar not growing" — the live advance-order report).
const GOAL_HYSTERESIS_RADIUS: i32 = 3;

// ────────────────────────────────────────────────────────────────────────────
// Division-order targets
// ────────────────────────────────────────────────────────────────────────────

/// A player-issued division order's goal, injected into the per-unit
/// planning. `None` (the whole-side AI paths) leaves every
/// behavior exactly as before — the enemy AI is unaffected.
///
/// The order overrides the MOVEMENT goal and biases assault/fire selection;
/// everything else (roles, damage thresholds, §7.3 constraints) stays
/// tactic-driven. The division-order state machine (maneuver vs. hold-back
/// phase) lives in the game controller, which picks the tactic card per
/// phase; this type only carries the TARGET.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivOrderTarget {
    /// 占领/固守 (Seize): march on the hex, assault what blocks the path.
    /// Used for both the maneuver phase (march on the point) and the
    /// hold-back phase (defense card + the same anchor as the defensive
    /// ground).
    Seize { hex: HexCoord },
    /// 歼敌 (Engage): pursue the target unit through its last known
    /// position; prefer assaulting / shelling the target itself.
    Engage { unit: usize, last_pos: HexCoord },
}

// ────────────────────────────────────────────────────────────────────────────
// Layer 1 — strategic objective (§7.2)
// ────────────────────────────────────────────────────────────────────────────

/// High-level goal for the turn (§7.1 layer 1). Derived from the tactic via
/// the §7.2 mapping table; the global force evaluation can downgrade an
/// aggressive objective to `Hold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategicObjective {
    PushCenter,
    Hold,
    FlankLeft,
    FlankRight,
    Delay,
    DeepPenetration,
    Attrition,
    HitAndRun,
    Pincer,
    ExploitGaps,
}

// ────────────────────────────────────────────────────────────────────────────
// Layer 2 — unit roles (§7.1)
// ────────────────────────────────────────────────────────────────────────────

/// Per-battalion assignment for the turn (§7.1 layer 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitRole {
    /// Close-combat battalion that seeks and assaults the enemy.
    Assault,
    /// Ranged battalion (artillery / AT / AA) providing fire support from
    /// 1–2 hexes behind the frontline (§7.3).
    SupportFire,
    /// Line-holding battalion; engages only when engaged or clearly favored.
    HoldPosition,
    /// Armor swinging around a flank (encirclement / elastic counter).
    Flank,
    /// Recon probing the far flanks (§7.2 infiltration_assault).
    Probe,
    /// Damaged (<30% org) battalion withdrawing from the frontline (§7.3).
    Reserve,
    /// Support company staying near the combat battalion it buffs (§7.3,
    /// §6.8 co-located buffs).
    Attached,
    /// The single unit left holding the front during a tactical withdrawal
    /// (§7.2 tactical_withdrawal: one rearguard covers the retreat).
    Rearguard,
    /// Division HQ (§6.13): never attacks — shadows its division,
    /// staying inside command range of the centroid while keeping off the
    /// frontline.
    Headquarters,
}

// ────────────────────────────────────────────────────────────────────────────
// Planning context
// ────────────────────────────────────────────────────────────────────────────

/// Shared read-only view for one turn's planning.
struct Ctx<'a> {
    grid: &'a HexGrid,
    own: &'a [BattalionUnit],
    enemy: &'a [BattalionUnit],
    /// Which side plans (flag goals are attacker-only, §7.3).
    side: Side,
    /// Passive friendlies (§7.5): same-side battalions the
    /// planner may NOT command (the player's own units and other allied
    /// nations' slices). They join `all` (they occupy hexes for pathing /
    /// ZOC) and count as nearby friends in the odds/trade statistics, but
    /// no returned `AiAction` ever carries a passive unit's id.
    passive: Vec<BattalionUnit>,
    /// Both sides concatenated (own + enemy + passive friendlies) —
    /// pathfinding needs one slice for occupancy and ZOC. Positions are the
    /// start-of-turn ones; proposals do not move units (pure planning), so
    /// `reserved` prevents double-booking.
    all: Vec<BattalionUnit>,
    /// The OPPONENT's PHYSICAL units —
    /// visible or not — fed by the caller (headless / the game's AI turn).
    /// Used ONLY by the blind-assault probe (`try_assault`): a wall the fog
    /// hides still blocks the route, and the halt at its hex is the only
    /// signal the planner gets. Nothing else reads it — fire targeting,
    /// pathing, odds and intel stay strictly fog-limited.
    physical_foes: Vec<BattalionUnit>,
    /// Destination hexes already claimed by earlier proposals this turn
    /// (§6.9: one battalion per hex).
    reserved: HashSet<HexCoord>,
    /// Blind-man's intel: pre-battle knowledge of the enemy
    /// deployment zone the side marches on when no enemy is visible.
    /// `intel_goal` aims at its CENTROID — a fixed, correct direction (see
    /// the method doc; nearest-hex goals spun and drifted, Poland battery
    /// 9452/9439 traces). Empty = no intel; a fog-limited
    /// planner then freezes in place.
    intel: Vec<HexCoord>,
    /// §6.11: the battle's flag zones — the ATTACKER's primary
    /// blind-march / fire goals when they exist; the DEFENDER's anchors for
    /// the tiered flag-defense response (§7.3).
    flags: Vec<tactical_core::flag::FlagZone>,
    /// A player-issued division order (Seize / Engage). When set,
    /// the flag response is skipped — the player's explicit command
    /// outranks the flag doctrine — and movement/fire goals point at the
    /// order target.
    order: Option<DivOrderTarget>,
}

impl<'a> Ctx<'a> {
    fn new(
        grid: &'a HexGrid,
        own: &'a [BattalionUnit],
        enemy: &'a [BattalionUnit],
        side: Side,
    ) -> Self {
        let all: Vec<BattalionUnit> = own.iter().chain(enemy.iter()).cloned().collect();
        Ctx {
            grid,
            own,
            enemy,
            side,
            passive: Vec::new(),
            all,
            physical_foes: Vec::new(),
            reserved: HashSet::new(),
            intel: Vec::new(),
            flags: Vec::new(),
            order: None,
        }
    }

    /// The whole same-side force for STATISTICS (§7.5): own units
    /// plus passive friendlies — e.g. the own–enemy axis for a flanking
    /// waypoint. ACTION assignment always iterates `own` alone; passive
    /// units never receive orders.
    fn friends(&self) -> Vec<BattalionUnit> {
        if self.passive.is_empty() {
            return self.own.to_vec();
        }
        self.own
            .iter()
            .chain(self.passive.iter())
            .cloned()
            .collect()
    }

    /// The division-order MOVEMENT goal: a
    /// Seize/Engage order spreads the division across the objective AREA —
    /// the point plus its ring-1/ring-2 hexes — instead of funneling every
    /// battalion onto the single goal hex (a whole division routing to one
    /// hex queued into a single corridor and stacked on one city corner,
    /// Warsaw siege). Each unit picks the nearest free, unoccupied,
    /// unreserved hex of the area; the point itself carries a 3-hex
    /// preference so the nearest battalion marches ONTO it — someone must
    /// occupy the hex to declare it seized. Always takes priority over the
    /// nearest-enemy / blind-march goals — a commanded division marches on
    /// its objective, not on whatever it happens to see.
    fn order_goal(&self, unit: &BattalionUnit) -> Option<HexCoord> {
        let (center, max_ring) = match self.order {
            Some(DivOrderTarget::Seize { hex }) => (hex, 2),
            Some(DivOrderTarget::Engage { last_pos, .. }) => (last_pos, 1),
            None => return None,
        };
        // Hysteresis: keep the standing order's destination while
        // it still lands inside the objective area and stays legal. A fresh
        // spread every turn let reservation churn from earlier proposals
        // flap destinations, and the execution layer's re-affirm path
        // punished every flip by resetting invested march hours — any unit
        // slower than 1 hex/turn was pinned forever (arena mirror trace:
        // 8.INF zero displacement in 144 turns).
        if let Some(dest) = unit
            .move_order
            .as_ref()
            .and_then(|o| o.path.last())
            .copied()
        {
            if dest.distance(center) <= max_ring && self.goal_hex_free(dest, unit.id) {
                return Some(dest);
            }
        }
        self.spread_goal(unit.position, center, max_ring)
    }

    /// A keepable standing destination — on-map, passable, not
    /// reserved by an earlier proposal this turn, and not held by another
    /// combat-effective unit (own or enemy).
    fn goal_hex_free(&self, h: HexCoord, self_id: usize) -> bool {
        let passable = self.grid.cell(h).map(|c| c.is_passable).unwrap_or(false);
        if !passable || self.reserved.contains(&h) {
            return false;
        }
        !self
            .all
            .iter()
            .any(|u| u.id != self_id && u.is_combat_effective() && u.position == h)
    }

    /// The EXACT order point — fire missions and blind bombardment converge
    /// here (movement spreads, shells concentrate).
    fn order_fire_goal(&self) -> Option<HexCoord> {
        match self.order {
            Some(DivOrderTarget::Seize { hex }) => Some(hex),
            Some(DivOrderTarget::Engage { last_pos, .. }) => Some(last_pos),
            None => None,
        }
    }

    /// The order goal if it lies inside the unit's firing envelope — the
    /// siege/bombardment fallback for a commanded division's guns.
    fn order_goal_in_range(&self, unit: &BattalionUnit) -> Option<HexCoord> {
        let hex = self.order_fire_goal()?;
        let d = unit.position.distance(hex);
        (d <= unit.attack_range && d >= unit.min_attack_range()).then_some(hex)
    }

    /// The nearest hex of the objective area around `center` (the point +
    /// rings 1..=`max_ring`): passable, not held by a combat-effective unit
    /// (own or enemy), not already reserved by an earlier proposal this
    /// turn. The point itself scores 3 hexes of preference — the nearest
    /// battalion takes it so the seizure actually completes — and ford
    /// hexes are never chosen as destinations (river discipline).
    /// Falls back to the exact point when the area is fully blocked (the
    /// caller's approach then uses its adjacent-hex fallback).
    fn spread_goal(&self, h: HexCoord, center: HexCoord, max_ring: i32) -> Option<HexCoord> {
        let occupied: HashSet<HexCoord> = self
            .all
            .iter()
            .filter(|u| u.is_combat_effective())
            .map(|u| u.position)
            .collect();
        let mut best: Option<HexCoord> = None;
        let mut best_score = f32::INFINITY;
        for c in center.hexes_in_range(max_ring) {
            if self.grid.cell(c).is_none() {
                continue; // off-map
            }
            if occupied.contains(&c) || self.reserved.contains(&c) {
                continue;
            }
            let ford = self
                .grid
                .cell(c)
                .map(|g| g.terrain == Terrain::River)
                .unwrap_or(false);
            let mut score = h.distance(c) as f32;
            if c == center {
                score -= 3.0;
            }
            if ford {
                score += 100.0;
            }
            let take = match best {
                Some(b) => {
                    score < best_score
                        || (score == best_score && (c.q < b.q || (c.q == b.q && c.r < b.r)))
                }
                None => true,
            };
            if take {
                best = Some(c);
                best_score = score;
            }
        }
        best.or(Some(center))
    }

    /// The blind-march goal: the intel hex 3-8 hexes ahead of the unit — the
    /// band where the defender's deployed line actually sits (ai_deploy puts
    /// it ~3 hexes inside the zone). The direction comes from each unit's
    /// own position, so a whole line fans into the zone and SWEEPS it, not a
    /// single corridor (9439: a vanguard marched straight to the centroid
    /// 13+ hexes past the defender line and never touched it). The depth
    /// floor kills the three earlier failure modes (Poland battery):
    ///  - edge spin (9452): a goal at distance 1-2 parked units ON the zone
    ///    edge, 6+ hexes from the defender line - never in contact;
    ///  - boundary drift (9439): goals at distance 1 let the q tie-break
    ///    march units ALONG the edge and, once inside, toward the far corner;
    ///  - centroid stall: a deep fixed goal left the vanguard
    ///    sitting at the zone centre with no contact.
    /// A unit already deeper than 8 hexes (or with no ring to aim at) falls
    /// back to the centroid - a fixed, correct direction that keeps pushing.
    fn intel_goal(&self, h: HexCoord) -> Option<HexCoord> {
        if self.intel.is_empty() {
            return None;
        }
        let (sq, sr) = self.intel.iter().fold((0i64, 0i64), |(aq, ar), i| {
            (aq + i.q as i64, ar + i.r as i64)
        });
        let n = self.intel.len() as f32;
        let (cq, cr) = (sq as f32 / n, sr as f32 / n);
        let ring: Vec<HexCoord> = self
            .intel
            .iter()
            .copied()
            .filter(|i| (3..=8).contains(&h.distance(*i)))
            .collect();
        if !ring.is_empty() {
            return ring.into_iter().min_by(|a, b| {
                let d2 = |x: &HexCoord| {
                    let (dq, dr) = (x.q as f32 - cq, x.r as f32 - cr);
                    dq * dq + dr * dr
                };
                // Among equally-near ring hexes prefer the one
                // closest to the zone CENTRE — the old (q, r) tie-break
                // systematically drifted the march toward the zone's NW
                // corner and a besieging force swept PAST the city (Warsaw
                // siege trace: the front wheeled north at q~16-18 for 150
                // turns while the city sat at q30-35, never engaged).
                h.distance(*a)
                    .cmp(&h.distance(*b))
                    .then(d2(a).partial_cmp(&d2(b)).unwrap_or(Ordering::Equal))
                    .then(a.q.cmp(&b.q))
                    .then(a.r.cmp(&b.r))
            });
        }
        self.intel.iter().copied().min_by(|a, b| {
            let d2 = |x: &HexCoord| {
                let (dq, dr) = (x.q as f32 - cq, x.r as f32 - cr);
                dq * dq + dr * dr
            };
            d2(a)
                .partial_cmp(&d2(b))
                .unwrap_or(Ordering::Equal)
                .then(a.q.cmp(&b.q))
                .then(a.r.cmp(&b.r))
        })
    }

    /// The blind march / blind-fire goal (§7.3): when the battle
    /// HAS flag zones, the ATTACKER aims at them — the besieged city / the
    /// field objectives — instead of the raw pre-battle intel ring. The
    /// defender never uses this (its intel is empty; the flags are ITS
    /// ground).
    fn blind_goal(&self, h: HexCoord) -> Option<HexCoord> {
        if self.side == Side::Attacker && !self.flags.is_empty() {
            return self.flag_goal(h);
        }
        self.intel_goal(h)
    }

    /// The nearest flag zone hex in the 3-8 hex band ahead of `h` (the
    /// same geometry as `intel_goal`: the band floor kills edge spin, the
    /// centre tie-break keeps a long front from drifting along the zone),
    /// falling back to the nearest zone hex of all flags.
    fn flag_goal(&self, h: HexCoord) -> Option<HexCoord> {
        let zones: Vec<HexCoord> = self
            .flags
            .iter()
            .flat_map(|f| f.zone.iter().copied())
            .collect();
        if zones.is_empty() {
            return None;
        }
        let (sq, sr) = zones.iter().fold((0i64, 0i64), |(aq, ar), z| {
            (aq + z.q as i64, ar + z.r as i64)
        });
        let n = zones.len() as f32;
        let (cq, cr) = (sq as f32 / n, sr as f32 / n);
        let ring: Vec<HexCoord> = zones
            .iter()
            .copied()
            .filter(|z| (3..=8).contains(&h.distance(*z)))
            .collect();
        let pick = |iter: Vec<HexCoord>| {
            iter.into_iter().min_by(|a, b| {
                let d2 = |x: &HexCoord| {
                    let (dq, dr) = (x.q as f32 - cq, x.r as f32 - cr);
                    dq * dq + dr * dr
                };
                h.distance(*a)
                    .cmp(&h.distance(*b))
                    .then(d2(a).partial_cmp(&d2(b)).unwrap_or(Ordering::Equal))
                    .then(a.q.cmp(&b.q))
                    .then(a.r.cmp(&b.r))
            })
        };
        if !ring.is_empty() {
            return pick(ring);
        }
        pick(zones)
    }

    /// The capture progress of the flag whose zone contains `h` (0 when no
    /// flag covers the hex) — the defender's in-zone hold gate (§7.3).
    fn zone_flag_progress(&self, h: HexCoord) -> i32 {
        self.flags
            .iter()
            .find(|f| f.zone.contains(&h))
            .map(|f| f.progress)
            .unwrap_or(0)
    }

    /// §6.3 precise-fire rule: a mission
    /// resolves PRECISE (full damage on the aim hex) only when the aim hex
    /// holds a currently-VISIBLE enemy — `ctx.enemy` is already the
    /// fog-filtered view, so that is simply "an enemy stands there". The
    /// player-facing equivalent: right-clicking a visible enemy = precise;
    /// the F-key barrage and intel-goal fire = area (weighted 4/10-1/10
    /// zone). Mirrors the
    /// registration-time check (game.rs / headless.rs), so what the
    /// planner ranks is what happens.
    fn precise_hex(&self, h: HexCoord) -> bool {
        self.enemy
            .iter()
            .any(|e| e.is_combat_effective() && e.position == h)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// TacticalAi
// ────────────────────────────────────────────────────────────────────────────

/// Tactic-driven enemy AI (§7). One instance plans one side's turns for the
/// whole battle; the RNG keeps tie-breaks deterministic per seed.
///
/// **Fog of war:** the planner is fog-agnostic — the CALLER
/// pre-filters `enemy_units` to what the side can currently see (the `all`
/// occupancy view derives from the same slices), so hidden enemies neither
/// block routes nor draw fire. `plan_turn_toward` supplies the blind-man's
/// objective for the zero-contact case.
/// Clone: checkpoint snapshots (restart/rollback).
#[derive(Clone)]
pub struct TacticalAi {
    pub side: Side,
    pub tactic: CombatTactic,
    pub params: CombatParams,
    pub rng: XorShift64,
}

impl TacticalAi {
    pub fn new(side: Side, tactic: CombatTactic, seed: u64) -> Self {
        // Default is "no doctrine": a defender holds and engages the
        // nearest enemy, but an attacker handed the baseline card must
        // still prosecute the plain advance — otherwise a generic vanilla
        // attack roll (basic_attack) parks outside the enemy's view and
        // never moves. Fold the attacker's Default onto Assault, the
        // plain-advance card; the card's identity elsewhere (UI panel,
        // deploy profile) stays Default.
        let tactic = if tactic == CombatTactic::Default && side == Side::Attacker {
            CombatTactic::Assault
        } else {
            tactic
        };
        TacticalAi {
            side,
            tactic,
            params: CombatParams::default(),
            rng: XorShift64::new(seed),
        }
    }

    /// Plan one full turn for `own_units` against `enemy_units` (§6.12: the
    /// moving side completes all its actions). Returns proposed actions in
    /// execution order, always terminated by `AiAction::EndTurn`.
    ///
    /// Units that are not combat-effective (retreating / eliminated /
    /// surrendered / withdrawn, §6.8) are skipped — the caller's state
    /// machine owns their behavior. Dead enemies are ignored everywhere.
    pub fn plan_turn(
        &mut self,
        grid: &HexGrid,
        own_units: &[BattalionUnit],
        enemy_units: &[BattalionUnit],
    ) -> Vec<AiAction> {
        self.plan_turn_toward(grid, own_units, enemy_units, None)
    }

    /// `objective`: pre-battle intel (typically the enemy deployment zone)
    /// the side marches on when no enemy is visible — without it a
    /// fog-limited planner has no goal and freezes.
    pub fn plan_turn_toward(
        &mut self,
        grid: &HexGrid,
        own_units: &[BattalionUnit],
        enemy_units: &[BattalionUnit],
        objective: Option<HexCoord>,
    ) -> Vec<AiAction> {
        let intel = objective.map(|h| vec![h]).unwrap_or_default();
        self.plan_turn_zone(
            grid,
            own_units,
            enemy_units,
            (!intel.is_empty()).then_some(&intel),
        )
    }

    /// `intel_zone`: pre-battle knowledge of the enemy deployment zone —
    /// the whole zone (its centroid becomes the blind-march goal, so a
    /// long stitched front pushes coherently — Warsaw siege tuning).
    pub fn plan_turn_zone(
        &mut self,
        grid: &HexGrid,
        own_units: &[BattalionUnit],
        enemy_units: &[BattalionUnit],
        intel_zone: Option<&[HexCoord]>,
    ) -> Vec<AiAction> {
        self.plan_turn_flags(grid, own_units, enemy_units, intel_zone, None, None)
    }

    /// Flag-aware planning (§7.3): `flags` = the battle's flag
    /// board (§6.11). When flags exist, the ATTACKER's blind march and
    /// blind fire aim at the flag zones instead of the raw intel ring, and
    /// the DEFENDER scales its reaction to the per-flag capture progress:
    ///
    /// - < 1/3: normal doctrine;
    /// - 1/3–2/3: second-line units reroute to SCREEN the threatened flag
    ///   (reusing the `screen_threat` primitive around the flag anchor);
    /// - beyond 2/3: nearby line units COUNTERATTACK into the zone to
    ///   press the control ratio;
    ///
    /// and a flag zone at ≥ 1/3 progress is never ceded: an in-zone
    /// defender holds instead of falling back. Threats are ordered by
    /// progress (highest first), never split flatly.
    ///
    /// **Passive friendlies (DESIGN §7.5):** `passive_friendlies`
    /// are same-side battalions the planner may NOT command — the player's
    /// own units and other allied nations' slices in a multi-nation force.
    /// They join the occupancy view (`ctx.all`), so they block pathing and
    /// ZOC like any unit, and they count as nearby friends in the
    /// odds/trade statistics (the §7.3 local 3:1 gate, the storm rule, the
    /// §7.1 global force ratio, rocket blast safety, the flank axis) — but
    /// they never receive actions: no returned `AiAction` carries a passive
    /// unit's id. `None` (the whole-side enemy AI / headless paths) leaves
    /// behavior exactly as before.
    pub fn plan_turn_flags(
        &mut self,
        grid: &HexGrid,
        own_units: &[BattalionUnit],
        enemy_units: &[BattalionUnit],
        intel_zone: Option<&[HexCoord]>,
        flags: Option<&FlagState>,
        passive_friendlies: Option<&[BattalionUnit]>,
    ) -> Vec<AiAction> {
        self.plan_turn_full(
            grid,
            own_units,
            enemy_units,
            intel_zone,
            flags,
            passive_friendlies,
            None,
        )
    }

    /// The full planner entry: `physical_foes` is
    /// the opponent's PHYSICAL unit list (fog-independent) feeding ONLY the
    /// blind-assault probe in `try_assault` — a dark-fog wall that halts
    /// the march is probed when beaten/overwhelmed. `None` keeps the
    /// planner exactly as before (all tests / passive-only callers).
    pub fn plan_turn_full(
        &mut self,
        grid: &HexGrid,
        own_units: &[BattalionUnit],
        enemy_units: &[BattalionUnit],
        intel_zone: Option<&[HexCoord]>,
        flags: Option<&FlagState>,
        passive_friendlies: Option<&[BattalionUnit]>,
        physical_foes: Option<&[BattalionUnit]>,
    ) -> Vec<AiAction> {
        let mut ctx = Ctx::new(grid, own_units, enemy_units, self.side);
        if let Some(p) = passive_friendlies {
            ctx.passive = p.to_vec();
            ctx.all.extend(p.iter().cloned());
        }
        if let Some(f) = physical_foes {
            ctx.physical_foes = f.iter().filter(|u| u.side != self.side).cloned().collect();
        }
        if let Some(z) = intel_zone {
            ctx.intel.extend_from_slice(z);
        }
        if let Some(fs) = flags {
            ctx.flags.clone_from(&fs.flags);
        }

        // Layer 1: strategic objective (§7.2 table + §7.1 global evaluation).
        // The §7.1 global force ratio counts the WHOLE side —
        // passive friendlies fight too, they are just not this planner's to
        // command (a weak allied slice propped up by a strong player force
        // does not refuse the battle).
        let objective = if ctx.passive.is_empty() {
            self.select_objective(own_units, enemy_units)
        } else {
            self.select_objective(&ctx.friends(), enemy_units)
        };

        // §7.2 tactical_withdrawal: choose the rearguard once per turn.
        let rearguard_id = if self.tactic == CombatTactic::TacticalWithdrawal {
            pick_rearguard(own_units, enemy_units)
        } else {
            None
        };

        // §7.3: the defender's tiered flag response — these
        // overrides win over the normal role actions below.
        let flag_actions = self.plan_flag_defense(&mut ctx);

        let mut actions = Vec::new();
        for unit in own_units {
            if !unit.is_combat_effective() {
                continue; // state machine handles non-Active units (§6.8)
            }
            // §6.2: a unit that already acted this turn holds position.
            if unit.acted {
                actions.push(AiAction::Hold { unit_id: unit.id });
                continue;
            }
            if let Some((_, action)) = flag_actions.iter().find(|(id, _)| *id == unit.id) {
                if let AiAction::MoveUnit { path, .. } = action {
                    if let Some(dest) = path.last() {
                        ctx.reserved.insert(*dest);
                    }
                }
                actions.push(action.clone());
                continue;
            }

            // Layer 2: role assignment (§7.1).
            let role = self.assign_role(unit, rearguard_id);

            // Layer 3: action execution (§7.1, §7.3).
            let action = self.execute_unit(unit, role, objective, &ctx);
            if let AiAction::MoveUnit { path, .. } = &action {
                if let Some(dest) = path.last() {
                    ctx.reserved.insert(*dest);
                }
            }
            actions.push(action);
        }

        actions.push(AiAction::EndTurn);
        actions
    }

    /// Plan one turn for ONE division under a player-issued division order
    /// (DESIGN §7.4): `own_units` is the division's own slice
    /// (caller-filtered; per-division planning deliberately has no
    /// cross-division coordination) and `enemy_units` the visible enemies
    /// (fog pre-filtered by the caller, same as the whole-side paths).
    ///
    /// The order overrides movement goals and biases assault/fire target
    /// selection ([`DivOrderTarget`]); the tactic card (chosen by the
    /// controller per phase: assault card while maneuvering, a defense card
    /// once the point is held) drives everything else. Flag-tier responses
    /// are SKIPPED — an explicit player command outranks the flag doctrine —
    /// but the flag zones still anchor the blind march (`flags`), so a
    /// besieged city draws a commanded advance.
    ///
    /// Returns actions WITHOUT the `EndTurn` marker: the player side's turn
    /// belongs to the player — this fills her units' orders only.
    pub fn plan_turn_div_order(
        &mut self,
        grid: &HexGrid,
        own_units: &[BattalionUnit],
        enemy_units: &[BattalionUnit],
        intel_zone: Option<&[HexCoord]>,
        flags: Option<&FlagState>,
        order: Option<DivOrderTarget>,
    ) -> Vec<AiAction> {
        // §7.4: the DIVISION SENSOR RADIUS — a
        // division-order plan reacts only to enemies within
        // `div_sensor_radius` of its own battalions, so an Advance pushes
        // and searches its OWN front instead of being pulled across the map
        // by enemies another division detected. Applies to all three order
        // kinds (no front/rear direction split). Far enemies are NOT
        // dropped from pathfinding occupancy (`ctx.all` below) — a route
        // still routes around them, they just never become targets.
        let radius = self.params.div_sensor_radius.max(1);
        let near: Vec<BattalionUnit> = enemy_units
            .iter()
            .filter(|e| {
                own_units
                    .iter()
                    .any(|u| u.position.distance(e.position) <= radius)
            })
            .cloned()
            .collect();
        let mut ctx = Ctx::new(grid, own_units, &near, self.side);
        let near_ids: HashSet<usize> = near.iter().map(|e| e.id).collect();
        for e in enemy_units {
            if !near_ids.contains(&e.id) {
                ctx.all.push(e.clone());
            }
        }
        if let Some(z) = intel_zone {
            ctx.intel.extend_from_slice(z);
        }
        if let Some(fs) = flags {
            ctx.flags.clone_from(&fs.flags);
        }
        ctx.order = order;

        let objective = self.select_objective(own_units, enemy_units);

        let mut actions = Vec::new();
        for unit in own_units {
            if !unit.is_combat_effective() {
                continue;
            }
            if unit.acted {
                actions.push(AiAction::Hold { unit_id: unit.id });
                continue;
            }
            // §7.4: the player's manual commands win — the
            // division AI never re-plans a manually-overridden battalion.
            if unit.manual_override {
                continue;
            }
            let role = self.assign_role(unit, None);
            let action = self.execute_unit(unit, role, objective, &ctx);
            if let AiAction::MoveUnit { path, .. } = &action {
                if let Some(dest) = path.last() {
                    ctx.reserved.insert(*dest);
                }
            }
            actions.push(action);
        }
        actions
    }

    /// §7.3: the defender's three-tier flag response. Returns
    /// `(unit_id, action)` overrides — out-of-contact line battalions are
    /// assigned to the threatened flags in DESCENDING progress order
    /// (never a flat split): the highest-progress flag takes its share of
    /// the nearest units first. Reinforcement (tier 1/3–2/3) screens the
    /// flag anchor at standoff 2; counterattack (> 2/3) marches INTO the
    /// zone to press the control ratio. Units inside a zone are never
    /// rerouted away (they hold — see `plan_hold_role`).
    ///
    /// Takes `&mut Ctx` because the batch reserves each destination in
    /// `ctx.reserved` AS it plans, so two flag reinforcements can never
    /// claim the same hex (§6.9 one-battalion-per-hex).
    fn plan_flag_defense(&self, ctx: &mut Ctx) -> Vec<(usize, AiAction)> {
        let mut out = Vec::new();
        if self.side != Side::Defender || ctx.flags.is_empty() {
            return out;
        }
        let cap = self.params.flag_progress_cap.max(1);
        // Pool: out-of-contact line battalions (no artillery / AT / AA /
        // HQ — those keep their fire-base or command behavior), and not
        // already inside a flag zone.
        let mut pool: Vec<usize> = ctx
            .own
            .iter()
            .filter(|u| u.is_combat_effective() && !u.acted)
            .filter(|u| {
                !u.is_hq() && !u.is_indirect_artillery() && !u.attrs.has(Attrs::AT | Attrs::AA)
            })
            .filter(|u| min_enemy_dist(u.position, ctx.enemy) > 1)
            .filter(|u| ctx.flags.iter().all(|f| !f.zone.contains(&u.position)))
            .map(|u| u.id)
            .collect();
        if pool.is_empty() {
            return out;
        }
        // Threat ordering: by progress, highest first.
        let mut ordered: Vec<&tactical_core::flag::FlagZone> =
            ctx.flags.iter().filter(|f| f.progress > 0).collect();
        ordered.sort_by(|a, b| {
            b.progress
                .cmp(&a.progress)
                .then(a.anchor.q.cmp(&b.anchor.q))
                .then(a.anchor.r.cmp(&b.anchor.r))
        });
        let total: i32 = ordered.iter().map(|f| f.progress).sum();
        for flag in ordered {
            if pool.is_empty() {
                break;
            }
            let tier = flag.progress as f32 / cap as f32;
            if tier < 1.0 / 3.0 {
                continue; // < 1/3: normal doctrine
            }
            // Progress-proportional share of the remaining pool, highest
            // progress claiming its units first.
            let share = ((pool.len() as f32) * (flag.progress as f32 / total as f32))
                .ceil()
                .max(1.0) as usize;
            for _ in 0..share.min(pool.len()) {
                // The nearest still-unassigned unit to this flag.
                let pick = pool.iter().enumerate().min_by_key(|(_, id)| {
                    let u = ctx.own.iter().find(|x| x.id == **id).unwrap();
                    flag.zone
                        .iter()
                        .map(|z| u.position.distance(*z))
                        .min()
                        .unwrap_or(i32::MAX)
                });
                let Some((idx, uid)) = pick else { break };
                let uid = *uid;
                pool.remove(idx);
                let unit = ctx.own.iter().find(|x| x.id == uid).unwrap();
                let action = if tier > 2.0 / 3.0 {
                    // Counterattack INTO the zone: press the control ratio.
                    self.approach(unit, ctx, flag.nearest_hex(unit.position), None)
                } else {
                    self.screen_threat(unit, ctx, Some(flag.anchor), 2)
                };
                if let Some(a) = action {
                    // Reserve the destination NOW — later pool members plan
                    // against an updated `reserved` (the &mut Ctx contract
                    // above).
                    if let AiAction::MoveUnit { path, .. } = &a {
                        if let Some(dest) = path.last() {
                            ctx.reserved.insert(*dest);
                        }
                    }
                    out.push((uid, a));
                }
            }
        }
        out
    }

    // ── Layer 1 ────────────────────────────────────────────────────────────

    /// §7.2 tactic → objective mapping. §7.1 layer 1 also evaluates the
    /// global situation: an aggressive tactic executed while outnumbered 3:1
    /// overall degrades to `Hold`. `pub(crate)` for the passive-friendlies
    /// test: the PushCenter-vs-Hold downgrade is behaviorally invisible
    /// outside the Attrition fire plan, so the merged own+passive slice is
    /// verified at this function directly.
    pub(crate) fn select_objective(
        &self,
        own_units: &[BattalionUnit],
        enemy_units: &[BattalionUnit],
    ) -> StrategicObjective {
        let base = match self.tactic {
            CombatTactic::Blitz => StrategicObjective::DeepPenetration,
            CombatTactic::ElasticDefense => StrategicObjective::Delay,
            CombatTactic::OverwhelmingFire => StrategicObjective::Attrition,
            CombatTactic::InfiltrationAssault => StrategicObjective::ExploitGaps,
            CombatTactic::MassCharge => StrategicObjective::PushCenter,
            CombatTactic::GuerrillaTactics => StrategicObjective::HitAndRun,
            CombatTactic::TacticalWithdrawal => StrategicObjective::Delay,
            CombatTactic::Encirclement => StrategicObjective::Pincer,
            CombatTactic::Default => StrategicObjective::Hold,
            // The four defender cards hold/delay — none is
            // aggressive, so the 3:1 downgrade never touches them.
            CombatTactic::Counterattack => StrategicObjective::Delay,
            CombatTactic::Ambush => StrategicObjective::Hold,
            CombatTactic::RiverDefense => StrategicObjective::Delay,
            CombatTactic::UrbanDefense => StrategicObjective::Hold,
            CombatTactic::Delay => StrategicObjective::Delay,
            // Attacker cards are aggressive pushes.
            CombatTactic::Assault => StrategicObjective::PushCenter,
            CombatTactic::RiverAssault => StrategicObjective::PushCenter,
        };

        let aggressive = matches!(
            base,
            StrategicObjective::DeepPenetration
                | StrategicObjective::PushCenter
                | StrategicObjective::Pincer
                | StrategicObjective::ExploitGaps
        );
        if aggressive {
            let own_power = force_power(own_units);
            let enemy_power = force_power(enemy_units);
            if enemy_power > 0.0 && own_power * REFUSE_ODDS < enemy_power {
                return StrategicObjective::Hold;
            }
        }
        base
    }

    // ── Layer 2 ────────────────────────────────────────────────────────────

    /// Assign the unit's role from its type and condition (§7.1 layer 2,
    /// §7.3 damaged-unit rule). Type mapping per §6.1 ranges and §6.8.
    fn assign_role(&self, unit: &BattalionUnit, rearguard_id: Option<usize>) -> UnitRole {
        // §6.13: the HQ never takes a combat role — it shadows
        // its division even when damaged (Reserve would park it at the map
        // edge, silencing the aura it exists to project).
        if unit.is_hq() {
            return UnitRole::Headquarters;
        }
        // §7.3: damaged units (<30% org) withdraw from the frontline.
        if unit.org_ratio() < DAMAGED_ORG_RATIO {
            return UnitRole::Reserve;
        }
        if rearguard_id == Some(unit.id) {
            return UnitRole::Rearguard;
        }
        // §6.1: artillery / AT / AA → support fire, whatever the carriage
        // (an SP gun supports just like a towed one).
        if unit.is_indirect_artillery() || unit.attrs.has(Attrs::AT | Attrs::AA) {
            return UnitRole::SupportFire;
        }
        let t = unit.unit_type;
        if t.is_armor() {
            return match self.tactic {
                // §7.2 encirclement: tanks swing to the extreme flanks.
                CombatTactic::Encirclement => UnitRole::Flank,
                // Elastic defense counter-punches around the flanks.
                CombatTactic::ElasticDefense => UnitRole::Flank,
                CombatTactic::TacticalWithdrawal | CombatTactic::OverwhelmingFire => {
                    UnitRole::HoldPosition
                }
                _ => UnitRole::Assault,
            };
        }
        match t {
            // §7.2 infiltration_assault: recon probes the far flanks.
            UnitType::Recon => UnitRole::Probe,
            // §6.8 non-combat support companies buff co-located battalions.
            t if t.is_support_company() => UnitRole::Attached,
            UnitType::Motorized | UnitType::Mechanized => match self.tactic {
                // §7.2 blitz: motorized follows the tanks to widen the breach.
                CombatTactic::Blitz
                | CombatTactic::MassCharge
                | CombatTactic::InfiltrationAssault
                | CombatTactic::GuerrillaTactics
                // The standard-assault line and the river
                // crossing advance together, motorized included.
                | CombatTactic::Assault
                | CombatTactic::RiverAssault => UnitRole::Assault,
                CombatTactic::Encirclement => UnitRole::HoldPosition,
                _ => UnitRole::HoldPosition,
            },
            // Line infantry (incl. marine/mountaineer/paratrooper/cavalry).
            _ => match self.tactic {
                CombatTactic::MassCharge
                | CombatTactic::InfiltrationAssault
                | CombatTactic::GuerrillaTactics
                // The assault line and the fording infantry are
                // the heart of the attack — they advance and assault.
                | CombatTactic::Assault
                | CombatTactic::RiverAssault => UnitRole::Assault,
                // §7.2 blitz/encirclement: infantry holds the shoulders/pins.
                _ => UnitRole::HoldPosition,
            },
        }
    }

    // ── Layer 3 ────────────────────────────────────────────────────────────

    fn execute_unit(
        &mut self,
        unit: &BattalionUnit,
        role: UnitRole,
        objective: StrategicObjective,
        ctx: &Ctx,
    ) -> AiAction {
        match role {
            UnitRole::Reserve => self.plan_reserve(unit, ctx),
            UnitRole::SupportFire => self.plan_support_fire(unit, objective, ctx),
            UnitRole::Probe => self.plan_probe(unit, ctx),
            UnitRole::Attached => self.plan_attached(unit, ctx),
            UnitRole::Flank => self.plan_flank(unit, ctx),
            UnitRole::Rearguard => AiAction::Hold { unit_id: unit.id },
            UnitRole::Headquarters => self.plan_hq(unit, ctx),
            UnitRole::Assault => self.plan_assault_role(unit, ctx),
            UnitRole::HoldPosition => self.plan_hold_role(unit, ctx),
        }
    }

    /// Assault-role battalion: strike if a permitted target is adjacent,
    /// otherwise advance per the tactic's movement doctrine (§7.2).
    fn plan_assault_role(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        if let Some(a) = self.try_assault(unit, ctx) {
            return a;
        }
        match self.tactic {
            // §7.2 mass_charge: max 1 hex forward per turn.
            CombatTactic::MassCharge => self
                .advance_one_hex(unit, ctx)
                .unwrap_or(AiAction::Hold { unit_id: unit.id }),
            // §7.2 guerrilla_tactics: never end the turn adjacent (§7.2).
            CombatTactic::GuerrillaTactics => self.plan_hit_and_run(unit, ctx),
            // §7.2 infiltration_assault: concentrate on weakly-held hexes.
            CombatTactic::InfiltrationAssault => {
                // Adjacent to a visible enemy the assault gates refused
                // (strong ground / support): hold the contact — §6.5
                // consumes every march at the first step, so "slipping
                // past" only shuffles in place turn after turn.
                if ctx.order.is_none() && contact_foe(unit, ctx.enemy) {
                    return AiAction::Hold { unit_id: unit.id };
                }
                let goal = weakest_held_enemy(unit.position, ctx.enemy).map(|e| e.position);
                match goal {
                    Some(g) => self
                        .approach(unit, ctx, Some(g), None)
                        .unwrap_or(AiAction::Hold { unit_id: unit.id }),
                    None => AiAction::Hold { unit_id: unit.id },
                }
            }
            // Blitz / default advance: close on the nearest enemy (§7.2 blitz:
            // aggressive push on a narrow front). No contact yet? March on
            // the pre-battle objective (the blind-man's goal).
            // Under a division order the goal IS the order — the
            // commanded unit marches on the seize hex / the engage target's
            // last position, assaulting whatever blocks the path.
            _ => {
                // Adjacent to a visible enemy the assault gates refused
                // (hopeless trade / doctrine): hunker in contact instead of
                // re-ramming the ring — §6.5 consumes every order at the
                // first step, so the approach only shuffles in place turn
                // after turn. Commanded divisions keep pushing: the order
                // outranks (fight-through assaults whatever it can).
                if ctx.order.is_none() && contact_foe(unit, ctx.enemy) {
                    return AiAction::Hold { unit_id: unit.id };
                }
                let goal = ctx
                    .order_goal(unit)
                    .or_else(|| nearest_enemy(unit.position, ctx.enemy).map(|e| e.position))
                    .or_else(|| ctx.blind_goal(unit.position));
                self.approach(unit, ctx, goal, None)
                    .unwrap_or(AiAction::Hold { unit_id: unit.id })
            }
        }
    }

    /// Hold-role battalion: tactic-dependent restraint (§7.2).
    fn plan_hold_role(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        // §7.3: inside a THREATENED flag zone (≥ 1/3 capture
        // progress) the defender never gives ground — fall-back paths
        // must keep a contestable flag contested. Assault opportunities
        // excepted; tactical_withdrawal is the deliberate abandon card.
        // A division ORDER outranks the flag doctrine (the
        // controller skips the tiered flag response; this hold gate must
        // not freeze a commanded counterattack either).
        if self.side == Side::Defender
            && ctx.order.is_none()
            && self.tactic != CombatTactic::TacticalWithdrawal
            && ctx.zone_flag_progress(unit.position) * 3 >= self.params.flag_progress_cap
        {
            if let Some(a) = self.try_assault(unit, ctx) {
                return a;
            }
            return AiAction::Hold { unit_id: unit.id };
        }
        match self.tactic {
            // §7.2 tactical_withdrawal: fall back 1 hex per turn (rearguard
            // already holds via its own role).
            CombatTactic::TacticalWithdrawal => self
                .fall_back_one_hex(unit, ctx)
                .unwrap_or(AiAction::Hold { unit_id: unit.id }),
            // §7.2 elastic_defense: fall back 1 hex when attacked;
            // counter-attack only isolated enemies.
            CombatTactic::ElasticDefense => {
                let d = min_enemy_dist(unit.position, ctx.enemy);
                if d <= 1 {
                    if let Some(a) = self.try_assault(unit, ctx) {
                        return a; // isolated target, odds acceptable
                    }
                    return self
                        .fall_back_one_hex(unit, ctx)
                        .unwrap_or(AiAction::Hold { unit_id: unit.id });
                }
                // Reinforce the threatened sector — step toward a
                // nearby visible enemy but stay out of contact (second line
                // at distance 2, ready to counter-punch). Otherwise the
                // unengaged half of the line listens to the other half die.
                // The second line forms on the OWN side of any
                // river (screen_threat) — never across it.
                if d <= 3 {
                    let goal = nearest_enemy(unit.position, ctx.enemy).map(|e| e.position);
                    return self
                        .screen_threat(unit, ctx, goal, 2)
                        .unwrap_or(AiAction::Hold { unit_id: unit.id });
                }
                AiAction::Hold { unit_id: unit.id }
            }
            // §7.2 default: engage nearest, prefer defensive terrain.
            CombatTactic::Default => {
                if let Some(a) = self.try_assault(unit, ctx) {
                    return a;
                }
                self.shift_to_cover(unit, ctx)
                    .unwrap_or(AiAction::Hold { unit_id: unit.id })
            }
            // Counterattack: hold like Default, but the assault
            // filter only opens on isolated/beaten targets (assault_permitted)
            // — the counter-punch that keeps the ground.
            // The RESERVE closes up — an out-of-
            // contact battalion within 8 hexes of the enemy approaches to
            // standoff 2 (the same screen_threat primitive as elastic's
            // reinforcement, longer reach), so the deployment's reserve
            // echelon actually reaches the line before the window opens.
            CombatTactic::Counterattack => {
                if let Some(a) = self.try_assault(unit, ctx) {
                    return a;
                }
                let d = min_enemy_dist(unit.position, ctx.enemy);
                if d > 1 && d <= 8 {
                    let goal = nearest_enemy(unit.position, ctx.enemy).map(|e| e.position);
                    if let Some(a) = self.screen_threat(unit, ctx, goal, 2) {
                        return a;
                    }
                }
                self.shift_to_cover(unit, ctx)
                    .unwrap_or(AiAction::Hold { unit_id: unit.id })
            }
            // Ambush: never move — lurk in cover, strike only an
            // enemy that steps adjacent. No cover-shifting either: a hidden
            // ambusher does not give away its position (the default's
            // shift_to_cover shuffles units around under shelling).
            CombatTactic::Ambush => {
                if let Some(a) = self.try_assault(unit, ctx) {
                    return a;
                }
                AiAction::Hold { unit_id: unit.id }
            }
            // River defense: hold the bank at all costs. The only
            // aggression is assaulting a half-forded enemy; falling back is
            // forbidden (no fall_back_one_hex, no shift_to_cover).
            CombatTactic::RiverDefense => {
                if let Some(a) = self.try_assault(unit, ctx) {
                    return a;
                }
                AiAction::Hold { unit_id: unit.id }
            }
            // Urban defense: fight enemies that enter the city and
            // never leave it. A battalion outside the urban hexes (city too
            // small for the whole force) holds in place — it does not march
            // out to meet the enemy.
            CombatTactic::UrbanDefense => {
                let in_city = ctx
                    .grid
                    .cell(unit.position)
                    .map(|c| c.terrain == Terrain::Urban)
                    .unwrap_or(false);
                if in_city {
                    if let Some(a) = self.try_assault(unit, ctx) {
                        return a;
                    }
                }
                AiAction::Hold { unit_id: unit.id }
            }
            // Delay: mobile resistance — keep the enemy inside the
            // 2-3 hex contact band with constant fire. In contact: fall back
            // exactly one hex (never break contact the way
            // tactical_withdrawal does — the line is a moving screen, not a
            // rout). At the band edge or beyond: hold, never pursue.
            CombatTactic::Delay => {
                let d = min_enemy_dist(unit.position, ctx.enemy);
                if d <= 1 {
                    return self
                        .fall_back_one_hex(unit, ctx)
                        .unwrap_or(AiAction::Hold { unit_id: unit.id });
                }
                AiAction::Hold { unit_id: unit.id }
            }
            // Blitz infantry holds the shoulders; encirclement infantry pins
            // the center; overwhelming_fire holds the line (§7.2). An
            // ATTACKING side with NO ENEMY VISIBLE marches on the pre-battle
            // intel instead — a parked shoulder line seals the vanguard in
            // with friendly-packed hexes and pathing deadlocks (headless
            // Warsaw trace: tanks stuck at the frontier, goal
            // unreachable through their own infantry). With the enemy in
            // sight the doctrine stands (hold the line; assault-role units
            // and artillery carry the fight).
            _ => {
                if let Some(a) = self.try_assault(unit, ctx) {
                    return a;
                }
                // (Warsaw siege trace) the blind march must
                // not freeze on RETREATING enemies in view — the gate is
                // "no COMBAT-EFFECTIVE enemy visible" (a fleeing unit cannot
                // hold the line, but it still filled ctx.enemy and parked
                // the whole attacker force at the city edge forever).
                // A division order opens the gate for ANY side —
                // a commanded division's shoulders advance on the order
                // goal, not just an attacker's intel.
                //
                // §6.11 occupation bypass: a flag zone whose garrison has
                // been driven OUT is the decided point of the battle. The
                // global gate above confuses "no effective enemy visible
                // ANYWHERE" with "the flag is contested" — a remnant pocket
                // still in the fog view on the far side of the map parks
                // the shoulder line a couple of hexes from an empty city
                // until the turn cap (Alamein trace: a dozen full-strength
                // battalions ringing an empty flag zone for 100+ turns,
                // progress 0/cap). A NEARBY shoulder unit therefore walks
                // in as soon as no effective defender stands inside the
                // zone; units beyond FLAG_OCCUPY_REACH keep holding, so the
                // line never surges (the Warsaw failure mode above).
                let cleared_flag_nearby = self.side == Side::Attacker
                    && ctx.flags.iter().any(|f| {
                        f.progress < self.params.flag_progress_cap
                            && f.zone
                                .iter()
                                .map(|z| unit.position.distance(*z))
                                .min()
                                .unwrap_or(i32::MAX)
                                <= FLAG_OCCUPY_REACH
                            && !f.zone.iter().any(|z| {
                                ctx.enemy
                                    .iter()
                                    .any(|e| e.is_combat_effective() && e.position == *z)
                            })
                    });
                if (self.side == Side::Attacker || ctx.order.is_some())
                    && (cleared_flag_nearby
                        || !ctx.enemy.iter().any(|e| e.is_combat_effective()))
                {
                    let goal = ctx
                        .order_goal(unit)
                        .or_else(|| ctx.blind_goal(unit.position));
                    return self
                        .approach(unit, ctx, goal, None)
                        .unwrap_or(AiAction::Hold { unit_id: unit.id });
                }
                AiAction::Hold { unit_id: unit.id }
            }
        }
    }

    /// §7.3: damaged units (<30% org) withdraw from the frontline. If pinned
    /// in contact with no safe hex, propose a full retreat (§6.8).
    /// A ford costs as much as a hex of distance — a battered unit caught
    /// mid-river is a dead unit (×2 damage taken, §6.6).
    fn plan_reserve(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        let cur = min_enemy_dist(unit.position, ctx.enemy);
        let step_out = self.best_step(unit, ctx, |h, cost| {
            let d = min_enemy_dist(h, ctx.enemy);
            let ford = ctx
                .grid
                .cell(h)
                .map(|c| (c.terrain == Terrain::River) as u8 as f32 * 100.0)
                .unwrap_or(0.0);
            (d > cur).then(|| -(d as f32) * 100.0 + cost + ford)
        });
        if let Some((_, path)) = step_out {
            return AiAction::MoveUnit {
                unit_id: unit.id,
                path,
            };
        }
        if cur <= 1 {
            // Pinned in contact and cannot disengage → retreat from battle.
            return AiAction::Retreat { unit_id: unit.id };
        }
        AiAction::Hold { unit_id: unit.id }
    }

    /// §7.3: artillery stays 1–2 hexes behind the frontline. Fire on the best
    /// target in range (§6.3 fire support); creep forward when out of range.
    /// Towed guns run the §6.3 emplacement state machine: limbered → emplace
    /// when the enemy enters the firing envelope → fire; limber again when
    /// the enemy leaves it. Rocket artillery needs no emplacement but only
    /// fires area missions outside its minimum range.
    ///
    /// Warsaw headless finding: the DEFENDER's towed guns never fired —
    /// they crept toward visible enemies 20+ hexes away (sparse long front),
    /// the re-planned route kept resetting their march hours, and the battle
    /// ended before they arrived. The defender is a fire base: the enemy
    /// comes to it, so emplacing in place beats chasing. The attacker keeps
    /// the §7.3 creep (its guns must follow the advance).
    fn plan_support_fire(
        &mut self,
        unit: &BattalionUnit,
        objective: StrategicObjective,
        ctx: &Ctx,
    ) -> AiAction {
        let d = min_enemy_dist(unit.position, ctx.enemy);
        // The FIRING envelope also counts retreating enemies (a leaver is
        // a free damage window); the melee standoff above deliberately does
        // not (broken troops hold no ground and are no threat).
        let d_fire = min_enemy_dist_incl_retreating(unit.position, ctx.enemy);
        let towed = unit.requires_emplacement();
        // Direct-fire guns (AT/AA) fire precision missions at units only —
        // never blind bombardment (no zone saturation, no self-splash).
        let direct_gun = unit.is_direct_gun();

        // In contact — get off the frontline first (§7.3). An emplaced gun
        // must spend a turn limbering before it can march away.
        if d < ARTILLERY_MIN_STANDOFF {
            if unit.is_emplaced {
                return AiAction::Limber { unit_id: unit.id };
            }
            let escape = self.best_step(unit, ctx, |h, cost| {
                let hd = min_enemy_dist(h, ctx.enemy);
                // A towed gun dragged through a ford is a sitting
                // duck — prefer dry escape hexes at equal distance.
                let ford = ctx
                    .grid
                    .cell(h)
                    .map(|c| (c.terrain == Terrain::River) as u8 as f32 * 100.0)
                    .unwrap_or(0.0);
                (hd >= ARTILLERY_MIN_STANDOFF).then(|| -(hd as f32) * 100.0 + cost + ford)
            });
            if let Some((_, path)) = escape {
                return AiAction::MoveUnit {
                    unit_id: unit.id,
                    path,
                };
            }
            // No safe hex: keep fighting from here.
        }

        if towed && !unit.is_emplaced {
            // Enemy inside the firing envelope → emplace (fires next turn).
            if d_fire <= unit.attack_range {
                return AiAction::Emplace { unit_id: unit.id };
            }
            // Out of range: the defender emplaces as a fire base (the enemy
            // approaches it); the attacker marches toward the enemy (or the
            // objective when blind), stopping at the envelope edge (§7.3).
            // Under a division order the guns creep on the order
            // goal (the commanded division's guns follow its march).
            // A commanded division in the SEIZED hold-back phase
            // (the elastic-defense card) is a fire base like the defender —
            // emplace in place, don't chase the point with the guns.
            if (self.side == Side::Defender && ctx.order.is_none())
                || (ctx.order.is_some() && self.tactic == CombatTactic::ElasticDefense)
            {
                return AiAction::Emplace { unit_id: unit.id };
            }
            let goal = ctx
                .order_goal(unit)
                .or_else(|| nearest_enemy(unit.position, ctx.enemy).map(|e| e.position))
                .or_else(|| ctx.blind_goal(unit.position));
            // The cut is the MELEE standoff (2), not the
            // attack range — cutting at the envelope edge froze the guns
            // behind every physical foe at once (Ioannina 2/10 → 0/10);
            // cutting at contact only stops them from stepping on the
            // enemy, and the blind-goal emplace branch above parks them
            // once the objective enters the envelope.
            return self
                .approach(unit, ctx, goal, Some(ARTILLERY_MIN_STANDOFF))
                .unwrap_or(AiAction::Hold { unit_id: unit.id });
        }
        // The enemy left the envelope: the ATTACKER limbers to follow the
        // advance; the defender stays put — a fire base does not chase
        // (same Warsaw finding as above). Unless the intel zone
        // (the besieged city) is in range — then the gun STAYS and shells
        // it (blind bombardment below) instead of wandering off.
        // A commanded division's gun keeps its emplacement and
        // shells the order goal instead of limbering away after it.
        if towed
            && unit.is_emplaced
            && d_fire > unit.attack_range
            && self.side == Side::Attacker
            && ctx.order.is_none()
            && self.intel_goal_in_range(unit, ctx).is_none()
        {
            return AiAction::Limber { unit_id: unit.id };
        }

        // A reloading rocket launcher cannot fire — skip the
        // fire-mission pick (towed (un)limber prep above and the
        // out-of-range creep below still run while the tubes are loaded;
        // only rockets ever carry a cooldown).
        if unit.fire_cooldown <= 0 {
            if let Some(target_hex) = self.choose_fire_target(unit, objective, ctx) {
                return AiAction::FireSupport {
                    attacker_id: unit.id,
                    target_hex,
                };
            }
            // BLIND BOMBARDMENT — with no visible
            // target, an emplaced tube/rocket battery shells its intel goal
            // (the enemy deployment zone / the besieged city) when in range.
            // A siege must bleed: before this, the Warsaw garrison hid
            // behind fog + urban LoS and NOTHING fired — the battle froze
            // into a staring match for 300 turns.
            // A commanded division's gun shells its ORDER goal
            // first (siege of the seized point / pursuit of the target).
            // Direct-fire guns (AT/AA) never blind-bombard — their
            // missions are precision-only, and an area mission at their
            // own feet splashes the gun itself.
            if !direct_gun {
                if let Some(hex) = ctx
                    .order_goal_in_range(unit)
                    .or_else(|| self.intel_goal_in_range(unit, ctx))
                {
                    return AiAction::FireSupport {
                        attacker_id: unit.id,
                        target_hex: hex,
                    };
                }
            }
        }

        // Mobile battery (rocket) out of range: creep into the envelope
        // (toward the objective when blind) — unless the intel zone is
        // already in range, then shell it instead. Direct-fire guns
        // (AT/AA) never blind-bombard (precision-only missions).
        if !towed && d_fire > unit.attack_range {
            if !direct_gun {
                if let Some(hex) = ctx
                    .order_goal_in_range(unit)
                    .or_else(|| self.intel_goal_in_range(unit, ctx))
                {
                    return AiAction::FireSupport {
                        attacker_id: unit.id,
                        target_hex: hex,
                    };
                }
            }
            let goal = ctx
                .order_goal(unit)
                .or_else(|| nearest_enemy(unit.position, ctx.enemy).map(|e| e.position))
                .or_else(|| ctx.blind_goal(unit.position));
            return self
                .approach(unit, ctx, goal, Some(unit.attack_range))
                .unwrap_or(AiAction::Hold { unit_id: unit.id });
        }
        AiAction::Hold { unit_id: unit.id }
    }

    /// The intel goal if it lies inside the unit's firing envelope — the
    /// blind-bombardment fallback.
    fn intel_goal_in_range(&self, unit: &BattalionUnit, ctx: &Ctx) -> Option<HexCoord> {
        let hex = ctx.blind_goal(unit.position)?;
        let d = unit.position.distance(hex);
        (d <= unit.attack_range && d >= unit.min_attack_range()).then_some(hex)
    }

    /// §7.2 infiltration_assault: recon probes the far flanks.
    fn plan_probe(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        // A prober that bumps into a visible enemy strikes when the gates
        // allow; when they refuse, it holds the contact — ramming the ring
        // only burns the order at the first step (§6.5), turn after turn.
        if let Some(a) = self.try_assault(unit, ctx) {
            return a;
        }
        if contact_foe(unit, ctx.enemy) {
            return AiAction::Hold { unit_id: unit.id };
        }
        let left = unit.id.is_multiple_of(2);
        // The own–enemy axis counts passive friendlies too — the
        // flank is relative to the WHOLE side's line, not this slice.
        let friends = ctx.friends();
        self.approach(
            unit,
            ctx,
            flank_waypoint(ctx.grid, &friends, ctx.enemy, left)
                .or_else(|| ctx.blind_goal(unit.position)),
            None,
        )
        .unwrap_or(AiAction::Hold { unit_id: unit.id })
    }

    /// §7.2 encirclement: tanks swing to the extreme flanks (left/right split
    /// by unit id for the two pincer arms).
    fn plan_flank(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        // A flanker that bumps into a screen strikes when the gates allow;
        // when they refuse, it holds the contact instead of re-ramming the
        // ring (§6.5 consumes every march at the first step).
        if let Some(a) = self.try_assault(unit, ctx) {
            return a;
        }
        if contact_foe(unit, ctx.enemy) {
            return AiAction::Hold { unit_id: unit.id };
        }
        let left = unit.id.is_multiple_of(2);
        let friends = ctx.friends();
        match flank_waypoint(ctx.grid, &friends, ctx.enemy, left)
            .or_else(|| ctx.blind_goal(unit.position))
        {
            Some(wp) if unit.position.distance(wp) > 1 => self
                .approach(unit, ctx, Some(wp), None)
                .unwrap_or(AiAction::Hold { unit_id: unit.id }),
            _ => AiAction::Hold { unit_id: unit.id },
        }
    }

    /// §7.3/§6.8: support companies stay near the battalion they buff.
    fn plan_attached(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        // Caught in contact: strike when the gates allow (a support
        // company CAN fight in self-defense), otherwise hold — stepping
        // inside the ring burns the order every turn (§6.5).
        if let Some(a) = self.try_assault(unit, ctx) {
            return a;
        }
        if contact_foe(unit, ctx.enemy) {
            return AiAction::Hold { unit_id: unit.id };
        }
        let anchor = ctx
            .own
            .iter()
            .filter(|u| {
                u.id != unit.id && u.is_combat_effective() && !u.unit_type.is_support_company()
            })
            .min_by(|a, b| {
                unit.position
                    .distance(a.position)
                    .cmp(&unit.position.distance(b.position))
                    .then(a.id.cmp(&b.id))
            });
        match anchor {
            Some(a) if unit.position.distance(a.position) > 1 => self
                .approach(unit, ctx, Some(a.position), None)
                .unwrap_or(AiAction::Hold { unit_id: unit.id }),
            _ => AiAction::Hold { unit_id: unit.id },
        }
    }

    /// §6.13: the HQ never attacks — it shadows its division,
    /// keeping inside command range of the division's centroid while staying
    /// off the frontline. Contact with the enemy breaks the leash: a dead HQ
    /// costs the whole division 20% max org, so survival outranks coverage.
    fn plan_hq(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        let radius = self.params.hq_aura_radius;
        // Division anchor: the member hex nearest the float centroid of the
        // division's surviving battalions.
        let members: Vec<&BattalionUnit> = ctx
            .own
            .iter()
            .filter(|u| !u.is_hq() && u.division == unit.division && u.is_combat_effective())
            .collect();
        if members.is_empty() {
            return AiAction::Hold { unit_id: unit.id };
        }
        let (sq, sr) = members.iter().fold((0i64, 0i64), |(aq, ar), u| {
            (aq + u.position.q as i64, ar + u.position.r as i64)
        });
        let n = members.len() as f32;
        let (cq, cr) = (sq as f32 / n, sr as f32 / n);
        let anchor = members
            .iter()
            .map(|u| u.position)
            .min_by(|a, b| {
                let d2 = |x: &HexCoord| {
                    let (dq, dr) = (x.q as f32 - cq, x.r as f32 - cr);
                    dq * dq + dr * dr
                };
                d2(a)
                    .partial_cmp(&d2(b))
                    .unwrap_or(Ordering::Equal)
                    .then(a.q.cmp(&b.q))
                    .then(a.r.cmp(&b.r))
            })
            .unwrap();

        let enemy_d = min_enemy_dist(unit.position, ctx.enemy);
        // In contact: sidestep away, preferring hexes still on the leash.
        if enemy_d <= 1 {
            let escape = self.best_step(unit, ctx, |h, cost| {
                let d = min_enemy_dist(h, ctx.enemy);
                let overshoot = (h.distance(anchor) - radius).max(0) as f32;
                (d > enemy_d).then(|| -(d as f32) * 100.0 + overshoot * 50.0 + cost)
            });
            if let Some((_, path)) = escape {
                return AiAction::MoveUnit {
                    unit_id: unit.id,
                    path,
                };
            }
            return AiAction::Hold { unit_id: unit.id };
        }
        // On the leash and safe: hold position.
        if unit.position.distance(anchor) < radius {
            return AiAction::Hold { unit_id: unit.id };
        }
        // The division moved on — follow, keeping a 2-hex enemy standoff.
        //
        // The follow target must be a FREE hex by the anchor: aiming at the
        // anchor itself tailgates an occupied hex — in a static battle the
        // anchor member never vacates it (in combat / emplaced / jammed),
        // and the HQ sits blocked-waiting with a standing move order for the
        // rest of the fight. Hysteresis first: a standing order whose
        // destination is still free and on the leash is re-affirmed as-is,
        // so centroid creep can never reset invested movement hours.
        let occupied = |h: HexCoord| {
            ctx.all
                .iter()
                .any(|u| u.is_combat_effective() && u.position == h)
        };
        if let Some(order) = &unit.move_order {
            if let Some(&dest) = order.path.last() {
                if anchor.distance(dest) <= radius && !occupied(dest) {
                    if let Some(action) = self.approach(unit, ctx, Some(dest), Some(2)) {
                        return action;
                    }
                    // Became unreachable — fall through to a fresh pick.
                }
            }
        }
        let mut candidates: Vec<HexCoord> = anchor
            .neighbors()
            .into_iter()
            .chain(anchor.ring(2))
            .filter(|h| anchor.distance(*h) < radius && ctx.grid.cell(*h).is_some() && !occupied(*h))
            .collect();
        candidates.sort_by_key(|h| (anchor.distance(*h), unit.position.distance(*h)));
        for target in candidates {
            if let Some(action) = self.approach(unit, ctx, Some(target), Some(2)) {
                return action;
            }
        }
        // Saturation fallback: no free leash hex reachable — tailgate the
        // anchor as before.
        self.approach(unit, ctx, Some(anchor), Some(2))
            .unwrap_or(AiAction::Hold { unit_id: unit.id })
    }

    /// §7.2 guerrilla_tactics: attack → retreat; never end the turn adjacent
    /// to the enemy. (§6.2 makes a literal attack-then-move in one turn
    /// impossible — assault consumes the turn's action — so hit-and-run
    /// alternates across turns: strike when adjacent, otherwise move while
    /// staying out of contact.)
    fn plan_hit_and_run(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> AiAction {
        let cur = min_enemy_dist(unit.position, ctx.enemy);
        if cur <= 1 {
            // Couldn't or wouldn't strike (odds) → disengage.
            let escape = self.best_step(unit, ctx, |h, cost| {
                let d = min_enemy_dist(h, ctx.enemy);
                (d >= 2).then(|| -(d as f32) * 100.0 + cost)
            });
            return match escape {
                Some((_, path)) => AiAction::MoveUnit {
                    unit_id: unit.id,
                    path,
                },
                None => AiAction::Retreat { unit_id: unit.id },
            };
        }
        // Approach an isolated victim, but stop outside adjacency.
        let goal = most_isolated_enemy(unit.position, ctx.enemy)
            .or_else(|| nearest_enemy(unit.position, ctx.enemy))
            .map(|e| e.position)
            .or_else(|| ctx.blind_goal(unit.position));
        match goal {
            Some(g) => self
                .approach(unit, ctx, Some(g), Some(2))
                .unwrap_or(AiAction::Hold { unit_id: unit.id }),
            None => AiAction::Hold { unit_id: unit.id },
        }
    }

    // ── Layer 3 primitives ─────────────────────────────────────────────────

    /// Propose an assault on the best permitted adjacent target, or `None`.
    /// §6.10: combat is manually triggered, so adjacency alone is not enough —
    /// the tactic must permit it and local odds must be acceptable (§7.3).
    fn try_assault(&mut self, unit: &BattalionUnit, ctx: &Ctx) -> Option<AiAction> {
        // §6.2/§6.3 gates: acted / holding / emplaced / rocket artillery.
        if !unit.can_assault() {
            return None;
        }
        // FIGHT-THROUGH — a commanded division
        // fights whatever stands in its way. An adjacent combat-effective
        // enemy is assaultable despite the trade/odds doctrine gates,
        // because refusing leaves the unit pinned: every step of a march
        // stays inside the contact ring, §6.5 consumes the order at the
        // first step, and the enemy's hex blocks the route — a battalion
        // 贴脸 that "cannot assault" neither fights nor advances. The
        // doctrine stays fully intact for the whole-side AI (`order` is
        // None there) — this is the player's explicit command.
        let fight_through = |e: &BattalionUnit| -> bool {
            ctx.order.is_some()
                && e.is_combat_effective()
                && unit.position.distance(e.position) == 1
        };
        // An Engage order assaults THE TARGET directly when it is
        // adjacent and permitted — no trading it for a weaker neighbour.
        // Fight-through also applies to the target itself: a 歼敌 command
        // must strike its quarry even when the trade math is unkind.
        if let Some(DivOrderTarget::Engage { unit: uid, .. }) = ctx.order {
            if let Some(t) = ctx.enemy.iter().find(|e| e.id == uid) {
                // The pursuit is personal: a quarry that turns to run stays
                // the quarry — a retreating target never counters.
                let fleeing = t.state == UnitState::Retreating;
                if unit.position.distance(t.position) == 1
                    && (fleeing || self.assault_permitted(t, ctx) || fight_through(t))
                    && (fleeing
                        || local_odds_acceptable(unit, t.position, ctx)
                        || self.storm_target(unit, t, ctx)
                        || fight_through(t))
                {
                    return Some(AiAction::Assault {
                        attacker_id: unit.id,
                        target_id: t.id,
                    });
                }
            }
        }
        let mut candidates: Vec<&BattalionUnit> = ctx
            .enemy
            .iter()
            // An org-0 remnant (Withdrawn on the
            // zone rim, or Active at 0) is a corridor blockage, not a
            // combatant — the AI must CLEAR it instead of treating it as
            // invisible to assault while pathfinding routes around its hex
            // (the bridgehead stall: the bottleneck plus a wall of
            // unattackable org-0 remnants froze the whole advance). It
            // cannot fight back, so the trade filters below pass
            // automatically (`dealt >= e.org` with org 0).
            .filter(|e| {
                e.is_combat_effective()
                    || (e.org <= 0.0 && e.state != UnitState::Eliminated)
                    // A RETREATING enemy of any org is in the pool too:
                    // it never counters (return fire is structurally zero),
                    // so striking it is free damage — the window closes
                    // when it exits the map.
                    || e.state == UnitState::Retreating
            })
            .filter(|e| unit.position.distance(e.position) == 1)
            .filter(|e| self.assault_permitted(e, ctx) || fight_through(e))
            // §7.3 local odds (3:1 refusal) — unless this is a STORM: a
            // beaten urban garrison's packed neighbors no longer protect
            // it (the same storm exception as the trade filter below).
            // A retreating target skips the gate: it adds no counter of
            // its own and the strike hits only the leaver.
            .filter(|e| {
                e.state == UnitState::Retreating
                    || local_odds_acceptable(unit, e.position, ctx)
                    || self.storm_target(unit, e, ctx)
                    || fight_through(e)
            })
            // Refuse hopeless trades (infantry storming a tank is
            // suicide, not "counter-attack") — unless the strike is expected
            // to break the target outright.
            .filter(|e| {
                // A retreating target never counters — the trade math
                // does not apply, the strike is free damage.
                if e.state == UnitState::Retreating {
                    return true;
                }
                // The point-blank defender cards strike regardless
                // of the trade math — the ambusher fires first, the garrison
                // fights in its own terrain, the river line punishes the
                // half-ford (whose wet hex halves the estimate anyway via
                // the attack-modifier column). Every other card still
                // refuses hopeless trades unless the strike breaks the
                // target. The BESIEGER storms — an urban garrison beaten
                // below 40% org (the 26 Sep Warsaw general assault after
                // days of bombardment) or locally outnumbered ≥3:1 may be
                // assaulted despite the trade math; the gate also opens on
                // a beaten target the fog hides (the Narvik 7%-org field
                // remnant — see storm_target). A DIVISION ORDER also opens
                // the gate — a commanded unit fights its way through (see
                // fight_through above).
                let storm = self.storm_target(unit, e, ctx);
                // Pool-aware trade: assaults resolve as pooled
                // volleys (P = Σ(q·g)×Σg — numbers square), so the strike
                // estimate counts the whole pool this unit joins — every
                // adjacent assault-capable friend. Solo evaluation refused
                // almost every even match (their defense vs our
                // breakthrough), freezing 1:1 battles into standoffs
                // (arena mirror trace). The counter total is
                // ~constant (split across the pool ÷n), so the counter
                // estimate stays the solo one. Firepower regime: aimed.
                let dealt = self.pooled_assault_estimate(unit, e, ctx);
                let taken = est_org_damage(
                    e,
                    unit,
                    true,
                    &self.params,
                    ctx.grid,
                    1,
                    tactical_core::damage::FirepowerForm::Aimed,
                );
                // The break-outright exemption must respect the delivery cap:
                // one strike can never break more than org_cap_ratio of the
                // target's max org (the pre-fix estimate had no
                // cap and no damage scale, so this check was far too strict).
                let dealt_capped = dealt.min(e.max_org * self.params.org_cap_ratio);
                dealt_capped >= e.org
                    || taken <= dealt * 1.5
                    || matches!(
                        self.tactic,
                        CombatTactic::Ambush
                            | CombatTactic::UrbanDefense
                            | CombatTactic::RiverDefense
                    )
                    || storm
                    || fight_through(e)
            })
            .collect();
        // (Ioannina 3914 fog-wall trace): the LAST enemy stand
        // can sit in dark fog — mountains block the line of sight and the
        // fog reveal has expired — invisible to planning while its hexes
        // still block the route. The blind march halts adjacent to the wall
        // every turn (contact consumes the order, the enemy cannot be
        // targeted) and the battle freezes at gap 1-2 forever (9/10 seeds
        // in the 288-turn battery). PROBE into the fog: an adjacent
        // PHYSICAL enemy the planner cannot see is assaultable when the
        // storm conditions hold (beaten below 40% org, or ≥3:1 local) — the
        // besieger storms blind exactly like the 26 Sep urban general
        // assault. Nothing visible changes: a visible enemy never comes from
        // this list, and a fresh (≥40%) or locally matched invisible wall is
        // still not worth a blind trade.
        if !ctx.physical_foes.is_empty() {
            for e in ctx.physical_foes.iter() {
                if ctx.enemy.iter().any(|v| v.id == e.id) {
                    continue; // visible — already in the candidate pool
                }
                if unit.position.distance(e.position) != 1 {
                    continue;
                }
                if !e.is_combat_effective() && !(e.org <= 0.0 && e.state != UnitState::Eliminated) {
                    continue;
                }
                if !self.storm_target(unit, e, ctx) {
                    continue;
                }
                candidates.push(e);
            }
        }
        if candidates.is_empty() {
            return None;
        }
        // A Seize order prefers targets on / next to the seized
        // hex — the defenders of the point itself come first.
        let seize_bias = match ctx.order {
            Some(DivOrderTarget::Seize { hex }) => Some((hex, move |e: &BattalionUnit| {
                e.position == hex || e.position.distance(hex) == 1
            })),
            _ => None,
        };
        if let Some((_, on_target)) = &seize_bias {
            if let Some(best) = candidates.iter().find(|c| on_target(c)) {
                return Some(AiAction::Assault {
                    attacker_id: unit.id,
                    target_id: best.id,
                });
            }
        }
        // Weakest first; guerrilla/elastic prefer isolated victims (§7.2).
        let prefer_isolated = matches!(
            self.tactic,
            CombatTactic::GuerrillaTactics | CombatTactic::ElasticDefense
        );
        // The joinable pool per candidate — shared criteria make
        // independent planners converge on the same victim, so the volley
        // actually forms.
        let pool = |t: &BattalionUnit| {
            ctx.own
                .iter()
                .chain(ctx.passive.iter())
                .filter(|o| {
                    o.is_combat_effective()
                        && o.can_assault()
                        && o.position.distance(t.position) == 1
                })
                .count()
        };
        let key = |c: &BattalionUnit| target_key(c, ctx.enemy, prefer_isolated, pool(c));
        candidates.sort_by(|a, b| key(a).cmp(&key(b)));
        let best_key = candidates.first().map(|c| key(*c))?;
        let tied: Vec<usize> = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| key(c) == best_key)
            .map(|(i, _)| i)
            .collect();
        let pick = tied[self.rng.next_below(tied.len() as u64) as usize];
        Some(AiAction::Assault {
            attacker_id: unit.id,
            target_id: candidates[pick].id,
        })
    }

    /// Expected org damage of the assault volley this unit joins —
    /// the unit plus every adjacent assault-capable friend (own and allied;
    /// the resolver pools by target regardless of command), computed with the
    /// same core math `strike_group` resolves (the est=strike invariant,
    /// extended to pools). Convergent by construction: every friend adjacent
    /// to the same target sees the same pool and independently reaches the
    /// same decision.
    fn pooled_assault_estimate(
        &self,
        unit: &BattalionUnit,
        target: &BattalionUnit,
        ctx: &Ctx,
    ) -> f32 {
        let mut pool: Vec<&BattalionUnit> = vec![unit];
        pool.extend(
            ctx.own
                .iter()
                .chain(ctx.passive.iter())
                .filter(|o| {
                    o.id != unit.id
                        && o.is_combat_effective()
                        && !o.acted
                        && o.can_assault()
                        && o.position.distance(target.position) == 1
                }),
        );
        tactical_core::damage::strike_group_org_estimate(&pool, target, ctx.grid, &self.params)
    }

    /// The volley of the assault pool that COULD
    /// form against `target` — every assault-capable friend (own + allied)
    /// adjacent to it, regardless of whether it has already been signed an
    /// order this turn. The fire planner predicts the pool that would
    /// exist; filtering `acted` (as `pooled_assault_estimate` does) would
    /// make the estimate depend on the planning order and break the
    /// convergence. Same core math the resolver pools with (single
    /// source); 0 when no pool can form.
    fn imagined_pool_estimate(&self, target: &BattalionUnit, ctx: &Ctx) -> f32 {
        let pool: Vec<&BattalionUnit> = ctx
            .own
            .iter()
            .chain(ctx.passive.iter())
            .filter(|o| {
                o.is_combat_effective()
                    && o.can_assault()
                    && o.position.distance(target.position) == 1
            })
            .collect();
        if pool.is_empty() {
            return 0.0;
        }
        tactical_core::damage::strike_group_org_estimate(&pool, target, ctx.grid, &self.params)
    }

    /// The besieger storms — an urban garrison
    /// beaten below 40% org (the 26 Sep Warsaw general assault after days
    /// of bombardment) or locally outnumbered ≥3:1 is assaultable despite
    /// the trade math and the local-odds gate (a broken garrison's packed
    /// neighbors no longer protect it). The storm is deliberately
    /// URBAN/INVISIBLE-only: widening it to ANY beaten target was tried
    /// and reverted (Narvik/Ioannina 10×288 battery):
    /// at 60% the AI stormed half-health mountain defenders and bled the
    /// army dry (8→6/10); at 40% all-target the visible-field assaults on
    /// rough ground still lost more than they gained (8→6/10) — the trade
    /// doctrine's refusal IS the mountain-fight discipline, and finishing
    /// a visible field remnant is the PLAYER's call, not an AI free-for-all.
    fn storm_target(&self, _unit: &BattalionUnit, e: &BattalionUnit, ctx: &Ctx) -> bool {
        let urban = ctx
            .grid
            .cell(e.position)
            .map(|c| c.terrain == Terrain::Urban)
            .unwrap_or(false);
        // The storm also opens on an
        // INVISIBLE enemy — a dark-fog wall cannot be assessed by the trade
        // doctrine, and the halt at its hex proves it blocks the route.
        // Beaten (<40% org) or locally overwhelmed (≥3:1) walls get probed
        // blind; fresh or matched ones stay unmoved (see try_assault).
        let invisible = !ctx.enemy.iter().any(|v| v.id == e.id);
        if !urban && !invisible {
            return false;
        }
        if e.org_ratio() < 0.4 {
            return true;
        }
        // Passive friendlies count — an allied battalion standing
        // on the garrison's flank presses the siege just like our own.
        let friends = ctx
            .own
            .iter()
            .chain(ctx.passive.iter())
            .filter(|o| o.position.distance(e.position) == 1)
            .count();
        friends >= 3 * (adjacent_enemy_friends(e, ctx.enemy) + 1)
    }

    /// Whether the tactic allows assaulting this target at all (§7.2).
    fn assault_permitted(&self, target: &BattalionUnit, ctx: &Ctx) -> bool {
        match self.tactic {
            // Withdrawal, artillery-centric and delay doctrines never assault
            // (delay holds a moving screen — striking is counterattack's job).
            CombatTactic::TacticalWithdrawal
            | CombatTactic::OverwhelmingFire
            | CombatTactic::Delay => false,
            // §7.2 elastic_defense: counter-attack only isolated enemy units.
            CombatTactic::ElasticDefense => adjacent_enemy_friends(target, ctx.enemy) == 0,
            // Counterattack: the counter-punch opens only on a
            // window — an isolated target OR one already beaten below half
            // org. Striking into a supported, fresh line is not a
            // counter-offensive, it is suicide.
            CombatTactic::Counterattack => {
                adjacent_enemy_friends(target, ctx.enemy) == 0 || target.org_ratio() < 0.5
            }
            // River defense: the only aggression is the half-ford
            // strike — an enemy caught IN a river hex (×2 est damage; the
            // trade filter passes naturally). Hold against everyone else.
            CombatTactic::RiverDefense => ctx
                .grid
                .cell(target.position)
                .map(|c| c.terrain == Terrain::River)
                .unwrap_or(false),
            // §7.2 infiltration_assault: avoid frontal assaults on strong
            // positions (rough defensive terrain or a supported defender).
            CombatTactic::InfiltrationAssault => {
                // v3.3: ground quality = cover (village/mountain/
                // urban pass at ≥ 0.30; forest no longer qualifies).
                let strong_ground = ctx
                    .grid
                    .cell(target.position)
                    .map(|c| c.terrain.cover_percent() >= 0.30)
                    .unwrap_or(false);
                !strong_ground && adjacent_enemy_friends(target, ctx.enemy) < 2
            }
            _ => true,
        }
    }

    /// Fire-support target selection (§6.3). Prefers a visible enemy on
    /// the aim hex (precise fire, full damage — the gesture rule);
    /// area shots (weighted zone) are still proposed but only outrank precise
    /// ones on a blank field. Rockets rank by raw strength (they never
    /// dilute).
    /// Under the Attrition objective all batteries concentrate on the
    /// globally weakest hex (§7.2 overwhelming_fire); under the assault
    /// cards the preparation instead lands on the hex the assault pool
    /// will storm (coordinated fires), weakest-hex as the
    /// no-pool fallback.
    fn choose_fire_target(
        &mut self,
        unit: &BattalionUnit,
        objective: StrategicObjective,
        ctx: &Ctx,
    ) -> Option<HexCoord> {
        let mut in_range: Vec<&BattalionUnit> = ctx
            .enemy
            .iter()
            // Retreating enemies stay fire-visible: a leaver is a free
            // damage window that closes when it exits the map.
            .filter(|e| e.is_combat_effective() || e.state == UnitState::Retreating)
            .filter(|e| {
                let d = unit.position.distance(e.position);
                d <= unit.attack_range && d >= unit.min_attack_range()
            })
            .collect();
        if in_range.is_empty() {
            return None;
        }
        // Direct-fire guns (AT/AA) fire precision missions at
        // combat-effective targets only — a broken/router unit is not an
        // aim-hex target for them (the registration gate would no-op the
        // mission anyway).
        if unit.is_direct_gun() {
            in_range.retain(|e| e.is_combat_effective());
            if in_range.is_empty() {
                return None;
            }
        }
        // An Engage order shells the target unit itself whenever
        // it is in range (the pursuit is personal); a Seize order prefers
        // the point's own defenders. The rocket blast-safety check
        // covers passive friendlies too — an allied battalion in the blast
        // is a friendly casualty all the same.
        match ctx.order {
            Some(DivOrderTarget::Engage { unit: uid, .. }) => {
                if let Some(t) = in_range.iter().find(|e| e.id == uid) {
                    if !unit.is_rocket()
                        || !ctx.own.iter().chain(ctx.passive.iter()).any(|f| {
                            f.is_combat_effective() && f.position.distance(t.position) <= 1
                        })
                    {
                        return Some(t.position);
                    }
                }
            }
            Some(DivOrderTarget::Seize { hex }) => {
                if let Some(best) = in_range
                    .iter()
                    .filter(|e| e.position == hex || e.position.distance(hex) == 1)
                    .min_by(|a, b| weakness_cmp(a, b).then(a.id.cmp(&b.id)))
                {
                    if !unit.is_rocket()
                        || !ctx.own.iter().chain(ctx.passive.iter()).any(|f| {
                            f.is_combat_effective() && f.position.distance(best.position) <= 1
                        })
                    {
                        return Some(best.position);
                    }
                }
            }
            None => {}
        }
        // Rocket salvos hit EVERY unit in the 7-hex zone at full
        // strength — refuse aim hexes whose blast contains a friendly. (The
        // player may still choose to accept friendly fire; the AI never
        // does so deliberately.) "Friendly" includes passive
        // friendlies.
        if unit.is_rocket() {
            in_range.retain(|e| {
                !ctx.own
                    .iter()
                    .chain(ctx.passive.iter())
                    .any(|f| f.is_combat_effective() && f.position.distance(e.position) <= 1)
            });
            if in_range.is_empty() {
                return None;
            }
        }

        // The standard-assault cards run FIRE PREPARATION before
        // the line advances. The preparation prefers a hex a
        // visible enemy holds (precise = full damage); an empty aim hex is
        // only the fallback (area, weighted). Under
        // the assault cards the preparation lands on the hex the assault
        // POOL will storm — each battery ranks every in-range enemy by its
        // would-be pool's volley (imagined_pool_estimate — the pooled-volley
        // core math), and since the assault side converges on the same target
        // via the pool-size key, the fire and melee pools land on the same
        // breach point. The unified fire phase resolves both pools the same
        // end-of-turn — the coordination is stacked org collapse at the
        // breach, not sequential softening. No poolable target → fall back
        // to the globally weakest hex, which remains the Attrition
        // doctrine's own rule.
        let assault_card = matches!(
            self.tactic,
            CombatTactic::Assault | CombatTactic::RiverAssault
        );
        if objective == StrategicObjective::Attrition || assault_card {
            let cands: Vec<&BattalionUnit> = if in_range.iter().any(|e| ctx.precise_hex(e.position))
            {
                in_range
                    .iter()
                    .copied()
                    .filter(|e| ctx.precise_hex(e.position))
                    .collect()
            } else {
                in_range.to_vec()
            };
            if assault_card {
                let mut ranked: Vec<(&BattalionUnit, f32)> = cands
                    .iter()
                    .map(|e| (*e, self.imagined_pool_estimate(e, ctx)))
                    .collect();
                // Volley estimate desc, then weaker first, then id — the
                // same table for every battery, so the preparation
                // converges on one hex without any shared state.
                ranked.sort_by(|(a, ea), (b, eb)| {
                    eb.partial_cmp(ea)
                        .unwrap_or(Ordering::Equal)
                        .then(weakness_cmp(a, b))
                        .then(a.id.cmp(&b.id))
                });
                if let Some((best, est)) = ranked.first() {
                    if *est > 0.0 {
                        return Some(best.position);
                    }
                }
            }
            if let Some(weakest) = cands
                .iter()
                .min_by(|a, b| weakness_cmp(a, b).then(a.id.cmp(&b.id)))
            {
                return Some(weakest.position);
            }
        }

        let mut scored: Vec<(&BattalionUnit, f32, bool)> = in_range
            .into_iter()
            .map(|e| {
                // Rank by EXPECTED damage, not raw weakness —
                // otherwise batteries waste missions on tanks they cannot
                // scratch while soft targets sit in range. The
                // estimate sees terrain, so targets caught mid-ford (×2)
                // bubble to the top. Rank by the REAL
                // expected value — a mission only deals full damage on a
                // visible enemy (precise); an area shot on an empty or
                // intel hex weighs its strike by the aim hex's share
                // (×area_center_share) at resolution. est_org_damage stays
                // a pure strike() mirror; the share factor lives in the
                // ranking only. Rockets never dilute, so they
                // also sort on the full-strength footing — damage decides.
                let exp = est_org_damage(
                    unit,
                    e,
                    false,
                    &self.params,
                    ctx.grid,
                    unit.position.distance(e.position),
                    // Fire missions are AREA fire (linear).
                    tactical_core::damage::FirepowerForm::Area,
                );
                let precise = ctx.precise_hex(e.position) || unit.is_direct_gun();
                let rocket = unit.is_rocket();
                let eff = if precise || rocket {
                    exp
                } else {
                    exp * self.params.area_center_share
                };
                (e, eff, precise || rocket)
            })
            .collect();
        scored.sort_by(|(a, ea, la), (b, eb, lb)| {
            // Precise first (full damage), then expected damage, then weakest.
            lb.cmp(la)
                .then(eb.partial_cmp(ea).unwrap_or(Ordering::Equal))
                .then(weakness_cmp(a, b))
                .then(a.id.cmp(&b.id))
        });
        let (_, best_exp, best_los) = scored[0];
        let tied: Vec<usize> = scored
            .iter()
            .enumerate()
            .filter(|(_, (_, e, l))| *l == best_los && (*e - best_exp).abs() < 1e-3)
            .map(|(i, _)| i)
            .collect();
        let pick = tied[self.rng.next_below(tied.len() as u64) as usize];
        Some(scored[pick].0.position)
    }

    // ── Movement primitives ────────────────────────────────────────────────

    /// March toward `goal` with a standing order (§6.2): the full A* path is
    /// issued and the unit advances at its own speed across turns (progress
    /// persists inside the order; the AI re-plans and may redirect every
    /// turn). When `goal` is unreachable (occupied), the nearest reachable
    /// hex adjacent to it is used instead. `min_standoff` cuts the path
    /// before the first hex violating the standoff (guerrilla / artillery
    /// creep rules).
    fn approach(
        &self,
        unit: &BattalionUnit,
        ctx: &Ctx,
        goal: Option<HexCoord>,
        min_standoff: Option<i32>,
    ) -> Option<AiAction> {
        let goal = goal?;
        let free = |h: HexCoord| {
            ctx.grid.cell(h).map(|c| c.is_passable).unwrap_or(false)
                && !ctx.reserved.contains(&h)
                && !ctx
                    .all
                    .iter()
                    .any(|u| u.id != unit.id && u.is_combat_effective() && u.position == h)
        };
        // Destination hysteresis (§7.2 order persistence): a fresh
        // position-dependent goal (the intel ring, a moving enemy) must
        // not re-base the destination every turn — every flip wipes the
        // invested march hours at the apply layer and the progress bar
        // sits frozen at one turn's budget (the "has order, bar not
        // growing" report). Keep the standing destination while it stays
        // free and within the objective's neighbourhood; the objective
        // area advances only when the goal truly moves on.
        let mut path: Option<Vec<HexCoord>> = None;
        if let Some(dest) = unit
            .move_order
            .as_ref()
            .and_then(|o| o.path.last())
            .copied()
        {
            if dest != unit.position && dest.distance(goal) <= GOAL_HYSTERESIS_RADIUS && free(dest)
            {
                if let Some((p, _)) = find_path(ctx.grid, unit, &ctx.all, dest, &self.params) {
                    if !p.is_empty() {
                        path = Some(p);
                    }
                }
            }
        }
        let mut path = match path {
            Some(p) => p,
            None => {
                let (mut p, _) = match find_path(ctx.grid, unit, &ctx.all, goal, &self.params) {
                    Some((p, _)) if !p.is_empty() => (p, ()),
                    _ => {
                        // Goal blocked: aim for the cheapest reachable adjacent hex.
                        let mut best: Option<(Vec<HexCoord>, f32)> = None;
                        for n in goal.neighbors() {
                            if let Some((p, c)) =
                                find_path(ctx.grid, unit, &ctx.all, n, &self.params)
                            {
                                if p.is_empty() {
                                    continue;
                                }
                                let better =
                                    best.as_ref().map(|(_, bc)| c < *bc).unwrap_or(true);
                                if better {
                                    best = Some((p, c));
                                }
                            }
                        }
                        (best?.0, ())
                    }
                };
                // A FRIENDLY-held goal is a queue anchor, not a
                // destination: an occupied far-off objective froze whole
                // columns behind it ("has order, no movement" — the
                // dense-deployment live report). March to the goal's
                // edge and stop on the last FREE hex; the final step
                // into the goal belongs to execution-time blocking
                // (§6.9). Enemy-held goals keep the old semantics —
                // marching into contact is how the line engages.
                let friendly_on_goal = ctx.all.iter().any(|u| {
                    u.id != unit.id
                        && u.side == unit.side
                        && u.is_combat_effective()
                        && u.position == goal
                });
                if friendly_on_goal {
                    if p.last() == Some(&goal) {
                        p.pop();
                    }
                    if p.is_empty() {
                        // Queued right behind the anchor with nowhere left
                        // to stand on the approach: sidestep onto the
                        // cheapest free ring hex instead of booking the
                        // anchor's own hex.
                        let mut best: Option<(Vec<HexCoord>, f32)> = None;
                        for n in goal.neighbors() {
                            if !free(n) {
                                continue;
                            }
                            if let Some((p, c)) =
                                find_path(ctx.grid, unit, &ctx.all, n, &self.params)
                            {
                                if p.is_empty() {
                                    continue;
                                }
                                let better =
                                    best.as_ref().map(|(_, bc)| c < *bc).unwrap_or(true);
                                if better {
                                    best = Some((p, c));
                                }
                            }
                        }
                        p = best?.0;
                    }
                }
                p
            }
        };
        if let Some(ms) = min_standoff {
            // The standoff cut reads the PHYSICAL enemy (the fog-filtered
            // list shows nothing ahead of a blind-marching battery, so the
            // old cut never fired and the guns walked onto the garrison's
            // own hex, tripped the d < standoff escape rule, and oscillated
            // forever — Narvik seed 4: city-edge → 40-hex retreat loop,
            // 288 turns, 0 fire missions). The cut distance is the MELEE
            // standoff, not the attack range: cutting at the envelope edge
            // (attack_range) parks the guns against EVERY physical foe
            // simultaneously, freezing the advance behind a deep defensive
            // line (Ioannina: the city sat 40 hexes behind the packed line
            // and the battery never got its target in range — 2/10 → 0/10).
            // Cutting only at contact (standoff 2) lets the march advance
            // behind the friendly front until the blind-fire goal enters
            // the envelope, where plan_support_fire's emplace branch parks
            // the guns for good.
            let foes: &[BattalionUnit] = if ctx.physical_foes.is_empty() {
                ctx.enemy
            } else {
                &ctx.physical_foes
            };
            let cut = path
                .iter()
                .position(|h| min_enemy_dist(*h, foes) < ms)
                .unwrap_or(path.len());
            path.truncate(cut);
        }
        // Never book a destination claimed by an earlier proposal (§6.9).
        while path
            .last()
            .map(|h| ctx.reserved.contains(h))
            .unwrap_or(false)
        {
            path.pop();
        }
        if path.is_empty() {
            return None;
        }
        Some(AiAction::MoveUnit {
            unit_id: unit.id,
            path,
        })
    }

    /// §7.2 mass_charge: advance exactly one hex toward the nearest enemy
    /// (or the objective when blind).
    fn advance_one_hex(&self, unit: &BattalionUnit, ctx: &Ctx) -> Option<AiAction> {
        let goal = nearest_enemy(unit.position, ctx.enemy)
            .map(|e| e.position)
            .or_else(|| ctx.blind_goal(unit.position))?;
        let cur = unit.position.distance(goal);
        self.best_step(unit, ctx, |h, cost| {
            let d = h.distance(goal);
            (d < cur).then_some(d as f32 * 100.0 + cost)
        })
        .map(|(_, path)| AiAction::MoveUnit {
            unit_id: unit.id,
            path,
        })
    }

    /// §7.2 elastic_defense / tactical_withdrawal: step one hex directly away
    /// from the enemy, increasing the distance to the nearest enemy.
    /// Line anchoring: never fall back INTO a ford (a unit caught
    /// mid-river takes double damage, §6.6); among equal-distance steps the
    /// best defensive ground wins (tie-break only — distance stays primary).
    fn fall_back_one_hex(&self, unit: &BattalionUnit, ctx: &Ctx) -> Option<AiAction> {
        let cur = min_enemy_dist(unit.position, ctx.enemy);
        self.best_step(unit, ctx, |h, cost| {
            let t = ctx.grid.cell(h).map(|c| c.terrain)?;
            if t == Terrain::River {
                return None;
            }
            let d = min_enemy_dist(h, ctx.enemy);
            (d > cur).then(|| -(d as f32) * 100.0 + cost - t.cover_percent() * 30.0)
        })
        .map(|(_, path)| AiAction::MoveUnit {
            unit_id: unit.id,
            path,
        })
    }

    /// §7.2 default: prefer defensive terrain — sidestep one hex into better
    /// cover when it does not move the unit into contact. v3.3:
    /// ground quality IS cover (the defense column is retired), and river
    /// hexes are never "cover" (negative — exposed mid-ford).
    fn shift_to_cover(&self, unit: &BattalionUnit, ctx: &Ctx) -> Option<AiAction> {
        let ground = |t: Terrain| t.cover_percent();
        let cur_def = ctx
            .grid
            .cell(unit.position)
            .map(|c| ground(c.terrain))
            .unwrap_or(0.0);
        let cur_dist = min_enemy_dist(unit.position, ctx.enemy);
        self.best_step(unit, ctx, |h, cost| {
            if min_enemy_dist(h, ctx.enemy) < cur_dist {
                return None; // don't drift toward the enemy while idling
            }
            let t = ctx.grid.cell(h).map(|c| c.terrain)?;
            if t == Terrain::River {
                return None;
            }
            let def = ground(t);
            (def > cur_def).then(|| -def * 100.0 + cost)
        })
        .map(|(_, path)| AiAction::MoveUnit {
            unit_id: unit.id,
            path,
        })
    }

    /// River discipline for the second line: form the standoff
    /// screen on the OWN side of any river. The old approach-cut only
    /// checked distance, so the A* route could deposit the reinforcing unit
    /// on the threat's side of a river (Sedan trace: 4.Inf ordered across
    /// the Meuse into the panzers' lap). Picks the best ring hex at exactly
    /// `standoff` from the threat: river-shielded first (a river between the
    /// hex and the threat), then defensive terrain, then shortest travel.
    /// River hexes themselves are never valid screens. A unit already
    /// holding a shielded ring hex stays put (no per-turn dancing); and when
    /// every reachable ring hex is on the threat's side while the unit is
    /// already behind the river — i.e. the shielded ring is manned by the
    /// front line — the unit holds instead of crossing (manned-ring rule).
    /// Falls back to the plain approach cut when no ring hex is reachable.
    fn screen_threat(
        &self,
        unit: &BattalionUnit,
        ctx: &Ctx,
        threat: Option<HexCoord>,
        standoff: i32,
    ) -> Option<AiAction> {
        let threat = threat?;
        let cur_d = min_enemy_dist(unit.position, ctx.enemy);
        let currently_shielded =
            river_between(&unit.position, ctx.grid, threat.q as f32, threat.r as f32);
        if cur_d == standoff && currently_shielded {
            return Some(AiAction::Hold { unit_id: unit.id });
        }
        let occupied: Vec<HexCoord> = ctx
            .all
            .iter()
            .filter(|u| u.id != unit.id && u.is_combat_effective())
            .map(|u| u.position)
            .collect();
        let best = threat
            .ring(standoff)
            .into_iter()
            .filter(|h| !occupied.contains(h) && !ctx.reserved.contains(h))
            .filter(|h| {
                ctx.grid
                    .cell(*h)
                    .map(|c| c.is_passable && c.terrain != Terrain::River)
                    .unwrap_or(false)
            })
            .filter_map(|h| {
                let (path, cost) = find_path(ctx.grid, unit, &ctx.all, h, &self.params)?;
                if path.is_empty() {
                    return None;
                }
                // Defensive repositioning never fords: a shielded hex on the
                // far bank is still a crossing (Sedan turn 5: 1.Inf ordered
                // WEST across the Meuse to "hide" behind it from a panzer
                // that was already over — abandoning the east-bank line).
                if path.iter().any(|h| {
                    ctx.grid
                        .cell(*h)
                        .map(|c| c.terrain == Terrain::River)
                        .unwrap_or(false)
                }) {
                    return None;
                }
                let shielded = river_between(&h, ctx.grid, threat.q as f32, threat.r as f32);
                let mut score = -cost;
                if shielded {
                    score += 80.0;
                }
                let t = ctx
                    .grid
                    .cell(h)
                    .map(|c| c.terrain)
                    .unwrap_or(Terrain::Plains);
                score += t.cover_percent() * 2.0;
                Some((score, shielded, path))
            })
            .max_by(|a, b| {
                a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal).then(
                    a.2.last()
                        .map(|h| (h.q, h.r))
                        .cmp(&b.2.last().map(|h| (h.q, h.r))),
                )
            });
        match best {
            Some((_, shielded, path)) => {
                // Every reachable ring hex is on the threat's side of the
                // river — i.e. the shielded ring is already manned by the
                // front line. If WE are already behind the river, stay put:
                // reinforcing across would repeat 4.Inf's Sedan adventure.
                if !shielded && currently_shielded {
                    return Some(AiAction::Hold { unit_id: unit.id });
                }
                Some(AiAction::MoveUnit {
                    unit_id: unit.id,
                    path,
                })
            }
            None => {
                // No dry-route ring hex at all: behind the river that means
                // "the shielded ring is manned — the line is holding", stay;
                // anywhere else the map is simply weird — plain approach cut.
                if currently_shielded {
                    Some(AiAction::Hold { unit_id: unit.id })
                } else {
                    self.approach(unit, ctx, Some(threat), Some(standoff))
                }
            }
        }
    }

    /// Doctrine one-hex step: score the passable, unclaimed, unoccupied
    /// neighbour hexes and order the best. `score` returns `None` for
    /// rejected hexes; lower is better, with the step's hours as tie-break.
    /// A one-hex order persists across turns (§6.2), so even a 3 km/h towed
    /// gun completes it eventually — no per-turn reach budget is applied.
    fn best_step(
        &self,
        unit: &BattalionUnit,
        ctx: &Ctx,
        score: impl Fn(HexCoord, f32) -> Option<f32>,
    ) -> Option<(HexCoord, Vec<HexCoord>)> {
        let enemy_zoc =
            tactical_core::pathfinding::zoc_hexes(ctx.grid, &ctx.all, unit.side.opponent());
        let occupied: Vec<HexCoord> = ctx
            .all
            .iter()
            .filter(|u| u.id != unit.id && u.is_combat_effective())
            .map(|u| u.position)
            .collect();
        let best = ctx
            .grid
            .passable_neighbors(unit.position)
            .into_iter()
            .filter(|h| !occupied.contains(h) && !ctx.reserved.contains(h))
            .filter_map(|h| {
                let cost = tactical_core::pathfinding::step_hours(
                    ctx.grid,
                    unit,
                    &enemy_zoc,
                    unit.position,
                    h,
                    &self.params,
                )?;
                score(h, cost).map(|s| (s, cost, h))
            })
            .min_by(|a, b| {
                a.0.partial_cmp(&b.0)
                    .unwrap_or(Ordering::Equal)
                    .then(a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal))
                    .then(a.2.q.cmp(&b.2.q))
                    .then(a.2.r.cmp(&b.2.r))
            })?;
        Some((best.2, vec![best.2]))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Free helpers
// ────────────────────────────────────────────────────────────────────────────

/// Effective combat power of a force (§7.1 layer 1 global evaluation).
fn force_power(units: &[BattalionUnit]) -> f32 {
    units
        .iter()
        .filter(|u| u.is_combat_effective())
        .map(|u| u.strength * u.org_ratio())
        .sum()
}

/// Distance to the nearest living enemy; `i32::MAX` when none exist.
fn min_enemy_dist(h: HexCoord, enemy: &[BattalionUnit]) -> i32 {
    enemy
        .iter()
        .filter(|e| e.is_combat_effective())
        .map(|e| h.distance(e.position))
        .min()
        .unwrap_or(i32::MAX)
}

/// The FIRE-envelope variant: also counts retreating enemies. They hold no
/// ground (the melee standoff/escape reads stay on [`min_enemy_dist`]), but
/// they are targetable free damage until they exit the map — a gun must not
/// limber away from, or skip emplacing for, a target the fog of the
/// effective-only filter would hide.
fn min_enemy_dist_incl_retreating(h: HexCoord, enemy: &[BattalionUnit]) -> i32 {
    enemy
        .iter()
        .filter(|e| e.is_combat_effective() || e.state == UnitState::Retreating)
        .map(|e| h.distance(e.position))
        .min()
        .unwrap_or(i32::MAX)
}

/// An adjacent combat-effective VISIBLE enemy — contact-ring membership.
/// §6.5 consumes any march at the first step into the ring, so a unit that
/// keeps "approaching" while adjacent just shuffles in place turn after
/// turn: when the assault gates refuse, holding the contact beats
/// re-ramming it.
fn contact_foe(unit: &BattalionUnit, enemy: &[BattalionUnit]) -> bool {
    enemy
        .iter()
        .any(|e| e.is_combat_effective() && unit.position.distance(e.position) == 1)
}

fn nearest_enemy(h: HexCoord, enemy: &[BattalionUnit]) -> Option<&BattalionUnit> {
    enemy
        .iter()
        .filter(|e| e.is_combat_effective())
        .min_by(|a, b| {
            h.distance(a.position)
                .cmp(&h.distance(b.position))
                .then(a.id.cmp(&b.id))
        })
}

/// Other living enemies adjacent to this one (its local support).
fn adjacent_enemy_friends(target: &BattalionUnit, enemy: &[BattalionUnit]) -> usize {
    enemy
        .iter()
        .filter(|e| {
            e.id != target.id
                && e.is_combat_effective()
                && e.position.distance(target.position) == 1
        })
        .count()
}

/// §7.2 guerrilla_tactics: prefer isolated enemy battalions.
fn most_isolated_enemy(h: HexCoord, enemy: &[BattalionUnit]) -> Option<&BattalionUnit> {
    enemy
        .iter()
        .filter(|e| e.is_combat_effective())
        .min_by(|a, b| {
            adjacent_enemy_friends(a, enemy)
                .cmp(&adjacent_enemy_friends(b, enemy))
                .then(h.distance(a.position).cmp(&h.distance(b.position)))
                .then(a.id.cmp(&b.id))
        })
}

/// §7.2 infiltration_assault: the most weakly-held enemy hex (fewest
/// supporting friends, then nearest).
fn weakest_held_enemy(h: HexCoord, enemy: &[BattalionUnit]) -> Option<&BattalionUnit> {
    most_isolated_enemy(h, enemy)
}

/// Weakness ordering: lowest org, then lowest strength (§7.2 "weakest hex").
fn weakness_cmp(a: &BattalionUnit, b: &BattalionUnit) -> Ordering {
    a.org.partial_cmp(&b.org).unwrap_or(Ordering::Equal).then(
        a.strength
            .partial_cmp(&b.strength)
            .unwrap_or(Ordering::Equal),
    )
}

/// Sort key for assault targets: lower is juicier. Deliberately NO unit id:
/// including it made every key unique and the RNG tie-break in `try_assault`
/// unreachable (its `tied` list always held one entry).
/// Equal-key targets are picked by the seeded RNG instead.
/// The default path sorts the JOINABLE POOL first (bigger pool =
/// juicier victim, negated) — shared criteria make every adjacent attacker
/// converge on the same target, so the pooled volley actually forms.
fn target_key(
    t: &BattalionUnit,
    enemy: &[BattalionUnit],
    prefer_isolated: bool,
    pool: usize,
) -> (usize, i64, i64, i64) {
    let friends = adjacent_enemy_friends(t, enemy);
    // A retreating target is free damage (it never counters) — rank it
    // like an already-broken (org-0) remnant so the volley prefers the
    // leaver over a same-pool standing unit.
    let org = if t.state == UnitState::Retreating {
        0
    } else {
        (t.org * 100.0) as i64
    };
    let str_ = (t.strength * 100.0) as i64;
    if prefer_isolated {
        // Isolation count leads; the pool slot folds to 0 on this path.
        (friends, 0, org, str_)
    } else {
        (0, -(pool as i64), org, str_)
    }
}

/// §7.3: refuse assault when locally outnumbered 3:1. Local odds are counted
/// around the *target hex*: the attacker plus its adjacent friends versus the
/// defender plus its adjacent friends. Passive friendlies (§7.5) count as
/// friends — an allied battalion adjacent to the target tips the
/// local odds even though this planner never commands it.
fn local_odds_acceptable(attacker: &BattalionUnit, target_hex: HexCoord, ctx: &Ctx) -> bool {
    let friends = 1 + ctx
        .own
        .iter()
        .chain(ctx.passive.iter())
        .filter(|u| {
            u.id != attacker.id && u.is_combat_effective() && u.position.distance(target_hex) <= 1
        })
        .count();
    let foes = ctx
        .enemy
        .iter()
        .filter(|u| u.is_combat_effective() && u.position.distance(target_hex) <= 1)
        .count();
    (foes as f32) < REFUSE_ODDS * friends as f32
}

/// One-strike org-damage estimate for AI decisions — the SAME
/// numbers the resolution applies: the whole formula lives in
/// `tactical-core::damage::strike_org_damage`, which `strike()` also
/// computes through (the est=strike invariant is now by
/// construction, not by hand-kept mirrors). `target_uses_breakthrough`
/// matches the resolver: our unit caught mid-attack defends with
/// breakthrough when the enemy counter-fires it. `form` is the
/// firepower regime — callers pass the resolver's rule: Aimed for assault /
/// direct estimates and for every counter (the direct-lay gate removed
/// long-range counter-battery), Area for fire missions. One deliberate
/// plan-side gap: the command aura is absent (planning has no links),
/// making the estimate slightly conservative.
/// (The old `flanked = false` gap retired with the
/// flank bonus itself.)
///
/// Returns the UNCAPPED expectation (the resolver caps delivery at
/// `org_cap_ratio` of the target's max org): the trade-ratio gate compares
/// potential damages, where a cap would hide the imbalance; the
/// break-outright check applies the cap itself (one strike can never break
/// more than the cap allows).
fn est_org_damage(
    attacker: &BattalionUnit,
    target: &BattalionUnit,
    target_uses_breakthrough: bool,
    params: &CombatParams,
    grid: &HexGrid,
    distance: i32,
    form: tactical_core::damage::FirepowerForm,
) -> f32 {
    tactical_core::damage::strike_org_damage(
        attacker,
        target,
        target_uses_breakthrough,
        distance,
        false,
        false,
        grid,
        params,
        form,
    )
}

/// §7.2 tactical_withdrawal: the rearguard is the healthy, frontline
/// (range-1, non-support) battalion closest to the enemy — it holds while the
/// rest fall back.
fn pick_rearguard(own: &[BattalionUnit], enemy: &[BattalionUnit]) -> Option<usize> {
    own.iter()
        .filter(|u| {
            u.is_combat_effective()
                && u.org_ratio() >= DAMAGED_ORG_RATIO
                && u.attack_range == 1
                && !u.unit_type.is_support_company()
        })
        .min_by(|a, b| {
            min_enemy_dist(a.position, enemy)
                .cmp(&min_enemy_dist(b.position, enemy))
                .then(a.id.cmp(&b.id))
        })
        .map(|u| u.id)
}

/// Waypoint 2 hexes past the enemy flank (perpendicular to the own–enemy
/// axis), snapped to the nearest passable in-bounds hex. Used by flanking
/// armor (§7.2 encirclement) and probing recon (§7.2 infiltration_assault).
fn flank_waypoint(
    grid: &HexGrid,
    own: &[BattalionUnit],
    enemy: &[BattalionUnit],
    left: bool,
) -> Option<HexCoord> {
    let (eq, er) = centroid(enemy)?;
    let (oq, orr) = centroid(own)?;
    let (dq, dr) = (eq - oq, er - orr);
    // Perpendicular to the advance axis.
    let (mut pq, mut pr) = if left { (-dr, dq) } else { (dr, -dq) };
    let len = (pq * pq + pr * pr).sqrt();
    if len < 1e-3 {
        // Forces overlap — pick an arbitrary bearing.
        pq = 1.0;
        pr = 0.0;
    } else {
        pq /= len;
        pr /= len;
    }
    let (tq, tr) = (eq + pq * 2.0, er + pr * 2.0);
    // Local search window: the ideal point is within a hex or two of the
    // enemy centroid, so the nearest passable hex sits inside a small ring
    // around it — scanning the WHOLE grid per flank/probe unit per turn was
    // the dominant planner cost on 512×512 maps. The full-grid
    // scan remains as the fallback for maps whose window is all water.
    let anchor = HexCoord::new(tq.round() as i32, tr.round() as i32);
    let window: Vec<HexCoord> = anchor.hexes_in_range(3);
    let nearest = |cands: &[HexCoord]| {
        cands
            .iter()
            .filter(|h| grid.cell(**h).map(|c| c.is_passable).unwrap_or(false))
            .min_by(|a, b| {
                let da = (a.q as f32 - tq).powi(2) + (a.r as f32 - tr).powi(2);
                let db = (b.q as f32 - tq).powi(2) + (b.r as f32 - tr).powi(2);
                da.partial_cmp(&db)
                    .unwrap_or(Ordering::Equal)
                    .then(a.q.cmp(&b.q))
                    .then(a.r.cmp(&b.r))
            })
            .copied()
    };
    nearest(&window).or_else(|| {
        let all: Vec<HexCoord> = grid.iter_coords().collect();
        nearest(&all)
    })
}

/// Centroid of living units, in axial space.
fn centroid(units: &[BattalionUnit]) -> Option<(f32, f32)> {
    let mut n = 0usize;
    let (mut sq, mut sr) = (0.0f32, 0.0f32);
    for u in units.iter().filter(|u| u.is_combat_effective()) {
        sq += u.position.q as f32;
        sr += u.position.r as f32;
        n += 1;
    }
    (n > 0).then(|| (sq / n as f32, sr / n as f32))
}
