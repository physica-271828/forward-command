//! Combat parameters — DESIGN §12.2 (hardcoded tuning table) + §6.3 constants.

#[derive(Debug, Clone)]
pub struct CombatParams {
    // §6.3 strike model (reworked — supersedes the earlier
    // Lanchester-denominator form): attack QUALITY enters linearly,
    // NUMBERS enter squared (lone strike P = q×g²; a concentrated pool is
    // P = Σ(q_i·g_i) × Σg_i), and defense gates the vanilla hit step
    // (00_defines.lua: 10% defended / 40% saturated) instead of dividing —
    // no denominator, so breakthrough 0 is safe and the attack/defense
    // asymmetry stays mild.
    pub hit_base: f32,      // 0.10 — defended hit rate (vanilla BASE_CHANCE_TO_AVOID_HIT = 90)
    pub hit_saturated: f32, // 0.40 — saturated hit rate (vanilla CHANCE_TO_AVOID_HIT_AT_NO_DEF = 60)
    pub damage_scale: f32,  // 1.0 — global lethality K₂ (1.0 ≈ an infantry duel breaking in ~10 h)
    /// Global dial on every per-battalion terrain adjuster
    /// (§6.6 v3.3): multiplies both the firing-side attack adjuster and the
    /// absorbing-side defense adjuster. 1.0 = vanilla values verbatim,
    /// 0.0 switches battalion terrain identity off entirely.
    pub terrain_modifier_scale: f32, // 1.0
    /// Strength damage per delivered org damage: org × break_str_loss ×
    /// (max_str/max_org) — a full break (max_org cumulative org damage)
    /// costs this fraction of max strength for EVERY battalion class
    /// (0.12 = the vanilla division-scale break cost, 00_defines.lua dice).
    pub break_str_loss: f32, // 0.12
    /// Strength conversion rate against a target whose org is ALREADY 0 at
    /// the start of the volley (broken/routing — there is no organization
    /// left to absorb the fire, so it lands on manpower/equipment). 0.68 =
    /// the vanilla per-hit dice ratio (org 1d4×0.053 vs str 1d2×0.060) that
    /// `break_str_loss` deliberately undercuts for division-scale pacing;
    /// that pacing argument dies with the target's organization.
    /// Judged per volley at volley start, never mid-volley.
    pub broken_str_loss: f32, // 0.68
    pub org_cap_ratio: f32,         // 0.40 — hard cap: one org hit ≤ max_org × this
    pub shock_threshold_ratio: f32, // 0.25 — one phase's aggregate ≥ max_org × this → Shocked
    pub direct_fire_falloff: f32,   // 0.60 — direct-fire multiplier beyond range 1
    pub random_spread: f32,         // 0.0 — deterministic resolution (v3.2); jitter mechanism kept
    /// §6.3 rocket salvo reload: after a rocket fire mission
    /// resolves, the launcher may not fire again for this many turns
    /// (fire end of turn N → next allowed on turn N + this).
    pub rocket_fire_cooldown_turns: i32, // 3
    /// §6.3 counter-fire gate for indirect artillery: no radar-less
    /// counter-battery — a tube-artillery
    /// defender replies only when the attacker is within this direct-lay
    /// self-defense circle (guns over open sights); rocket launchers never
    /// counter (unguided saturation has no direct-lay mode). Every surviving
    /// counter is therefore AIMED fire (square law) by construction.
    pub counter_direct_lay_range: i32, // 2

    // §6.4 encirclement (redesigned: Semi/Flanked merged, damage bonus
    // retired — multi-directional attacks are already rewarded
    // by the Lanchester concentration itself; attrition retuned to the
    // 10-minute turn, §8.1).
    pub partial_encircle_org_attrition: f32, // 0.025
    pub full_encircle_org_attrition: f32,    // 0.05

    // §6.5 ZOC — surcharges zeroed for a feel trial (enemy-adjacent
    // maneuver felt too punishing); mechanism kept, re-enable here.
    pub zoc_entry_ap_cost: f32,  // +1 when enabled
    pub zoc_to_zoc_ap_cost: f32, // +2 when enabled

    // §6.2/§6.9 lane spreading: soft surcharges (effective km) for
    // pathing through another FRIENDLY standing order's route — path hexes
    // cost a little, an order's destination a lot — so sequentially issued
    // orders fan out into parallel lanes instead of an accordion column.
    // Soft (never blocks), so single-file corridors still work.
    pub friendly_path_penalty_km: f32, // 0.5
    pub friendly_dest_penalty_km: f32, // 2.0

    // §6.8
    pub hold_defense_bonus: f32,         // 0.25
    pub entrench_max_layers: u8,         // 3
    pub entrench_defense_per_layer: f32, // 0.10

    // §6.3 fire support: an AREA mission (F-key barrage / intel fire)
    // weighs each zone victim's own strike by its hex — the aim hex takes
    // `area_center_share` (4/10), every one of the 6 neighbours takes
    // `area_neighbor_share` (1/10).
    // Lifted into core so the AI's expected-damage ranking (tactical-ai)
    // uses the SAME weights the combat resolution applies (est=strike
    // invariant). Friends in the zone take their share too
    // (friendly fire — the F-barrage cannot tell friend from foe).
    pub area_center_share: f32,   // 0.40
    pub area_neighbor_share: f32, // 0.10

    // §6.6 crest rules: the slope's bite is dynamic — fixed terrain
    // modifiers were halved and moved here.
    /// Per-elevation-level gain for MELEE (distance ≤ 1) fights: a unit one
    /// level higher deals ×(1 + gain), one level lower ×(1 − gain). The
    /// occupant of the crest dominates the assault from below (峰顶近战占优).
    pub melee_elevation_gain: f32, // 0.15
    /// Clamp on the melee elevation gain (3 levels × 0.15 = 0.45).
    pub melee_elevation_cap: f32, // 0.45
    /// Indirect-fire multiplier on an EXPOSED crest target: its neighbour
    /// toward the gun is LOWER than it, so the whole body sits above the
    /// skyline — the ridge line is the death line under plunging fire.
    pub exposed_crest_mult: f32, // 1.5
    /// Indirect-fire multiplier on a DEFILADED target: its neighbour toward
    /// the gun is HIGHER than it, so the shell's impact angle is throttled
    /// by the ridge shoulder (Korean-war reverse-slope defence: the guns
    /// cannot reach the back of the hill). LOS is NOT checked — the shell
    /// clears the ridge on a high arc; only the target's own step decides.
    pub defilade_mult: f32, // 0.5

    // §8.1
    pub turns_per_strategic_hour: u32, // 6

    // Fog of war
    pub fog_reveal_duration_turns: u32, // 3

    // §6.13 command / division HQ
    /// Command aura radius in hexes (same-division battalions only).
    pub hq_aura_radius: i32,
    /// Fraction of max org regenerated per FULL turn by in-command units
    /// OUT OF CONTACT (an adjacent enemy means the unit is fighting, not
    /// regrouping — the old unconditional 5% made frontline units immortal
    /// at Warsaw; hence 2% + the no-adjacent-enemy condition).
    pub hq_org_regen_frac: f32,
    /// Linear post-step attack/defense bonus for in-command units —
    /// applied outside the firepower/hit-step core so the effect really is ±10%.
    pub hq_combat_bonus: f32,
    /// Fraction of max org every surviving same-division battalion loses
    /// when the HQ is annihilated (strength 0; retreat/surrender don't count).
    pub hq_death_org_frac: f32,
    /// Bonus added to the HQ's aura radius when the HQ hosts a signal
    /// company (§6.13: 3 km base → 6 km with signal).
    pub hq_signal_radius_bonus: i32,

    // §6.11 flag-capture victory
    /// Progress counter cap: 12 turns of zone dominance ≈ 2 strategic
    /// hours (§8.1) before the flag is taken.
    pub flag_progress_cap: i32,
    /// Zone control ratio threshold: attacker:defender unit count inside a
    /// flag zone > this → +1 progress per turn.
    pub flag_capture_ratio: f32,
    /// Below this attacker:defender ratio the attacker LOSES a progress
    /// point per turn (defender > 2:1 in the zone presses it back).
    pub flag_decay_ratio: f32,
    /// Field-flag cluster radius: a flag governs the hexes within this many
    /// hexes of its anchor (≈19 hexes at radius 2), clipped to the defender
    /// zone and passable terrain.
    pub flag_cluster_radius: i32,
    /// Field battles fly this many flags (model A: ALL must be full to
    /// trigger the collapse).
    pub field_flag_count: i32,
    /// Fallback sampling band: the fraction of defender-zone hexes FARTHEST
    /// from the inter-zone boundary the field anchors are sampled from.
    pub flag_deep_core_fraction: f32,

    // §7.4 division orders
    /// Division sensor radius: a division-order plan reacts only to enemies
    /// within this many hexes of its own battalions — an Advance pushes and
    /// searches its OWN front instead of being pulled across the map by
    /// enemies another division detected (R = 10, applied to ALL division
    /// orders, no front/rear direction split).
    /// Far enemies still block pathfinding (they stay occupancy), they just
    /// never become targets.
    pub div_sensor_radius: i32,

    // §6.14 out-of-bounds leaving
    /// Consecutive full turns a unit may END standing on an out-of-bounds
    /// hex before it leaves the battle (org 0, strength frozen, OFFBOARD —
    /// slipped away, not annihilated). 6 turns = 1 strategic hour (§8.1).
    pub oob_leaving_turns: u8,
    /// Soft pathfinding surcharge (effective km) for stepping INTO an
    /// out-of-bounds hex: planned routes (AI and player standing orders)
    /// detour around the ring — at 40 km a shortcut through the 1-px margin
    /// (~7 hexes) never pays. Rout BFS ignores costs, so broken units still
    /// slip away off the edge (§6.14's "劣势避战主动撤出" clause).
    pub oob_step_penalty_km: f32,
}

impl Default for CombatParams {
    fn default() -> Self {
        CombatParams {
            hit_base: 0.10,
            hit_saturated: 0.40,
            damage_scale: 1.0,
            terrain_modifier_scale: 1.0,
            break_str_loss: 0.12,
            broken_str_loss: 0.68,
            org_cap_ratio: 0.40,
            shock_threshold_ratio: 0.25,
            direct_fire_falloff: 0.60,
            random_spread: 0.0,
            partial_encircle_org_attrition: 0.025,
            full_encircle_org_attrition: 0.05,
            zoc_entry_ap_cost: 0.0,
            zoc_to_zoc_ap_cost: 0.0,
            friendly_path_penalty_km: 0.5,
            friendly_dest_penalty_km: 2.0,
            hold_defense_bonus: 0.25,
            rocket_fire_cooldown_turns: 3,
            counter_direct_lay_range: 2,
            entrench_max_layers: 3,
            entrench_defense_per_layer: 0.10,
            area_center_share: 0.40,
            area_neighbor_share: 0.10,
            melee_elevation_gain: 0.15,
            melee_elevation_cap: 0.45,
            exposed_crest_mult: 1.5,
            defilade_mult: 0.5,
            turns_per_strategic_hour: 6,
            fog_reveal_duration_turns: 3,
            hq_aura_radius: 3,
            hq_org_regen_frac: 0.02,
            hq_combat_bonus: 0.10,
            hq_death_org_frac: 0.20,
            hq_signal_radius_bonus: 3,
            flag_progress_cap: 12,
            flag_capture_ratio: 2.0,
            flag_decay_ratio: 0.5,
            flag_cluster_radius: 2,
            field_flag_count: 3,
            flag_deep_core_fraction: 0.40,
            div_sensor_radius: 10,
            oob_leaving_turns: 6,
            oob_step_penalty_km: 40.0,
        }
    }
}

/// Piercing-vs-armor damage multiplier tiers (§6.3).
pub fn piercing_multiplier(piercing: f32, armor: f32) -> f32 {
    if armor <= 0.0 {
        return 1.0;
    }
    let ratio = piercing / armor;
    if ratio >= 1.0 {
        1.0
    } else if ratio >= 0.75 {
        0.80
    } else if ratio >= 0.5 {
        0.65
    } else {
        0.50
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piercing_tiers() {
        assert_eq!(piercing_multiplier(10.0, 10.0), 1.0);
        assert_eq!(piercing_multiplier(8.0, 10.0), 0.80);
        assert_eq!(piercing_multiplier(6.0, 10.0), 0.65);
        assert_eq!(piercing_multiplier(3.0, 10.0), 0.50);
        assert_eq!(piercing_multiplier(1.0, 0.0), 1.0);
    }

    #[test]
    fn defaults_match_design() {
        let p = CombatParams::default();
        assert_eq!(p.turns_per_strategic_hour, 6);
        assert_eq!(p.hit_base, 0.10);
        assert_eq!(p.hit_saturated, 0.40);
        assert_eq!(p.damage_scale, 1.0);
        assert_eq!(p.terrain_modifier_scale, 1.0);
        assert_eq!(p.break_str_loss, 0.12);
        assert_eq!(p.broken_str_loss, 0.68);
        assert_eq!(p.random_spread, 0.0);
        assert_eq!(p.org_cap_ratio, 0.40);
        assert_eq!(p.partial_encircle_org_attrition, 0.025);
        assert_eq!(p.full_encircle_org_attrition, 0.05);
        assert_eq!(p.area_center_share, 0.40);
        assert_eq!(p.area_neighbor_share, 0.10);
        assert_eq!(p.melee_elevation_gain, 0.15);
        assert_eq!(p.exposed_crest_mult, 1.5);
        assert_eq!(p.defilade_mult, 0.5);
    }
}
