//! tactical-combat — combat resolution engine.
//!
//! **Hit-step × numbers-squared model** (v3.2 rework;
//! terrain layer v3.3):
//!
//! ```text
//! q = (soft×(1−h) + hard×h×piercing tier) × precision factor
//! P = q × g² aimed (g = strength ratio); AREA fire (indirect-artillery
//!   fire missions) is linear: P = q × g — "shells are shells";
//!   pooled aimed fire: P = Σ(q_i·g_i) × Σg_i
//! D = defense (or breakthrough) × Hold × entrenchment × battalion terrain
//!     defense adjuster (v3.3 — the global terrain defense column is retired)
//! hit = hit_base + (hit_saturated − hit_base) × P/(P+D)   (vanilla 10%/40%)
//! org damage = K₂ × P × hit × ∏linear modifiers   (deterministic, no jitter)
//! linear modifiers (NEVER inside P): battalion terrain attack adjuster,
//!   direct-fire falloff beyond range 1 (×0.6), terrain cover (negative on
//!   Desert/River = exposed; the river ×2 ford rule is retired), command
//!   aura ±10%, melee elevation / indirect crest
//! hard cap: one org hit ≤ max_org × 0.40; a target is Shocked when the
//!   org damage DELIVERED to it within one fire phase, aggregated across
//!   every lane (a group volley's per-attacker shares are summed per target
//!   first), reaches max_org × 0.25
//! strength damage = org damage × 0.12 × (max_str/max_org) — a full break
//!   costs 12% of max strength for every class (vanilla break cost); a
//!   target already at org 0 when the volley starts converts at 0.68
//!   (`broken_str_loss`, the vanilla dice ratio) instead
//! ```
//!
//! **Formula-chain capture:** every strike path also fills a
//! `tactical_core::damage::HitBreakdown` (q → P → D → hit → linear stack →
//! cap → delivered org/str), threaded DamageEntry → CombatResult → the
//! engagement-detail panel. Capture is pure bookkeeping — no RNG draws, no
//! behavior change, deterministic battles stay bit-identical.
//!
//! **Rocket artillery:** a salvo strikes
//! EVERY unit inside the 7-hex zone (aim hex + neighbours) at full
//! tube-artillery strength — same accuracy factor (0.30), no dilution, no
//! precision shot, and no friend-or-foe discrimination. Friendly fire is the
//! price of saturation; the AI refuses aim hexes whose zone contains friends.
//!
//! **Unified fire phase (预令统一结算)**: the acting side only REGISTERS
//! attack orders during its turn; at end of turn, after the movement phase,
//! every order resolves together — all damage is computed first and applied
//! simultaneously, so mutual destruction is possible and counter-fire is
//! symmetric. Counter-fire: the defender's firepower split LINEARLY (P/n,
//! fire conservation) across every attacker inside the defender's own range,
//! each share evaluating its own hit step against the caught attacker's
//! breakthrough. Direct-lay gate: indirect artillery
//! replies only inside its direct-lay self-defense circle
//! (`counter_direct_lay_range` = 2) — no radar-less counter-battery — and
//! rocket launchers never counter, so every surviving counter is AIMED
//! fire (square law) by construction.
//!
//! All randomness flows through the engine's seeded [`XorShift64`], so a
//! battle is fully deterministic from its seed.
//!
//! Spec: DESIGN.md §6.3, §6.4, §6.6, §6.8, §12.

use std::collections::HashMap;

use tactical_core::damage::{HitBreakdown, LinearFactor, PoolInfo};
use tactical_core::encirclement::{detect_encirclement, org_attrition_fraction, EncirclementLevel};
use tactical_core::{
    compute_command_links, in_command, BattalionUnit, CombatParams, CommandLink, HexCoord,
    HexDirection, HexGrid, Side, Terrain, UnitState, XorShift64,
};

/// One registered attack order (pre-order mode): resolved in the
/// unified fire phase at end of turn, NEVER immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttackOrder {
    pub attacker: usize,
    pub target: AttackTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttackTarget {
    /// Adjacent close combat: occupies the hex if the defender breaks (§6.3).
    Assault(usize),
    /// Ranged direct fire at a specific unit (armor / AT range 2).
    DirectFire(usize),
    /// Indirect fire mission on a hex:
    /// PRECISE = right-clicking a visible enemy unit — 100% damage on that
    /// hex. AREA = the F-key barrage / intel-goal fire — every targetable
    /// unit (friends included) in the hex + 6 neighbours takes ITS OWN
    /// strike weighted by its hex: the aim hex 4/10, each neighbour 1/10
    /// (`CombatParams.area_center_share` / `area_neighbor_share`).
    /// Rockets always resolve as area and strike EVERY unit in the zone at
    /// full strength — friends included (§6.3).
    FireMission { hex: HexCoord, precise: bool },
}

/// Outcome of one resolved attack order (one lane of the fire phase).
/// Damage fields are the amounts actually subtracted, after simultaneous
/// application — `taken` is the counter-fire the attacker received.
#[derive(Debug, Clone, Default)]
pub struct CombatResult {
    pub attacker_id: usize,
    pub defender_id: usize,
    /// Hex the exchange happened on (the defender's hex) — camera focus for
    /// the end-of-turn battle tour (SD2-style).
    pub hex: HexCoord,
    /// Org damage the defender took (0 when `target_lost`).
    pub org_damage_dealt: f32,
    /// Strength damage the defender took.
    pub str_damage_dealt: f32,
    /// Org damage the attacker took (counter-fire).
    pub org_damage_taken: f32,
    /// Strength damage the attacker took (counter-fire).
    pub str_damage_taken: f32,
    /// Defender's org hit 0 and it entered involuntary retreat (§6.8).
    pub defender_broken: bool,
    /// Assaulting unit moved into the vacated hex (§6.3 Assault).
    pub advanced: bool,
    /// Defender broke while fully encircled → surrender (§6.4).
    pub surrendered: bool,
    /// Defender was annihilated — strength wiped out.
    pub eliminated: bool,
    /// Defender was shocked by the hit (≥ 25% max org).
    pub shocked_defender: bool,
    /// Attacker was shocked by the counter-fire.
    pub shocked_attacker: bool,
    /// Order fizzled: target died / moved out of the envelope before the
    /// fire phase ("Target lost" orange floater).
    pub target_lost: bool,
    /// The full formula chain of the outgoing strike (None for a
    /// fizzled order), for the engagement-detail panel.
    pub breakdown: Option<HitBreakdown>,
    /// The defender's counter-fire chain, when it fired back.
    pub counter_breakdown: Option<HitBreakdown>,
}

/// Deterministic combat engine. Holds the config constants (§12) and the
/// seeded PRNG; every random draw goes through `self.rng`.
/// Clone: checkpoint snapshots (restart/rollback).
#[derive(Clone)]
pub struct CombatEngine {
    params: CombatParams,
    rng: XorShift64,
    /// Unit ids shocked by the most recent [`Self::resolve_fire_phase`]:
    /// [`Self::expire_shocks`] keeps exactly these and clears
    /// everything older, so a shock always persists until the END OF THE
    /// NEXT turn-end after the one that inflicted it.
    last_shocked: Vec<usize>,
    /// §6.13: HQs annihilated in the most recent fire phase and
    /// the division-wide org collapse each caused. Drained by the game loop
    /// via [`Self::take_hq_events`] (log lines, floaters, HOI4 injection).
    last_hq_events: Vec<HqLossEvent>,
    /// §6.8: the battle's deployment zones — the
    /// DEFENDER's routs are scored against its own zone's eastern rim (the
    /// reachable province boundary) instead of the map edge, which stitched
    /// maps can place outside the battle province.
    retreat_zones: Option<(Vec<HexCoord>, Vec<HexCoord>)>,
}

/// §6.13: record of a division HQ annihilated in the fire phase
/// (strength 0 — retreat/surrender do NOT count) and the command collapse
/// that followed: every surviving same-division battalion loses
/// `hq_death_org_frac` of its max org.
#[derive(Debug, Clone, Default)]
pub struct HqLossEvent {
    pub hq_id: usize,
    pub division: String,
    /// (unit id, org lost) per affected battalion.
    pub losses: Vec<(usize, f32)>,
    /// Ids of battalions the collapse broke (org 0 → retreat/surrender).
    pub broke: Vec<usize>,
}

/// One pending damage application in the fire-phase ledger.
struct DamageEntry {
    src: usize,
    dst: usize,
    org: f32,
    str_: f32,
    counter: bool,
    /// This strike's formula chain (None only for paths that
    /// never computed one — currently all real strikes carry it).
    breakdown: Option<HitBreakdown>,
}

impl CombatEngine {
    pub fn new(params: CombatParams, seed: u64) -> Self {
        CombatEngine {
            params,
            rng: XorShift64::new(seed),
            last_shocked: Vec::new(),
            last_hq_events: Vec::new(),
            retreat_zones: None,
        }
    }

    /// §6.8: attach the battle's deployment zones so
    /// the DEFENDER's retreats aim at its own zone's eastern rim (the
    /// reachable province boundary) instead of the map edge. Called once at
    /// battle start; clone-safe for checkpoint snapshots.
    pub fn set_retreat_zones(&mut self, zones: Option<(Vec<HexCoord>, Vec<HexCoord>)>) {
        self.retreat_zones = zones;
    }

    pub fn params(&self) -> &CombatParams {
        &self.params
    }

    /// Drain the HQ-loss events (§6.13) accumulated by the most
    /// recent [`Self::resolve_fire_phase`].
    pub fn take_hq_events(&mut self) -> Vec<HqLossEvent> {
        std::mem::take(&mut self.last_hq_events)
    }

    /// Shock bookkeeping at a side's turn end: a shock inflicted during
    /// the fire phase that just ran
    /// PERSISTS; every older shock wears off. Net effect: a shock always
    /// lasts until the end of the next turn-end after the one that caused
    /// it — counter-fire shock now survives the inflicting turn instead of
    /// being wiped immediately. Call once per turn end, AFTER
    /// [`Self::resolve_fire_phase`] (or without one — all stale shocks then
    /// expire). Covers BOTH sides' units.
    pub fn expire_shocks(&mut self, units: &mut [BattalionUnit]) {
        for u in units.iter_mut() {
            if u.shocked && !self.last_shocked.contains(&u.id) {
                u.shocked = false;
            }
        }
        self.last_shocked.clear();
    }

    /// §6.6 crest gain: for MELEE fights
    /// (distance ≤ 1) the height difference decides — each level of
    /// advantage over the defender adds `melee_elevation_gain`, clamped at
    /// `melee_elevation_cap` (3 levels). Flat ground or non-contact fights
    /// are neutral (×1.0). Symmetric by construction: the same strike run
    /// for the defender's counter-fire gives the uphill unit the edge both
    /// ways. Core-exported so the AI mirror uses the SAME numbers.
    fn melee_elevation_factor(
        &self,
        grid: &HexGrid,
        a_pos: HexCoord,
        d_pos: HexCoord,
        distance: i32,
    ) -> f32 {
        tactical_core::los::melee_elevation_mult(
            grid,
            a_pos,
            d_pos,
            distance,
            self.params.melee_elevation_gain,
            self.params.melee_elevation_cap,
        )
    }

    /// One strike from `units[ai]` against `units[di]` (§6.3 v3.2).
    /// `target_uses_breakthrough`: the target defends with breakthrough
    /// instead of defense — the HOI4 convention for a unit caught
    /// mid-attack (i.e. counter-fire against the original attacker).
    ///
    /// Returns (org_damage, str_damage, breakdown) AFTER the optional
    /// jitter (default off — `random_spread` is 0 under v3.2 deterministic
    /// resolution) and the hard cap (`org_cap_ratio`); the shock test is
    /// applied at application time against the delivered amount. The
    /// breakdown carries the whole formula chain for the
    /// engagement-detail panel.
    ///
    /// The formula itself lives in `tactical-core::damage` — the
    /// AI planning estimate computes through the SAME function, so plan and
    /// resolution can never drift apart again.
    fn strike(
        &mut self,
        grid: &HexGrid,
        units: &[BattalionUnit],
        ai: usize,
        di: usize,
        distance: i32,
        target_uses_breakthrough: bool,
        links: &[CommandLink],
    ) -> (f32, f32, HitBreakdown) {
        let a = &units[ai];
        let d = &units[di];

        let mut bd = HitBreakdown::default();
        // `strike` serves FIRE MISSIONS only (assault/direct-fire
        // lanes all route through `strike_group`) — indirect fire is the
        // AREA regime, linear in numbers (P = q·g, shells are shells).
        let mut org = tactical_core::damage::strike_org_damage_explained(
            a,
            d,
            target_uses_breakthrough,
            distance,
            in_command(links[ai]),
            in_command(links[di]),
            grid,
            &self.params,
            1.0,
            tactical_core::damage::FirepowerForm::Area,
            &mut bd,
        );

        // Optional jitter — DISABLED by default (§6.3 v3.2 deterministic
        // resolution); the mechanism is kept for a possible
        // resolution-fog mode.
        let spread = self.params.random_spread;
        if spread > 0.0 {
            let j = 1.0 - spread + self.rng.next_f32() * spread * 2.0;
            org *= j;
            bd.jitter_mult = j;
        }

        // Hard cap; strength rides the delivered org at the target's
        // pool-shaped rate (a full break costs `break_str_loss` of max
        // str, or `broken_str_loss` when the target is already at org 0 —
        // judged here, before delivery, i.e. at volley start).
        bd.org_cap = d.max_org * self.params.org_cap_ratio;
        bd.org_capped = org > bd.org_cap;
        org = org.min(bd.org_cap);
        bd.org_pool_final = org;
        bd.org_final = org;
        let str_ = tactical_core::damage::strength_damage_explained(org, d, &self.params, &mut bd);
        (org, str_, bd)
    }

    /// Merged group strike (§6.3 v3.2 concentration): the pool's firepower
    /// is P = Σ(q_i·g_i) × Σg_i — quality linear, numbers squared; two
    /// half-strength battalions equal one full one (fire conservation) and
    /// combined-arms pairs cross-multiply. Per-attacker linear factors
    /// (direct-fire falloff, melee elevation, command share, terrain attack
    /// adjuster) are weighted by each attacker's contribution q_i·g_i;
    /// damage is credited in that same proportion. (Piercing no longer
    /// needs a pool average — it sits inside each attacker's q.)
    fn strike_group(
        &mut self,
        grid: &HexGrid,
        units: &[BattalionUnit],
        ais: &[(usize, i32)],
        di: usize,
        ledger: &mut Vec<DamageEntry>,
        links: &[CommandLink],
    ) {
        let d = &units[di];
        let def_terrain = grid
            .cell(d.position)
            .map(|c| c.terrain)
            .unwrap_or(Terrain::Plains);
        let mut sum_qg = 0.0;
        let mut sum_g = 0.0;
        let mut falloff_weighted = 0.0;
        let mut cmd_weighted = 0.0;
        let mut melee_weighted = 0.0;
        let mut adj_weighted = 0.0;
        for (ai, dist) in ais {
            let a = &units[*ai];
            // The attacker's contribution unit: quality × its own guns.
            let w = tactical_core::damage::attack_quality(a, d) * a.strength_ratio();
            sum_qg += w;
            sum_g += a.strength_ratio();
            // §6.13: attack-weighted share of in-command
            // attackers — the pool bonus stays linear, never squared.
            if in_command(links[*ai]) {
                cmd_weighted += w;
            }
            // Direct-fire falloff stays LINEAR on damage: the
            // pool's effective falloff is the contribution-weighted average,
            // so a lone sniper at range 2 gets exactly ×0.6.
            let f = if *dist > 1 && !a.is_indirect_artillery() {
                self.params.direct_fire_falloff
            } else {
                1.0
            };
            falloff_weighted += f * w;
            // §6.6 crest: melee members (distance ≤ 1)
            // each bring their own height gain, contribution-weighted like
            // the falloff — a range-2 member in the same group gets none.
            melee_weighted +=
                self.melee_elevation_factor(grid, a.position, d.position, *dist) * w;
            // §6.6 v3.3: each member's own terrain attack
            // adjuster against the target's ground, contribution-weighted
            // (a mountaineer in the pool pulls the average up, towed guns
            // drag it down), per-member floored at 0.
            adj_weighted += (1.0
                + self.params.terrain_modifier_scale * a.terrain_adj.attack_on(def_terrain))
            .max(0.0)
                * w;
        }
        if sum_qg <= 0.0 || sum_g <= 0.0 {
            return;
        }
        let p = sum_qg * sum_g;
        // Pool-level capture template: D, the hit step and the
        // linear stack are shared by every member lane; each lane's own q
        // rows and its credited share are filled per attacker below.
        let mut tpl = HitBreakdown::default();
        let defense = tactical_core::damage::defense_value_explained(
            d,
            false,
            def_terrain,
            &self.params,
            &mut tpl,
        );
        let falloff_avg = falloff_weighted / sum_qg;
        let melee_avg = melee_weighted / sum_qg;
        let cmd_aura = 1.0 + self.params.hq_combat_bonus * (cmd_weighted / sum_qg);
        let adj_avg = adj_weighted / sum_qg;
        let cover = 1.0 - def_terrain.cover_percent();
        let mut linear = falloff_avg;
        linear *= melee_avg;
        linear *= cmd_aura;
        if in_command(links[di]) {
            linear *= 1.0 - self.params.hq_combat_bonus;
        }
        linear *= adj_avg;
        linear *= cover;
        let mut org = tactical_core::damage::resolve_org(p, defense, linear, &self.params);

        // Linear rows: the pool's contribution-weighted factors. A member's
        // own terrain adjuster / elevation rides inside the weighted
        // average, so every lane shows these pool-level rows.
        if cmd_weighted > 0.0 {
            tpl.push_linear(LinearFactor::CommandAura, cmd_aura);
        }
        if in_command(links[di]) {
            tpl.push_linear(LinearFactor::TargetCommand, 1.0 - self.params.hq_combat_bonus);
        }
        tpl.push_linear(LinearFactor::TerrainAttack, adj_avg);
        if (falloff_avg - 1.0).abs() > 1e-6 {
            tpl.push_linear(LinearFactor::DirectFireFalloff, falloff_avg);
        }
        if (melee_avg - 1.0).abs() > 1e-6 {
            tpl.push_linear(LinearFactor::MeleeElevation, melee_avg);
        }
        tpl.push_linear(LinearFactor::Cover, cover);
        tpl.linear_total = linear;
        tpl.pool = if ais.len() > 1 {
            Some(PoolInfo {
                sum_qg,
                sum_g,
                members: ais.len() as u8,
            })
        } else {
            // A singleton "pool" is numerically identical to a lone strike
            // (q·g × g = q·g²) — display it as one, not as a 1-man volley.
            None
        };
        tpl.p = p;
        tpl.hit_base = self.params.hit_base;
        tpl.hit_saturated = self.params.hit_saturated;
        tpl.hit = tactical_core::damage::hit_fraction(p, defense, &self.params);
        tpl.damage_scale = self.params.damage_scale;
        tpl.org_raw = org;
        let spread = self.params.random_spread;
        if spread > 0.0 {
            let j = 1.0 - spread + self.rng.next_f32() * spread * 2.0;
            org *= j;
            tpl.jitter_mult = j;
        }
        tpl.org_cap = d.max_org * self.params.org_cap_ratio;
        tpl.org_capped = org > tpl.org_cap;
        org = org.min(tpl.org_cap);
        tpl.org_pool_final = org;

        // Credit each attacker in proportion to its own contribution.
        for (ai, _dist) in ais {
            let a = &units[*ai];
            let mut bd = tpl;
            let q = tactical_core::damage::attack_quality_explained(a, d, &mut bd);
            bd.strength_ratio = a.strength_ratio();
            let share = q * a.strength_ratio() / sum_qg;
            let o = org * share;
            bd.pool_share = share;
            bd.org_final = o;
            let str_ =
                tactical_core::damage::strength_damage_explained(o, d, &self.params, &mut bd);
            ledger.push(DamageEntry {
                src: *ai,
                dst: di,
                org: o,
                str_,
                counter: false,
                breakdown: Some(bd),
            });
        }
    }

    /// Unified fire phase (预令统一结算): resolve every attack
    /// order of the acting side plus all counter-fire simultaneously.
    ///
    /// Stages: (1) validate orders & compute outgoing damage — illegal or
    /// fizzled orders come back as `target_lost` results; (2) compute
    /// counter-fire at full attack value split across in-range attackers;
    /// (3) apply the whole ledger at once (mutual destruction is possible);
    /// (4) break / surrender / shock transitions, assault occupation.
    pub fn resolve_fire_phase(
        &mut self,
        grid: &HexGrid,
        units: &mut Vec<BattalionUnit>,
        orders: &[AttackOrder],
    ) -> Vec<CombatResult> {
        self.last_shocked.clear();
        self.last_hq_events.clear();
        // §6.13: command links are evaluated ONCE on the
        // pre-damage state — a HQ dying this phase still commands for this
        // phase (simultaneous resolution). `hq_alive` snapshots which HQs
        // were not yet Eliminated, for the stage-4 command-collapse sweep.
        let links = compute_command_links(units, &self.params);
        let hq_alive: Vec<bool> = units
            .iter()
            .map(|u| u.is_hq() && u.state != UnitState::Eliminated)
            .collect();
        let mut results: Vec<CombatResult> = Vec::new();
        let mut ledger: Vec<DamageEntry> = Vec::new();
        // Counter-fire pool: defender index → [(attacker index, distance)].
        let mut counter_pool: HashMap<usize, Vec<(usize, i32)>> = HashMap::new();
        // Assault lanes for the occupation step: (attacker idx, defender idx).
        let mut assaults: Vec<(usize, usize)> = Vec::new();
        // Order lanes for result aggregation: (order, lane key).
        let mut lanes: Vec<(usize, usize, HexCoord)> = Vec::new(); // (ai, di, hex)
                                                                   // Fire-lane groups for concentration (§6.3 v3.2): assault /
                                                                   // direct-fire orders pooling on one target merge into ONE strike
                                                                   // (P = Σ(q·g) × Σg). (di, hex, is_assault, [(ai, dist)])
        let mut groups: Vec<(usize, HexCoord, bool, Vec<(usize, i32)>)> = Vec::new();

        for order in orders {
            let Some(ai) = units.iter().position(|u| u.id == order.attacker) else {
                continue;
            };
            let lost = |results: &mut Vec<CombatResult>, def_id: usize, hex: HexCoord| {
                results.push(CombatResult {
                    attacker_id: order.attacker,
                    defender_id: def_id,
                    hex,
                    target_lost: true,
                    ..Default::default()
                });
            };
            {
                let a = &units[ai];
                // Revalidated at resolution: shocked units' orders were
                // cancelled at registration; non-effective units cannot act.
                if !a.is_combat_effective() || a.shocked || !a.can_attack_order() {
                    continue;
                }
                // Defense in depth: a forged assault order from a
                // unit that can never assault (towed / rocket) fizzles —
                // registration and the AI both gate on `can_assault`, this
                // keeps the resolver total on the same rule.
                if matches!(order.target, AttackTarget::Assault(_)) && !a.can_assault() {
                    continue;
                }
            }
            match order.target {
                AttackTarget::Assault(def_id) | AttackTarget::DirectFire(def_id) => {
                    let is_assault = matches!(order.target, AttackTarget::Assault(_));
                    let Some(di) = units.iter().position(|u| u.id == def_id) else {
                        continue;
                    };
                    let hex = units[di].position;
                    let dist = units[ai].position.distance(hex);
                    let reachable = if is_assault {
                        dist == 1
                    } else {
                        dist >= 1 && dist <= units[ai].attack_range
                    };
                    // §6.6 direct-fire line: a
                    // flat trajectory cannot shoot over a ridge — an
                    // intermediate hex standing strictly higher than BOTH
                    // endpoints makes the shot impossible ("无法攻击"),
                    // reported as target_lost. Indirect fire never gets
                    // here (fire missions resolve via FireMission); melee
                    // (dist 1) has no intermediate hex at all.
                    let ridge_blocked = !is_assault
                        && dist > 1
                        && !units[ai].is_indirect_artillery()
                        && !tactical_core::los::has_line_of_sight(grid, units[ai].position, hex);
                    if !reachable || ridge_blocked || !targetable(&units[di]) {
                        lost(&mut results, def_id, hex);
                        continue;
                    }
                    // §6.3 v3.2 concentration: pool into the target's
                    // group — one merged strike per target (stage 1b).
                    match groups
                        .iter_mut()
                        .find(|(d, _, k, _)| *d == di && *k == is_assault)
                    {
                        Some(g) => g.3.push((ai, dist)),
                        None => groups.push((di, hex, is_assault, vec![(ai, dist)])),
                    }
                    add_counter(&mut counter_pool, units, di, ai, dist, &self.params);
                    if is_assault {
                        assaults.push((ai, di));
                    }
                    lanes.push((ai, di, hex));
                }
                AttackTarget::FireMission { hex, precise } => {
                    let a = &units[ai];
                    let rocket = a.is_rocket();
                    let dist = a.position.distance(hex);
                    if dist == 0
                        || dist < a.min_attack_range()
                        || dist > a.attack_range
                        || !a.can_fire_support()
                    {
                        lost(&mut results, 0, hex);
                        continue;
                    }
                    // §6.3 gesture rule: rockets can never be
                    // precise — every launcher order resolves as area fire.
                    let precise = precise && !rocket;
                    let side = a.side;
                    // The whole 7-hex zone is the target set. ANY targetable
                    // enemy in the zone keeps the mission live ("圈内任敌即可");
                    // only a PRECISE strike
                    // requires one exactly on the aim hex (it singles out
                    // the clicked unit).
                    let zone: Vec<usize> = units
                        .iter()
                        .enumerate()
                        .filter(|(_, u)| {
                            u.side != side && u.position.distance(hex) <= 1 && targetable(u)
                        })
                        .map(|(j, _)| j)
                        .collect();
                    let center = zone.iter().copied().find(|&j| units[j].position == hex);
                    if precise {
                        let Some(di) = center else {
                            lost(&mut results, 0, hex);
                            continue;
                        };
                        let (org, str_, bd) = self.strike(grid, units, ai, di, dist, false, &links);
                        ledger.push(DamageEntry {
                            src: ai,
                            dst: di,
                            org,
                            str_,
                            counter: false,
                            breakdown: Some(bd),
                        });
                        lanes.push((ai, di, hex));
                        add_counter(&mut counter_pool, units, di, ai, dist, &self.params);
                        continue;
                    }
                    if rocket {
                        // §6.3: the salvo
                        // strikes EVERY unit in the 7-hex zone at full
                        // tube-artillery strength — no dilution, no doubling,
                        // no friend-or-foe discrimination. Each victim gets
                        // its own strike: its own terrain column and its own
                        // jitter roll (dispersion). The salvo lands REGARDLESS
                        // of who is in the zone (shells always land —
                        // "点击任意射程内地点"), so a
                        // friends-only aim still bleeds them.
                        // Every unit in the 7-hex zone, friends at the aim
                        // hex included — the salvo saturates the ground.
                        let affected: Vec<usize> = units
                            .iter()
                            .enumerate()
                            .filter(|(_, u)| u.position.distance(hex) <= 1 && targetable(u))
                            .map(|(j, _)| j)
                            .collect();
                        if affected.is_empty() {
                            lost(&mut results, 0, hex);
                            continue;
                        }
                        for j in affected {
                            let dj = units[ai].position.distance(units[j].position);
                            let (org, str_, bd) =
                                self.strike(grid, units, ai, j, dj, false, &links);
                            ledger.push(DamageEntry {
                                src: ai,
                                dst: j,
                                org,
                                str_,
                                counter: false,
                                breakdown: Some(bd),
                            });
                            // One lane per victim → per-hex battle reports.
                            lanes.push((ai, j, units[j].position));
                        }
                        // The salvo is spent — reload for
                        // `rocket_fire_cooldown_turns` (ticks down at the
                        // start of the owner's turns).
                        units[ai].fire_cooldown = self.params.rocket_fire_cooldown_turns;
                    } else {
                        // Area fire: every TARGETABLE unit in the zone —
                        // enemies AND friends (the F-barrage cannot tell
                        // friend from foe; the zone
                        // outline shows the risk before End Turn) — takes
                        // ITS OWN strike weighted by its hex: the aim hex
                        // 4/10, each of the 6 neighbours 1/10. The barrage
                        // lands REGARDLESS of who is in the zone ("点击任意
                        // 射程内地点" — a friends-only zone still bleeds);
                        // only a zone with NO targetable unit at all
                        // fizzles target-lost.
                        // Every targetable unit in the 7-hex zone — friends
                        // AT THE AIM HEX included (the weight rides the HEX,
                        // not the side; a friendly
                        // on the aim hex takes the 4/10 share too).
                        // The FIRER itself is never a victim: a battery
                        // does not shell its own hex (a point-blank zone
                        // used to splash the gun — the AT self-hit report).
                        let affected: Vec<usize> = units
                            .iter()
                            .enumerate()
                            .filter(|(j, u)| {
                                *j != ai && u.position.distance(hex) <= 1 && targetable(u)
                            })
                            .map(|(j, _)| j)
                            .collect();
                        if affected.is_empty() {
                            lost(&mut results, 0, hex);
                            continue;
                        }
                        for j in affected {
                            let weight = if units[j].position == hex {
                                self.params.area_center_share
                            } else {
                                self.params.area_neighbor_share
                            };
                            let dj = units[ai].position.distance(units[j].position);
                            let (org, str_, mut bd) =
                                self.strike(grid, units, ai, j, dj, false, &links);
                            bd.apply_area_weight(weight);
                            ledger.push(DamageEntry {
                                src: ai,
                                dst: j,
                                org: org * weight,
                                str_: str_ * weight,
                                counter: false,
                                breakdown: Some(bd),
                            });
                            lanes.push((ai, j, units[j].position));
                        }
                    }
                    // Counter-battery: the aim-hex enemy replies when one
                    // stands there; on an empty aim hex the zone's strongest
                    // ENEMY answers instead (a diluted splash alone does not
                    // draw fire; a friends-only zone draws no reply either).
                    let reply = center.or_else(|| {
                        zone.iter()
                            .copied()
                            .max_by(|a, b| units[*a].org.total_cmp(&units[*b].org))
                    });
                    if let Some(reply) = reply {
                        add_counter(&mut counter_pool, units, reply, ai, dist, &self.params);
                    }
                }
            }
        }

        // --- Stage 1b: merged group strikes (§6.3 v3.2 concentration —
        // the pool's firepower is Σ(q_i·g_i) × Σg_i: quality linear,
        // numbers squared; fire missions stay separate above). ---
        for (di, _hex, _is_assault, ais) in &groups {
            self.strike_group(grid, units, ais, *di, &mut ledger, &links);
        }

        // --- Stage 2: counter-fire (firepower split LINEARLY, P/n, across
        // every attacker inside the defender's range; evaluated on the
        // PRE-damage state, so a dying unit still shoots back — simultaneous
        // resolution). Retreating units never counter (§6.8). ---
        // The pool is a HashMap — iterate it in SORTED defender
        // order, or the optional jitter draws land in a different order per
        // process (Rust HashMap iteration is randomized) and a fixed-seed
        // battle diverges mid-run. (Jitter defaults to OFF since v3.2 — the
        // ordering guard stays so enabling it never reintroduces the drift.)
        let mut counter_defs: Vec<usize> = counter_pool.keys().copied().collect();
        counter_defs.sort_unstable();
        for di in counter_defs {
            let d = &units[di];
            if !d.is_combat_effective() || d.state == UnitState::Retreating {
                continue;
            }
            let attackers = &counter_pool[&di];
            let n = attackers.len() as f32;
            for (ai, dist) in attackers {
                // The defender's strike with its firepower split 1/n —
                // the target (original attacker) defends with breakthrough.
                let (org, str_, bd) = self.strike_split(grid, units, di, *ai, *dist, n, &links);
                ledger.push(DamageEntry {
                    src: di,
                    dst: *ai,
                    org,
                    str_,
                    counter: true,
                    breakdown: Some(bd),
                });
            }
        }

        // --- Stage 3: simultaneous application. ---
        // Shock test on the DELIVERED amount, AGGREGATED per
        // target: a concentrated group strike is judged on its pre-split
        // total, so a pool capping at 40% max org suppresses even when each
        // attacker's share stays below the 25% threshold — and the phase
        // total (assault + fire missions + counter-fire) is what the unit
        // actually weathered.
        let mut org_by_target: HashMap<usize, f32> = HashMap::new();
        for e in &ledger {
            *org_by_target.entry(e.dst).or_insert(0.0) += e.org;
        }
        let mut shocked: Vec<usize> = Vec::new();
        for e in &ledger {
            let d = &mut units[e.dst];
            let str_final = e.str_ * d.support_str_damage_mult();
            d.strength = (d.strength - str_final).max(0.0);
            d.org = (d.org - e.org).max(0.0);
        }
        for (dst, total) in org_by_target {
            let d = &units[dst];
            if total >= d.max_org * self.params.shock_threshold_ratio && total > 0.0 {
                shocked.push(dst);
            }
        }
        // Iterate the shock list deterministically (a
        // HashMap would shuffle `last_shocked` per process).
        shocked.sort_unstable();
        for idx in shocked {
            units[idx].shocked = true;
            self.last_shocked.push(units[idx].id);
        }

        // --- Stage 4: break / surrender transitions + assault occupation. ---
        for (ai, di) in &assaults {
            let d_org = units[*di].org;
            // An org-0 Withdrawn remnant is cleared by an assault like a
            // broken Active unit — it retreats off and the attacker occupies
            // the hex. Without this, the remnant (is_targetable now includes
            // it) was hit but never moved: org stays 0 so the break event
            // below never fired, and the corridor stayed blocked forever.
            let d_active = matches!(units[*di].state, UnitState::Active | UnitState::Withdrawn);
            if d_org <= 0.0 && d_active {
                if detect_encirclement(grid, &units[*di], units) == EncirclementLevel::Full {
                    units[*di].state = UnitState::Surrendered; // §6.4
                } else {
                    units[*di].state = UnitState::Retreating; // §6.8
                    let def_id = units[*di].id;
                    let vacated = units[*di].position; // capture BEFORE the step
                    if retreat_step_zoned(
                        grid,
                        units,
                        def_id,
                        true,
                        self.retreat_zones
                            .as_ref()
                            .map(|z| (z.0.as_slice(), z.1.as_slice())),
                    ) && units[*ai].is_combat_effective()
                    {
                        // Attacker occupies the vacated hex (§6.3).
                        units[*ai].position = vacated;
                    }
                }
            }
        }
        // Non-assault defenders that hit org 0: break or surrender too
        // (fire support never advances, §6.3).
        let damaged: Vec<usize> = {
            let mut v: Vec<usize> = ledger.iter().map(|e| e.dst).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        // Strength wiped out → annihilated. Equipment casualties
        // kill the battalion outright — no untargetable zombies holding
        // hexes on the map.
        for di in &damaged {
            if units[*di].strength <= 0.0 {
                units[*di].state = UnitState::Eliminated;
            }
        }
        for di in damaged {
            let d = &units[di];
            if d.org <= 0.0 && d.state == UnitState::Active {
                if detect_encirclement(grid, d, units) == EncirclementLevel::Full {
                    units[di].state = UnitState::Surrendered;
                } else {
                    units[di].state = UnitState::Retreating;
                }
            }
        }

        // --- §6.13: command collapse. An HQ ANNIHILATED this
        // phase (retreat/surrender do NOT count) costs every surviving
        // same-division battalion hq_death_org_frac of its max org; units
        // the collapse breaks follow the standard break/surrender rule. ---
        for hqi in 0..units.len() {
            if !hq_alive[hqi] || units[hqi].state != UnitState::Eliminated {
                continue;
            }
            let (hq_id, division) = (units[hqi].id, units[hqi].division.clone());
            let mut ev = HqLossEvent {
                hq_id,
                division: division.clone(),
                ..Default::default()
            };
            let mut broke: Vec<usize> = Vec::new();
            for (i, u) in units.iter_mut().enumerate() {
                if i == hqi || u.division != division || u.state != UnitState::Active {
                    continue;
                }
                let lost = (u.max_org * self.params.hq_death_org_frac).min(u.org);
                if lost <= 0.0 {
                    continue;
                }
                u.org -= lost;
                ev.losses.push((u.id, lost));
                if u.org <= 0.0 {
                    broke.push(i);
                }
            }
            for i in broke {
                if detect_encirclement(grid, &units[i], units) == EncirclementLevel::Full {
                    units[i].state = UnitState::Surrendered;
                } else {
                    units[i].state = UnitState::Retreating;
                }
                ev.broke.push(units[i].id);
            }
            self.last_hq_events.push(ev);
        }
        // Assault action consumed: attackers act (§6.2) and lose any
        // standing move order (a march into contact ends there). Attacking
        // also drops the take-cover stance.
        for (ai, _, _) in &lanes {
            units[*ai].acted = true;
            units[*ai].move_order = None;
            units[*ai].is_holding = false;
        }

        // --- Aggregate per-lane results (outgoing + counter-fire damage,
        // shock / break / surrender flags, assault advance flag). ---
        for (ai, di, hex) in &lanes {
            let mut r = CombatResult {
                attacker_id: units[*ai].id,
                defender_id: units[*di].id,
                hex: *hex,
                ..Default::default()
            };
            for e in &ledger {
                if e.src == *ai && e.dst == *di && !e.counter {
                    r.org_damage_dealt += e.org;
                    r.str_damage_dealt += e.str_;
                    if e.breakdown.is_some() {
                        r.breakdown = e.breakdown;
                    }
                }
                if e.src == *di && e.dst == *ai && e.counter {
                    r.org_damage_taken += e.org;
                    r.str_damage_taken += e.str_;
                    if e.breakdown.is_some() {
                        r.counter_breakdown = e.breakdown;
                    }
                }
            }
            r.shocked_defender = units[*di].shocked;
            r.shocked_attacker = units[*ai].shocked;
            r.surrendered = units[*di].state == UnitState::Surrendered;
            r.eliminated = units[*di].state == UnitState::Eliminated;
            r.defender_broken = units[*di].state == UnitState::Retreating
                && r.org_damage_dealt > 0.0
                && units[*di].org <= 0.0;
            r.advanced = assaults.contains(&(*ai, *di))
                && units[*ai].position == *hex
                && units[*ai].is_combat_effective();
            results.push(r);
        }
        results
    }

    /// Counter-fire strike: the defender's firepower split LINEARLY across
    /// `n` attackers (§6.3 v3.2 fire conservation — vanilla distributes its
    /// attacks linearly too; the retired ÷n² form made a surrounded
    /// defender's output evaporate, an 81:1 melee exchange). Each share
    /// evaluates its own hit step against the caught attacker's
    /// breakthrough (the HOI4 mid-attack convention).
    ///
    /// Like [`Self::strike`], a distance>1 counter-battery shot picks up
    /// `indirect_crest_mult` via the shared linear stack (×1.5 on an exposed
    /// crest target, ×0.5 in defilade) — intentional alignment: on maps with
    /// elevation contrast, counter-battery damage shifts with the terrain
    /// exactly as direct fire missions do.
    fn strike_split(
        &mut self,
        grid: &HexGrid,
        units: &[BattalionUnit],
        ai: usize,
        di: usize,
        distance: i32,
        n: f32,
        links: &[CommandLink],
    ) -> (f32, f32, HitBreakdown) {
        let a = &units[ai];
        let d = &units[di];

        let mut bd = HitBreakdown::default();
        // Every surviving counter is AIMED fire — the direct-lay
        // gate (`add_counter`) removed long-range counter-battery, and a
        // gun crew firing over open sights IS the aimed regime (square).
        let mut org = tactical_core::damage::strike_org_damage_explained(
            a,
            d,
            true,
            distance,
            in_command(links[ai]),
            in_command(links[di]),
            grid,
            &self.params,
            n,
            tactical_core::damage::FirepowerForm::Aimed,
            &mut bd,
        );
        let spread = self.params.random_spread;
        if spread > 0.0 {
            let j = 1.0 - spread + self.rng.next_f32() * spread * 2.0;
            org *= j;
            bd.jitter_mult = j;
        }
        bd.org_cap = d.max_org * self.params.org_cap_ratio;
        bd.org_capped = org > bd.org_cap;
        org = org.min(bd.org_cap);
        bd.org_pool_final = org;
        bd.org_final = org;
        let str_ = tactical_core::damage::strength_damage_explained(org, d, &self.params, &mut bd);
        (org, str_, bd)
    }

    /// End-of-turn encirclement attrition (§6.4): a
    /// partially encircled unit loses 2.5% of max org per turn, a fully
    /// encircled one 5% — paced to the 10-minute turn (§8.1): a static
    /// pocket collapses over strategic hours, not minutes. A unit ground
    /// down to org 0 surrenders when fully encircled, otherwise enters
    /// involuntary retreat (§6.8).
    pub fn apply_encirclement_attrition(&self, grid: &HexGrid, units: &mut Vec<BattalionUnit>) {
        let snapshot: &[BattalionUnit] = units;
        let levels: Vec<EncirclementLevel> = snapshot
            .iter()
            .map(|u| {
                if u.is_combat_effective() {
                    detect_encirclement(grid, u, snapshot)
                } else {
                    EncirclementLevel::None
                }
            })
            .collect();
        for (u, lvl) in units.iter_mut().zip(levels) {
            let frac = org_attrition_fraction(lvl, &self.params);
            if frac <= 0.0 {
                continue;
            }
            u.org = (u.org - frac * u.max_org).max(0.0);
            if u.org <= 0.0 {
                u.state = if lvl == EncirclementLevel::Full {
                    UnitState::Surrendered // §6.4: org 0 while fully encircled
                } else {
                    UnitState::Retreating // §6.8 involuntary retreat
                };
            }
        }
    }

    /// §6.13: in-command battalions regenerate
    /// `hq_org_regen_frac` of their max org each FULL turn. Call from the
    /// full-turn closeout (both sides acted), next to the maintenance
    /// attachment strength regen. Retreating/eliminated units never recover.
    /// Only OUT OF CONTACT — a unit with a
    /// combat-effective enemy adjacent is fighting, not regrouping
    /// (an unconditional front-line regen kept the
    /// attacker's org pinned at full while its units took counter-fire).
    pub fn apply_command_regen(&self, units: &mut Vec<BattalionUnit>) {
        let links = compute_command_links(units, &self.params);
        for i in 0..units.len() {
            if !units[i].is_combat_effective()
                || !in_command(links[i])
                || has_adjacent_enemy(units, &units[i])
            {
                continue;
            }
            let u = &mut units[i];
            u.org = (u.org + u.max_org * self.params.hq_org_regen_frac).min(u.max_org);
        }
    }
}

/// Whether any combat-effective enemy stands adjacent (distance 1) — the
/// §6.13 regen "out of contact" gate.
fn has_adjacent_enemy(units: &[BattalionUnit], u: &BattalionUnit) -> bool {
    units.iter().any(|e| {
        e.side != u.side && e.is_combat_effective() && e.position.distance(u.position) == 1
    })
}

/// Register a counter-fire lane when the defender can reply (§6.3):
/// artillery duels are legal, but a rifle battalion shelled from 9 hexes
/// cannot reply. No radar-less
/// counter-battery — indirect artillery replies only inside its
/// direct-lay self-defense circle (`counter_direct_lay_range`, guns over
/// open sights); rocket launchers never counter (unguided saturation has
/// no direct-lay mode). A LIMBERED
/// towed unit (requires emplacement, not emplaced) never replies either —
/// no damage in any form while limbered.
fn add_counter(
    pool: &mut HashMap<usize, Vec<(usize, i32)>>,
    units: &[BattalionUnit],
    di: usize,
    ai: usize,
    dist: i32,
    params: &CombatParams,
) {
    let d = &units[di];
    let reply_range = if d.is_rocket() {
        0 // never counter (no direct-lay mode)
    } else if d.requires_emplacement() && !d.is_emplaced {
        0 // limbered towed units deal no damage in any form
    } else if d.is_indirect_artillery() {
        params.counter_direct_lay_range
    } else {
        d.attack_range
    };
    if dist <= reply_range {
        pool.entry(di).or_default().push((ai, dist));
    }
}

/// One disordered retreat step (§6.8): move `unit_id` one hex toward its own
/// deployment edge — Attacker side falls back to its entry edges
/// (`grid.attack_dirs`, §4.2 step 9), Defender side to the far (eastern) map
/// edge. With `toward_own_edge == false` the unit instead steps directly away
/// from the nearest combat-effective enemy (ties break toward the home edge,
/// so the unit can't freeze at a local maximum).
///
/// A unit that starts its step on (or steps onto) its own edge hex leaves the
/// map (`UnitState::Withdrawn`, §6.8). Retreat steps respect stacking
/// (§6.9: one battalion per hex) and impassable terrain. §6.14:
/// out-of-bounds ring hexes are the PREFERRED rout exit — the unit holds
/// there and the dwell counter (see [`apply_oob_leaving`]) walks it off the
/// battle; the deployment edge remains the fallback when no ring is
/// reachable.
///
/// Returns `true` if the unit moved or withdrew, `false` when no safe step
/// exists (blocked by terrain or occupied hexes).
///
/// §6.8's "deployment-zone edge" is the REAL retreat
/// boundary — on stitched maps the map edge can lie outside the battle
/// province, so a rout dead-ended at the province rim (a routed garrison
/// pinned at q57 while the map edge sat at q62, the battle never
/// converging). [`retreat_step_zoned`] scores the DEFENDER against its own
/// zone's eastern rim instead.
pub fn retreat_step(
    grid: &HexGrid,
    units: &mut Vec<BattalionUnit>,
    unit_id: usize,
    toward_own_edge: bool,
) -> bool {
    retreat_step_zoned(grid, units, unit_id, toward_own_edge, None)
}

/// [`retreat_step`] with the battle's deployment zones: the DEFENDER's own
/// edge becomes the eastern rim of its deployment zone (the hexes whose
/// east neighbor lies outside the zone — i.e. the province boundary the
/// rout can actually reach, §6.8 "deployment-zone edge"). `zones.1` = the
/// defender zone; the attacker keeps its entry-edge logic.
pub fn retreat_step_zoned(
    grid: &HexGrid,
    units: &mut Vec<BattalionUnit>,
    unit_id: usize,
    toward_own_edge: bool,
    zones: Option<(&[HexCoord], &[HexCoord])>,
) -> bool {
    let Some(idx) = units.iter().position(|u| u.id == unit_id) else {
        return false;
    };
    if !targetable(&units[idx]) {
        return false;
    }
    let (pos, side) = (units[idx].position, units[idx].side);
    let view: &[BattalionUnit] = units;
    // The defender's own-edge target set: the eastern rim of its zone
    // (computed once per call — shared by every score evaluation).
    let defender_rim: Vec<HexCoord> = zones
        .map(|(_, d)| d)
        .map(|zone| {
            zone.iter()
                .copied()
                .filter(|z| {
                    let east = HexCoord::new(z.q + 1, z.r);
                    !zone.contains(&east)
                })
                .collect()
        })
        .unwrap_or_default();
    // The greedy 1-step descent froze on plateaus of
    // the cube-distance field (a diagonal rim: stepping east alone never
    // reduced the score) and in terrain dead-ends, so routed units sat
    // pinned forever in pockets the attackers never reached. Rout with a
    // REAL path: BFS to the side's own-edge target set and take its first
    // step. Falls back to the greedy when no path exists (genuinely
    // enclosed pocket — no escape) or when `toward_own_edge` is false.
    if toward_own_edge {
        if let Some(step) = bfs_edge_step(grid, view, idx, side, &defender_rim) {
            let withdraws = is_own_edge(step, side, grid, &defender_rim);
            units[idx].position = step;
            if withdraws {
                units[idx].state = UnitState::Withdrawn; // §6.8: left the map
            }
            return true;
        }
        // No route to the rim: keep the legacy greedy (it at least holds
        // position instead of wandering; §6.8 accepts a pinned rout).
    }
    // Lower score = closer to safety.
    let score = |h: HexCoord| -> i32 {
        if toward_own_edge {
            own_edge_distance(grid, side, h, &defender_rim)
        } else {
            // Away-from-enemy mode: enemy distance dominates (×1024 keeps it
            // decisive on a ≤512-wide grid), ties break toward the home edge
            // (P2). The old pure-distance score froze the unit at ANY local
            // maximum — and with no enemies left (constant MAX/2 distance)
            // every hex scored equal, so it never moved at all. The MAX/2
            // fallback × 1024 overflows i32 — saturate instead (debug builds
            // panicked here).
            -nearest_enemy_distance(h, side, unit_id, view).saturating_mul(1024)
                + own_edge_distance(grid, side, h, &defender_rim)
        }
    };

    if toward_own_edge && score(pos) == 0 {
        units[idx].state = UnitState::Withdrawn; // §6.8: reached the edge
        return true;
    }
    let mut best: Option<HexCoord> = None;
    let mut best_score = score(pos);
    for n in grid.passable_neighbors(pos) {
        let occupied = view
            .iter()
            .any(|u| u.id != unit_id && u.position == n && targetable(u));
        if occupied {
            continue; // §6.9: one battalion per hex
        }
        let s = score(n);
        if s < best_score {
            best_score = s;
            best = Some(n);
        }
    }
    let Some(h) = best else {
        return false; // no safe step: blocked by terrain or friendly stacking
    };
    let withdraws = toward_own_edge && score(h) == 0;
    units[idx].position = h;
    if withdraws {
        units[idx].state = UnitState::Withdrawn; // §6.8: left the map
    }
    true
}

/// A unit that can be attacked and still occupies its hex. Eliminated and
/// surrendered units are off the map for combat purposes. A Withdrawn
/// remnant IS targetable — it holds a corridor hex
/// and must be clearable (retreat_step_zoned also gates on this, so the
/// remnant can finally be pushed off its blocking hex).
fn targetable(u: &BattalionUnit) -> bool {
    u.is_targetable()
}

/// Distance in hexes from `h` to the side's own deployment edge (§6.8, §4.2).
/// Attacker entry edges come from `grid.attack_dirs` (defaults to the western
/// edge); the Defender falls back to the far (eastern) edge — or, when the
/// battle provides its zones, the eastern rim of its own deployment zone
/// (stitched maps make the map edge unreachable).
fn own_edge_distance(grid: &HexGrid, side: Side, h: HexCoord, defender_rim: &[HexCoord]) -> i32 {
    match side {
        Side::Defender if !defender_rim.is_empty() => defender_rim
            .iter()
            .map(|r| h.distance(*r))
            .min()
            .unwrap_or(grid.width as i32),
        Side::Defender => (grid.width as i32 - 1 - h.q).max(0),
        Side::Attacker => {
            grid.attack_dirs
                .iter()
                .map(|d| match d {
                    HexDirection::W => h.q,
                    HexDirection::E => grid.width as i32 - 1 - h.q,
                    HexDirection::NW | HexDirection::NE => h.r,
                    HexDirection::SW | HexDirection::SE => grid.height as i32 - 1 - h.r,
                })
                .min()
                // Empty `attack_dirs` (synthetic/script battles) falls back
                // to the documented default — the western entry edge. The
                // old `unwrap_or(0)` made EVERY hex the own edge and routed
                // units withdrew instantly from anywhere.
                .unwrap_or(h.q)
                .max(0)
        }
    }
}

fn nearest_enemy_distance(h: HexCoord, side: Side, unit_id: usize, units: &[BattalionUnit]) -> i32 {
    units
        .iter()
        .filter(|u| u.id != unit_id && u.side != side && u.is_combat_effective())
        .map(|u| h.distance(u.position))
        .min()
        .unwrap_or(i32::MAX / 2)
}

/// §6.8: is `h` on the unit's own deployment edge — the defender's eastern
/// zone rim when the battle provides it, otherwise the far map edge; the
/// attacker's entry-edge columns per `grid.attack_dirs`.
fn is_own_edge(h: HexCoord, side: Side, grid: &HexGrid, defender_rim: &[HexCoord]) -> bool {
    match side {
        Side::Defender if !defender_rim.is_empty() => defender_rim.contains(&h),
        _ => own_edge_distance(grid, side, h, defender_rim) == 0,
    }
}

/// The first hex of the shortest passable, unoccupied route from the unit
/// to its way off the battle — the replacement for the greedy 1-step
/// descent, which froze on score-field plateaus and in
/// terrain dead-ends (a routed garrison pinned in a pocket the attacker
/// never reached stalled the battle forever). `None` = no route (a truly
/// enclosed pocket — the rout stays put; §6.8 accepts a pinned rout).
///
/// §6.14 ("劣势避战主动撤出边界"): the preferred exit
/// is the OUT-OF-BOUNDS ring itself — a broken unit slips off the map there
/// (holds the hex while the dwell counter walks it to LeftBattle) instead
/// of parking as a corridor-clogging Withdrawn remnant. The deployment edge
/// (§6.8 Withdrawn) stays as the fallback for water-locked maps where no
/// out-of-bounds land is reachable.
fn bfs_edge_step(
    grid: &HexGrid,
    units: &[BattalionUnit],
    idx: usize,
    side: Side,
    defender_rim: &[HexCoord],
) -> Option<HexCoord> {
    use std::collections::{HashSet, VecDeque};
    let start = units[idx].position;
    let oob = |h: HexCoord| {
        grid.cell(h)
            .map(|c| c.out_of_bounds && c.is_passable)
            .unwrap_or(false)
    };
    // Already out of bounds → hold the hex and let the dwell counter finish
    // the exit (apply_oob_leaving, §6.14) — no further retreat steps.
    if oob(start) {
        return Some(start);
    }
    // Already on the edge → leave immediately (caller applies Withdrawn).
    if is_own_edge(start, side, grid, defender_rim) {
        return Some(start);
    }
    let blocked = |h: HexCoord| -> bool {
        units
            .iter()
            .any(|u| u.id != units[idx].id && u.position == h && targetable(u))
    };
    let bfs = |goal: &dyn Fn(HexCoord) -> bool| -> Option<HexCoord> {
        let mut seen: HashSet<HexCoord> = HashSet::new();
        let mut queue: VecDeque<(HexCoord, Option<HexCoord>)> = VecDeque::new();
        seen.insert(start);
        for n in grid.passable_neighbors(start) {
            if !blocked(n) && seen.insert(n) {
                queue.push_back((n, Some(n)));
            }
        }
        while let Some((h, first)) = queue.pop_front() {
            if goal(h) {
                return first;
            }
            for n in grid.passable_neighbors(h) {
                if !blocked(n) && seen.insert(n) {
                    queue.push_back((n, first));
                }
            }
        }
        None
    };
    // §6.14: nearest out-of-bounds exit first, the §6.8 deployment edge only
    // when the ring cannot be reached at all.
    bfs(&oob).or_else(|| bfs(&|h| is_own_edge(h, side, grid, defender_rim)))
}

// ── §6.14 out-of-bounds leaving ─────────────────────────────────────────────

/// One battalion that left the battle at this full-turn end by lingering
/// out of bounds: id/side/name for the battle log; `org_frac` = the share
/// of its max org wiped by the exit (fed to the sync damage channel — the
/// frozen strength is NEVER recorded as damage).
#[derive(Debug, Clone)]
pub struct OobDeparture {
    pub unit_id: usize,
    pub side: Side,
    pub name: String,
    pub org_frac: f32,
}

/// §6.14 out-of-bounds leaving — run once at every
/// FULL-turn end (game.rs `finish_full_turn` / headless `full_turn_upkeep`,
/// after the retreat steps so a rout that just stumbled into the ring
/// starts counting at once). Rules:
///
/// - A unit ending the turn on an `out_of_bounds` hex (the
///   shoreline margin ring, enclaves, foreign islands) accrues one dwell
///   turn (`oob_turns += 1`); ending the turn back in bounds resets it.
///   Only consecutive dwell counts — passing through the ring is free.
/// - At `CombatParams::oob_leaving_turns` the unit LEAVES THE BATTLE: org
///   0, strength frozen (it slipped away — NOT annihilated),
///   `UnitState::LeftBattle`, removed from the board (`OFFBOARD`),
///   uncommandable, ignored by the AI, resolved for victory.
/// - Only still-on-board states dwell (Active / Retreating / Withdrawn —
///   Eliminated and Surrendered are already resolved).
///
/// Returns this turn's departures for logging / sync damage recording.
pub fn apply_oob_leaving(
    grid: &HexGrid,
    units: &mut Vec<BattalionUnit>,
    params: &CombatParams,
) -> Vec<OobDeparture> {
    let mut out = Vec::new();
    for u in units.iter_mut() {
        if !matches!(
            u.state,
            UnitState::Active | UnitState::Retreating | UnitState::Withdrawn
        ) {
            continue;
        }
        let on_oob = grid
            .cell(u.position)
            .map(|c| c.out_of_bounds)
            .unwrap_or(false);
        if on_oob {
            u.oob_turns = u.oob_turns.saturating_add(1);
        } else {
            u.oob_turns = 0;
        }
        if u.oob_turns >= params.oob_leaving_turns {
            out.push(OobDeparture {
                unit_id: u.id,
                side: u.side,
                name: u.name.clone(),
                org_frac: u.org_ratio(),
            });
            u.org = 0.0; // org wiped, strength FROZEN (slipped away, §6.14)
            u.state = UnitState::LeftBattle;
            u.position = BattalionUnit::OFFBOARD;
            u.move_order = None;
            u.is_holding = false;
            u.oob_turns = 0;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests (§6.3 v3.2 hit-step model + unified fire phase)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_core::UnitType;

    fn grid(w: usize, h: usize) -> HexGrid {
        HexGrid::new(w, h, Terrain::Plains)
    }

    /// No jitter: deterministic damage magnitudes.
    fn engine(seed: u64) -> CombatEngine {
        CombatEngine::new(
            CombatParams {
                random_spread: 0.0,
                ..Default::default()
            },
            seed,
        )
    }

    /// Infantry battalion, HOI4-calibrated: soft 6 / def 22 / org 60.
    fn inf(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        let mut u = BattalionUnit::new(id, format!("Inf{id}"), UnitType::Infantry, side, pos);
        u.soft_attack = 6.0;
        u.hard_attack = 1.0;
        u.defense = 22.0;
        u.breakthrough = 3.0;
        u.piercing = 4.0;
        u
    }

    /// Medium tank: soft 19 / hard 14 / armor 60 / pier 61 / org 10.
    fn tank(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        let mut u = BattalionUnit::new(id, format!("Tk{id}"), UnitType::MediumArmor, side, pos);
        u.soft_attack = 19.0;
        u.hard_attack = 14.0;
        u.defense = 5.0;
        u.breakthrough = 36.0;
        u.armor = 60.0;
        u.piercing = 61.0;
        u.hardness = 0.9;
        u
    }

    /// Towed AT gun: hard 20 / pier 60 / org 30 (emplaced when firing).
    fn at_gun(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        let mut u = BattalionUnit::new(id, format!("AT{id}"), UnitType::AntiTankBrigade, side, pos);
        u.soft_attack = 4.0;
        u.hard_attack = 20.0;
        u.defense = 4.0;
        u.piercing = 60.0;
        u.is_emplaced = true;
        u
    }

    /// Towed artillery: soft 25 / range 9 (emplaced when firing).
    fn artillery(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        let mut u =
            BattalionUnit::new(id, format!("Ar{id}"), UnitType::ArtilleryBrigade, side, pos);
        u.soft_attack = 25.0;
        u.hard_attack = 2.0;
        u.defense = 10.0;
        u.is_emplaced = true;
        u
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// Regression: `strike` and `strike_split` must deliver exactly
    /// the shared `tactical-core::damage` numbers when the jitter is
    /// disabled (spread 0 — the default since v3.2) — the est=strike
    /// invariant the AI planner relies on, enforced where the formula lives.
    #[test]
    fn strike_matches_shared_formula_without_jitter() {
        let g = grid(8, 8);
        let units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        // Real link computation (the test units have no division, so the
        // links are NoHq — the aura column stays neutral but is exercised).
        let links = compute_command_links(&units, &e.params);
        let expect = tactical_core::damage::strike_org_damage(
            &units[0],
            &units[1],
            false,
            1,
            in_command(links[0]),
            in_command(links[1]),
            &g,
            &e.params,
            tactical_core::damage::FirepowerForm::Area, // strike() serves fire missions
        );
        let (org, _, _) = e.strike(&g, &units, 0, 1, 1, false, &links);
        assert!(close(org, expect), "strike {org} != shared {expect}");

        // Counter-fire split (§6.3 v3.2 fire conservation): firepower ÷ n
        // BEFORE the hit step, evaluated against the caught attacker's
        // breakthrough. inf q=6 → P=6/2=3 vs brk 3 → hit 0.25 → 0.75.
        let q = tactical_core::damage::attack_quality(&units[1], &units[0]);
        let p = tactical_core::damage::firepower(q, units[1].strength_ratio()) / 2.0;
        let def = tactical_core::damage::defense_value(&units[0], true, Terrain::Plains, &e.params);
        let lin = tactical_core::damage::linear_factors(
            &units[1],
            &units[0],
            1,
            in_command(links[1]),
            in_command(links[0]),
            &g,
            &e.params,
        );
        let expect_split = tactical_core::damage::resolve_org(p, def, lin, &e.params);
        assert!(close(expect_split, 0.75), "hand-check {expect_split}");
        let (org_s, _, _) = e.strike_split(&g, &units, 1, 0, 1, 2.0, &links);
        assert!(
            close(org_s, expect_split),
            "strike_split {org_s} != shared split {expect_split}"
        );
    }

    #[test]
    fn infantry_vs_infantry_baseline() {
        // §6.3 v3.2: P = 6, D = 22 → hit = 0.1+0.3×6/28 ≈ 0.164 → 0.986 org;
        // counter at brk 3 → hit 0.30 → 1.8. str rides at 0.12×25/60 = 0.05.
        let g = grid(8, 8);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        let r = &res[0];
        assert!(
            close(r.org_damage_dealt, 6.0 * (0.1 + 0.3 * 6.0 / 28.0)),
            "{}",
            r.org_damage_dealt
        );
        assert!(close(r.str_damage_dealt, 6.0 * (0.1 + 0.3 * 6.0 / 28.0) * 0.05));
        assert!(
            close(r.org_damage_taken, 1.8),
            "{}",
            r.org_damage_taken
        );
        assert!(!r.target_lost && !r.shocked_defender);
    }

    #[test]
    fn retreating_defender_remains_targetable() {
        // §6.8 pursuit: a broken unit keeps taking fire and never counters.
        let g = grid(8, 8);
        let mut d = inf(1, Side::Defender, HexCoord::new(2, 1));
        d.org = 0.0;
        d.state = UnitState::Retreating;
        let mut units = vec![inf(0, Side::Attacker, HexCoord::new(1, 1)), d];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        let r = &res[0];
        assert!(!r.target_lost, "retreating unit stays a valid target");
        assert!(r.org_damage_dealt > 0.0, "broken unit keeps taking damage");
        assert_eq!(r.org_damage_taken, 0.0, "retreating units never counter");
        assert_eq!(units[1].state, UnitState::Retreating);
    }

    #[test]
    fn assault_clears_withdrawn_remnant_from_corridor() {
        // An org-0 Withdrawn remnant holds a corridor hex and is targetable
        // — the assault pushes it one retreat step off and the attacker
        // occupies the vacated hex. Before: org stayed 0 so the break event
        // never fired, the remnant sat on its hex forever and the advance
        // froze (attackers parked behind a wall of such remnants).
        let g = grid(8, 8);
        let zone: Vec<HexCoord> = (0..8)
            .flat_map(|q| (0..8).map(move |r| HexCoord::new(q, r)))
            .collect();
        let zones = (vec![HexCoord::ZERO], zone);
        let mut d = inf(1, Side::Defender, HexCoord::new(3, 1));
        d.org = 0.0;
        d.state = UnitState::Withdrawn;
        let mut units = vec![inf(0, Side::Attacker, HexCoord::new(2, 1)), d];
        let mut e = engine(1);
        e.set_retreat_zones(Some(zones));
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        assert!(!res[0].target_lost, "withdrawn remnant is a valid target");
        assert!(res[0].advanced, "attacker occupies the cleared hex");
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        assert_eq!(
            units[1].position.q, 4,
            "remnant pushed east off the corridor hex (was at (3,1))"
        );
        assert_ne!(units[1].position, HexCoord::new(3, 1));
        assert_eq!(
            units[1].state,
            UnitState::Retreating,
            "cleared remnant enters the retreat flow (§6.8)"
        );
    }

    #[test]
    fn defender_rout_exits_at_its_zone_rim_not_the_map_edge() {
        // On a stitched map the defender zone ends
        // at q=5 while the map runs to q=11 — the rout must Withdraw at the
        // zone's eastern rim, not dead-end trying to reach the map edge.
        let g = grid(12, 8);
        let zone: Vec<HexCoord> = (0..5)
            .flat_map(|q| (0..8).map(move |r| HexCoord::new(q, r)))
            .collect();
        let zones = (vec![HexCoord::ZERO], zone.clone());
        let mut d = inf(1, Side::Defender, HexCoord::new(4, 4));
        d.org = 0.0;
        d.state = UnitState::Retreating;
        let mut units = vec![d];
        // With the zones attached the unit is AT the rim → immediately off.
        assert!(retreat_step_zoned(
            &g,
            &mut units,
            1,
            true,
            Some((&zones.0, &zones.1))
        ));
        assert_eq!(units[0].state, UnitState::Withdrawn);
        // Without the zones the old map-edge logic applies: q4 of a 12-wide
        // map is NOT the far edge, so the unit just marches (not off-map).
        let mut d2 = inf(2, Side::Defender, HexCoord::new(4, 4));
        d2.org = 0.0;
        d2.state = UnitState::Retreating;
        let mut units2 = vec![d2];
        assert!(retreat_step(&g, &mut units2, 2, true));
        assert_eq!(units2[0].state, UnitState::Retreating);
        assert_eq!(units2[0].position.q, 5, "old logic steps east");
        // A mid-zone rout walks to the rim in steps, then withdraws.
        let mut d3 = inf(3, Side::Defender, HexCoord::new(1, 4));
        d3.org = 0.0;
        d3.state = UnitState::Retreating;
        let mut units3 = vec![d3];
        for _ in 0..4 {
            retreat_step_zoned(&g, &mut units3, 3, true, Some((&zones.0, &zones.1)));
        }
        assert_eq!(
            units3[0].state,
            UnitState::Withdrawn,
            "reached the zone rim"
        );
    }

    #[test]
    fn routed_unit_escapes_a_plateau_and_a_dead_end() {
        // The greedy 1-step descent froze a rout on
        // score plateaus (diagonal rim) and in terrain pockets — the BFS
        // replacement must escape both. Zone = q0..7 with a 2-wide east
        // "peninsula" sticking past the rim at r=4..5 (the rim sits at
        // q=7; hexes at q8+ are passable map, outside the zone).
        let mut g = grid(12, 8);
        // Make q8-9 into a dead-end pocket (impassable beyond q9 at r4).
        for q in 10..12 {
            g.set_terrain(HexCoord::new(q, 4), Terrain::Water);
            g.set_terrain(HexCoord::new(q, 5), Terrain::Water);
        }
        let zone: Vec<HexCoord> = (0..8)
            .flat_map(|q| (0..8).map(move |r| HexCoord::new(q, r)))
            .collect();
        let zones = (vec![HexCoord::ZERO], zone.clone());
        // Case 1: start ON the rim → withdraw immediately.
        let mut d = inf(1, Side::Defender, HexCoord::new(7, 6));
        d.org = 0.0;
        d.state = UnitState::Retreating;
        let mut units = vec![d];
        assert!(retreat_step_zoned(
            &g,
            &mut units,
            1,
            true,
            Some((&zones.0, &zones.1))
        ));
        assert_eq!(units[0].state, UnitState::Withdrawn);
        // Case 2: stuck inside the q8-9 dead-end pocket whose only exit is
        // back WEST (worse for the east-rim score) — BFS must walk it out.
        let mut d2 = inf(2, Side::Defender, HexCoord::new(9, 4));
        d2.org = 0.0;
        d2.state = UnitState::Retreating;
        let mut units2 = vec![d2];
        for _ in 0..5 {
            retreat_step_zoned(&g, &mut units2, 2, true, Some((&zones.0, &zones.1)));
        }
        assert_eq!(
            units2[0].state,
            UnitState::Withdrawn,
            "pocket rout must find its way out: {:?}",
            units2[0]
        );
    }

    #[test]
    fn retreat_step_without_enemies_does_not_overflow() {
        // No combat-effective enemy on the field: nearest_enemy_distance
        // falls back to i32::MAX/2 and the ×1024 score weight must SATURATE
        // (debug builds panicked on the overflow). The
        // enemy-distance term is then constant across hexes, so the
        // tie-break walks the unit toward its own edge (attacker with empty
        // attack_dirs: the western edge, q decreasing).
        let g = grid(8, 8);
        let mut units = vec![inf(0, Side::Attacker, HexCoord::new(3, 3))];
        assert!(retreat_step(&g, &mut units, 0, false));
        assert!(
            units[0].position.q < 3,
            "tie-break steps toward the own (western) edge: {:?}",
            units[0].position
        );
    }

    // ── §6.14 out-of-bounds leaving ────────────────────────────────────────

    #[test]
    fn oob_dwell_counts_then_resets_in_bounds() {
        let mut g = grid(6, 6);
        let oob_hex = HexCoord::new(0, 0);
        g.cell_mut(oob_hex).unwrap().out_of_bounds = true;
        let p = CombatParams::default();
        let mut units = vec![inf(1, Side::Attacker, oob_hex)];
        apply_oob_leaving(&g, &mut units, &p);
        apply_oob_leaving(&g, &mut units, &p);
        assert_eq!(units[0].oob_turns, 2, "dwell accrues per full turn out");
        units[0].position = HexCoord::new(3, 3); // back in bounds
        apply_oob_leaving(&g, &mut units, &p);
        assert_eq!(units[0].oob_turns, 0, "in-bounds turn end resets dwell");
        assert_eq!(units[0].state, UnitState::Active);
    }

    #[test]
    fn oob_leaving_at_threshold_freezes_strength_offboard() {
        let mut g = grid(6, 6);
        let oob_hex = HexCoord::new(0, 0);
        g.cell_mut(oob_hex).unwrap().out_of_bounds = true;
        let p = CombatParams::default(); // oob_leaving_turns = 6
        let mut u = inf(1, Side::Defender, oob_hex);
        u.org = 30.0;
        u.strength = 20.0;
        let mut units = vec![u];
        for _ in 0..p.oob_leaving_turns - 1 {
            assert!(apply_oob_leaving(&g, &mut units, &p).is_empty());
        }
        assert_eq!(
            units[0].state,
            UnitState::Active,
            "still in the battle at 5"
        );
        let departures = apply_oob_leaving(&g, &mut units, &p);
        assert_eq!(departures.len(), 1);
        assert_eq!(departures[0].unit_id, 1);
        assert!(
            (departures[0].org_frac - 0.5).abs() < 1e-4,
            "30/60 org wiped"
        );
        let u = &units[0];
        assert_eq!(u.state, UnitState::LeftBattle);
        assert_eq!(u.org, 0.0);
        assert_eq!(
            u.strength, 20.0,
            "strength frozen — slipped away, not annihilated"
        );
        assert_eq!(u.position, BattalionUnit::OFFBOARD);
        assert!(u.move_order.is_none());
        assert!(!u.is_targetable(), "gone for good — the AI ignores it");
    }

    #[test]
    fn oob_leaving_covers_withdrawn_remnants() {
        // A Withdrawn remnant parked on an out-of-bounds hex dwells out too
        // (edge-parked remnants slowly dissolve off the board).
        let mut g = grid(6, 6);
        let oob_hex = HexCoord::new(5, 5);
        g.cell_mut(oob_hex).unwrap().out_of_bounds = true;
        let p = CombatParams::default();
        let mut u = inf(1, Side::Attacker, oob_hex);
        u.state = UnitState::Withdrawn;
        u.org = 0.0;
        let mut units = vec![u];
        for _ in 0..p.oob_leaving_turns {
            apply_oob_leaving(&g, &mut units, &p);
        }
        assert_eq!(units[0].state, UnitState::LeftBattle);
    }

    #[test]
    fn retreat_prefers_oob_exit_and_holds_there() {
        // §6.14 ("劣势避战主动撤出边界"): a rout beside the ring steps INTO
        // it — not toward its farther deployment edge — then holds the hex
        // while the dwell counter finishes the exit.
        let mut g = grid(8, 8);
        for h in [
            HexCoord::new(0, 0),
            HexCoord::new(0, 1),
            HexCoord::new(1, 0),
        ] {
            g.cell_mut(h).unwrap().out_of_bounds = true;
        }
        let mut d = inf(1, Side::Defender, HexCoord::new(1, 1));
        d.org = 0.0;
        d.state = UnitState::Retreating;
        let mut units = vec![d];
        // The §6.8 zone rim sits at the far east — farther than the ring.
        let zones = (
            vec![HexCoord::new(7, 7)],
            vec![HexCoord::new(7, 0), HexCoord::new(7, 1)],
        );
        assert!(retreat_step_zoned(
            &g,
            &mut units,
            1,
            true,
            Some((&zones.0, &zones.1))
        ));
        let pos = units[0].position;
        assert!(
            g.cell(pos).unwrap().out_of_bounds,
            "rout fled into the ring: {pos:?}"
        );
        // Holding: the next retreat step keeps it in place, still Retreating
        // (the dwell counter — not the retreat — walks it off the battle).
        assert!(retreat_step_zoned(
            &g,
            &mut units,
            1,
            true,
            Some((&zones.0, &zones.1))
        ));
        assert_eq!(units[0].position, pos);
        assert_eq!(units[0].state, UnitState::Retreating);
    }

    #[test]
    fn tank_precision_factor_scales_quality_linearly() {
        // §6.3 v3.2: precision is inside q (linear), never squared.
        // Tank q = 19×0.5 = 9.5 → P = 9.5, D = 22 → hit ≈ 0.190 → 1.810.
        let g = grid(8, 8);
        let mut units = vec![
            tank(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert!(
            close(res[0].org_damage_dealt, 9.5 * (0.1 + 0.3 * 9.5 / 31.5)),
            "{}",
            res[0].org_damage_dealt
        );
        // Infantry counters vs hardness 0.9: q = 6×0.1 + 1×0.9×0.5 (piercing
        // 4 vs armor 60 → ×0.5 tier) = 1.05; P = 1.05 vs brk 36 → ≈0.114.
        assert!(
            close(res[0].org_damage_taken, 1.05 * (0.1 + 0.3 * 1.05 / 37.05)),
            "{}",
            res[0].org_damage_taken
        );
    }

    #[test]
    fn at_versus_tank_hard_cap_and_shock() {
        // AT vs hardness 0.9: q = (4×0.1 + 20×0.9×1.0)×0.8 = 14.72 → P,
        // D = 5 → hit ≈ 0.324 → raw 4.77, hard-capped to 10×0.4 = 4.0;
        // 4.0 ≥ 10×0.25 → shocked. str rides at 0.12×2/10 = 0.024/org.
        // (Direct fire at range 1: a towed gun cannot assault.)
        let g = grid(8, 8);
        let mut units = vec![
            at_gun(0, Side::Attacker, HexCoord::new(1, 1)),
            tank(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::DirectFire(1),
            }],
        );
        assert!(
            close(res[0].org_damage_dealt, 4.0),
            "{}",
            res[0].org_damage_dealt
        );
        assert!(close(res[0].str_damage_dealt, 0.096));
        assert!(res[0].shocked_defender);
        assert!(units[1].shocked);
    }

    #[test]
    fn shock_persists_until_next_turn_end() {
        // A shock inflicted by this turn-end's fire phase
        // survives the same turn-end's expiry and wears off only at the
        // NEXT turn-end. (Previously counter-fire shock was wiped at the
        // inflicting turn's own end, never suppressing anything.)
        // (Direct fire at range 1: a towed gun cannot assault.)
        let g = grid(8, 8);
        let mut units = vec![
            at_gun(0, Side::Attacker, HexCoord::new(1, 1)),
            tank(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::DirectFire(1),
            }],
        );
        assert!(units[1].shocked, "shock applied");
        e.expire_shocks(&mut units); // same turn end — fresh shock persists
        assert!(units[1].shocked, "fresh shock survives its own turn end");
        e.expire_shocks(&mut units); // next turn end — wears off
        assert!(!units[1].shocked, "stale shock expires one turn-end later");
    }

    #[test]
    fn shock_rearm_on_repeat_hit() {
        // A unit shocked again while still shocked must stay shocked past
        // the next expiry — the fresh stamp re-arms the timer. (Tank str
        // beefed up so it survives two capped hits: 10 org, threshold 2.5,
        // each hit delivers the 4.0 cap.)
        let g = grid(8, 8);
        let mut t = tank(1, Side::Defender, HexCoord::new(2, 1));
        t.max_strength = 50.0;
        t.strength = 50.0;
        let mut at = at_gun(0, Side::Attacker, HexCoord::new(1, 1));
        // Keep the ATTACKER alive and un-shockable (the tank's ~12-org /
        // ~5-str counter would otherwise kill its 4 str or shock it,
        // fizzling the second assault).
        at.max_org = 1000.0;
        at.org = 1000.0;
        at.max_strength = 100.0;
        at.strength = 100.0;
        let mut units = vec![at, t];
        let mut e = engine(1);
        // (Direct fire at range 1: a towed gun cannot assault.)
        let order = AttackOrder {
            attacker: 0,
            target: AttackTarget::DirectFire(1),
        };
        e.resolve_fire_phase(&g, &mut units, std::slice::from_ref(&order));
        e.expire_shocks(&mut units);
        assert!(units[1].shocked, "first hit shocks");
        e.resolve_fire_phase(&g, &mut units, std::slice::from_ref(&order));
        e.expire_shocks(&mut units);
        assert!(units[1].shocked, "re-shocked unit stays shocked");
        e.expire_shocks(&mut units);
        assert!(!units[1].shocked, "and expires one turn-end later");
    }

    #[test]
    fn counter_fire_split_across_attackers() {
        // Two in-range attackers: the defender's firepower splits LINEARLY
        // (P/n, §6.3 v3.2 fire conservation), each share stepping against
        // its own target's breakthrough.
        let g = grid(8, 8);
        let mut units = vec![
            tank(0, Side::Attacker, HexCoord::new(1, 1)),
            at_gun(1, Side::Attacker, HexCoord::new(2, 2)),
            inf(2, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        // (The AT direct-fires: a towed gun cannot assault. The
        // counter split is per-lane either way.)
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[
                AttackOrder {
                    attacker: 0,
                    target: AttackTarget::Assault(2),
                },
                AttackOrder {
                    attacker: 1,
                    target: AttackTarget::DirectFire(2),
                },
            ],
        );
        assert_eq!(res.len(), 2);
        // Reply vs tank: q = 6×0.1+1×0.9×0.5 = 1.05, P/2 = 0.525 vs brk 36
        // → hit 0.104 → 0.0548.
        assert!(
            close(res[0].org_damage_taken, 0.525 * (0.1 + 0.3 * 0.525 / 36.525)),
            "{}",
            res[0].org_damage_taken
        );
        // Reply vs AT: AT fixture hardness 0 → q = 6 (pure soft), P/2 = 3
        // vs brk 0 → hit 0.4 (the hit step needs no breakthrough floor) → 1.2.
        assert!(
            close(res[1].org_damage_taken, 3.0 * 0.4),
            "{}",
            res[1].org_damage_taken
        );
    }

    #[test]
    fn no_counter_beyond_defender_range() {
        // Artillery at 8 hexes shells infantry (range 1): no counter-fire.
        let g = grid(14, 6);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(9, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: true,
                },
            }],
        );
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].org_damage_taken, 0.0);
        // Spotted, full on target: q = 25×0.3 = 7.5, P = 7.5 vs D 22 →
        // hit ≈ 0.176 → 1.322.
        assert!(
            close(res[0].org_damage_dealt, 7.5 * (0.1 + 0.3 * 7.5 / 29.5)),
            "{}",
            res[0].org_damage_dealt
        );
    }

    #[test]
    fn counter_battery_artillery_duel() {
        // Direct-lay gate: an artillery duel survives
        // ONLY inside the direct-lay circle — at 2 hexes the defender's
        // guns reply over open sights (aimed, square law); one hex farther
        // out the radar-less counter-battery is gone (see
        // artillery_has_no_radar_less_counter_battery).
        let g = grid(6, 4);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            artillery(1, Side::Defender, HexCoord::new(3, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(3, 1),
                    precise: true,
                },
            }],
        );
        // Defender counters inside the circle: P = 7.5 vs the attacker's
        // breakthrough 0 → hit saturates at 0.40 (no division blow-up),
        // indirect fire → no falloff: 7.5×0.4 = 3.0.
        assert!(res[0].org_damage_taken > 0.0, "direct-lay counter expected");
        assert!(
            close(res[0].org_damage_taken, 3.0),
            "{}",
            res[0].org_damage_taken
        );
    }

    #[test]
    fn direct_fire_falloff_beyond_adjacency() {
        // Tank direct fire at range 2: ×0.6 falloff; target defends normally.
        let g = grid(8, 8);
        let mut units = vec![
            tank(0, Side::Attacker, HexCoord::new(1, 1)),
            at_gun(1, Side::Defender, HexCoord::new(3, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::DirectFire(1),
            }],
        );
        // AT fixture hardness 0 → q = 19×0.5 = 9.5 (pure soft); P = 9.5
        // vs D 4 → hit = 0.1+0.3×9.5/13.5 ≈ 0.311; ×0.6 falloff → ≈1.773.
        assert!(
            close(res[0].org_damage_dealt, 9.5 * (0.1 + 0.3 * 9.5 / 13.5) * 0.6),
            "{}",
            res[0].org_damage_dealt
        );
    }

    #[test]
    fn mutual_destruction_is_possible() {
        // Simultaneous application: two nearly-broken tanks kill each other.
        // (v3.2 numbers: tank-vs-tank deals ≈2.01, the counter ≈1.09 — org
        // 1.0 puts both inside both envelopes.)
        let g = grid(8, 8);
        let mut a = tank(0, Side::Attacker, HexCoord::new(1, 1));
        a.org = 1.0;
        let mut b = tank(1, Side::Defender, HexCoord::new(2, 1));
        b.org = 1.0;
        let mut units = vec![a, b];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert!(res[0].org_damage_taken >= 1.0, "counter should land too");
        assert!(units[0].org <= 0.0 && units[1].org <= 0.0);
        assert_ne!(units[0].state, UnitState::Active);
        assert_ne!(units[1].state, UnitState::Active);
    }

    #[test]
    fn target_lost_when_out_of_reach() {
        let g = grid(8, 8);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(4, 1)), // 3 hexes away
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        assert!(res[0].target_lost);
        assert_eq!(res[0].org_damage_dealt, 0.0);
    }

    #[test]
    fn area_fire_weights_center_and_neighbors() {
        // Every victim takes ITS OWN strike
        // weighted by its hex — the aim hex 4/10, each neighbour 1/10.
        // Both victims sit on equal flat ground at the same range, so the
        // centre bleeds 4× the neighbour's share.
        let g = grid(14, 6);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(9, 1)), // aim hex — 4/10
            inf(2, Side::Defender, HexCoord::new(9, 2)), // neighbour — 1/10
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: false,
                },
            }],
        );
        assert_eq!(res.len(), 2, "one lane per victim (per-hex reports)");
        let strike = 7.5 * (0.1 + 0.3 * 7.5 / 29.5); // q = 25×0.3 = 7.5, P = 7.5 vs D 22 → hit ≈ 0.176
        assert!(
            close(units[1].max_org - units[1].org, strike * 0.40),
            "centre takes 4/10: {}",
            units[1].org
        );
        assert!(
            close(units[2].max_org - units[2].org, strike * 0.10),
            "neighbour takes 1/10: {}",
            units[2].org
        );
    }

    #[test]
    fn area_fire_bleeds_zone_when_aim_hex_is_empty() {
        // Zone rule ("圈内任敌即可"): an area-fire aim hex with
        // NO enemy at the centre still hits every enemy in the outlined
        // zone — each takes its own strike ×1/10 (neighbour weight), none
        // is target-lost.
        let g = grid(14, 6);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(2, Side::Defender, HexCoord::new(9, 2)), // in the zone
            inf(3, Side::Defender, HexCoord::new(9, 0)), // in the zone
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: false,
                },
            }],
        );
        assert_eq!(res.len(), 2, "both zone neighbours bleed");
        assert!(res.iter().all(|r| !r.target_lost));
        for r in &res {
            assert!(
                close(r.org_damage_dealt, 7.5 * (0.1 + 0.3 * 7.5 / 29.5) * 0.10),
                "{}",
                r.org_damage_dealt
            );
        }
    }

    #[test]
    fn area_fire_never_hits_the_firing_battery() {
        // The firer's own hex sits inside the 7-hex zone for a
        // point-blank mission (dist 1 to the aim hex) — and a battery
        // never shells itself (the AT self-splash report).
        let g = grid(14, 6);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(8, 1)), // firer — inside the zone
            inf(1, Side::Defender, HexCoord::new(9, 1)),       // aim hex — 4/10
            inf(2, Side::Defender, HexCoord::new(9, 2)),       // neighbour — 1/10
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: false,
                },
            }],
        );
        // Two victim lanes and NOTHING else: pre-fix the firer's own hex
        // (inside the zone at dist 1) took a third lane. The firer still
        // loses org to the aim-hex enemy's counter-battery reply — a
        // separate, intended mechanic, not self-splash.
        assert_eq!(res.len(), 2, "one lane per enemy victim only: {res:?}");
    }

    #[test]
    fn area_fire_splashes_friends_in_the_zone() {
        // The F-barrage cannot tell friend
        // from foe — a co-side battalion inside the zone takes its own
        // strike at its hex's weight, exactly like a rocket salvo's
        // friendly fire.
        let g = grid(14, 6);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(9, 1)), // aim hex — enemy
            inf(2, Side::Attacker, HexCoord::new(9, 2)), // FRIENDLY neighbour
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: false,
                },
            }],
        );
        assert_eq!(res.len(), 2, "one lane per victim, friendly included");
        let strike = 7.5 * (0.1 + 0.3 * 7.5 / 29.5); // q = 25×0.3 = 7.5, P = 7.5 vs D 22 → hit ≈ 0.176
        assert!(
            close(units[1].max_org - units[1].org, strike * 0.40),
            "enemy centre takes 4/10: {}",
            units[1].org
        );
        assert!(
            close(units[2].max_org - units[2].org, strike * 0.10),
            "friendly neighbour takes 1/10: {}",
            units[2].org
        );
        assert!(res
            .iter()
            .any(|r| r.defender_id == 2 && r.org_damage_dealt > 0.0));
    }

    #[test]
    fn area_fire_splashes_friend_at_the_aim_hex() {
        // The weight rides the HEX, not the side — a friendly standing on
        // the aim hex takes the 4/10 share too (the mission stays live
        // because an enemy holds a neighbour).
        let g = grid(14, 6);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Attacker, HexCoord::new(9, 1)), // FRIENDLY at aim hex
            inf(2, Side::Defender, HexCoord::new(9, 2)), // enemy neighbour
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: false,
                },
            }],
        );
        assert_eq!(
            res.len(),
            2,
            "one lane per victim, friendly centre included"
        );
        let strike = 7.5 * (0.1 + 0.3 * 7.5 / 29.5); // q = 25×0.3 = 7.5, P = 7.5 vs D 22 → hit ≈ 0.176
        assert!(
            close(units[1].max_org - units[1].org, strike * 0.40),
            "friendly at centre takes 4/10: {}",
            units[1].org
        );
        assert!(
            close(units[2].max_org - units[2].org, strike * 0.10),
            "enemy neighbour takes 1/10: {}",
            units[2].org
        );
        assert!(res
            .iter()
            .any(|r| r.defender_id == 1 && r.org_damage_dealt > 0.0));
    }

    #[test]
    fn area_fire_on_an_empty_zone_is_lost() {
        // No targetable unit anywhere in the 7-hex zone: the whole mission
        // fizzles (target-lost), exactly like a shelling of open ground.
        let g = grid(14, 6);
        let mut units = vec![artillery(0, Side::Attacker, HexCoord::new(1, 1))];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: false,
                },
            }],
        );
        assert_eq!(res.len(), 1);
        assert!(res[0].target_lost);
        assert_eq!(res[0].org_damage_dealt, 0.0);
    }

    #[test]
    fn area_fire_bleeds_friends_when_zone_has_no_enemy() {
        // The old "enemy in zone" gate fizzled the whole mission when only
        // friends stood inside (a barrage on a friends-only zone produced
        // no reports at all). Rule: the
        // barrage lands REGARDLESS — "点击任意射程内地点" — a friends-only
        // zone takes its weighted shares (centre 4/10, neighbour 1/10).
        let g = grid(14, 6);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Attacker, HexCoord::new(9, 1)), // FRIENDLY at aim hex
            inf(2, Side::Attacker, HexCoord::new(9, 2)), // FRIENDLY neighbour
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(9, 1),
                    precise: false,
                },
            }],
        );
        assert_eq!(res.len(), 2, "both friends bleed — the mission is live");
        assert!(res.iter().all(|r| !r.target_lost));
        let strike = 7.5 * (0.1 + 0.3 * 7.5 / 29.5); // q = 25×0.3 = 7.5, P = 7.5 vs D 22 → hit ≈ 0.176
        assert!(
            close(units[1].max_org - units[1].org, strike * 0.40),
            "friendly centre takes 4/10: {}",
            units[1].org
        );
        assert!(
            close(units[2].max_org - units[2].org, strike * 0.10),
            "friendly neighbour takes 1/10: {}",
            units[2].org
        );
    }

    #[test]
    fn rocket_area_full_strength_and_friendly_fire() {
        // A rocket salvo strikes EVERY unit in the 7-hex zone at
        // full tube-artillery strength (accuracy 0.30, no dilution, no
        // doubling) — friends included.
        let g = grid(14, 6);
        let mut rkt = BattalionUnit::new(
            3,
            "Rk3",
            UnitType::MotRocketArtillery,
            Side::Attacker,
            HexCoord::new(1, 1),
        );
        rkt.soft_attack = 30.0;
        let mut units = vec![
            rkt,
            inf(4, Side::Defender, HexCoord::new(6, 1)), // aim hex
            inf(5, Side::Defender, HexCoord::new(6, 2)), // neighbour
            inf(6, Side::Attacker, HexCoord::new(7, 1)), // FRIENDLY neighbour
        ];
        let mut e = engine(2);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 3,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(6, 1),
                    precise: false,
                },
            }],
        );
        // q = 30×0.30 = 9, P = 9 vs D 22 → hit ≈ 0.187 → ≈1.684 per victim
        // — one lane each (per-hex reports); the aim-hex infantry cannot
        // reply from 5 hexes.
        let per_hex = 9.0 * (0.1 + 0.3 * 9.0 / 31.0);
        assert_eq!(res.len(), 3, "one lane per victim (per-hex report)");
        for r in &res {
            assert!(
                close(r.org_damage_dealt, per_hex),
                "{} vs {}",
                r.org_damage_dealt,
                per_hex
            );
            assert_eq!(r.org_damage_taken, 0.0);
        }
        assert!(close(units[1].max_org - units[1].org, per_hex));
        assert!(close(units[2].max_org - units[2].org, per_hex));
        // Friendly fire: the co-side battalion in the zone took the same hit.
        assert!(
            close(units[3].max_org - units[3].org, per_hex),
            "friendly fire"
        );
        assert!(res
            .iter()
            .any(|r| r.defender_id == 6 && r.hex == HexCoord::new(7, 1)));
    }

    #[test]
    fn rocket_salvo_hits_zone_when_aim_hex_is_empty() {
        // Zone rule ("圈内任敌即可"): a salvo aimed at an EMPTY centre still
        // saturates the whole zone — enemy neighbours take full-strength
        // strikes, and the mission is live (not target-lost).
        let g = grid(14, 6);
        let mut rkt = BattalionUnit::new(
            3,
            "Rk3",
            UnitType::MotRocketArtillery,
            Side::Attacker,
            HexCoord::new(1, 1),
        );
        rkt.soft_attack = 30.0;
        let mut units = vec![
            rkt,
            inf(4, Side::Defender, HexCoord::new(6, 2)), // zone neighbour
            inf(5, Side::Defender, HexCoord::new(7, 1)), // zone neighbour
        ];
        let mut e = engine(2);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 3,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(6, 1),
                    precise: false,
                },
            }],
        );
        let per_hex = 9.0 * (0.1 + 0.3 * 9.0 / 31.0);
        assert_eq!(res.len(), 2, "both zone neighbours take full-strength hits");
        assert!(res.iter().all(|r| !r.target_lost));
        for r in &res {
            assert!(close(r.org_damage_dealt, per_hex), "{}", r.org_damage_dealt);
        }
    }

    #[test]
    fn rocket_salvo_triggers_reload_cooldown() {
        // After a salvo the launcher reloads for
        // `rocket_fire_cooldown_turns`; can_fire_support gates the next
        // mission; refresh_turn ticks it down (fire end of N → next on N+3).
        let g = grid(14, 6);
        let mut rkt = BattalionUnit::new(
            3,
            "Rk3",
            UnitType::MotRocketArtillery,
            Side::Attacker,
            HexCoord::new(1, 1),
        );
        rkt.soft_attack = 30.0;
        let mut units = vec![rkt, inf(4, Side::Defender, HexCoord::new(6, 1))];
        let mut e = engine(2);
        let order = AttackOrder {
            attacker: 3,
            target: AttackTarget::FireMission {
                hex: HexCoord::new(6, 1),
                precise: false,
            },
        };
        let res = e.resolve_fire_phase(&g, &mut units, std::slice::from_ref(&order));
        assert_eq!(res.len(), 1);
        assert_eq!(units[0].fire_cooldown, 3);
        assert!(!units[0].can_fire_support());
        // Re-firing while hot fizzles (validation rejects the mission).
        let res2 = e.resolve_fire_phase(&g, &mut units, std::slice::from_ref(&order));
        assert!(res2.is_empty() || res2.iter().all(|r| r.target_lost));
        // N+1 → 2, N+2 → 1, N+3 → 0 (refresh also clears `acted`).
        units[0].refresh_turn();
        assert_eq!(units[0].fire_cooldown, 2);
        assert!(!units[0].can_fire_support());
        units[0].refresh_turn();
        units[0].refresh_turn();
        assert_eq!(units[0].fire_cooldown, 0);
        assert!(units[0].can_fire_support());
    }

    #[test]
    fn attacking_drops_cover_stance() {
        // Resolving an attack drops the attacker's cover stance
        // (cover protects only a stationary, non-attacking unit).
        let g = grid(8, 8);
        let mut a = inf(0, Side::Attacker, HexCoord::new(1, 1));
        a.is_holding = true;
        let mut units = vec![a, inf(1, Side::Defender, HexCoord::new(2, 1))];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        assert!(units[0].acted);
        assert!(!units[0].is_holding);
    }

    #[test]
    fn assault_advances_into_vacated_hex() {
        let g = grid(8, 8);
        let mut d = inf(1, Side::Defender, HexCoord::new(3, 1));
        d.org = 1.0; // one hit breaks it
        let mut units = vec![tank(0, Side::Attacker, HexCoord::new(2, 1)), d];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert!(res[0].defender_broken);
        assert!(res[0].advanced, "attacker should occupy the vacated hex");
        assert_eq!(units[0].position, HexCoord::new(3, 1));
        assert!(units[0].acted);
    }

    #[test]
    fn river_ford_negative_cover_exposes_the_crosser() {
        let mut g = grid(8, 8);
        g.set_terrain(HexCoord::new(2, 1), Terrain::River);
        let mut units = vec![
            tank(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        // v3.3: the old ×2 ford rule is retired — River's
        // cover is −0.50, so the forded defender takes ×1.5. (Fixtures
        // carry no terrain adjusters.) × melee crest ×1.15 — the tank
        // stands on the bank (elev 0) firing DOWN into the river bed (−1).
        assert!(
            close(
                res[0].org_damage_dealt,
                9.5 * (0.1 + 0.3 * 9.5 / 31.5) * 1.5 * 1.15
            ),
            "{}",
            res[0].org_damage_dealt
        );
    }

    #[test]
    fn no_flank_bonus_after_2026_08_14_redesign() {
        // Redesigned flank rule: an opposite-side pincer earns NO damage
        // multiplier — multi-directional attacks are already rewarded by
        // the Lanchester concentration itself; encirclement is a pure
        // attrition/surrender model. (This fixture asserted the retired
        // ×1.25/×0.9 flank bonus before the redesign.)
        let g = grid(8, 8);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)), // W of target
            inf(1, Side::Defender, HexCoord::new(2, 1)),
            inf(2, Side::Attacker, HexCoord::new(3, 1)), // E of target (opposite)
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        // Plain shared formula: P = 6, D = 22 → hit ≈ 0.164 → 0.986 org,
        // no flank terms.
        let expect = 6.0 * (0.1 + 0.3 * 6.0 / 28.0);
        assert!(
            close(res[0].org_damage_dealt, expect),
            "{} vs {}",
            res[0].org_damage_dealt,
            expect
        );
    }

    #[test]
    fn retreating_defender_does_not_counter() {
        let g = grid(8, 8);
        let mut d = inf(1, Side::Defender, HexCoord::new(2, 1));
        d.state = UnitState::Retreating;
        let mut units = vec![inf(0, Side::Attacker, HexCoord::new(1, 1)), d];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res[0].org_damage_taken, 0.0);
    }

    #[test]
    fn group_strike_sums_attack_before_square() {
        // Two infantry pooling on one target (§6.3 v3.2): P = Σ(q·g) × Σg
        // = 12×2 = 24 → hit = 0.1+0.3×24/46 ≈ 0.257 → 6.26 total, 3.13
        // each — versus 2×0.986 = 1.97 struck separately. Concentration is
        // superlinear (numbers squared + saturation); each attacker is
        // credited its share.
        let g = grid(8, 8);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Attacker, HexCoord::new(2, 2)),
            inf(2, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[
                AttackOrder {
                    attacker: 0,
                    target: AttackTarget::Assault(2),
                },
                AttackOrder {
                    attacker: 1,
                    target: AttackTarget::Assault(2),
                },
            ],
        );
        let total: f32 = res.iter().map(|r| r.org_damage_dealt).sum();
        let expect = 24.0 * (0.1 + 0.3 * 24.0 / 46.0);
        assert!(close(total, expect), "{} vs {}", total, expect);
        assert!(close(res[0].org_damage_dealt, expect / 2.0));
        assert!(close(res[1].org_damage_dealt, expect / 2.0));
    }

    #[test]
    fn fire_phase_attaches_formula_chains() {
        // A lone assault lane carries the full formula chain; the
        // counter lane carries its own, judged against breakthrough.
        let g = grid(4, 4);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        let r = &res[0];
        let bd = r.breakdown.expect("outgoing chain");
        // inf vs inf: q = 6 (soft 6, hardness 0), P = 6·1², D = 22.
        assert!(close(bd.q, 6.0));
        assert!(close(bd.p, 6.0));
        assert!(bd.pool.is_none());
        assert!(close(bd.counter_split, 1.0));
        assert!(!bd.uses_breakthrough);
        assert!(close(bd.d_base, 22.0));
        assert!(close(bd.d, 22.0));
        assert!(close(bd.hit, 0.1 + 0.3 * 6.0 / 28.0));
        assert!(close(bd.org_final, r.org_damage_dealt));
        assert!(close(bd.str_final, r.str_damage_dealt));
        let rows = &bd.linear[..bd.linear_len as usize];
        assert!(rows.iter().any(|(k, _)| *k == LinearFactor::TerrainAttack));
        assert!(rows.iter().any(|(k, _)| *k == LinearFactor::Cover));

        let cbd = r.counter_breakdown.expect("counter chain");
        assert!(cbd.uses_breakthrough);
        assert!(close(cbd.d_base, 3.0), "breakthrough, not defense");
        assert!(close(cbd.counter_split, 1.0));
        assert!(close(cbd.org_final, r.org_damage_taken));
    }

    #[test]
    fn group_volley_lanes_share_pool_breakdowns() {
        // Pooled lanes show the SAME pool figures with
        // complementary shares summing to 1; the defender's counter against
        // two caught attackers is split ÷2.
        let g = grid(8, 8);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Attacker, HexCoord::new(2, 2)),
            inf(2, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[
                AttackOrder {
                    attacker: 0,
                    target: AttackTarget::Assault(2),
                },
                AttackOrder {
                    attacker: 1,
                    target: AttackTarget::Assault(2),
                },
            ],
        );
        assert_eq!(res.len(), 2);
        let b0 = res[0].breakdown.expect("lane 0 chain");
        let b1 = res[1].breakdown.expect("lane 1 chain");
        let p0 = b0.pool.expect("pool info");
        assert_eq!(p0.members, 2);
        assert_eq!(p0, b1.pool.expect("pool info"));
        assert!(close(p0.sum_qg, 12.0));
        assert!(close(p0.sum_g, 2.0));
        assert!(close(b0.p, 24.0));
        // Identical attackers → equal shares; each lane's q rows are its own.
        assert!(close(b0.pool_share, 0.5));
        assert!(close(b0.pool_share + b1.pool_share, 1.0));
        assert!(close(b0.q, 6.0));
        assert!(close(b1.q, 6.0));
        let pool_final = 24.0 * (0.1 + 0.3 * 24.0 / 46.0);
        assert!(close(b0.org_pool_final, pool_final));
        assert!(close(b0.org_final, pool_final * 0.5));
        assert!(close(b0.org_final, res[0].org_damage_dealt));
        // Counter-fire against two attackers: ÷2 split on each counter lane.
        let c0 = res[0].counter_breakdown.expect("counter chain");
        assert!(close(c0.counter_split, 2.0));
        assert!(c0.uses_breakthrough);
    }

    #[test]
    fn fizzled_order_has_no_breakdown() {
        // A target-lost lane never computed a strike — no chain,
        // no detail button in the UI.
        let g = grid(8, 8);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(0, 0)),
            inf(1, Side::Defender, HexCoord::new(4, 0)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        assert!(res[0].target_lost);
        assert!(res[0].breakdown.is_none());
        assert!(res[0].counter_breakdown.is_none());
    }

    #[test]
    fn artillery_has_no_radar_less_counter_battery() {
        // Direct-lay gate: a battery shelled from 5 hexes does NOT reply —
        // counter-battery beyond the direct-lay circle is gone.
        let g = grid(10, 4);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            artillery(1, Side::Defender, HexCoord::new(6, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(6, 1),
                    precise: true,
                },
            }],
        );
        assert_eq!(res.len(), 1);
        let r = &res[0];
        // Area form: q = 25×0.30 = 7.5, P = q·g = 7.5 (linear), D = 10.
        let bd = r.breakdown.expect("chain");
        assert!(bd.area_fire);
        assert!(close(bd.p, 7.5));
        assert!(close(r.org_damage_dealt, 7.5 * (0.1 + 0.3 * 7.5 / 17.5)));
        assert_eq!(r.org_damage_taken, 0.0, "no counter-battery past the gate");
        assert!(r.counter_breakdown.is_none());
    }

    #[test]
    fn artillery_counters_inside_the_direct_lay_circle_aimed() {
        // Direct-lay gate: assaulted at range 1 the battery fires over open
        // sights — the counter exists and is AIMED fire (square law).
        let g = grid(4, 4);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            artillery(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        let cbd = res[0].counter_breakdown.expect("direct-lay counter");
        assert!(!cbd.area_fire, "direct lay is the AIMED regime");
        assert!(cbd.uses_breakthrough);
        // q = 25×0.30 = 7.5, P = q·g² = 7.5 vs breakthrough 3 → hit ≈ 0.314.
        assert!(close(cbd.p, 7.5));
        assert!(close(cbd.org_final, 7.5 * (0.1 + 0.3 * 7.5 / 10.5)));
    }

    #[test]
    fn rockets_never_counter() {
        // Direct-lay gate: unguided saturation has no direct-lay mode — a
        // launcher caught in melee does not reply at all.
        let g = grid(4, 4);
        let mut rkt = BattalionUnit::new(
            1,
            "Rk1".to_string(),
            UnitType::MotRocketArtillery,
            Side::Defender,
            HexCoord::new(2, 1),
        );
        rkt.soft_attack = 30.0;
        let mut units = vec![inf(0, Side::Attacker, HexCoord::new(1, 1)), rkt];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].org_damage_taken, 0.0);
        assert!(res[0].counter_breakdown.is_none());
    }

    #[test]
    fn limbered_towed_guns_deal_no_damage_in_any_form() {
        // A LIMBERED towed unit cannot
        // assault, cannot counter, and (already) cannot fire support.
        let mut limbered = artillery(3, Side::Defender, HexCoord::new(2, 1));
        limbered.is_emplaced = false;
        assert!(limbered.requires_emplacement());
        assert!(!limbered.can_assault(), "limbered towed guns never assault");

        // Assaulted at range 1 while limbered: no direct-lay reply.
        let g = grid(4, 4);
        let mut units = vec![inf(0, Side::Attacker, HexCoord::new(1, 1)), limbered];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(3),
            }],
        );
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].org_damage_taken, 0.0, "limbered gun never counters");
        assert!(res[0].counter_breakdown.is_none());

        // And a forged limbered assault order fizzles at the resolver.
        let mut limbered_attacker = artillery(4, Side::Attacker, HexCoord::new(1, 1));
        limbered_attacker.is_emplaced = false;
        let mut guns = vec![limbered_attacker, inf(5, Side::Defender, HexCoord::new(2, 1))];
        let res2 = e.resolve_fire_phase(
            &g,
            &mut guns,
            &[AttackOrder {
                attacker: 4,
                target: AttackTarget::Assault(5),
            }],
        );
        assert!(res2.is_empty(), "forged limbered assault must fizzle");
    }

    #[test]
    fn rocket_mission_below_min_range_fizzles() {
        // Rockets join no fight inside 3
        // hexes — a mission aimed at ring 2 fizzles (target lost).
        let g = grid(6, 4);
        let mut rkt = BattalionUnit::new(
            3,
            "Rk3".to_string(),
            UnitType::MotRocketArtillery,
            Side::Attacker,
            HexCoord::new(1, 1),
        );
        rkt.soft_attack = 30.0;
        let mut units = vec![rkt, inf(4, Side::Defender, HexCoord::new(3, 1))];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 3,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(3, 1),
                    precise: false,
                },
            }],
        );
        assert_eq!(res.len(), 1);
        assert!(res[0].target_lost);
        assert_eq!(res[0].org_damage_dealt, 0.0);
    }

    #[test]
    fn rocket_precise_order_resolves_as_area() {
        // Rockets can never be precise — a
        // "precise" right-click still saturates the whole 7-hex zone.
        let g = grid(14, 6);
        let mut rkt = BattalionUnit::new(
            3,
            "Rk3".to_string(),
            UnitType::MotRocketArtillery,
            Side::Attacker,
            HexCoord::new(1, 1),
        );
        rkt.soft_attack = 30.0;
        let mut units = vec![
            rkt,
            inf(4, Side::Defender, HexCoord::new(6, 1)), // aim hex
            inf(5, Side::Defender, HexCoord::new(6, 2)), // neighbour
        ];
        let mut e = engine(2);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 3,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(6, 1),
                    precise: true, // forced back to area for rockets
                },
            }],
        );
        // Both victims take the FULL per-victim strike (area saturation,
        // no precise singling-out): q = 30×0.30 = 9, P = q·g = 9 (linear)
        // vs D 22 → hit ≈ 0.187.
        let per_hex = 9.0 * (0.1 + 0.3 * 9.0 / 31.0);
        assert_eq!(res.len(), 2, "both zone victims struck — area, not precise");
        for r in &res {
            assert!(close(r.org_damage_dealt, per_hex), "{}", r.org_damage_dealt);
        }
    }

    #[test]
    fn group_strike_aggregate_shock_crosses_threshold() {
        // Shock is judged on the AGGREGATED delivered org damage per target:
        // each attacker's share stays below the
        // 25%-of-max-org threshold (60×0.25 = 15) but the pooled sum crosses
        // it — the target must be Shocked even though no single lane would
        // have shocked it on its own. (The concentration test above deals
        // 6.26 total, deliberately short of the threshold.)
        let g = grid(8, 8);
        let mut a0 = inf(0, Side::Attacker, HexCoord::new(1, 1));
        a0.soft_attack = 13.0;
        let mut a1 = inf(1, Side::Attacker, HexCoord::new(2, 2));
        a1.soft_attack = 13.0;
        let mut units = vec![a0, a1, inf(2, Side::Defender, HexCoord::new(2, 1))];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[
                AttackOrder {
                    attacker: 0,
                    target: AttackTarget::Assault(2),
                },
                AttackOrder {
                    attacker: 1,
                    target: AttackTarget::Assault(2),
                },
            ],
        );
        // Pooled (§6.3 v3.2): P = 26×2 = 52 → hit ≈ 0.311 → 16.16 total,
        // 8.08 each.
        let threshold = units[2].max_org * 0.25;
        let total: f32 = res.iter().map(|r| r.org_damage_dealt).sum();
        assert!(
            total >= threshold,
            "aggregate {total} must cross the shock threshold {threshold}"
        );
        assert!(
            res.iter().all(|r| r.org_damage_dealt < threshold),
            "each share must stay BELOW the threshold, else the aggregation is untested: {res:?}"
        );
        assert!(units[2].shocked, "the aggregated hit must Shock the target");
        assert!(res[0].shocked_defender && res[1].shocked_defender);
    }

    #[test]
    fn strength_wipeout_annihilates() {
        // Defender at 0.05 strength: str damage wipes it off the map
        // entirely (no untargetable zombie holding its hex). (v3.2: the
        // tank's strike deals ≈1.81 org → ≈0.09 str at the λ=0.12 rate.)
        let g = grid(8, 8);
        let mut d = inf(1, Side::Defender, HexCoord::new(2, 1));
        d.strength = 0.05;
        let mut units = vec![tank(0, Side::Attacker, HexCoord::new(1, 1)), d];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert_eq!(units[1].state, UnitState::Eliminated);
        assert!(res[0].eliminated);
    }

    // ── §6.13 command / division HQ ───────────────────────────────────────

    fn inf_div(id: usize, side: Side, pos: HexCoord, div: &str) -> BattalionUnit {
        let mut u = inf(id, side, pos);
        u.division = div.to_string();
        u
    }

    fn add_hq(units: &mut Vec<BattalionUnit>, side: Side, pos: HexCoord) {
        let mut next_id = 1000;
        tactical_core::synthesize_hqs(units, &mut next_id, side, |_| pos);
    }

    #[test]
    fn command_aura_boosts_attack_and_defense_linear() {
        let g = grid(8, 8);
        let orders = [AttackOrder {
            attacker: 0,
            target: AttackTarget::Assault(1),
        }];

        // Baseline: no HQ on the map.
        let mut units = vec![
            inf_div(0, Side::Attacker, HexCoord::new(1, 1), "A"),
            inf_div(1, Side::Defender, HexCoord::new(2, 1), "B"),
        ];
        let base = engine(1).resolve_fire_phase(&g, &mut units, &orders)[0].org_damage_dealt;

        // Attacker in command: +10% exactly (linear, post-square).
        let mut units = vec![
            inf_div(0, Side::Attacker, HexCoord::new(1, 1), "A"),
            inf_div(1, Side::Defender, HexCoord::new(2, 1), "B"),
        ];
        add_hq(&mut units, Side::Attacker, HexCoord::new(1, 2));
        let boosted = engine(1).resolve_fire_phase(&g, &mut units, &orders)[0].org_damage_dealt;
        assert!(close(boosted, base * 1.10), "{boosted} vs {}", base * 1.10);

        // Defender in command: -10% damage taken.
        let mut units = vec![
            inf_div(0, Side::Attacker, HexCoord::new(1, 1), "A"),
            inf_div(1, Side::Defender, HexCoord::new(2, 1), "B"),
        ];
        add_hq(&mut units, Side::Defender, HexCoord::new(2, 2));
        let shielded = engine(1).resolve_fire_phase(&g, &mut units, &orders)[0].org_damage_dealt;
        assert!(
            close(shielded, base * 0.90),
            "{shielded} vs {}",
            base * 0.90
        );
    }

    #[test]
    fn command_group_bonus_is_attack_weighted() {
        // Two identical attackers pool on one target; the HQ reaches only
        // one of them → pool bonus = 1 + 0.10 × (in-command share) = 1.05.
        let g = grid(9, 9);
        let orders = [
            AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(2),
            },
            AttackOrder {
                attacker: 1,
                target: AttackTarget::Assault(2),
            },
        ];
        let mk = || {
            vec![
                inf_div(0, Side::Attacker, HexCoord::new(4, 3), "A"),
                inf_div(1, Side::Attacker, HexCoord::new(2, 3), "A"),
                inf_div(2, Side::Defender, HexCoord::new(3, 3), "B"),
            ]
        };
        let mut units = mk();
        let base: f32 = engine(1)
            .resolve_fire_phase(&g, &mut units, &orders)
            .iter()
            .map(|r| r.org_damage_dealt)
            .sum();
        let mut units = mk();
        // Radius 3: (6,3) reaches id 0 only (dist 2 vs 4).
        add_hq(&mut units, Side::Attacker, HexCoord::new(6, 3));
        let boosted: f32 = engine(1)
            .resolve_fire_phase(&g, &mut units, &orders)
            .iter()
            .map(|r| r.org_damage_dealt)
            .sum();
        assert!(close(boosted, base * 1.05), "{boosted} vs {}", base * 1.05);
    }

    #[test]
    fn hq_annihilation_collapses_division() {
        let g = grid(8, 8);
        let mut killer = tank(0, Side::Attacker, HexCoord::new(1, 1));
        killer.soft_attack = 30.0; // enough to maul the HQ in one hit
        killer.division = "Atk".to_string();
        let mut weak = inf_div(2, Side::Defender, HexCoord::new(5, 5), "Def");
        weak.org = 10.0; // the collapse (12 org) breaks this one
        let mut units = vec![
            killer,
            inf_div(1, Side::Defender, HexCoord::new(4, 4), "Def"),
            weak,
        ];
        add_hq(&mut units, Side::Defender, HexCoord::new(2, 1)); // id 1000, adjacent to the killer
        // v3.2: the λ=0.12 str rate makes one-hit strength wipes of a
        // full-strength HQ impossible by design — pre-weaken the remnant
        // so the annihilation path (not a break) is what gets exercised.
        let hqi = units.iter().position(|u| u.is_hq()).unwrap();
        units[hqi].strength = 0.05;
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1000),
            }],
        );
        assert!(res[0].eliminated);
        let hq = units.iter().find(|u| u.is_hq()).unwrap();
        assert_eq!(hq.state, UnitState::Eliminated);
        // 20% of max org (60 × 0.2 = 12) off every surviving same-division
        // battalion; the killer (division "Atk") takes no collapse damage.
        assert!(close(units[1].org, 48.0), "{}", units[1].org);
        let events = e.take_hq_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].division, "Def");
        assert_eq!(events[0].losses.len(), 2);
        assert_eq!(events[0].broke, vec![2usize]);
        assert_eq!(units[2].state, UnitState::Retreating);
    }

    #[test]
    fn hq_retreat_or_survival_triggers_no_collapse() {
        let g = grid(8, 8);
        let mut units = vec![
            inf_div(0, Side::Attacker, HexCoord::new(1, 1), "A"),
            inf_div(1, Side::Defender, HexCoord::new(2, 1), "B"),
        ];
        add_hq(&mut units, Side::Defender, HexCoord::new(2, 2));
        // HQ broken but alive (encirclement-attrition path): no collapse.
        let hqi = units.iter().position(|u| u.is_hq()).unwrap();
        units[hqi].org = 0.0;
        units[hqi].state = UnitState::Retreating;
        let mut e = engine(1);
        e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        assert!(e.take_hq_events().is_empty());
    }

    #[test]
    fn command_regen_only_for_in_command_active() {
        let mut units = vec![
            inf_div(0, Side::Attacker, HexCoord::new(1, 1), "D"), // in command
            inf_div(1, Side::Attacker, HexCoord::new(8, 1), "D"), // out of range (dist 7 > 3)
            inf_div(2, Side::Attacker, HexCoord::new(1, 2), "D"), // in command, at cap edge
            inf_div(3, Side::Attacker, HexCoord::new(2, 1), "D"), // retreating: never regens
            inf_div(4, Side::Attacker, HexCoord::new(3, 1), "D"), // in command, in contact: no regen
            inf_div(5, Side::Defender, HexCoord::new(2, 0), "E"), // adjacent to unit 0
        ];
        units[0].org = 30.0;
        units[1].org = 30.0;
        units[2].org = 59.0;
        units[3].org = 0.0;
        units[3].state = UnitState::Retreating;
        units[4].org = 30.0;
        add_hq(&mut units, Side::Attacker, HexCoord::new(1, 1));
        let e = engine(1);
        let mut units = units;
        e.apply_command_regen(&mut units);
        // Regen 2% of max org, only out of contact.
        assert!(close(units[0].org, 30.0), "{}", units[0].org); // adjacent enemy blocks regen
        assert!(close(units[1].org, 30.0));
        assert!(close(units[2].org, 60.0)); // capped at max_org
        assert!(close(units[3].org, 0.0));
        assert!(close(units[4].org, 31.2), "{}", units[4].org); // +2% of 60
        let hq = units.iter().find(|u| u.is_hq()).unwrap();
        assert!(close(hq.org, 20.0)); // the HQ never benefits from its own aura
    }

    // ── §6.6 crest rules ───────────────────────────────────────────────────

    fn set_elev(g: &mut HexGrid, q: i32, r: i32, e: i32) {
        g.cell_mut(HexCoord::new(q, r)).unwrap().elevation = e;
    }

    #[test]
    fn melee_crest_occupant_shrugs_uphill_assault() {
        // Valley attacker (elev 0) assaults a crest defender (elev 3):
        // outgoing ×(1 − 0.45) capped, the counter-fire runs DOWNHILL ×1.45
        // (strike_split reads the same melee gain).
        let mut g = grid(8, 8);
        set_elev(&mut g, 1, 1, 0);
        set_elev(&mut g, 2, 1, 3);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        let r = &res[0];
        // v3.2 numbers: base strike 0.986 (hit-step), base counter 1.8.
        assert!(
            close(r.org_damage_dealt, 6.0 * (0.1 + 0.3 * 6.0 / 28.0) * 0.55),
            "{}",
            r.org_damage_dealt
        );
        assert!(
            close(r.org_damage_taken, 1.8 * 1.45),
            "{}",
            r.org_damage_taken
        );
    }

    #[test]
    fn melee_downhill_attacker_rolls_over_valley() {
        // The crest occupant strikes down at ×1.45; the valley defender's
        // counter-fire is throttled to ×0.55.
        let mut g = grid(8, 8);
        set_elev(&mut g, 1, 1, 3);
        set_elev(&mut g, 2, 1, 0);
        let mut units = vec![
            inf(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(2, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::Assault(1),
            }],
        );
        let r = &res[0];
        // v3.2 numbers: base strike 0.986, base counter 1.8.
        assert!(
            close(r.org_damage_dealt, 6.0 * (0.1 + 0.3 * 6.0 / 28.0) * 1.45),
            "{}",
            r.org_damage_dealt
        );
        assert!(
            close(r.org_damage_taken, 1.8 * 0.55),
            "{}",
            r.org_damage_taken
        );
    }

    #[test]
    fn melee_gain_inactive_beyond_contact() {
        // Range-2 direct fire: no height gain — only the ×0.6 falloff.
        // The tank's line clears a LOW intermediate step (elev 2 ≤ max 3,0),
        // so the shot resolves.
        let mut g = grid(8, 8);
        set_elev(&mut g, 1, 1, 3);
        set_elev(&mut g, 2, 1, 2);
        set_elev(&mut g, 3, 1, 0);
        let mut units = vec![
            tank(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(3, 1)),
        ];
        units[0].attack_range = 2;
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::DirectFire(1),
            }],
        );
        let r = &res[0];
        assert!(!r.target_lost);
        // q = 19 × 0.5 (tank accuracy) = 9.5 → P = 9.5; D = 22 →
        // hit ≈ 0.190 → 1.810; ×0.6 falloff. Melee gain stays neutral at
        // range 2.
        assert!(
            close(r.org_damage_dealt, 9.5 * (0.1 + 0.3 * 9.5 / 31.5) * 0.6),
            "{}",
            r.org_damage_dealt
        );
    }

    #[test]
    fn direct_fire_ridge_blocks_tank() {
        // A flat trajectory cannot shoot over a ridge: intermediate hex
        // strictly higher than BOTH endpoints → the shot is impossible
        // (target_lost, "无法攻击").
        let mut g = grid(8, 8);
        set_elev(&mut g, 1, 1, 0);
        set_elev(&mut g, 2, 1, 3);
        set_elev(&mut g, 3, 1, 0);
        let mut units = vec![
            tank(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(3, 1)),
        ];
        units[0].attack_range = 2;
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::DirectFire(1),
            }],
        );
        assert!(res[0].target_lost, "the ridge blocks the flat trajectory");
        assert_eq!(res[0].org_damage_dealt, 0.0);
        assert_eq!(
            res[0].org_damage_taken, 0.0,
            "no counter-fire from an impossible shot"
        );
    }

    #[test]
    fn artillery_fires_over_the_ridge_exposed_crest() {
        // Indirect fire ignores ridge LOS entirely (high arc) — the mission
        // RESOLVES; the crest factor then reads the target's OWN step:
        // gun (1,1) elev 0 → target (5,1) elev 3; ridge (2,1)/(3,1) elev 4
        // (> max(0,3): the LOS line is dead, the shell flies over it); the
        // target's near step (4,1) elev 0 < 3 → EXPOSED ×1.5.
        let mut g = grid(8, 8);
        set_elev(&mut g, 1, 1, 0);
        set_elev(&mut g, 2, 1, 4);
        set_elev(&mut g, 3, 1, 4);
        set_elev(&mut g, 4, 1, 0);
        set_elev(&mut g, 5, 1, 3);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(5, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(5, 1),
                    precise: true,
                },
            }],
        );
        let r = &res[0];
        assert!(!r.target_lost, "the high arc clears the ridge");
        // q = 25×0.3 = 7.5, P = 7.5 vs D 22 → hit ≈ 0.176, ×1.5 exposed.
        assert!(
            close(r.org_damage_dealt, 7.5 * (0.1 + 0.3 * 7.5 / 29.5) * 1.5),
            "{}",
            r.org_damage_dealt
        );
    }

    #[test]
    fn artillery_reverse_slope_defilade() {
        // Reverse-slope defence (Korean-war style): the target's near step
        // stands HIGHER than it — the ridge shoulder throttles the impact
        // angle → ×0.5.
        let mut g = grid(8, 8);
        set_elev(&mut g, 1, 1, 0);
        set_elev(&mut g, 4, 1, 2);
        set_elev(&mut g, 5, 1, 0);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(5, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(5, 1),
                    precise: true,
                },
            }],
        );
        let r = &res[0];
        assert!(!r.target_lost);
        assert!(
            close(r.org_damage_dealt, 7.5 * (0.1 + 0.3 * 7.5 / 29.5) * 0.5),
            "{}",
            r.org_damage_dealt
        );
    }

    #[test]
    fn artillery_crest_neutral_equal_step() {
        // The target and its near step are level → neutral ×1.0.
        let mut g = grid(8, 8);
        set_elev(&mut g, 1, 1, 0);
        set_elev(&mut g, 4, 1, 1);
        set_elev(&mut g, 5, 1, 1);
        let mut units = vec![
            artillery(0, Side::Attacker, HexCoord::new(1, 1)),
            inf(1, Side::Defender, HexCoord::new(5, 1)),
        ];
        let mut e = engine(1);
        let res = e.resolve_fire_phase(
            &g,
            &mut units,
            &[AttackOrder {
                attacker: 0,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(5, 1),
                    precise: true,
                },
            }],
        );
        let r = &res[0];
        assert!(!r.target_lost);
        assert!(
            close(r.org_damage_dealt, 7.5 * (0.1 + 0.3 * 7.5 / 29.5)),
            "{}",
            r.org_damage_dealt
        );
    }
}
