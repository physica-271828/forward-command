//! Tactical battalion creation from parsed divisions (DESIGN.md §5.3).
//!
//! Stat pipeline per battalion (all game-data-derived stats are baked in at
//! launch; only in-wargame effects become tactical buffs):
//!
//! ```text
//! soft/hard attack = Σ(equipment stat × apportioned count × tech modifier)
//!                  × (1 + doctrine attack + country attack
//!                      + leader attack + leader per-type attack)
//!                  × (1 - (1 - equipment_ratio) × degradation_factor)
//!                  × experience factor (attack side only)
//! defense/breakthrough = same shape with their own doctrine+country
//!                        +leader factors (no experience)
//! max_org     = template max_organisation × doctrine org factor
//!               × (1 + country org factor) + country flat org
//! org         = max_org × save organization ratio
//!               (absolute-form saves divide by division_maxima, whose org
//!               divisor carries the same doctrine+country factors, so a
//!               full HOI4 division arrives at ratio 1.0)
//! max_strength= template max_strength
//! strength    = × min(save strength ratio, equipment fill)
//! armor       = max equipment armor
//! piercing    = equipment-count weighted average
//! hardness    = equipment-count weighted average
//! speed_kmh   = battalion-type table (§6.1)
//! ```
//!
//! Equipment is division-level in the save, so it is apportioned per
//! battalion in proportion to the battalion's template `needs` (§5.3):
//! `alloc_e = available_e × need_battalion_e / total_division_need_e`.
//!
//! Tech modifier note: no tech-modifier table is part of the pre-extracted
//! data (§5.4), and the researched tech level is already reflected by which
//! equipment variant (`infantry_equipment_0..3`) appears in the save — so the
//! tech modifier is 1.0 here.

use std::collections::HashMap;

use tactical_core::{BattalionUnit, HexCoord, Side, UnitType};

use crate::mapping::{
    canonical_template_key, fallback_equipment_archetype, fallback_equipment_target,
    map_support_kind, UnitNaming,
};
use crate::model::{BattalionInfo, CountryData, DivisionData, LeaderData, SaveGame};
use crate::tables::{
    DoctrineTable, EquipmentTable, ModifierTable, UnitTemplateStats, UnitTemplateTable,
};

/// §5.3 degradation factor: at 0% equipment fill a battalion still fights at
/// 50% of its equipment-derived stats (people fight, not gear).
pub const DEFAULT_DEGRADATION_FACTOR: f32 = 0.5;

/// HOI4 division experience → soft/hard attack multiplier.
///
/// Vanilla (`00_defines.lua`): level thresholds `UNIT_EXP_LEVELS` =
/// {0.1, 0.3, 0.75, 0.9} over the 0..1 experience fraction, effect =
/// (level − 1) × `EXPERIENCE_COMBAT_FACTOR` (0.25) → Green −25%, Trained
/// 0%, Regular +25%, Seasoned +50%, Veteran +75%. HOI4 applies it as a
/// damage-dealt modifier, so it multiplies soft/hard attack only (never
/// defense/breakthrough).
///
/// Input forms: 1.19 saves serialize POINTS (fraction × 10000 — Trained
/// 0.1 = 1000; verified against ITA saves: 801.22 → 0.0801 Green); legacy
/// synthetic saves carry the 0..1 fraction directly (same ≤1 disambiguation
/// idiom as the org/strength fields). Negative = field absent (synthetic
/// fixtures) → neutral Trained.
pub fn experience_attack_factor(experience: f32) -> f32 {
    if experience < 0.0 {
        return 1.0;
    }
    let frac = if experience > 1.0 {
        experience / 10000.0
    } else {
        experience
    };
    let level = [0.1_f32, 0.3, 0.75, 0.9]
        .iter()
        .filter(|&&t| frac >= t)
        .count();
    1.0 + (level as f32 - 1.0) * 0.25
}

/// A country's org modifiers aggregated over its enabled dynamic modifiers,
/// active national spirits, unexpired country-leader traits and appointed
/// advisors (DESIGN §5.3): HOI4 applies them division-wide as
/// `org × (1 + factor) + flat`, so the tactical side does the same at
/// battalion level. Neutral by default (0/0) — countries without modifiers,
/// and every caller missing the modifier table, keep the old behavior.
///
/// Snapshot semantics: all sources are read once at battle
/// assembly; mid-battle changes are not tracked. (The doctrine factor's
/// divisor side stays symmetric: `division_maxima`
/// carries the same doctrine org factor as the battalion formula.)
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CountryOrgModifier {
    /// Sum of `army_org_factor` values (fraction of base org).
    pub factor: f32,
    /// Sum of flat `army_org` values.
    pub flat: f32,
}

impl CountryOrgModifier {
    /// Aggregate from the parsed country state through the modifier table.
    ///
    /// Four sources:
    /// - Dynamic modifiers: the save carries the current values as a bare
    ///   float array in the definition's modifier-key order — resolve
    ///   `army_org_factor`/`army_org` by index via the table's ordered key
    ///   list.
    /// - Static ideas: look each spirit token up directly.
    /// - Country-leader traits: the unexpired roles' trait tokens,
    ///   looked up like ideas.
    /// - Appointed advisors: the advisor roles' idea tokens —
    ///   advisor tokens ARE idea tokens, so they resolve through the same
    ///   ideas table.
    ///
    /// Snapshot semantics: read once at battle assembly;
    /// mid-battle changes are not tracked. Unknown tokens (modded content,
    /// missing table entries) contribute nothing.
    pub fn of(country: &CountryData, mods: &ModifierTable) -> Self {
        let mut out = CountryOrgModifier::default();
        for (name, values) in &country.dynamic_modifiers {
            let Some(keys) = mods.dynamic_keys(name) else {
                continue;
            };
            for (index, key) in keys.iter().enumerate() {
                // A shorter save array (shouldn't happen — the serializer
                // pads to the definition length) stops the lookup safely.
                let Some(value) = values.get(index) else {
                    break;
                };
                match key.as_str() {
                    "army_org_factor" => out.factor += value,
                    "army_org" => out.flat += value,
                    _ => {}
                }
            }
        }
        for token in country.active_ideas.iter().chain(&country.active_advisors) {
            if let Some(idea) = mods.idea(token) {
                out.factor += idea.army_org_factor;
                out.flat += idea.army_org;
            }
        }
        for token in &country.leader_traits {
            if let Some(trait_mods) = mods.leader_trait(token) {
                out.factor += trait_mods.army_org_factor;
                out.flat += trait_mods.army_org;
            }
        }
        out
    }
}

/// A country's combat-stat modifiers: `army_attack_factor` /
/// `army_defence_factor` (British spelling, as in vanilla) /
/// `breakthrough_factor`, aggregated over the same four sources and with
/// the same snapshot semantics as [`CountryOrgModifier`]. Unknown tokens
/// contribute nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CountryCombatModifier {
    pub attack: f32,
    pub defense: f32,
    pub breakthrough: f32,
}

impl CountryCombatModifier {
    /// Aggregate from the parsed country state through the modifier table
    /// (dynamic modifiers by index, then ideas + advisors, then leader
    /// traits — same sources as [`CountryOrgModifier::of`]).
    pub fn of(country: &CountryData, mods: &ModifierTable) -> Self {
        let mut out = CountryCombatModifier::default();
        for (name, values) in &country.dynamic_modifiers {
            let Some(keys) = mods.dynamic_keys(name) else {
                continue;
            };
            for (index, key) in keys.iter().enumerate() {
                let Some(value) = values.get(index) else {
                    break;
                };
                match key.as_str() {
                    "army_attack_factor" => out.attack += value,
                    "army_defence_factor" => out.defense += value,
                    "breakthrough_factor" => out.breakthrough += value,
                    _ => {}
                }
            }
        }
        for token in country.active_ideas.iter().chain(&country.active_advisors) {
            if let Some(idea) = mods.idea(token) {
                out.attack += idea.army_attack_factor;
                out.defense += idea.army_defence_factor;
                out.breakthrough += idea.breakthrough_factor;
            }
        }
        for token in &country.leader_traits {
            if let Some(trait_mods) = mods.leader_trait(token) {
                out.attack += trait_mods.army_attack_factor;
                out.defense += trait_mods.army_defence_factor;
                out.breakthrough += trait_mods.breakthrough_factor;
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Division commander bonuses
// ---------------------------------------------------------------------------

/// Per-skill-point combat bonus of a HOI4 unit leader: each attack/defense
/// skill point gives `offence`/`defence` 0.025
/// (`common/unit_leader/00_attack_skills.txt` / `00_defense_skills.txt`).
const LEADER_SKILL_BONUS: f32 = 0.025;
/// A field marshal's "regular" bonuses (skills, `modifier` trait values)
/// apply to the armies of his group at half strength
/// (`FIELD_MARSHAL_ARMY_BONUS_RATIO = 0.5`, `common/defines/00_defines.lua`).
const FIELD_MARSHAL_RATIO: f32 = 0.5;

/// A division's commander combat bonuses (DESIGN §5.3),
/// resolved from the save's command chain by [`leader_bonus`]. All fields
/// are additive fractions stacking into the battalion multipliers
/// HOI4-style; the neutral default (no chain link, missing table) keeps the
/// pre-commander-bonus behavior.
#[derive(Debug, Clone, Default)]
pub struct LeaderBonus {
    /// Army-wide attack fraction (attack skills).
    pub attack: f32,
    /// Army-wide defense fraction (defense skills + trait `defence` keys).
    pub defense: f32,
    /// Army-wide breakthrough fraction (trait `breakthrough_factor` keys).
    pub breakthrough: f32,
    /// Per-type attack fractions keyed by battalion FAMILY name ("armor",
    /// "infantry", "artillery", "cavalry", ...) — matched against the
    /// battalion's `Subunit::leader_groups`.
    pub type_attack: HashMap<String, f32>,
    /// Per-type defense fractions, same keying.
    pub type_defense: HashMap<String, f32>,
}

/// Resolve a division's commander bonuses through the save's command chain:
/// division id → the army listing it as a member → that
/// army's `leader` (general) → the army group listing that army as a child
/// → the group's `leader` (field marshal). Every link is optional (§11.3):
/// a division in no army, a leaderless army/group, or an id missing from
/// the character database simply contributes nothing.
///
/// Vanilla application rules (1.19): a general's skill bonuses and trait
/// `modifier` values apply at full; a field marshal's skill bonuses and HIS
/// `modifier` trait values apply at [`FIELD_MARSHAL_RATIO`]; a field
/// marshal's `field_marshal_modifier` trait values apply at full — and ONLY
/// for a FM holder (a corps general holding such a trait gets nothing from
/// it). The extractor marks FM-block provenance with an `fm:` key prefix
/// (see [`ModifierTable::unit_leader_trait`]).
pub fn leader_bonus(save: &SaveGame, traits: &ModifierTable, division_id: u64) -> LeaderBonus {
    let mut out = LeaderBonus::default();
    let Some(army) = save
        .armies
        .iter()
        .find(|a| !a.is_fm_group && a.members.contains(&division_id))
    else {
        return out;
    };
    if let Some(general) = army.leader.and_then(|id| save.leaders.get(&id)) {
        apply_leader(&mut out, general, traits, false);
    }
    let marshal = save
        .armies
        .iter()
        .find(|a| a.is_fm_group && a.child_armies.contains(&army.id))
        .and_then(|g| g.leader)
        .and_then(|id| save.leaders.get(&id));
    if let Some(marshal) = marshal {
        apply_leader(&mut out, marshal, traits, true);
    }
    out
}

/// Add one leader's skill and trait bonuses into `out`; `as_field_marshal`
/// says the holder commands an army GROUP (halved regular bonuses + his
/// FM-only trait values), not an army.
fn apply_leader(
    out: &mut LeaderBonus,
    leader: &LeaderData,
    traits: &ModifierTable,
    as_field_marshal: bool,
) {
    let ratio = if as_field_marshal {
        FIELD_MARSHAL_RATIO
    } else {
        1.0
    };
    out.attack += LEADER_SKILL_BONUS * leader.attack_skill * ratio;
    out.defense += LEADER_SKILL_BONUS * leader.defense_skill * ratio;
    for token in &leader.traits {
        let Some(map) = traits.unit_leader_trait(token) else {
            continue;
        };
        for (key, &value) in map {
            // `fm:`-prefixed entries came from the trait's
            // `field_marshal_modifier` block (extractor provenance marker) —
            // full strength for a FM holder, inert for a corps general.
            // Plain entries (the `modifier` block) apply at full for a
            // general and at FIELD_MARSHAL_ARMY_BONUS_RATIO for a FM.
            let (key, value) = match key.strip_prefix("fm:") {
                Some(k) if as_field_marshal => (k, value),
                Some(_) => continue,
                None => (key.as_str(), value * ratio),
            };
            accumulate_trait_key(out, key, value);
        }
    }
}

/// Split one aggregated trait key into the [`LeaderBonus`] fields: army-wide
/// `defence` → defense and `breakthrough_factor` → breakthrough; per-type
/// `army_<family>_(attack|defence)_factor` and bare `<family>_(attack|
/// defence)_factor` keys (British `defence`, as in vanilla) → the type maps
/// keyed by the FAMILY name (so `army_armor_attack_factor` and
/// `cavalry_attack_factor` land under "armor" / "cavalry").
///
/// Deliberately ignored: every other key (speed/org/supply/morale factors);
/// skill-GRANTING trait fields (`attack_skill = 1`, `attack_skill_factor`)
/// — they live at the trait's top level, the extractor never reads them,
/// and their grants are already baked into the save's serialized skill
/// ints; and terrain trait blocks (`river = { attack = 0.05 }`) — nested
/// objects the extractor skips as non-numeric, and a terrain-conditional
/// leader bonus would need a per-hex dimension the assembly pipeline does
/// not carry.
fn accumulate_trait_key(out: &mut LeaderBonus, key: &str, value: f32) {
    match key {
        "defence" => out.defense += value,
        "breakthrough_factor" => out.breakthrough += value,
        _ => {
            let stripped = key.strip_prefix("army_").unwrap_or(key);
            if let Some(family) = stripped.strip_suffix("_attack_factor") {
                *out.type_attack.entry(family.to_string()).or_default() += value;
            } else if let Some(family) = stripped.strip_suffix("_defence_factor") {
                *out.type_defense.entry(family.to_string()).or_default() += value;
            }
        }
    }
}

/// Division-level HOI4 maxima `(max_org, max_strength)` from the resolved
/// template composition: org = count-weighted mean of subunit max_org over
/// line battalions AND support companies, scaled by the doctrine org factor
/// (the battalion formula's `org_mult`, kept divisor-symmetric
/// so a full HOI4 division reads ratio 1.0) and the country modifier —
/// `mean × (1 + doctrine) × (1 + factor) + flat`. Support
/// companies count at their TABLE max_organisation: HOI4 1.19 gives them
/// real org (engineer 20, support artillery 0 — counting them at 0
/// under-reads the divisor, e.g. a full 6-inf+eng+arty ITA division sits at
/// 47.5×0.9 = 42.75, matching the save's 42.742/42.738 combat-worn values).
/// strength = Σ subunit max_strength INCLUDING support companies (verified
/// exact against tac_probe: 6×25+2 = 152), no country/doctrine scaling.
/// Extracted from [`create_tactical_units`] so the live assembly can
/// aggregate per-province damage bases for the `damage_units` sync batches
/// (DESIGN §3.2; only the strength half feeds those bases — the
/// org base is the assembled battalion pool).
pub fn division_maxima(
    division: &DivisionData,
    unit_templates: &UnitTemplateTable,
    country: CountryOrgModifier,
    doctrine_org_factor: f32,
) -> (f32, f32) {
    let subunits: Vec<Subunit> = division
        .battalions
        .iter()
        .map(|info| Subunit::resolve(info, unit_templates))
        .collect();
    let (battalion_count, org_sum) = subunits.iter().fold((0usize, 0.0f32), |(n, s), x| {
        (n + x.info.count, s + x.max_org * x.info.count as f32)
    });
    // Support companies DO count in the org mean, at their table org value
    // (unknown tokens resolve to 0 — graceful fallback, §5.3).
    let (support_count, support_org) =
        division
            .support_companies
            .iter()
            .fold((0usize, 0.0f32), |(n, s), sc| {
                let org = unit_templates
                    .get(&sc.token)
                    .map(|t| t.max_organisation)
                    .unwrap_or(0.0);
                (n + sc.count, s + org * sc.count as f32)
            });
    let total_count = battalion_count + support_count;
    let mean_org = if total_count > 0 {
        (org_sum + support_org) / total_count as f32
    } else {
        0.0
    };
    let max_org = mean_org * (1.0 + doctrine_org_factor) * (1.0 + country.factor) + country.flat;
    let max_str = subunits
        .iter()
        .map(|x| x.max_strength * x.info.count as f32)
        .sum::<f32>()
        + division
            .support_companies
            .iter()
            .map(|sc| {
                unit_templates
                    .get(&sc.token)
                    .map(|t| t.max_strength)
                    .unwrap_or(0.0)
                    * sc.count as f32
            })
            .sum::<f32>();
    (max_org, max_str)
}

/// Org/strength fallbacks when the unit template table has no entry.
const FALLBACK_MAX_ORG: f32 = 30.0;
const FALLBACK_MAX_STRENGTH: f32 = 25.0;

/// Expand a division's battalions and support companies into individual
/// [`BattalionUnit`]s ("1. Inf", "2. Inf", ...) with §5.3 stats baked in.
///
/// `doctrines`, when provided, should already be filtered to the country's
/// researched doctrines (see [`DoctrineTable::researched`]); its aggregated
/// factors are applied to attack/defense/breakthrough/organisation.
/// `country` carries the country's org modifiers (see
/// [`CountryOrgModifier::of`]; the neutral default keeps the pre-modifier
/// behavior). `combat` carries the country's combat modifiers (see
/// [`CountryCombatModifier::of`], stacking additively with the doctrine
/// factors HOI4-style). `leader` carries the division commander's bonuses
/// (see [`leader_bonus`]; the neutral default applies no
/// leader). `start_id` is the id assigned to the first produced
/// unit; ids increment.
#[allow(clippy::too_many_arguments)]
pub fn create_tactical_units(
    division: &DivisionData,
    side: Side,
    equipment_stats: &EquipmentTable,
    unit_templates: &UnitTemplateTable,
    doctrines: Option<&DoctrineTable>,
    country: CountryOrgModifier,
    combat: CountryCombatModifier,
    leader: &LeaderBonus,
    start_id: usize,
) -> Vec<BattalionUnit> {
    create_tactical_units_named(
        division,
        side,
        equipment_stats,
        unit_templates,
        doctrines,
        country,
        combat,
        leader,
        start_id,
        &UnitNaming::default(),
    )
}

/// [`create_tactical_units`] with an explicit battalion-naming table:
/// production passes the session language's table so generated
/// counter names / support tags / the HQ label read in the player's
/// language; the wrapper above keeps the English default for tests.
#[allow(clippy::too_many_arguments)]
pub fn create_tactical_units_named(
    division: &DivisionData,
    side: Side,
    equipment_stats: &EquipmentTable,
    unit_templates: &UnitTemplateTable,
    doctrines: Option<&DoctrineTable>,
    country: CountryOrgModifier,
    combat: CountryCombatModifier,
    leader: &LeaderBonus,
    start_id: usize,
    naming: &UnitNaming,
) -> Vec<BattalionUnit> {
    let factors = doctrines.map(DoctrineTable::factors).unwrap_or_default();

    // 1. Resolve every LINE battalion group's template: needs + base
    //    org/strength. Support companies do NOT resolve into map units —
    //    they attach to battalions afterwards.
    let subunits: Vec<Subunit> = division
        .battalions
        .iter()
        .map(|info| Subunit::resolve(info, unit_templates))
        .collect();

    // 1.19 saves store division
    // `organisation`/`strength` as ABSOLUTE values (e.g. 41.28 / 152.2),
    // while this builder scales by 0..=1 ratios. The division's maxima come
    // from its resolved composition (shared with the live assembly
    // via `division_maxima`). Values already in ratio form (≤ 1,
    // synthetic/script paths) pass through untouched.
    //
    // The OTHER field disambiguates the ≤ 1 case —
    // a routed division carries absolute org 0.x alongside an absolute
    // strength > 1 (e.g. org 0.9 / str 10); reading org 0.9 as a 90% ratio
    // resurrected the broken unit at near-full org. Both fields ≤ 1 = ratio
    // form; a field ≤ 1 while its partner > 1 = absolute form.
    let (div_max_org, div_max_str) =
        division_maxima(division, unit_templates, country, factors.organisation);
    let org_ratio = if division.organization > 1.0 || division.strength > 1.0 {
        if div_max_org > 0.0 {
            (division.organization / div_max_org).clamp(0.0, 1.0)
        } else {
            (division.organization > 1.0) as u8 as f32
        }
    } else {
        division.organization.clamp(0.0, 1.0)
    };
    let str_ratio = if division.strength > 1.0 || division.organization > 1.0 {
        if div_max_str > 0.0 {
            (division.strength / div_max_str).clamp(0.0, 1.0)
        } else {
            (division.strength > 1.0) as u8 as f32
        }
    } else {
        division.strength.clamp(0.0, 1.0)
    };

    // 2. Division-level equipment requirement per archetype (§5.3).
    let mut total_need: HashMap<&str, f32> = HashMap::new();
    for s in &subunits {
        for (archetype, need) in &s.needs {
            *total_need.entry(archetype.as_str()).or_insert(0.0) += need * s.info.count as f32;
        }
    }

    // 3. Available equipment per archetype, keeping the actual variant keys:
    //    a division holding `infantry_equipment_0` must get THAT variant's
    //    stats, not the archetype's latest one (§5.3 Σ over equipment types).
    //    Unknown equipment keys (not in the table at all) are ignored
    //    gracefully (§5.3 fallback rule).
    let mut available: HashMap<String, Vec<(&str, f32)>> = HashMap::new();
    for (key, count) in &division.equipment {
        if let Some(archetype) = equipment_stats.archetype_of(key) {
            available
                .entry(archetype)
                .or_default()
                .push((key.as_str(), *count));
        }
    }

    // 4. Expand each subunit group into individual battalions.
    // Country combat modifiers stack additively with doctrine
    // factors (HOI4 modifier semantics); the experience multiplier scales
    // soft/hard attack only (a damage-dealt modifier in HOI4).
    // The division commander's army-wide bonuses stack into
    // the same multipliers; his per-type trait bonuses add per battalion
    // below (the battalion's leader_groups ∩ the trait-family maps).
    let attack_mult = 1.0 + factors.attack + combat.attack + leader.attack;
    let defense_mult = 1.0 + factors.defense + combat.defense + leader.defense;
    let breakthrough_mult = 1.0 + factors.breakthrough + combat.breakthrough + leader.breakthrough;
    let org_mult = 1.0 + factors.organisation;
    let exp_attack = experience_attack_factor(division.experience);

    let mut units = Vec::new();
    let mut next_id = start_id;
    let mut numbering: HashMap<UnitType, usize> = HashMap::new();

    for s in &subunits {
        // Per-type leader trait bonuses of this subunit's
        // families (a family with no trait entry contributes 0).
        let leader_attack: f32 = s
            .leader_groups
            .iter()
            .filter_map(|g| leader.type_attack.get(g))
            .sum();
        let leader_defense: f32 = s
            .leader_groups
            .iter()
            .filter_map(|g| leader.type_defense.get(g))
            .sum();
        for _ in 0..s.info.count {
            let stats = BattalionStats::compute(s, equipment_stats, &total_need, &available);

            let n = numbering.entry(s.info.unit_type).or_insert(0);
            *n += 1;
            let name = format!(
                "{}. {}",
                n,
                naming.subunit_abbrev(&s.info.token, s.info.unit_type)
            );

            let mut unit =
                BattalionUnit::new(next_id, name, s.info.unit_type, side, HexCoord::ZERO);
            next_id += 1;
            // Carry the HOI4 division's own name for the OOB tree.
            unit.division = division.name.clone();
            // The division's HOI4 province routes this battalion's
            // damage into the matching damage_units province line (§3.2).
            unit.hoi4_province = division.location;
            // The mid-battle roster keys on the HOI4 division id.
            unit.hoi4_division_id = Some(division.id);
            // Chassis + token flags complete the battalion
            // class; set_chassis also re-derives road speed (horse-towed 3,
            // truck-towed 12, SP by weight 12/10/8/6, foot from the type
            // table).
            unit.set_chassis(s.info.chassis);
            unit.attrs |= s.info.extra_attrs;
            // Per-token terrain adjusters (§6.6), baked at
            // assembly like every other game-data stat (§5.3).
            unit.terrain_adj = s.terrain_adj;

            unit.soft_attack = (stats.soft_attack
                * (attack_mult + leader_attack)
                * stats.degradation
                * exp_attack)
                .max(0.0);
            unit.hard_attack = (stats.hard_attack
                * (attack_mult + leader_attack)
                * stats.degradation
                * exp_attack)
                .max(0.0);
            unit.defense =
                (stats.defense * (defense_mult + leader_defense) * stats.degradation).max(0.0);
            unit.breakthrough =
                (stats.breakthrough * breakthrough_mult * stats.degradation).max(0.0);

            // Org/strength: template base × save ratios (§5.3). §6.3
            // baselines: HOI4 org-0 battalions (towed artillery /
            // AT / AA, tube & rocket — support companies in HOI4) are
            // promoted to the tactical per-type baseline, the same values
            // `BattalionUnit::new` uses on the demo path.
            if s.max_org <= 0.0 {
                unit.max_org = unit.unit_type.base_org();
                unit.max_strength = unit.unit_type.base_strength();
            } else {
                // Country org modifiers ride on top of the
                // doctrine factor, HOI4-shaped: base × doctrine ×
                // (1 + factor) + flat.
                unit.max_org =
                    (s.max_org * org_mult * (1.0 + country.factor) + country.flat).max(1.0);
                unit.max_strength = s.max_strength.max(0.0);
            }
            unit.org = unit.max_org * org_ratio;
            // HOI4's yellow bar is min(manpower ratio, equipment
            // ratio) (wiki "Current Fighting Strength"); the save's strength
            // field carries only the manpower side (live-save evidence: the
            // 4×irregular "Banda Irregolare Libica" divisions read a full
            // 120.0 points while their equipment sat at 224/320=70%). Clamp
            // with the battalion's own equipment fill — HOI4 pools equipment
            // at division level, so the per-type fill ratio applies
            // uniformly to every battalion sharing the type.
            unit.strength = unit.max_strength * str_ratio.min(stats.equipment_ratio);

            unit.armor = stats.armor.max(0.0);
            unit.piercing = stats.piercing.max(0.0);
            unit.hardness = stats.hardness.clamp(0.0, 1.0);

            // §6.8: entrenchment layers, capped at 3.
            unit.entrenchment = entrench_layers(division.entrenchment);

            units.push(unit);
        }
    }

    // Support companies do NOT become map units — they ride with
    // the division's battalions as attachments (round-robin spread so no
    // single battalion collects them all).
    if !units.is_empty() {
        let mut atts: Vec<tactical_core::SupportAttachment> = division
            .support_companies
            .iter()
            .map(|info| tactical_core::SupportAttachment {
                kind: map_support_kind(&info.token),
                name: naming.support_tag(map_support_kind(&info.token)),
            })
            .collect();
        let n = units.len();
        for (i, att) in atts.drain(..).enumerate() {
            units[i % n].attach(att);
        }
    }

    // Synthesize the division HQ (§6.13) — fragile command unit
    // with a fixed stat line, chassis classified from the division's
    // composition. No doctrine mults (HQs are not combat units); org /
    // strength / entrenchment DO follow the division's save ratios, so a
    // battered division fields a battered HQ.
    tactical_core::synthesize_hqs(&mut units, &mut next_id, side, |_| HexCoord::ZERO);
    if let Some(hq) = units.last_mut().filter(|u| u.is_hq()) {
        // The core synthesizer names every HQ "HQ" (locale-free);
        // relabel it from the naming table here.
        hq.name = naming.hq();
        hq.org = hq.max_org * org_ratio;
        // The synthetic HQ has no equipment needs of its own (not part of
        // HOI4's strength pool) — it follows the division's manpower-side
        // ratio only, no equipment-ratio clamp.
        hq.strength = hq.max_strength * str_ratio;
        hq.entrenchment = entrench_layers(division.entrenchment);
        // The HQ belongs to the division's roster entry like its
        // battalions (its damage routing stays as-is — the damage bases
        // deliberately exclude HQ org).
        hq.hoi4_division_id = Some(division.id);
    }

    units
}

/// A template-resolved subunit group ready for stat computation.
struct Subunit<'a> {
    info: &'a BattalionInfo,
    /// Equipment needs per single battalion: archetype → count.
    needs: Vec<(String, f32)>,
    max_org: f32,
    max_strength: f32,
    /// The battalion class's terrain adjusters (§6.6),
    /// resolved from the save's own subunit token (ranger_battalion keeps
    /// its forest identity even mapped to Infantry). Zero-filled when no
    /// template resolves (§5.3 fallback).
    terrain_adj: tactical_core::TerrainAdjusters,
    /// Candidate keys into [`LeaderBonus::type_attack`] / `type_defense`:
    /// the template's `group` plus its `types` entries
    /// and the category aliases of the flag-backed families. HOI4's
    /// per-type leader trait keys (`army_armor_attack_factor`,
    /// `cavalry_attack_factor`, ...) classify battalions by the unit
    /// definition's `type` list and flags — NOT by the coarser `group`
    /// field alone (cavalry/motorized/mechanized all share
    /// `group = mobile`; line artillery sits in `combat_support`), so the
    /// group alone would miss most vanilla trait families. Empty when no
    /// template resolved (§5.3 fallback → no type bonus).
    leader_groups: Vec<String>,
}

/// Build a subunit's `leader_groups` candidate set from its template (see
/// the field doc): `group` + `types` + aliases for the families HOI4
/// classifies by flag/category (`cavalry = yes` → category_cavalry, special
/// forces, rocket artillery). Deduped — a template listing the same family
/// in `group` and `types` (e.g. infantry) must not double-count a bonus.
fn leader_groups_of(template: &UnitTemplateStats) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(group) = &template.group {
        out.push(group.clone());
    }
    out.extend(template.types.iter().cloned());
    for (category, family) in [
        ("category_cavalry", "cavalry"),
        ("category_special_forces", "special_forces"),
        ("category_rocket_artillery", "rocket"),
    ] {
        if template.categories.iter().any(|c| c == category) {
            out.push(family.to_string());
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

impl<'a> Subunit<'a> {
    fn resolve(info: &'a BattalionInfo, unit_templates: &UnitTemplateTable) -> Self {
        // The save's own token is the primary table key (it is the real HOI4
        // subunit name); fall back to the canonical key for the mapped type.
        let template = unit_templates
            .get(&info.token)
            .or_else(|| unit_templates.get(canonical_template_key(info.unit_type)));
        match template {
            Some(t) if !t.needs.is_empty() => Subunit {
                info,
                needs: t.needs.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                max_org: t.max_organisation,
                max_strength: t.max_strength,
                terrain_adj: t.terrain_adjusters(),
                leader_groups: leader_groups_of(t),
            },
            Some(t) => Subunit {
                info,
                needs: vec![(
                    fallback_equipment_archetype(info.unit_type).to_string(),
                    fallback_equipment_target(info.unit_type),
                )],
                max_org: t.max_organisation,
                max_strength: t.max_strength,
                terrain_adj: t.terrain_adjusters(),
                leader_groups: leader_groups_of(t),
            },
            None => Subunit {
                info,
                needs: vec![(
                    fallback_equipment_archetype(info.unit_type).to_string(),
                    fallback_equipment_target(info.unit_type),
                )],
                max_org: FALLBACK_MAX_ORG,
                max_strength: FALLBACK_MAX_STRENGTH,
                terrain_adj: Default::default(),
                leader_groups: Vec::new(),
            },
        }
    }
}

/// Equipment-derived per-battalion stats before doctrine factors (§5.3).
/// (No speed here: unit speed comes solely from the chassis/type table,
/// unit.rs — equipment max_speed is intentionally not consulted.)
struct BattalionStats {
    soft_attack: f32,
    hard_attack: f32,
    defense: f32,
    breakthrough: f32,
    armor: f32,
    piercing: f32,
    hardness: f32,
    /// Equipment-ratio degradation multiplier (0.5..=1.0 by default).
    degradation: f32,
    /// Battalion equipment fill (alloc/need, count-weighted over its needs,
    /// 0..=1; 1.0 when the battalion has no needs). The str bar's
    /// equipment side — HOI4's yellow bar is min(manpower, equipment).
    equipment_ratio: f32,
}

impl BattalionStats {
    fn compute(
        s: &Subunit,
        equipment_stats: &EquipmentTable,
        total_need: &HashMap<&str, f32>,
        available: &HashMap<String, Vec<(&str, f32)>>,
    ) -> Self {
        let mut soft = 0.0;
        let mut hard = 0.0;
        let mut defense = 0.0;
        let mut breakthrough = 0.0;
        let mut armor: f32 = 0.0;
        let mut piercing_weighted = 0.0;
        let mut hardness_weighted = 0.0;
        let mut weight_sum = 0.0;

        let mut need_sum = 0.0;
        let mut alloc_sum = 0.0;

        for (archetype, need) in &s.needs {
            need_sum += need;
            // Apportion division equipment to this battalion by need share,
            // variant by variant (§5.3: stats come from the equipment the
            // division actually holds).
            let total = total_need.get(archetype.as_str()).copied().unwrap_or(0.0);
            if total <= 0.0 {
                continue;
            }
            let Some(variants) = available.get(archetype) else {
                continue;
            };
            // Surplus equipment cannot be used: when the division holds MORE
            // of an archetype than the line battalions need, each battalion
            // is capped at its own requirement (the rest stays in the depot).
            let arch_available: f32 = variants.iter().map(|(_, c)| *c).sum();
            let cap_scale = if arch_available > total && arch_available > 0.0 {
                total / arch_available
            } else {
                1.0
            };
            for (key, count) in variants {
                let alloc = count * need / total * cap_scale;
                alloc_sum += alloc;
                // Equipment missing from the stats table contributes nothing
                // (graceful fallback, §5.3).
                let Some(eq) = equipment_stats.resolve(key) else {
                    continue;
                };
                // Battalion-level normalization: HOI4's equipment stat is
                // the BATTALION-LEVEL value at a full complement — divide
                // the piece count by the per-battalion need (100 rifles →
                // ×1, not ×100); without it the save-path values read 100×
                // the HOI4/script scale.
                let fill = if *need > 0.0 { alloc / need } else { 0.0 };
                soft += eq.soft_attack * fill;
                hard += eq.hard_attack * fill;
                defense += eq.defense * fill;
                breakthrough += eq.breakthrough * fill;
                armor = armor.max(eq.armor);
                piercing_weighted += eq.piercing * alloc;
                hardness_weighted += eq.hardness * alloc;
                weight_sum += alloc;
            }
        }

        let equipment_ratio = if need_sum > 0.0 {
            (alloc_sum / need_sum).clamp(0.0, 1.0)
        } else {
            1.0
        };
        // §5.3: stats × (1 - (1 - equipment_ratio) × degradation_factor).
        let degradation = 1.0 - (1.0 - equipment_ratio) * DEFAULT_DEGRADATION_FACTOR;

        BattalionStats {
            soft_attack: soft,
            hard_attack: hard,
            defense,
            breakthrough,
            armor,
            piercing: if weight_sum > 0.0 {
                piercing_weighted / weight_sum
            } else {
                1.0
            },
            hardness: if weight_sum > 0.0 {
                hardness_weighted / weight_sum
            } else {
                0.0
            },
            degradation,
            equipment_ratio,
        }
    }
}

/// §6.8: entrenchment capped at 3 layers. Save values ≤ 1.0 are treated as a
/// ratio of the cap; larger values as an absolute layer count.
fn entrench_layers(save_entrenchment: f32) -> u8 {
    let e = save_entrenchment;
    let layers = if e <= 1.0 {
        (e * 3.0).round()
    } else {
        e.round()
    };
    layers.clamp(0.0, 3.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ArmyData;
    use crate::parser::SaveParser;
    use crate::tables::tests_helpers;

    fn mini_tables() -> (EquipmentTable, UnitTemplateTable) {
        (
            EquipmentTable::from_str(tests_helpers::MINI_EQUIPMENT_JSON).unwrap(),
            UnitTemplateTable::from_str(tests_helpers::MINI_UNIT_TEMPLATES_JSON).unwrap(),
        )
    }

    /// Save with one infantry division: 2 inf battalions + 1 engineer,
    /// fully equipped (200 inf equipment + 10 inf + 30 support).
    const DIVISION_SAVE: &str = r#"
countries = {
    GER = {
        division_template = {
            "Test-Division" = {
                regiments = {
                    infantry = { x = 0 y = 0 }
                    infantry = { x = 1 y = 0 }
                }
                support = {
                    engineer = { x = 0 y = 0 }
                }
            }
        }
        units = {
            division = {
                id = 1
                name = "1. Test-Division"
                division_template = "Test-Division"
                location = 6334
                organization = 0.85
                strength = 0.90
                equipment = {
                    infantry_equipment_0 = 210
                    support_equipment_1 = 30
                }
            }
        }
    }
}
"#;

    fn parse_first_division(text: &str) -> crate::DivisionData {
        let save = SaveParser::parse_save_from_str(text).unwrap();
        save.countries["GER"].divisions[0].clone()
    }

    #[test]
    fn battalions_expand_to_individual_named_units() {
        let (eq, ut) = mini_tables();
        let div = parse_first_division(DIVISION_SAVE);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            5,
        );
        // Only line battalions deploy — the engineer company rides
        // with the first battalion as an attachment instead.
        // +1 synthesized division HQ (§6.13).
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].id, 5);
        assert_eq!(units[1].id, 6);
        assert_eq!(units[0].name, "1. Inf");
        assert_eq!(units[1].name, "2. Inf");
        assert!(units.iter().all(|u| u.side == Side::Attacker));
        assert_eq!(units[0].support.len(), 1);
        assert_eq!(
            units[0].support[0].kind,
            tactical_core::SupportKind::Engineer
        );
        // Range/sight come from the UnitType table (§6.1).
        assert_eq!(units[0].attack_range, 1);
        assert_eq!(units[0].sight_range, 2);
    }

    #[test]
    fn naming_table_drives_names_tags_and_hq() {
        // The UnitNaming table owns every generated label —
        // battalion names, support tags, and the synthesized HQ.
        let (eq, ut) = mini_tables();
        let div = parse_first_division(DIVISION_SAVE);
        let mut naming = crate::mapping::UnitNaming::default();
        naming.set_type(UnitType::Infantry, "步兵".to_string());
        naming.set_type(UnitType::Headquarters, "指挥部".to_string());
        naming.set_support(tactical_core::SupportKind::Engineer, "工兵".to_string());
        let units = create_tactical_units_named(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            5,
            &naming,
        );
        assert_eq!(units[0].name, "1. 步兵");
        assert_eq!(units[0].support[0].name, "工兵");
        assert_eq!(units.last().unwrap().name, "指挥部");
    }

    #[test]
    fn org_zero_battalions_use_tactical_baseline() {
        // artillery_brigade is an org-0 support company in HOI4 —
        // promoted to a battalion here, it must take the §6.3 baseline
        // (towed 30 org / 4 str), not the template's 0 / 0.6 clamped to 1.0.
        let (eq, ut) = mini_tables();
        let save = r#"
countries = {
    GER = {
        division_template = {
            "Arty-Division" = {
                regiments = { artillery_brigade = { x = 0 y = 0 } }
            }
        }
        units = {
            division = {
                id = 1
                division_template = "Arty-Division"
                organization = 0.5
                strength = 1.0
                equipment = { infantry_equipment_0 = 210 }
            }
        }
    }
}
"#;
        let div = parse_first_division(save);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        // +1 synthesized HQ (§6.13): named divisions always field one.
        assert_eq!(units.len(), 2);
        let arty = &units[0];
        assert_eq!(arty.unit_type, UnitType::ArtilleryBrigade);
        assert!(
            (arty.max_org - 30.0).abs() < 1e-3,
            "max_org={}",
            arty.max_org
        );
        assert!(
            (arty.max_strength - 4.0).abs() < 1e-3,
            "max_str={}",
            arty.max_strength
        );
        // Save ratios still scale current values (org 0.5 → 15).
        assert!((arty.org - 15.0).abs() < 1e-2, "org={}", arty.org);
        assert!((arty.strength - 4.0).abs() < 1e-2, "str={}", arty.strength);
    }

    #[test]
    fn stat_math_matches_spec_formula() {
        let (eq, ut) = mini_tables();
        let div = parse_first_division(DIVISION_SAVE);
        let units = create_tactical_units(
            &div,
            Side::Defender,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let inf = &units[0];
        // Infantry needs 100 infantry_equipment; the division holds 210 but a
        // battalion can only use its own requirement (surplus stays in the
        // depot) → fill 1.0 → soft_attack = 3.0 (HOI4 battalion level, §5.3).
        assert!(
            (inf.soft_attack - 3.0).abs() < 1e-2,
            "soft={}",
            inf.soft_attack
        );
        assert!((inf.hard_attack - 0.5).abs() < 1e-2);
        // Org: 60 × save ratio 0.85 = 51; strength: 25 × 0.90 = 22.5.
        assert!((inf.max_org - 60.0).abs() < 1e-3);
        assert!((inf.org - 51.0).abs() < 1e-2);
        assert!((inf.max_strength - 25.0).abs() < 1e-3);
        assert!((inf.strength - 22.5).abs() < 1e-2);
        // Speed: battalion-type table (§6.1) — infantry 4 km/h.
        assert!((inf.speed_kmh - 4.0).abs() < 1e-3);
        assert!((inf.armor - 0.0).abs() < 1e-6);
        assert!((inf.piercing - 1.0).abs() < 1e-3);
    }

    #[test]
    fn armor_piercing_hardness_and_speed_from_equipment() {
        let (eq, ut) = mini_tables();
        let save = r#"
countries = {
    GER = {
        division_template = {
            "Panzer" = {
                regiments = { light_armor = { x = 0 y = 0 } }
            }
        }
        units = {
            division = {
                id = 1
                division_template = "Panzer"
                organization = 1.0
                strength = 1.0
                equipment = { light_tank_chassis_0 = 60 }
            }
        }
    }
}
"#;
        let div = parse_first_division(save);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        // +1 synthesized HQ (§6.13): named divisions always field one.
        assert_eq!(units.len(), 2);
        let tank = &units[0];
        assert_eq!(tank.unit_type, UnitType::LightArmor);
        // 60 light tanks fill the need (60) exactly → soft 10 / hard 8.
        assert!((tank.soft_attack - 10.0).abs() < 1e-1);
        assert!((tank.hard_attack - 8.0).abs() < 1e-1);
        // armor = max, piercing/hardness = weighted average (single type).
        assert!((tank.armor - 10.0).abs() < 1e-3);
        assert!((tank.piercing - 15.0).abs() < 1e-3);
        assert!((tank.hardness - 0.4).abs() < 1e-3);
        // Speed: battalion-type table — light armor 12 km/h (§6.1).
        assert!((tank.speed_kmh - 12.0).abs() < 1e-3);
    }

    #[test]
    fn support_companies_attach_round_robin() {
        let (eq, ut) = mini_tables();
        let div = parse_first_division(DIVISION_SAVE);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        // The engineer company does not deploy — it rides with the
        // first battalion (round-robin spread).
        // +1 synthesized division HQ (§6.13).
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].support.len(), 1);
        assert_eq!(
            units[0].support[0].kind,
            tactical_core::SupportKind::Engineer
        );
        assert_eq!(units[0].support[0].name, "ENG");
        assert!(units[1].support.is_empty());
    }

    #[test]
    fn division_hq_is_synthesized_from_composition() {
        let (eq, ut) = mini_tables();
        // Infantry division → foot HQ, save ratios applied (§6.13).
        let div = parse_first_division(DIVISION_SAVE);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let hq = units.iter().find(|u| u.is_hq()).expect("HQ synthesized");
        assert_eq!(hq.unit_type, UnitType::Headquarters);
        assert_eq!(hq.division, "1. Test-Division");
        assert_eq!(hq.chassis, tactical_core::Chassis::None);
        assert!((hq.max_org - 20.0).abs() < 1e-3);
        assert!((hq.org - 20.0 * 0.85).abs() < 1e-2, "org={}", hq.org);
        assert!(
            (hq.strength - 3.0 * 0.90).abs() < 1e-2,
            "str={}",
            hq.strength
        );
        assert!((hq.soft_attack - 3.0).abs() < 1e-3);

        // Division with a tank battalion → armored-car HQ (fixed armored
        // car, never a tank — §6.13).
        let save = DIVISION_SAVE.replace(
            "infantry = { x = 1 y = 0 }",
            "light_armor = { x = 1 y = 0 }",
        );
        let div = parse_first_division(&save);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let hq = units.iter().find(|u| u.is_hq()).expect("HQ synthesized");
        assert_eq!(hq.chassis, tactical_core::Chassis::Wheeled);
        assert!(hq.attrs.has(tactical_core::Attrs::HQ_ARMORED));
        assert!((hq.speed_kmh - 12.0).abs() < 1e-3);
    }

    #[test]
    fn equipment_ratio_degrades_stats() {
        let (eq, ut) = mini_tables();
        // Only half the infantry equipment the division needs.
        let save =
            DIVISION_SAVE.replace("infantry_equipment_0 = 210", "infantry_equipment_0 = 105");
        let div = parse_first_division(&save);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let inf = &units[0];
        // total need = 200 (line only) → alloc = 105 × 100/200 = 52.5 → fill 0.525.
        // ratio = 0.525 → multiplier = 1 - 0.475 × 0.5 = 0.7625 → 3 × 0.525 × 0.7625 ≈ 1.20.
        assert!(
            (inf.soft_attack - 1.201).abs() < 1e-2,
            "soft={}",
            inf.soft_attack
        );
    }

    #[test]
    fn missing_equipment_and_templates_fall_back_gracefully() {
        let (eq, ut) = mini_tables();
        // Unknown equipment keys + unknown subunit token + token with no
        // template entry: must not panic, must fall back sensibly.
        let save = r#"
countries = {
    GER = {
        division_template = {
            "Odd" = {
                regiments = {
                    warpack = { x = 0 y = 0 }
                    cavalry = { x = 1 y = 0 }
                }
            }
        }
        units = {
            division = {
                id = 1
                division_template = "Odd"
                organization = 0.5
                strength = 0.5
                equipment = {
                    alien_tech = 999
                    infantry_equipment_0 = 50
                }
            }
        }
    }
}
"#;
        let div = parse_first_division(save);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        // +1 synthesized HQ (§6.13): named divisions always field one.
        assert_eq!(units.len(), 3);
        // "warpack" is not a table token: falls back to Infantry type and the
        // canonical infantry template (org 60 in the mini table).
        let odd = &units[0];
        assert_eq!(odd.unit_type, UnitType::Infantry);
        assert!((odd.max_org - 60.0).abs() < 1e-3);
        // "cavalry" has no entry in the mini template table: hard fallback to
        // default org 30 / strength 25 with infantry-equipment fallback needs.
        let cav = &units[1];
        assert_eq!(cav.unit_type, UnitType::Cavalry);
        assert!((cav.max_org - 30.0).abs() < 1e-3);
        assert!((cav.max_strength - 25.0).abs() < 1e-3);
        // "alien_tech" was ignored; 50 infantry equipment is shared between
        // the two battalions (total need 200): each gets 25 → fill 0.25 →
        // degradation 0.625 → soft = 3 × 0.25 × 0.625 ≈ 0.469.
        assert!(odd.soft_attack > 0.0);
        assert!(odd.soft_attack < 3.0);
        assert!((odd.soft_attack - 0.469).abs() < 1e-2);
        assert!((odd.soft_attack - cav.soft_attack).abs() < 1e-3);
    }

    #[test]
    fn doctrine_factors_scale_stats() {
        let (eq, ut) = mini_tables();
        let doctrines = DoctrineTable::from_str(
            r#"{"tree": {"nodes": {"node": {
                "category_modifiers": {"infantry": {"soft_attack": 0.1, "defense": 0.2}}
            }}}}"#,
        )
        .unwrap();
        let div = parse_first_division(DIVISION_SAVE);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            Some(&doctrines),
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let inf = &units[0];
        // 3 × 1.1 attack = 3.3; 20 × 1.2 defense = 24
        // (battalion capped at its own requirement despite the surplus).
        assert!(
            (inf.soft_attack - 3.3).abs() < 1e-2,
            "soft={}",
            inf.soft_attack
        );
        assert!((inf.defense - 24.0).abs() < 1e-2, "def={}", inf.defense);
    }

    #[test]
    fn experience_attack_factor_levels() {
        // Absent (synthetic fixture) → neutral Trained.
        assert_eq!(experience_attack_factor(-1.0), 1.0);
        // 1.19 points form (fraction × 10000, ITA save evidence).
        assert_eq!(experience_attack_factor(0.0), 0.75); // Green
        assert_eq!(experience_attack_factor(801.22), 0.75); // 0.0801 Green
        assert_eq!(experience_attack_factor(2880.0), 1.0); // 0.288 Trained
        assert_eq!(experience_attack_factor(3500.0), 1.25); // Regular
        assert_eq!(experience_attack_factor(8000.0), 1.5); // Seasoned
        assert_eq!(experience_attack_factor(9000.0), 1.75); // Veteran
                                                            // Legacy fraction form (0..1 synthetic saves).
        assert_eq!(experience_attack_factor(0.05), 0.75);
        assert_eq!(experience_attack_factor(0.35), 1.25);
        assert_eq!(experience_attack_factor(1.0), 1.75);
    }

    #[test]
    fn combat_modifier_and_experience_scale_stats() {
        // Country combat factors stack additively with doctrines
        // (HOI4 modifier semantics); experience multiplies soft/hard ONLY
        // (a damage-dealt modifier in HOI4).
        let (eq, ut) = mini_tables();
        let mut div = parse_first_division(DIVISION_SAVE);
        let combat = CountryCombatModifier {
            attack: 0.1,
            defense: 0.2,
            breakthrough: 0.0,
        };
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            combat,
            &LeaderBonus::default(),
            0,
        );
        let inf = &units[0];
        // 3 × 1.1 = 3.3 attack; 20 × 1.2 = 24 defense (no experience field
        // in the fixture → neutral).
        assert!(
            (inf.soft_attack - 3.3).abs() < 1e-2,
            "soft={}",
            inf.soft_attack
        );
        assert!((inf.defense - 24.0).abs() < 1e-2, "def={}", inf.defense);

        // Regular experience (1.19 points 3500 = 0.35) → ×1.25 soft/hard,
        // defense untouched.
        div.experience = 3500.0;
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let inf = &units[0];
        assert!(
            (inf.soft_attack - 3.75).abs() < 1e-2,
            "soft={}",
            inf.soft_attack
        );
        assert!((inf.defense - 20.0).abs() < 1e-2, "def={}", inf.defense);
    }

    #[test]
    fn entrenchment_maps_to_layers() {
        assert_eq!(entrench_layers(0.0), 0);
        assert_eq!(entrench_layers(1.0), 3); // ratio of cap
        assert_eq!(entrench_layers(2.0), 2); // absolute layers
        assert_eq!(entrench_layers(9.0), 3); // capped (§6.8)
    }

    /// End-to-end against the REAL pre-extracted tables (§5.4).
    #[test]
    fn real_data_tables_drive_stat_creation() {
        let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("data");
        let eq = EquipmentTable::load(data_dir.join("equipment_stats.json")).unwrap();
        let ut = UnitTemplateTable::load(data_dir.join("unit_templates.json")).unwrap();
        let dt = DoctrineTable::load(data_dir.join("doctrine_bonuses.json")).unwrap();

        assert!(eq.len() >= 250, "equipment entries: {}", eq.len());
        assert_eq!(ut.line_battalions.len(), 68);
        assert_eq!(ut.support_companies.len(), 89);
        assert!(
            dt.tree_count() >= 100,
            "doctrine trees: {}",
            dt.tree_count()
        );

        // Archetype resolution works against the real data (no bare keys).
        assert!(eq.resolve("infantry_equipment").is_some());
        assert!(eq.resolve("light_tank_chassis").is_some());
        assert!(eq.resolve("support_equipment").is_some());
        assert!((ut.get("infantry").unwrap().max_organisation - 60.0).abs() < 1e-3);

        let save = r#"
division_templates = {
    division_template = {
        id = { id = 7 type = 47 }
        name = "Panzer-Division"
        regiments = {
            light_armor = { x = 0 y = 0 }
            light_armor = { x = 0 y = 1 }
            motorized = { x = 1 y = 0 }
        }
        support = { recon = { x = 0 y = 0 } }
    }
}
countries = {
    GER = {
        units = {
            division = {
                id = 1
                division_template_id = { id = 7 type = 47 }
                location = 6334
                organization = 0.9
                strength = 0.95
                equipment = {
                    light_tank_chassis_1 = 120
                    motorized_equipment_1 = 35
                    infantry_equipment_1 = 140
                    support_equipment_1 = 10
                }
            }
        }
    }
}
"#;
        let parsed = SaveParser::parse_save_from_str(save).unwrap();
        let div = &parsed.countries["GER"].divisions[0];
        let units = create_tactical_units(
            div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        // 3 line battalions deploy; the recon company attaches to
        // the first one (sight +1 on top of armor's base 1). +1 HQ (§6.13).
        assert_eq!(units.len(), 4);
        assert_eq!(units[0].support.len(), 1);
        assert_eq!(units[0].support[0].kind, tactical_core::SupportKind::Recon);
        assert_eq!(units[0].sight_range, 2);
        // Bare tank chassis carries no attack stats in the real data (the gun
        // is a module), but defines armor/hardness/speed (§5.3 max/avg/min).
        let tank = units
            .iter()
            .find(|u| u.unit_type == UnitType::LightArmor)
            .unwrap();
        assert!((tank.armor - 15.0).abs() < 1e-3, "armor={}", tank.armor);
        assert!(
            (tank.hardness - 0.8).abs() < 1e-3,
            "hardness={}",
            tank.hardness
        );
        assert!((tank.speed_kmh - 12.0).abs() < 1e-3, "v={}", tank.speed_kmh);
        assert!(tank.org > 0.0 && tank.strength > 0.0);
        // Motorized needs 100 infantry_equipment (real soft_attack 6) + 35
        // motorized_equipment: soft attack comes from the small arms.
        let mot = units
            .iter()
            .find(|u| u.unit_type == UnitType::Motorized)
            .unwrap();
        assert!(
            (mot.soft_attack - 6.0).abs() < 1e-1,
            "mot soft={}",
            mot.soft_attack
        );
        // Speed: battalion-type table — motorized 12 km/h (§6.1).
        assert!(
            (mot.speed_kmh - 12.0).abs() < 1e-3,
            "mot v={}",
            mot.speed_kmh
        );
        // No Recon unit deploys — the company rides with the first
        // battalion (asserted above).
    }

    #[test]
    fn division_maxima_counts_support_companies_in_org_divisor() {
        // HOI4 averages division org over ALL subunits, and
        // support companies count at their TABLE org (mini-table engineer
        // 20), not 0 — DIVISION_SAVE = 2 inf (60) + 1 engineer (20): org
        // divisor (120+20)/3 ≈ 46.667 (was 40 counting the engineer at 0);
        // str Σ unchanged (2×25 + 2 = 52).
        let (_eq, ut) = mini_tables();
        let div = parse_first_division(DIVISION_SAVE);
        let (max_org, max_str) = division_maxima(&div, &ut, CountryOrgModifier::default(), 0.0);
        assert!((max_org - 46.667).abs() < 1e-2, "max_org={}", max_org);
        assert!((max_str - 52.0).abs() < 1e-3, "max_str={}", max_str);
    }

    #[test]
    fn country_org_modifier_aggregates_dynamic_and_static_sources() {
        // Dynamic modifiers resolve org keys by index through the
        // table's ordered key list; static ideas and leader traits
        // look up directly. Unknown tokens and short value arrays degrade
        // silently.
        let mods = ModifierTable::from_str(
            r#"{
                "dynamic_modifiers": {
                    "ITA_regio_esercito_dynamic_modifier": [
                        "max_dig_in_factor", "land_doctrine_cost_factor",
                        "army_speed_factor", "army_org_factor",
                        "org_loss_when_moving", "army_attack_factor"
                    ],
                    "short_array_modifier": ["army_org_factor", "army_org"]
                },
                "ideas": {
                    "general_staff": {"army_org_factor": 0.05, "army_attack_factor": 0.05},
                    "flat_spirit": {"army_org": 5.0},
                    "advisor_idea": {"army_org_factor": 0.1, "breakthrough_factor": 0.05}
                },
                "leader_traits": {
                    "red_army_organizer": {"army_org_factor": 0.12, "army_defence_factor": 0.1}
                }
            }"#,
        )
        .unwrap();
        let country = CountryData {
            tag: "ITA".to_string(),
            divisions: Vec::new(),
            technologies: Vec::new(),
            active_ideas: vec![
                "general_staff".to_string(),
                "flat_spirit".to_string(),
                "unknown_spirit".to_string(),
            ],
            dynamic_modifiers: vec![
                (
                    "ITA_regio_esercito_dynamic_modifier".to_string(),
                    vec![0.1, 0.1, -0.1, -0.1, 0.15, 0.15],
                ),
                // Value array shorter than the key list: army_org unread.
                ("short_array_modifier".to_string(), vec![0.2]),
                ("unknown_modifier".to_string(), vec![9.9]),
            ],
            leader_traits: vec![
                "red_army_organizer".to_string(),
                "unknown_trait".to_string(),
            ],
            active_advisors: vec!["advisor_idea".to_string(), "unknown_advisor".to_string()],
        };
        let m = CountryOrgModifier::of(&country, &mods);
        // factor: -0.1 (dynamic idx 3) + 0.2 (short array idx 0) + 0.05
        // (idea) + 0.12 (leader trait) + 0.1 (advisor idea) = 0.37;
        // flat: +5.0 (idea) only.
        assert!((m.factor - 0.37).abs() < 1e-6, "factor={}", m.factor);
        assert!((m.flat - 5.0).abs() < 1e-6, "flat={}", m.flat);
        // The same aggregation feeds the combat keys (dynamic by
        // index 0.15 + idea 0.05; British `army_defence_factor` rides the
        // serde field; advisor breakthrough).
        let c = CountryCombatModifier::of(&country, &mods);
        assert!((c.attack - 0.20).abs() < 1e-6, "attack={}", c.attack);
        assert!((c.defense - 0.10).abs() < 1e-6, "defense={}", c.defense);
        assert!(
            (c.breakthrough - 0.05).abs() < 1e-6,
            "bt={}",
            c.breakthrough
        );
        // Neutral default for a country without modifiers.
        let empty = CountryData {
            tag: "ETH".to_string(),
            divisions: Vec::new(),
            technologies: Vec::new(),
            active_ideas: Vec::new(),
            dynamic_modifiers: Vec::new(),
            leader_traits: Vec::new(),
            active_advisors: Vec::new(),
        };
        assert_eq!(
            CountryOrgModifier::of(&empty, &mods),
            CountryOrgModifier::default()
        );
        assert_eq!(
            CountryCombatModifier::of(&empty, &mods),
            CountryCombatModifier::default()
        );
    }

    /// End-to-end replica of the tac_snap ITA fixture: template
    /// 505 (6 infantry @60 org + engineer @20 + support artillery @0),
    /// Regio Esercito dynamic modifier army_org_factor = -0.1, save org
    /// absolute 42.75 (the full-division value) → battalions field
    /// max_org = org = 54 (full ratio).
    #[test]
    fn ita_regio_esercito_fixture_yields_full_org_battalions() {
        let (eq, ut) = mini_tables();
        let mods = ModifierTable::from_str(
            r#"{
                "dynamic_modifiers": {
                    "ITA_regio_esercito_dynamic_modifier": [
                        "max_dig_in_factor", "land_doctrine_cost_factor",
                        "army_speed_factor", "army_org_factor",
                        "org_loss_when_moving"
                    ]
                },
                "ideas": {}
            }"#,
        )
        .unwrap();
        let save = r#"
countries = {
    ITA = {
        division_template = {
            "Divisione di Fanteria" = {
                regiments = {
                    infantry = { x = 0 y = 0 }
                    infantry = { x = 1 y = 0 }
                    infantry = { x = 2 y = 0 }
                    infantry = { x = 0 y = 1 }
                    infantry = { x = 1 y = 1 }
                    infantry = { x = 2 y = 1 }
                }
                support = {
                    engineer = { x = 0 y = 0 }
                    artillery_brigade = { x = 1 y = 0 }
                }
            }
        }
        dynamic_modifier = {
            modifier = {
                modifier = "ITA_regio_esercito_dynamic_modifier"
                value = { 0.1 0.1 -0.1 -0.1 0.15 }
                enabled = yes
            }
        }
        units = {
            division = {
                id = 1
                name = "1a Divisione di Fanteria"
                division_template = "Divisione di Fanteria"
                location = 13238
                organization = 42.75
                strength = 152.6
                equipment = {
                    infantry_equipment_0 = 622
                    support_equipment_1 = 30
                }
            }
        }
    }
}
"#;
        let parsed = SaveParser::parse_save_from_str(save).unwrap();
        let country = &parsed.countries["ITA"];
        let div = &country.divisions[0];
        let org_mod = CountryOrgModifier::of(country, &mods);
        assert!(
            (org_mod.factor + 0.1).abs() < 1e-6,
            "factor={}",
            org_mod.factor
        );

        // Division maxima: mean (6×60+20+0)/8 = 47.5 × 0.9 = 42.75 — the
        // save's full-division org, so the ratio resolves to exactly 1.
        let (max_org, _max_str) = division_maxima(div, &ut, org_mod, 0.0);
        assert!((max_org - 42.75).abs() < 1e-2, "div max_org={}", max_org);
        // The doctrine org factor multiplies alongside the
        // country factor — 47.5 × 1.2 × 0.9 = 51.3 (divisor symmetry with
        // the battalion formula).
        let (max_org_d, _) = division_maxima(div, &ut, org_mod, 0.2);
        assert!((max_org_d - 51.3).abs() < 1e-2, "div max_org={}", max_org_d);

        let units = create_tactical_units(
            div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            org_mod,
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let inf = units
            .iter()
            .find(|u| u.unit_type == UnitType::Infantry)
            .unwrap();
        assert!((inf.max_org - 54.0).abs() < 1e-2, "max_org={}", inf.max_org);
        assert!((inf.org - 54.0).abs() < 1e-2, "org={}", inf.org);
    }

    #[test]
    fn strength_bar_uses_min_of_manpower_and_equipment() {
        // HOI4's yellow bar = min(manpower ratio, equipment ratio).
        // Absolute-form division: org/strength full on the manpower side,
        // but only 100 of 200 needed infantry equipment → battalions at 50%.
        let (eq, ut) = mini_tables();
        let save = r#"
countries = {
    GER = {
        division_template = {
            "Half-Division" = {
                regiments = {
                    infantry = { x = 0 y = 0 }
                    infantry = { x = 1 y = 0 }
                }
            }
        }
        units = {
            division = {
                id = 1
                division_template = "Half-Division"
                organization = 60
                strength = 50
                equipment = { infantry_equipment_0 = 100 }
            }
        }
    }
}
"#;
        let div = parse_first_division(save);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        let inf = &units[0];
        // Manpower side full: 50/50 → 1.0; equipment 100/200 → 0.5.
        // str = 25 × min(1.0, 0.5) = 12.5; org untouched: 60 × (60/60) = 60.
        assert!((inf.max_strength - 25.0).abs() < 1e-3);
        assert!((inf.strength - 12.5).abs() < 1e-2, "str={}", inf.strength);
        assert!((inf.org - 60.0).abs() < 1e-2, "org={}", inf.org);
    }

    #[test]
    fn zero_equipment_division_spawns_at_zero_strength() {
        // Edge case: min(manpower, 0) = 0 — a save division
        // with no resolvable equipment arrives at str 0 (HOI4's bar reads
        // the same); normalize_broken_state retires it.
        let (eq, ut) = mini_tables();
        let save = r#"
countries = {
    GER = {
        division_template = {
            "Empty-Division" = {
                regiments = { infantry = { x = 0 y = 0 } }
            }
        }
        units = {
            division = {
                id = 1
                division_template = "Empty-Division"
                organization = 60
                strength = 25
                equipment = { }
            }
        }
    }
}
"#;
        let div = parse_first_division(save);
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &LeaderBonus::default(),
            0,
        );
        assert_eq!(units[0].strength, 0.0, "str={}", units[0].strength);
    }

    /// Command-chain fixture: division 1001 in army 42 (general 500),
    /// division 1002 in leaderless army 43; both armies under FM group 44
    /// (field marshal 600). The general additionally holds an FM-only trait
    /// to prove its `fm:` entries stay inert for a corps commander.
    fn leader_chain_save() -> SaveGame {
        let mut save = SaveGame::default();
        save.leaders.insert(
            500,
            LeaderData {
                attack_skill: 4.0,
                defense_skill: 3.0,
                traits: vec![
                    "panzer_leader".to_string(),
                    "war_hero".to_string(),
                    "unyielding_defender".to_string(),
                ],
                is_field_marshal: false,
            },
        );
        save.leaders.insert(
            600,
            LeaderData {
                attack_skill: 5.0,
                defense_skill: 2.0,
                traits: vec![
                    "unyielding_defender".to_string(),
                    "aggressive_assaulter".to_string(),
                    "fm_infantry_officer".to_string(),
                ],
                is_field_marshal: true,
            },
        );
        save.armies.push(ArmyData {
            id: 42,
            leader: Some(500),
            members: vec![1001],
            is_fm_group: false,
            child_armies: Vec::new(),
        });
        save.armies.push(ArmyData {
            id: 43,
            leader: None,
            members: vec![1002],
            is_fm_group: false,
            child_armies: Vec::new(),
        });
        save.armies.push(ArmyData {
            id: 44,
            leader: Some(600),
            members: Vec::new(),
            is_fm_group: true,
            child_armies: vec![42, 43],
        });
        save
    }

    fn leader_trait_table() -> ModifierTable {
        ModifierTable::from_str(
            r#"{
                "unit_leader_traits": {
                    "panzer_leader": {
                        "army_armor_speed_factor": 0.05,
                        "army_armor_attack_factor": 0.16
                    },
                    "war_hero": {"army_infantry_defence_factor": 0.1},
                    "unyielding_defender": {"fm:defence": 0.1},
                    "aggressive_assaulter": {"fm:breakthrough_factor": 0.1},
                    "fm_infantry_officer": {"army_infantry_attack_factor": 0.2}
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn leader_bonus_resolves_chain_and_fm_halving() {
        // Vanilla numbers: skills 0.025/point, the FM's at
        // ×0.5 (FIELD_MARSHAL_ARMY_BONUS_RATIO); a general's `modifier`
        // traits full, a FM's `modifier` traits ×0.5, a FM's
        // `field_marshal_modifier` traits full and FM-holder-only.
        let save = leader_chain_save();
        let mods = leader_trait_table();

        // Division 1001: general 500 (attack 4 / defense 3; panzer_leader +
        // war_hero + an FM-only trait that must stay inert) under FM 600
        // (attack 5 / defense 2; fm:defence + fm:breakthrough + halved
        // infantry attack).
        let b = leader_bonus(&save, &mods, 1001);
        // attack = 0.025×4 + 0.5×0.025×5 = 0.1625
        assert!((b.attack - 0.1625).abs() < 1e-6, "attack={}", b.attack);
        // defense = 0.025×3 + 0.5×0.025×2 + 0.1 (fm:defence FULL) = 0.2 —
        // the general's own unyielding_defender copy contributes nothing.
        assert!((b.defense - 0.2).abs() < 1e-6, "defense={}", b.defense);
        assert!((b.breakthrough - 0.1).abs() < 1e-6, "bt={}", b.breakthrough);
        // Type maps: general's armor attack full (0.16; the speed key is
        // dropped), FM's infantry attack halved (0.2 → 0.1), general's
        // infantry defense full (0.1).
        assert_eq!(b.type_attack.len(), 2);
        assert!((b.type_attack["armor"] - 0.16).abs() < 1e-6);
        assert!((b.type_attack["infantry"] - 0.1).abs() < 1e-6);
        assert_eq!(b.type_defense.len(), 1);
        assert!((b.type_defense["infantry"] - 0.1).abs() < 1e-6);

        // Division 1002: leaderless army — only the FM side applies.
        let b = leader_bonus(&save, &mods, 1002);
        assert!((b.attack - 0.0625).abs() < 1e-6, "attack={}", b.attack);
        assert!((b.defense - 0.125).abs() < 1e-6, "defense={}", b.defense);
        assert!((b.breakthrough - 0.1).abs() < 1e-6, "bt={}", b.breakthrough);
        assert_eq!(b.type_attack.len(), 1);
        assert!((b.type_attack["infantry"] - 0.1).abs() < 1e-6);
        assert!(b.type_defense.is_empty());

        // Division in no army → neutral (§11.3 graceful chain break).
        let b = leader_bonus(&save, &mods, 9999);
        assert!(b.attack.abs() < 1e-6 && b.defense.abs() < 1e-6);
        assert!(b.type_attack.is_empty() && b.type_defense.is_empty());
        // An empty table degrades every trait silently.
        let b = leader_bonus(&save, &ModifierTable::default(), 1001);
        assert!(
            (b.attack - 0.1625).abs() < 1e-6,
            "skills survive: {}",
            b.attack
        );
        assert!(b.type_attack.is_empty());
    }

    #[test]
    fn leader_type_bonus_applies_per_battalion_family() {
        // The armor trait bonus lands on the tank (group
        // "armor") but not on the infantry; the cavalry key reaches the
        // cavalry battalion through the category alias even though its
        // template group is "mobile". Generic leader bonuses apply to all.
        let eq = EquipmentTable::from_str(tests_helpers::MINI_EQUIPMENT_JSON).unwrap();
        let ut = UnitTemplateTable::from_str(
            r#"{
                "line_battalions": {
                    "infantry": {"max_strength": 25, "max_organisation": 60,
                        "group": "infantry", "types": ["infantry"],
                        "categories": ["category_front_line", "category_all_infantry"],
                        "needs": {"infantry_equipment": 100}},
                    "light_armor": {"max_strength": 2, "max_organisation": 10,
                        "group": "armor", "types": ["armor"],
                        "categories": ["category_tanks", "category_all_armor"],
                        "needs": {"light_tank_chassis": 60}},
                    "cavalry": {"max_strength": 25, "max_organisation": 70,
                        "group": "mobile", "types": ["infantry"],
                        "categories": ["category_front_line", "category_cavalry"],
                        "needs": {"infantry_equipment": 120}}
                }
            }"#,
        )
        .unwrap();
        let save = r#"
countries = {
    GER = {
        division_template = {
            "Leader-Division" = {
                regiments = {
                    infantry = { x = 0 y = 0 }
                    light_armor = { x = 1 y = 0 }
                    cavalry = { x = 2 y = 0 }
                }
            }
        }
        units = {
            division = {
                id = 1
                name = "1. Leader-Division"
                division_template = "Leader-Division"
                organization = 1.0
                strength = 1.0
                equipment = {
                    infantry_equipment_0 = 220
                    light_tank_chassis_0 = 60
                }
            }
        }
    }
}
"#;
        let div = parse_first_division(save);
        let leader = LeaderBonus {
            attack: 0.1,
            defense: 0.2,
            breakthrough: 0.05,
            type_attack: [("armor".to_string(), 0.16), ("cavalry".to_string(), 0.12)]
                .into_iter()
                .collect(),
            type_defense: [("armor".to_string(), 0.1)].into_iter().collect(),
        };
        let units = create_tactical_units(
            &div,
            Side::Attacker,
            &eq,
            &ut,
            None,
            CountryOrgModifier::default(),
            CountryCombatModifier::default(),
            &leader,
            0,
        );
        let inf = units
            .iter()
            .find(|u| u.unit_type == UnitType::Infantry)
            .unwrap();
        let tank = units
            .iter()
            .find(|u| u.unit_type == UnitType::LightArmor)
            .unwrap();
        let cav = units
            .iter()
            .find(|u| u.unit_type == UnitType::Cavalry)
            .unwrap();
        // Infantry: generic only — soft 3 × 1.1 = 3.3, defense 20 × 1.2 = 24.
        assert!(
            (inf.soft_attack - 3.3).abs() < 1e-2,
            "soft={}",
            inf.soft_attack
        );
        assert!((inf.defense - 24.0).abs() < 1e-2, "def={}", inf.defense);
        assert!(
            (inf.breakthrough - 2.1).abs() < 1e-2,
            "bt={}",
            inf.breakthrough
        );
        // Tank: armor type bonus stacks with the generic — soft 10 × 1.26,
        // hard 8 × 1.26, defense 5 × (1.2 + 0.1), breakthrough 20 × 1.05.
        assert!(
            (tank.soft_attack - 12.6).abs() < 1e-2,
            "soft={}",
            tank.soft_attack
        );
        assert!(
            (tank.hard_attack - 10.08).abs() < 1e-2,
            "hard={}",
            tank.hard_attack
        );
        assert!((tank.defense - 6.5).abs() < 1e-2, "def={}", tank.defense);
        assert!(
            (tank.breakthrough - 21.0).abs() < 1e-2,
            "bt={}",
            tank.breakthrough
        );
        // Cavalry (group "mobile"): the cavalry trait key applies via the
        // category alias — soft 3 × (1.1 + 0.12); defense untouched.
        assert!(
            (cav.soft_attack - 3.66).abs() < 1e-2,
            "soft={}",
            cav.soft_attack
        );
        assert!((cav.defense - 24.0).abs() < 1e-2, "def={}", cav.defense);
    }
}
