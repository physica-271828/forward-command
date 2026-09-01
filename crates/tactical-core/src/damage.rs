//! Shared strike math — the single source of truth for the damage formula
//! (DESIGN.md §6.3; the v3.2 formula rework and the v3.3 terrain rework).
//!
//! Model:
//! - Attack QUALITY is linear, NUMBERS are squared. Quality is the vanilla
//!   hardness-weighted mix — soft×(1−h) + hard×h — with the piercing tier
//!   multiplying the HARD component, times the class precision factor.
//!   A lone strike's firepower is P = q × g² (g = strength ratio, equipment
//!   fill included); a concentrated pool is
//!   P = Σ(q_i·g_i) × Σg_i — fire conservation (two half-strength
//!   battalions equal one full one) with combined-arms cross terms.
//!   The squaring is the AIMED-fire
//!   regime (assault / direct fire / every counter — artillery replies
//!   only inside its direct-lay circle, which IS aimed fire). AREA fire —
//!   indirect-artillery fire missions — is linear: P = q × g ("shells are
//!   shells, however distributed", the Deitchman area-fire law); at full
//!   strength the two forms coincide.
//! - Defense is NOT a denominator. The retired form divided by
//!   (D + 40), which quadratically amplified equipment gaps, had to guard
//!   breakthrough-0 division, and made attacking suicidal (breakthrough ≪
//!   defense). Defense now gates the vanilla hit step:
//!   hit = hit_base + (hit_saturated − hit_base) × P/(P+D)
//!   — the soft-step form of 00_defines.lua's 10%/40% hit chances, with
//!   the same asymptotes and no cliff at battalion-scale numbers.
//! - Counter-fire splits the defender's firepower LINEARLY (P_d/n, fire
//!   conservation — vanilla distributes its attacks linearly too); each
//!   share evaluates its own hit step against the caught attacker's
//!   breakthrough (the vanilla "attacker defends with breakthrough").
//! - Deterministic: no jitter (`random_spread` defaults to 0; the resolver
//!   keeps the mechanism). The fog of war carries the uncertainty, not
//!   the resolution.
//! - Strength damage follows the target's pool shape: a full break
//!   (max_org cumulative org damage) costs `break_str_loss` of max
//!   strength for EVERY class — 0.12 is the vanilla division-scale break
//!   cost (org dice 1d4×0.053 vs str dice 1d2×0.060 would read 0.68 per
//!   hit, but that ratio only works against HOI4's division-scale pools).
//!
//! v3.3 terrain:
//! - ONE uniform terrain layer: cover (per-terrain, reads the target's hex,
//!   negative = exposed — river −0.50 carries the retired ×2 ford
//!   rule). The old global attack/defense modifier tables are gone:
//!   they were linear duplicates of the cover channel, and vanilla terrain
//!   has no defense bonus at all.
//! - Unit-class identity comes from the battalion's own vanilla terrain
//!   adjusters (`BattalionUnit::terrain_adj`), strike-role form: the FIRER
//!   applies its attack adjuster (target's hex) as a standalone linear
//!   factor (× terrain_modifier_scale, floored at 0); the ABSORBER's
//!   defense adjuster multiplies its D (both the defense and the
//!   breakthrough side of the hit step). Line infantry has zero adjusters
//!   (vanilla defines none) — specialists and vehicle/towed classes carry
//!   the terrain game.
//!   A target already at org 0 when the volley starts (broken/routing)
//!   converts at `broken_str_loss` = 0.68 instead — no organization left
//!   to absorb the fire, so it lands on strength at the vanilla dice
//!   ratio.
//!
//! Single-source invariant: the combat resolver (`tactical-combat`) and the
//! AI planning estimate (`tactical-ai::est_org_damage`) both compute through
//! the functions below, so plan and resolution agree by construction.

use crate::los::{indirect_crest_mult, melee_elevation_mult};
use crate::params::piercing_multiplier;
use crate::unit::Attrs;
use crate::{BattalionUnit, CombatParams, HexGrid, Terrain};

// ---------------------------------------------------------------------------
// Formula-chain capture for the engagement-detail panel. Every
// `*_explained` variant computes EXACTLY what the matching plain function
// computes — the plain one is now a thin wrapper that discards the
// capture — so the panel can never drift from the resolution (the same
// single-source discipline as the plan/resolution invariant above).
// `HitBreakdown` is Copy + fixed-size: no allocation on the combat hot
// path.
// ---------------------------------------------------------------------------

/// One row of the §6.3 linear modifier stack, as displayed by the
/// engagement-detail panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearFactor {
    CommandAura,
    TargetCommand,
    TerrainAttack,
    DirectFireFalloff,
    MeleeElevation,
    IndirectCrest,
    Cover,
    AreaWeight,
}

/// Why the precision factor is what it is (the §6.3 class table) — the
/// display label for the accuracy row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccuracyClass {
    #[default]
    Standard,
    Artillery,
    AntiTank,
    AntiAir,
    Armored,
}

impl AccuracyClass {
    /// The same branch table as `BattalionUnit::accuracy_factor`.
    pub fn of(u: &BattalionUnit) -> Self {
        if u.attrs.has(Attrs::ROCKET) || u.attrs.has(Attrs::ARTILLERY) {
            AccuracyClass::Artillery
        } else if u.attrs.has(Attrs::AT) {
            AccuracyClass::AntiTank
        } else if u.attrs.has(Attrs::AA) {
            AccuracyClass::AntiAir
        } else if u.attrs.has(Attrs::ARMORED) {
            AccuracyClass::Armored
        } else {
            AccuracyClass::Standard
        }
    }

    pub fn factor(self) -> f32 {
        match self {
            AccuracyClass::Artillery => 0.30,
            AccuracyClass::AntiTank => 0.80,
            AccuracyClass::AntiAir => 0.70,
            AccuracyClass::Armored => 0.50,
            AccuracyClass::Standard => 1.0,
        }
    }
}

/// Concentrated-volley pool figures (§6.3 v3.2 concentration).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolInfo {
    pub sum_qg: f32,
    pub sum_g: f32,
    pub members: u8,
}

/// The two firepower regimes. AIMED fire
/// (assault / direct fire / every counter — the direct-lay gate removed
/// long-range counter-battery) squares the numbers — Lanchester: massing
/// aimed fire has a superlinear payoff, splitting it is punished. AREA fire
/// (indirect-artillery fire missions) is LINEAR in numbers — shells are
/// shells, however distributed (the Deitchman area-fire law), so a
/// half-strength battery delivers exactly half the firepower. At full
/// strength (g = 1) the two forms coincide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FirepowerForm {
    #[default]
    Aimed,
    Area,
}

/// The complete formula chain of ONE resolved strike — every
/// number the engagement-detail panel shows, captured at resolution time.
#[derive(Debug, Clone, Copy)]
pub struct HitBreakdown {
    // ── attack quality q ──
    pub soft_attack: f32,
    pub hard_attack: f32,
    pub target_hardness: f32,
    pub piercing_mult: f32,
    pub accuracy: f32,
    pub accuracy_class: AccuracyClass,
    pub q: f32,
    /// g — the firer's strength ratio; numbers enter squared (aimed fire)
    /// or linearly (area fire, see `area_fire`).
    pub strength_ratio: f32,
    /// This strike used the AREA-fire form (P = q·g, indirect
    /// artillery) instead of the aimed-fire square (P = q·g²).
    pub area_fire: bool,
    // ── firepower P ──
    /// Some for a concentrated group volley (§6.3 v3.2).
    pub pool: Option<PoolInfo>,
    /// Lone strike: q·g² (counter-fire: already ÷`counter_split`). Group
    /// volley: the whole pool's P.
    pub p: f32,
    /// Counter-fire split n (1.0 = not a counter).
    pub counter_split: f32,
    // ── defense D ──
    pub uses_breakthrough: bool,
    pub d_base: f32,
    pub d_hold_mult: f32,
    pub d_entrench_mult: f32,
    pub d_terrain_mult: f32,
    pub d: f32,
    pub target_terrain: Terrain,
    // ── hit step ──
    pub hit_base: f32,
    pub hit_saturated: f32,
    pub hit: f32,
    // ── linear modifier stack ──
    pub linear: [(LinearFactor, f32); 10],
    pub linear_len: u8,
    pub linear_total: f32,
    // ── org damage ──
    pub damage_scale: f32,
    /// damage_scale × P × hit × linear — before jitter and the hard cap
    /// (a group volley: the pool's raw total).
    pub org_raw: f32,
    /// 1.0 when `random_spread` is off (the default).
    pub jitter_mult: f32,
    pub org_cap: f32,
    pub org_capped: bool,
    /// After jitter + cap, before area weight / pool share (a group volley:
    /// the pool's capped total).
    pub org_pool_final: f32,
    /// Area-fire hex weight (1.0 = precise / not a fire mission).
    pub area_weight: f32,
    /// This lane's share of a group volley (1.0 = lone strike).
    pub pool_share: f32,
    /// Actually delivered by this lane (what the report line shows).
    pub org_final: f32,
    // ── strength damage ──
    pub str_rate: f32,
    pub str_rate_broken: bool,
    pub max_strength: f32,
    pub max_org: f32,
    pub str_final: f32,
}

impl Default for HitBreakdown {
    fn default() -> Self {
        HitBreakdown {
            soft_attack: 0.0,
            hard_attack: 0.0,
            target_hardness: 0.0,
            piercing_mult: 1.0,
            accuracy: 1.0,
            accuracy_class: AccuracyClass::Standard,
            q: 0.0,
            strength_ratio: 0.0,
            area_fire: false,
            pool: None,
            p: 0.0,
            counter_split: 1.0,
            uses_breakthrough: false,
            d_base: 0.0,
            d_hold_mult: 1.0,
            d_entrench_mult: 1.0,
            d_terrain_mult: 1.0,
            d: 0.0,
            target_terrain: Terrain::Plains,
            hit_base: 0.0,
            hit_saturated: 0.0,
            hit: 0.0,
            linear: [(LinearFactor::Cover, 1.0); 10],
            linear_len: 0,
            linear_total: 1.0,
            damage_scale: 1.0,
            org_raw: 0.0,
            jitter_mult: 1.0,
            org_cap: 0.0,
            org_capped: false,
            org_pool_final: 0.0,
            area_weight: 1.0,
            pool_share: 1.0,
            org_final: 0.0,
            str_rate: 0.0,
            str_rate_broken: false,
            max_strength: 0.0,
            max_org: 0.0,
            str_final: 0.0,
        }
    }
}

impl HitBreakdown {
    /// Push one row onto the linear stack (the stack cap is generous; the
    /// real stack never exceeds 7 rows).
    pub fn push_linear(&mut self, kind: LinearFactor, value: f32) {
        let n = self.linear_len as usize;
        if n < self.linear.len() {
            self.linear[n] = (kind, value);
            self.linear_len += 1;
        }
    }

    /// Area-fire hex weight (§6.3): the aim hex takes
    /// `area_center_share`, each neighbour `area_neighbor_share`. Org and
    /// strength both scale (strength is linear in org).
    pub fn apply_area_weight(&mut self, w: f32) {
        self.area_weight = w;
        self.org_final *= w;
        self.str_final *= w;
    }
}

/// Attack quality of one battalion against one target (§6.3): the vanilla
/// hardness-weighted soft/hard mix with the piercing tier multiplying the
/// HARD component, × the class precision factor. Strength is NOT part of
/// quality — numbers enter squared via [`firepower`].
pub fn attack_quality(attacker: &BattalionUnit, target: &BattalionUnit) -> f32 {
    let mut bd = HitBreakdown::default();
    attack_quality_explained(attacker, target, &mut bd)
}

/// [`attack_quality`] with the q-row capture.
pub fn attack_quality_explained(
    attacker: &BattalionUnit,
    target: &BattalionUnit,
    bd: &mut HitBreakdown,
) -> f32 {
    let h = target.hardness.clamp(0.0, 1.0);
    let piercing = piercing_multiplier(attacker.piercing, target.armor);
    let class = AccuracyClass::of(attacker);
    let soft = attacker.soft_attack * (1.0 - h);
    let hard = attacker.hard_attack * h * piercing;
    let q = (soft + hard) * class.factor();
    bd.soft_attack = attacker.soft_attack;
    bd.hard_attack = attacker.hard_attack;
    bd.target_hardness = h;
    bd.piercing_mult = piercing;
    bd.accuracy = class.factor();
    bd.accuracy_class = class;
    bd.q = q;
    q
}

/// Lone-strike firepower: quality × effective guns squared (§6.3).
pub fn firepower(q: f32, g: f32) -> f32 {
    q * g * g
}

/// The vanilla hit step, soft form: `hit_base` when P ≪ D, `hit_saturated`
/// when P ≫ D, the midpoint at P = D. Never divides by D alone, so a
/// breakthrough-0 target simply takes the saturated rate.
pub fn hit_fraction(p: f32, defense: f32, params: &CombatParams) -> f32 {
    if p <= 0.0 {
        return 0.0;
    }
    params.hit_base + (params.hit_saturated - params.hit_base) * p / (p + defense.max(0.0))
}

/// The defense side of the step: effective defense (Hold/entrenchment) or,
/// for a unit caught mid-attack, raw breakthrough — × the battalion's own
/// terrain defense adjuster on its hex (v3.3: the only terrain term left
/// on D; the global terrain defense column is retired).
pub fn defense_value(
    target: &BattalionUnit,
    target_uses_breakthrough: bool,
    terrain: Terrain,
    params: &CombatParams,
) -> f32 {
    let mut bd = HitBreakdown::default();
    defense_value_explained(target, target_uses_breakthrough, terrain, params, &mut bd)
}

/// [`defense_value`] with the D-row capture: base / Hold stance /
/// entrenchment / battalion terrain adjuster as separate rows.
pub fn defense_value_explained(
    target: &BattalionUnit,
    target_uses_breakthrough: bool,
    terrain: Terrain,
    params: &CombatParams,
    bd: &mut HitBreakdown,
) -> f32 {
    let (base, hold, entrench) = if target_uses_breakthrough {
        // Caught mid-attack: raw breakthrough, no stance/entrenchment (the
        // HOI4 convention — see `strike_split` in tactical-combat).
        (target.breakthrough, 1.0, 1.0)
    } else {
        (
            target.defense,
            if target.is_holding {
                1.0 + params.hold_defense_bonus
            } else {
                1.0
            },
            1.0 + target.entrenchment as f32 * params.entrench_defense_per_layer,
        )
    };
    let terrain_mult =
        (1.0 + params.terrain_modifier_scale * target.terrain_adj.defense_on(terrain)).max(0.0);
    let d = (base * hold * entrench * terrain_mult).max(0.0);
    bd.uses_breakthrough = target_uses_breakthrough;
    bd.d_base = base;
    bd.d_hold_mult = hold;
    bd.d_entrench_mult = entrench;
    bd.d_terrain_mult = terrain_mult;
    bd.d = d;
    bd.target_terrain = terrain;
    d
}

/// The linear modifier stack of one strike (§6.3/§6.6/§6.13) — everything
/// that must NOT enter P: command aura, the firer's terrain attack
/// adjuster, terrain cover, direct-fire falloff past adjacency, melee
/// elevation / indirect crest. (Piercing is no longer here — it multiplies
/// the hard component of [`attack_quality`]. v3.3: the uniform terrain
/// attack column and the river ×2 ford rule are retired — cover −0.50 on
/// River hexes carries the ford vulnerability now.)
pub fn linear_factors(
    attacker: &BattalionUnit,
    target: &BattalionUnit,
    distance: i32,
    attacker_in_command: bool,
    target_in_command: bool,
    grid: &HexGrid,
    params: &CombatParams,
) -> f32 {
    let mut bd = HitBreakdown::default();
    linear_factors_explained(
        attacker,
        target,
        distance,
        attacker_in_command,
        target_in_command,
        grid,
        params,
        &mut bd,
    )
}

/// [`linear_factors`] with the per-row capture. Row policy:
/// `TerrainAttack` and `Cover` are recorded even when neutral (the v3.3
/// terrain layer is the teaching point — the panel greys ×1.00 rows);
/// command rows only when a side is in command; falloff/melee/crest only
/// when they actually bite (≠ 1.0).
pub fn linear_factors_explained(
    attacker: &BattalionUnit,
    target: &BattalionUnit,
    distance: i32,
    attacker_in_command: bool,
    target_in_command: bool,
    grid: &HexGrid,
    params: &CombatParams,
    bd: &mut HitBreakdown,
) -> f32 {
    let def_terrain = grid
        .cell(target.position)
        .map(|c| c.terrain)
        .unwrap_or(Terrain::Plains);
    let mut m = 1.0;
    // §6.13: command aura stays linear so the bonus really is
    // ±10% (a factor inside P would warp the hit step).
    if attacker_in_command {
        let v = 1.0 + params.hq_combat_bonus;
        bd.push_linear(LinearFactor::CommandAura, v);
        m *= v;
    }
    if target_in_command {
        let v = 1.0 - params.hq_combat_bonus;
        bd.push_linear(LinearFactor::TargetCommand, v);
        m *= v;
    }
    // §6.6 v3.3: the firer's own terrain attack adjuster against
    // the target's ground, floored at 0 (a −0.40 urban armor adjuster must
    // never flip damage negative). Vanilla data, scaled by the global dial.
    let v = (1.0 + params.terrain_modifier_scale * attacker.terrain_adj.attack_on(def_terrain))
        .max(0.0);
    bd.push_linear(LinearFactor::TerrainAttack, v);
    m *= v;
    // Direct fire loses punch past adjacency; plunging (indirect) fire does
    // not — shell density is range-independent on a ballistic arc.
    if distance > 1 && !attacker.is_indirect_artillery() {
        let v = params.direct_fire_falloff;
        bd.push_linear(LinearFactor::DirectFireFalloff, v);
        m *= v;
    }
    // §6.6 crest rules: MELEE fights (distance ≤ 1) are
    // decided by the height difference; beyond contact, INDIRECT fire reads
    // the target's own step (exposed ×1.5, reverse slope ×0.5).
    let v = melee_elevation_mult(
        grid,
        attacker.position,
        target.position,
        distance,
        params.melee_elevation_gain,
        params.melee_elevation_cap,
    );
    if (v - 1.0).abs() > 1e-6 {
        bd.push_linear(LinearFactor::MeleeElevation, v);
    }
    m *= v;
    if distance > 1 && attacker.is_indirect_artillery() {
        let v = indirect_crest_mult(
            grid,
            attacker.position,
            target.position,
            params.exposed_crest_mult,
            params.defilade_mult,
        );
        if (v - 1.0).abs() > 1e-6 {
            bd.push_linear(LinearFactor::IndirectCrest, v);
        }
        m *= v;
    }
    let v = 1.0 - def_terrain.cover_percent();
    bd.push_linear(LinearFactor::Cover, v);
    m *= v;
    bd.linear_total = m;
    m
}

/// Core resolution shared by every strike path:
/// org = damage_scale × P × hit(P, D) × linear.
pub fn resolve_org(p: f32, defense: f32, linear: f32, params: &CombatParams) -> f32 {
    if p <= 0.0 {
        return 0.0;
    }
    p * hit_fraction(p, defense, params) * params.damage_scale * linear.max(0.0)
}

/// Expected org damage of ONE strike, pre-cap (the resolver applies the
/// optional jitter and the `org_cap_ratio` cap at delivery; with the
/// default `random_spread` of 0 this value is exactly what is delivered).
/// `form` selects the firepower regime: Aimed = q·g², Area = q·g.
pub fn strike_org_damage(
    attacker: &BattalionUnit,
    target: &BattalionUnit,
    target_uses_breakthrough: bool,
    distance: i32,
    attacker_in_command: bool,
    target_in_command: bool,
    grid: &HexGrid,
    params: &CombatParams,
    form: FirepowerForm,
) -> f32 {
    let mut bd = HitBreakdown::default();
    strike_org_damage_explained(
        attacker,
        target,
        target_uses_breakthrough,
        distance,
        attacker_in_command,
        target_in_command,
        grid,
        params,
        1.0,
        form,
        &mut bd,
    )
}

/// [`strike_org_damage`] with the full formula-chain capture.
/// `firepower_divisor` splits P linearly (the §6.3 counter-fire pool ÷n;
/// 1.0 for an ordinary strike). Fills every row EXCEPT the delivery-side
/// ones the resolver owns (jitter, the hard cap, area weight, pool share,
/// `org_final`) — those ride the resolver's RNG and cap order.
pub fn strike_org_damage_explained(
    attacker: &BattalionUnit,
    target: &BattalionUnit,
    target_uses_breakthrough: bool,
    distance: i32,
    attacker_in_command: bool,
    target_in_command: bool,
    grid: &HexGrid,
    params: &CombatParams,
    firepower_divisor: f32,
    form: FirepowerForm,
    bd: &mut HitBreakdown,
) -> f32 {
    let q = attack_quality_explained(attacker, target, bd);
    let g = attacker.strength_ratio();
    let divisor = firepower_divisor.max(1.0);
    // Aimed fire squares the numbers, area fire is linear.
    let p = match form {
        FirepowerForm::Aimed => firepower(q, g),
        FirepowerForm::Area => q * g,
    } / divisor;
    bd.strength_ratio = g;
    bd.area_fire = form == FirepowerForm::Area;
    bd.p = p;
    bd.counter_split = divisor;
    if p <= 0.0 {
        bd.d = 0.0;
        bd.org_raw = 0.0;
        return 0.0;
    }
    let terrain = grid
        .cell(target.position)
        .map(|c| c.terrain)
        .unwrap_or(Terrain::Plains);
    let defense = defense_value_explained(target, target_uses_breakthrough, terrain, params, bd);
    let linear = linear_factors_explained(
        attacker,
        target,
        distance,
        attacker_in_command,
        target_in_command,
        grid,
        params,
        bd,
    );
    bd.hit_base = params.hit_base;
    bd.hit_saturated = params.hit_saturated;
    bd.hit = hit_fraction(p, defense, params);
    bd.damage_scale = params.damage_scale;
    let org = resolve_org(p, defense, linear, params);
    bd.org_raw = org;
    org
}

/// Pooled-assault org estimate: the same math tactical-combat's
/// `strike_group` resolves — P = Σ(q_i·g_i) × Σg_i (quality linear, numbers
/// squared), with the linear stack contribution-weighted per member (melee
/// elevation, terrain attack adjuster) times the shared cover row. A
/// singleton pool is numerically identical to a lone [`strike_org_damage`]
/// (q·g × g = q·g²). Deliberate plan-side gaps mirror the solo estimate's:
/// no command aura (planning has no links) and no jitter (expectation);
/// assault pools are all-melee, so the direct-fire falloff row is 1.
/// Uncapped pre-delivery value, same convention as [`strike_org_damage`].
pub fn strike_group_org_estimate(
    attackers: &[&BattalionUnit],
    target: &BattalionUnit,
    grid: &HexGrid,
    params: &CombatParams,
) -> f32 {
    let def_terrain = grid
        .cell(target.position)
        .map(|c| c.terrain)
        .unwrap_or(Terrain::Plains);
    let mut sum_qg = 0.0;
    let mut sum_g = 0.0;
    let mut melee_weighted = 0.0;
    let mut adj_weighted = 0.0;
    for a in attackers {
        // The attacker's contribution unit: quality × its own guns.
        let w = attack_quality(a, target) * a.strength_ratio();
        sum_qg += w;
        sum_g += a.strength_ratio();
        melee_weighted += w
            * melee_elevation_mult(
                grid,
                a.position,
                target.position,
                1,
                params.melee_elevation_gain,
                params.melee_elevation_cap,
            );
        adj_weighted += w
            * (1.0 + params.terrain_modifier_scale * a.terrain_adj.attack_on(def_terrain))
                .max(0.0);
    }
    if sum_qg <= 0.0 || sum_g <= 0.0 {
        return 0.0;
    }
    let p = sum_qg * sum_g;
    let defense = defense_value(target, false, def_terrain, params);
    let linear =
        (melee_weighted / sum_qg) * (adj_weighted / sum_qg) * (1.0 - def_terrain.cover_percent());
    resolve_org(p, defense, linear, params)
}

/// org → strength conversion (§6.3 v3.2): a full break costs
/// `break_str_loss` of the target's max strength, whatever its class —
/// so org damage converts at the pool-shaped rate
/// break_str_loss × max_str/max_org. A target whose org is ALREADY 0 at
/// the start of the volley converts at `broken_str_loss` instead: with no
/// organization left, the fire lands on manpower/equipment (0.68 = the
/// vanilla per-hit dice ratio). The rate is judged per volley at volley
/// start — callers pass the pre-delivery target, so a unit broken by the
/// first shares of a group volley still converts the whole volley at the
/// unbroken rate.
pub fn strength_damage(org: f32, target: &BattalionUnit, params: &CombatParams) -> f32 {
    let mut bd = HitBreakdown::default();
    strength_damage_explained(org, target, params, &mut bd)
}

/// [`strength_damage`] with the strength-row capture.
pub fn strength_damage_explained(
    org: f32,
    target: &BattalionUnit,
    params: &CombatParams,
    bd: &mut HitBreakdown,
) -> f32 {
    bd.max_strength = target.max_strength;
    bd.max_org = target.max_org;
    if org <= 0.0 || target.max_org <= 0.0 {
        bd.str_final = 0.0;
        return 0.0;
    }
    let broken = target.org <= 0.0;
    let rate = if broken {
        params.broken_str_loss
    } else {
        params.break_str_loss
    };
    let dmg = org * rate * (target.max_strength / target.max_org);
    bd.str_rate = rate;
    bd.str_rate_broken = broken;
    bd.str_final = dmg;
    dmg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HexCoord, Side, UnitType};

    fn attacker() -> BattalionUnit {
        let mut u = BattalionUnit::new(
            0,
            "a".to_string(),
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(0, 0),
        );
        u.soft_attack = 10.0;
        u.hard_attack = 10.0;
        u.piercing = 10.0;
        u.defense = 20.0;
        u.breakthrough = 20.0;
        u
    }

    fn target() -> BattalionUnit {
        let mut u = BattalionUnit::new(
            1,
            "d".to_string(),
            UnitType::Infantry,
            Side::Defender,
            HexCoord::new(1, 0),
        );
        u.soft_attack = 5.0;
        u.hard_attack = 5.0;
        u.piercing = 5.0;
        u.defense = 15.0;
        u.breakthrough = 15.0;
        u
    }

    #[test]
    fn full_strength_baseline_matches_hit_step_form() {
        // Plains, adjacent, no command: q = 10 (soft, target hardness 0),
        // P = 10×1², D = 15, hit = 0.1 + 0.3×10/(10+15) = 0.22 → org = 2.2.
        let grid = HexGrid::new(3, 3, Terrain::Plains);
        let params = CombatParams::default();
        let a = attacker();
        let d = target();
        let expect = 10.0 * 0.22;
        let got = strike_org_damage(&a, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        assert!((got - expect).abs() < 1e-4, "got {got}, want {expect}");
    }

    #[test]
    fn group_estimate_singleton_matches_lone_strike() {
        // By construction, a singleton "pool" is numerically the
        // lone aimed strike (q·g × g = q·g², same linear stack).
        let grid = HexGrid::new(3, 3, Terrain::Plains);
        let params = CombatParams::default();
        let a = attacker();
        let d = target();
        let solo =
            strike_org_damage(&a, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        let pooled = strike_group_org_estimate(&[&a], &d, &grid, &params);
        assert!((solo - pooled).abs() < 1e-6, "solo {solo}, pooled {pooled}");
    }

    #[test]
    fn group_estimate_numbers_square() {
        // Two full-strength attackers pool P = (2q)·(2g) = 4qg —
        // numbers square, so the volley beats 2× the lone strike (the hit
        // step climbs with the bigger P on top of the doubled base).
        let grid = HexGrid::new(5, 5, Terrain::Plains);
        let params = CombatParams::default();
        let a = attacker();
        let mut a2 = attacker();
        a2.id = 2;
        a2.position = HexCoord::new(0, 1);
        let d = target();
        let solo =
            strike_org_damage(&a, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        let pooled = strike_group_org_estimate(&[&a, &a2], &d, &grid, &params);
        assert!(
            pooled > solo * 2.0,
            "pooled {pooled} should beat 2× solo {solo}"
        );
    }

    #[test]
    fn strength_ratio_scales_the_attacker_side() {
        // Regression: the estimate scales by the ATTACKER's
        // strength, never the target's. Numbers enter squared: halving the
        // attacker quarters its firepower P (and the hit step slides down
        // with the smaller P, so the delivered ratio is even below 1/4).
        let grid = HexGrid::new(3, 3, Terrain::Plains);
        let params = CombatParams::default();
        let a = attacker();
        let d = target();
        let base = strike_org_damage(&a, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);

        let mut weak_attacker = a.clone();
        weak_attacker.strength *= 0.5;
        let halved =
            strike_org_damage(&weak_attacker, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        // P = 10×0.5² = 2.5, hit = 0.1+0.3×2.5/17.5 ≈ 0.14286 → 0.3571.
        let expect = 2.5 * (0.1 + 0.3 * 2.5 / 17.5);
        assert!((halved - expect).abs() < 1e-4, "got {halved}, want {expect}");
        assert!(halved < base * 0.25, "numbers squared then some");

        let mut weak_target = d.clone();
        weak_target.strength *= 0.5;
        let unchanged =
            strike_org_damage(&a, &weak_target, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        assert!(
            (unchanged - base).abs() < 1e-4,
            "target strength must not scale damage"
        );
    }

    #[test]
    fn piercing_scales_only_the_hard_component() {
        // vs an armored, half-hard target: q = soft×0.5 + hard×0.5×tier.
        let mut d = target();
        d.armor = 10.0;
        d.hardness = 0.5;
        let q = |piercing: f32| {
            let mut aa = attacker();
            aa.piercing = piercing;
            attack_quality(&aa, &d)
        };
        // soft 10×0.5 = 5.0; hard 10×0.5×tier.
        assert!((q(10.0) - (5.0 + 5.0)).abs() < 1e-4);
        assert!((q(8.0) - (5.0 + 4.0)).abs() < 1e-4);
        assert!((q(6.0) - (5.0 + 3.25)).abs() < 1e-4);
        assert!((q(4.0) - (5.0 + 2.5)).abs() < 1e-4);
        // Soft targets: piercing is irrelevant (armor 0 → tier 1.0, and the
        // hard component is weighted by hardness 0 anyway).
        let soft_target = target();
        assert!((q(4.0) - attack_quality(&attacker(), &soft_target)).abs() > 1e-4);
        assert!((attack_quality(&attacker(), &soft_target) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn hit_step_asymptotes_and_zero_guards() {
        let params = CombatParams::default();
        // D = 0 (breakthrough-0 attacker caught mid-attack) → saturated.
        assert!((hit_fraction(10.0, 0.0, &params) - 0.40).abs() < 1e-6);
        // P ≪ D → base rate; P = D → midpoint.
        assert!((hit_fraction(0.01, 100.0, &params) - 0.10).abs() < 1e-3);
        assert!((hit_fraction(20.0, 20.0, &params) - 0.25).abs() < 1e-6);
        // No firepower → no damage, never a NaN.
        assert_eq!(resolve_org(0.0, 0.0, 1.0, &params), 0.0);
        assert_eq!(hit_fraction(0.0, 0.0, &params), 0.0);
    }

    #[test]
    fn fire_conservation_in_the_pool_form() {
        // §6.3: Σ(q·g) × Σg — two half-strength battalions equal one full
        // one; a lone half-strength strike quarters its firepower.
        let half = (6.0, 0.5);
        let pool = [(6.0, 0.5), (6.0, 0.5)];
        let sum_qg: f32 = pool.iter().map(|(q, g)| q * g).sum();
        let sum_g: f32 = pool.iter().map(|(_, g)| *g).sum();
        assert!((sum_qg * sum_g - 6.0).abs() < 1e-4);
        assert!((firepower(half.0, half.1) - 1.5).abs() < 1e-4);
    }

    #[test]
    fn break_costs_a_fixed_strength_fraction_for_every_class() {
        // λ = 0.12: breaking a 60-org/25-str battalion costs 3.0 strength
        // (60 × 0.12 × 25/60); a 10-org/2-str tank battalion costs 0.24.
        let params = CombatParams::default();
        let d = target(); // infantry fixture: org 60, str 25
        assert!((strength_damage(60.0, &d, &params) - 3.0).abs() < 1e-4);
        let mut t = BattalionUnit::new(
            2,
            "t".to_string(),
            UnitType::MediumArmor,
            Side::Defender,
            HexCoord::new(1, 0),
        );
        t.max_org = 10.0;
        t.max_strength = 2.0;
        assert!((strength_damage(10.0, &t, &params) - 0.24).abs() < 1e-4);
        assert_eq!(strength_damage(0.0, &d, &params), 0.0);
    }

    #[test]
    fn broken_target_converts_at_the_vanilla_dice_ratio() {
        // λ = 0.68 once the target's org is ALREADY 0 at volley start
        // (broken/routing): the same 60-org/25-str battalion loses
        // 60 × 0.68 × 25/60 = 17.0 strength per full org pool of fire.
        let params = CombatParams::default();
        let mut d = target(); // infantry fixture: org 60, str 25
        d.org = 0.0;
        assert!((strength_damage(60.0, &d, &params) - 17.0).abs() < 1e-4);
        // A sliver of org left still converts at the unbroken rate —
        // the switch is judged at volley start, never mid-volley.
        d.org = 0.01;
        assert!((strength_damage(60.0, &d, &params) - 3.0).abs() < 1e-4);
    }

    #[test]
    fn terrain_adjusters_scale_damage_and_defense() {
        // v3.3: the firer's attack adjuster is a standalone
        // linear factor; the absorber's defense adjuster multiplies D.
        // Forest hexes, both fixtures otherwise stock (q = 10, D = 15).
        let grid = HexGrid::new(3, 3, Terrain::Forest);
        let params = CombatParams::default();
        let hit = 0.1 + 0.3 * 10.0 / 25.0; // 0.22, D unmoved

        let mut a = attacker();
        a.terrain_adj.set_hoi4("forest", "attack", -0.2); // e.g. towed guns
        let d = target();
        let got = strike_org_damage(&a, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        // 10 × 0.22 × adjuster 0.8 × cover 0.85 = 1.496.
        assert!((got - 10.0 * hit * 0.8 * 0.85).abs() < 1e-4, "got {got}");

        let mut d2 = target();
        d2.terrain_adj.set_hoi4("forest", "defense", 0.25); // e.g. engineers
        let got = strike_org_damage(&attacker(), &d2, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        // D = 15 × 1.25 = 18.75 → hit = 0.1+0.3×10/28.75; × cover 0.85.
        let hit2 = 0.1 + 0.3 * 10.0 / 28.75;
        assert!((got - 10.0 * hit2 * 0.85).abs() < 1e-4, "got {got}");

        // The defense adjuster also rides the breakthrough side (caught
        // attacker absorbing counter-fire).
        let dv = defense_value(&d2, true, Terrain::Forest, &params);
        assert!((dv - d2.breakthrough * 1.25).abs() < 1e-4, "dv {dv}");
    }

    #[test]
    fn terrain_modifier_scale_zero_disables_adjusters() {
        let grid = HexGrid::new(3, 3, Terrain::Forest);
        let mut params = CombatParams::default();
        params.terrain_modifier_scale = 0.0;
        let mut a = attacker();
        a.terrain_adj.set_hoi4("forest", "attack", -0.2);
        let mut d = target();
        d.terrain_adj.set_hoi4("forest", "defense", 0.25);
        let got = strike_org_damage(&a, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        // Adjusters off: 10 × 0.22 × cover 0.85 = 1.87.
        let hit = 0.1 + 0.3 * 10.0 / 25.0;
        assert!((got - 10.0 * hit * 0.85).abs() < 1e-4, "got {got}");
    }

    #[test]
    fn river_hex_negative_cover_exposes_the_forded() {
        // v3.3: the ×2 ford rule is retired — River cover −0.50 multiplies
        // damage taken by exactly 1.5 (same fixtures as the plains base).
        // `set_terrain` also sinks the river bed to elevation −1, so the
        // bank-side attacker fires DOWN: melee elevation ×1.15 on top.
        let mut grid = HexGrid::new(3, 3, Terrain::Plains);
        grid.set_terrain(HexCoord::new(1, 0), Terrain::River);
        let params = CombatParams::default();
        let got = strike_org_damage(&attacker(), &target(), false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        let hit = 0.1 + 0.3 * 10.0 / 25.0;
        assert!((got - 10.0 * hit * 1.5 * 1.15).abs() < 1e-4, "got {got}");
    }

    #[test]
    fn attack_adjuster_bucket_floored_at_zero() {
        // A forged −1.5 adjuster must zero the strike, never heal the target.
        let grid = HexGrid::new(3, 3, Terrain::Urban);
        let params = CombatParams::default();
        let mut a = attacker();
        a.terrain_adj.set_hoi4("urban", "attack", -1.5);
        let got = strike_org_damage(&a, &target(), false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        assert_eq!(got, 0.0);
        // Vanilla's worst real case (armor −0.40 urban) stays positive.
        let mut a2 = attacker();
        a2.terrain_adj.set_hoi4("urban", "attack", -0.4);
        let got2 = strike_org_damage(&a2, &target(), false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        assert!(got2 > 0.0, "got {got2}");
    }

    // ── Formula-chain capture ──

    #[test]
    fn explained_matches_plain_and_fills_the_chain() {
        // Plains baseline: q = 10, P = 10, D = 15, hit = 0.22, org = 2.2.
        let grid = HexGrid::new(3, 3, Terrain::Plains);
        let params = CombatParams::default();
        let a = attacker();
        let d = target();
        let plain = strike_org_damage(&a, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        let mut bd = HitBreakdown::default();
        let explained = strike_org_damage_explained(
            &a, &d, false, 1, false, false, &grid, &params, 1.0, FirepowerForm::Aimed, &mut bd,
        );
        assert!((plain - explained).abs() < 1e-6);
        assert!((bd.soft_attack - 10.0).abs() < 1e-4);
        assert!((bd.q - 10.0).abs() < 1e-4);
        assert!((bd.strength_ratio - 1.0).abs() < 1e-6);
        assert!((bd.p - 10.0).abs() < 1e-4);
        assert!((bd.counter_split - 1.0).abs() < 1e-6);
        assert!((bd.d_base - 15.0).abs() < 1e-4);
        assert!((bd.d - 15.0).abs() < 1e-4);
        assert!((bd.hit - 0.22).abs() < 1e-4);
        assert!((bd.org_raw - 2.2).abs() < 1e-4);
        assert_eq!(bd.target_terrain, Terrain::Plains);
        // Row policy: TerrainAttack and Cover recorded even when neutral;
        // no command/falloff/elevation rows on this fixture.
        let rows = &bd.linear[..bd.linear_len as usize];
        let kinds: Vec<LinearFactor> = rows.iter().map(|e| e.0).collect();
        assert_eq!(
            kinds,
            vec![LinearFactor::TerrainAttack, LinearFactor::Cover]
        );
        assert!((bd.linear_total - 1.0).abs() < 1e-6);
        // Every wrapper agrees with its explained twin.
        assert!((attack_quality(&a, &d) - bd.q).abs() < 1e-6);
        assert!((defense_value(&d, false, Terrain::Plains, &params) - bd.d).abs() < 1e-6);
        assert!(
            (linear_factors(&a, &d, 1, false, false, &grid, &params) - bd.linear_total).abs() < 1e-6
        );
    }

    #[test]
    fn defense_explained_decomposes_hold_and_entrenchment() {
        let params = CombatParams::default();
        let mut d = target();
        d.is_holding = true;
        d.entrenchment = 2;
        let mut bd = HitBreakdown::default();
        let got = defense_value_explained(&d, false, Terrain::Plains, &params, &mut bd);
        let hold = 1.0 + params.hold_defense_bonus;
        let entrench = 1.0 + 2.0 * params.entrench_defense_per_layer;
        let expect = 15.0 * hold * entrench;
        assert!((got - expect).abs() < 1e-4, "got {got}, want {expect}");
        assert!((bd.d_base - 15.0).abs() < 1e-4);
        assert!((bd.d_hold_mult - hold).abs() < 1e-6);
        assert!((bd.d_entrench_mult - entrench).abs() < 1e-6);
        assert!((bd.d_terrain_mult - 1.0).abs() < 1e-6);
        // Breakthrough side: stance and entrenchment do not apply.
        let mut bd2 = HitBreakdown::default();
        let got2 = defense_value_explained(&d, true, Terrain::Plains, &params, &mut bd2);
        assert!((got2 - d.breakthrough).abs() < 1e-4);
        assert!(bd2.uses_breakthrough);
        assert!((bd2.d_hold_mult - 1.0).abs() < 1e-6);
        assert!((bd2.d_entrench_mult - 1.0).abs() < 1e-6);
    }

    #[test]
    fn counter_divisor_splits_firepower_linearly() {
        // §6.3: the counter pool splits P ÷n — divisor 2 halves P, and the
        // hit step slides down with the smaller P.
        let grid = HexGrid::new(3, 3, Terrain::Plains);
        let params = CombatParams::default();
        let a = attacker();
        let d = target();
        let mut bd = HitBreakdown::default();
        let got = strike_org_damage_explained(
            &a, &d, true, 1, false, false, &grid, &params, 2.0, FirepowerForm::Aimed, &mut bd,
        );
        assert!((bd.p - 5.0).abs() < 1e-4);
        assert!((bd.counter_split - 2.0).abs() < 1e-6);
        assert!(bd.uses_breakthrough);
        let hit = 0.1 + 0.3 * 5.0 / (5.0 + 15.0);
        assert!((got - 5.0 * hit).abs() < 1e-4, "got {got}");
    }

    #[test]
    fn strength_explained_rows_and_area_weight() {
        let params = CombatParams::default();
        let d = target(); // org 60 / str 25
        let mut bd = HitBreakdown::default();
        let got = strength_damage_explained(60.0, &d, &params, &mut bd);
        assert!((got - 3.0).abs() < 1e-4);
        assert!((bd.str_rate - params.break_str_loss).abs() < 1e-6);
        assert!(!bd.str_rate_broken);
        assert!((bd.max_strength - 25.0).abs() < 1e-4);
        assert!((bd.max_org - 60.0).abs() < 1e-4);
        // Area weight scales both delivered numbers (org-linear strength).
        bd.org_final = got;
        bd.str_final = got;
        bd.apply_area_weight(0.4);
        assert!((bd.org_final - 1.2).abs() < 1e-4);
        assert!((bd.str_final - 1.2).abs() < 1e-4);
        assert!((bd.area_weight - 0.4).abs() < 1e-6);
    }

    #[test]
    fn area_fire_is_linear_in_numbers() {
        // Area fire P = q·g — a half-strength battery delivers
        // exactly half the shells (no Lanchester square); at full strength
        // the two forms coincide. Halved attacker (q = 10, g = 0.5):
        // aimed P = 2.5 vs area P = 5.0.
        let grid = HexGrid::new(3, 3, Terrain::Plains);
        let params = CombatParams::default();
        let d = target();
        let mut half = attacker();
        half.strength *= 0.5;
        let aimed = strike_org_damage(&half, &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        let mut bd = HitBreakdown::default();
        let area = strike_org_damage_explained(
            &half,
            &d,
            false,
            1,
            false,
            false,
            &grid,
            &params,
            1.0,
            FirepowerForm::Area,
            &mut bd,
        );
        let expect_area = 5.0 * (0.1 + 0.3 * 5.0 / 20.0); // P=5, D=15 → hit 0.175
        assert!((area - expect_area).abs() < 1e-4, "got {area}");
        assert!(bd.area_fire);
        assert!((bd.p - 5.0).abs() < 1e-4);
        assert!(area > aimed * 2.0, "linear out-delivers squared when depleted");
        // Full strength: identical.
        let full_aimed =
            strike_org_damage(&attacker(), &d, false, 1, false, false, &grid, &params, FirepowerForm::Aimed);
        let full_area =
            strike_org_damage(&attacker(), &d, false, 1, false, false, &grid, &params, FirepowerForm::Area);
        assert!((full_aimed - full_area).abs() < 1e-6);
    }
}
