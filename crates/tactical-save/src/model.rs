//! Data model for extracted save-game state (DESIGN.md §5.2).

use std::collections::HashMap;

use tactical_core::UnitType;

/// A parsed HOI4 save: root-level division templates plus per-country data.
///
/// Real text saves store `division_templates` at root level, so templates
/// live here rather than inside each country.
#[derive(Debug, Clone, Default)]
pub struct SaveGame {
    /// Countries keyed by tag ("GER", "FRA", ...).
    pub countries: HashMap<String, CountryData>,
    /// Root-level `division_templates` (§5.2), resolved by name or id.
    pub templates: Vec<TemplateData>,
    /// Root-level `player="TAG"` — the played country in a single-player
    /// save: the mod cannot print its tag through HOI4's log interpolation,
    /// so the live loop takes it from the save instead.
    pub player: Option<String>,
    /// Root `date = "1936.1.1.13"` header as `(year, month, day, hour)`.
    /// Besides the leader-role expire comparisons, the live battle clock
    /// starts from this in-game time.
    pub date: Option<(i32, u32, u32, u32)>,
    /// Ongoing land battles (`combat = { land_combat = { ... } }`): the
    /// save's authoritative record of running battles — contested province,
    /// both sides' division ids, tactic ids and participating tags. 1.19.2
    /// serializes these even though wars are not replayed.
    pub land_combats: Vec<LandCombatData>,
    /// States carrying `tac_pick=1` in their `variables` block: the
    /// state-target entry decision marks the picked battle state with this
    /// variable; the post-tac_start snapshot locates the player's pick here.
    /// Usually 0 or 1 entries.
    pub picked_states: Vec<u32>,
    /// Unit leaders (generals and field marshals) keyed by leader INSTANCE
    /// id (type 4713) — the join key used by army/army-group `leader`
    /// references. Read from the `corps_commander` / `field_marshal` blocks
    /// of `character_manager.historical.character`; `navy_leader` blocks
    /// share the id space but are not collected.
    pub leaders: HashMap<u64, LeaderData>,
    /// Armies (`orders_group`) and army groups (`field_marshal_group`)
    /// collected from every country's `theatres` block. The command chain
    /// resolves through these: division id → army member list → army
    /// `leader` (general) → the group listing that army in `child_armies`
    /// → its `leader` (field marshal).
    pub armies: Vec<ArmyData>,
}

/// A unit leader (division commander) extracted from the character database:
/// a general's `corps_commander` block or a field marshal's
/// `field_marshal` block. Skills are the save's serialized ints (already
/// including trait grants); `traits` are the bare-token list.
#[derive(Debug, Clone, Default)]
pub struct LeaderData {
    pub attack_skill: f32,
    pub defense_skill: f32,
    pub traits: Vec<String>,
    /// Parsed from a `field_marshal` block (vs `corps_commander`).
    pub is_field_marshal: bool,
}

/// One army (`orders_group`) or army group (`field_marshal_group`) from a
/// country's `theatres` block. Every field degrades gracefully: a missing
/// `leader` key is `None` (leaderless army), missing members/child refs are
/// empty.
#[derive(Debug, Clone, Default)]
pub struct ArmyData {
    /// The group's own id (`id = { id = N type = 53 }`; 0 when absent).
    pub id: u64,
    /// Commanding leader instance id (`leader = { id = L type = 4713 }`).
    pub leader: Option<u64>,
    /// Member division ids (`member = { unit = { id = D type = 51 } }`) —
    /// armies only; empty on army groups.
    pub members: Vec<u64>,
    /// Parsed from a `field_marshal_group` block (vs `orders_group`).
    pub is_fm_group: bool,
    /// Child army ids (bare `orders_group = { id = N ... }` refs) — army
    /// groups only; empty on armies.
    pub child_armies: Vec<u64>,
}

/// One side of an ongoing HOI4 land battle (`land_combat.attacker` /
/// `.defender`).
#[derive(Debug, Clone, Default)]
pub struct LandCombatSideData {
    /// Participating division ids (`unit = { id = N type = 51 }`).
    pub unit_ids: Vec<u64>,
    /// HOI4 combat tactic id — 1-based definition index into
    /// `common/combat_tactics.txt` (see [`COMBAT_TACTIC_IDS`]).
    pub tactic: Option<u32>,
    /// Participating country tags (`log.combat_side_data.tags`).
    pub tags: Vec<String>,
}

/// An ongoing HOI4 land battle (`combat = { land_combat = { ... } }`).
#[derive(Debug, Clone, Default)]
pub struct LandCombatData {
    /// The contested province (`location`).
    pub location: u32,
    pub attacker: LandCombatSideData,
    pub defender: LandCombatSideData,
}

/// Vanilla 1.19.2 `common/combat_tactics.txt` definition order, 1-based —
/// the save's `tactic = N` indexes this list. Base verified by game logic
/// (an attacker can never hold the defender-only `tactical_withdrawal`;
/// tac_probe.hoi4 showed attacker `tactic = 12` = `shock`). Values are
/// tokens for `tactical_ai::CombatTactic::from_str` after stripping the
/// file's `tactic_` prefix. Mods that reorder/insert tactics shift ids;
/// out-of-range ids fall back to the default card.
pub const COMBAT_TACTIC_IDS: &[&str] = &[
    "basic_attack",
    "basic_defend",
    "counterattack",
    "assault",
    "cc_attack",
    "cc_defend",
    "cc_storm",
    "cc_local_strong_point",
    "cc_withdraw",
    "encirclement",
    "delay",
    "shock",
    "tactical_withdrawal",
    "tw_attack",
    "tw_defend",
    "tw_chase",
    "tw_evade",
    "tw_intercept",
    "breakthrough",
    "ambush",
    "blitz",
    "elastic_defense",
    "backhand_blow",
    "seize_bridge",
    "attacker_sb_hold",
    "attacker_sb_skillful_defence",
    "defender_sb_assault",
    "defender_sb_reckless_assault",
    "defender_sb_retake_bridge",
    "hold_bridge",
    "attacker_hb_attack",
    "attacker_hb_rush",
    "attacker_hb_storm",
    "defender_hb_hold",
    "defender_hb_skillful_defence",
    "guerrilla_tactics",
    "human_wave_tactics",
    "banzai_charge",
    "grand_banzai_charge",
    "infantry_charge",
    "planned_attack",
    "relentless_assault",
    "unexpected_thrust",
    "overwhelming_fire",
    "barrage",
    "masterful_blitz",
    "masterful_delay",
    "urban_defense",
    "sf_storm",
    "sf_barrage",
    "sf_armor_supported_assault",
    "sf_mouse_holing",
    "sf_defense",
    "sf_fortify",
    "sf_ambush",
];

/// A division template: which subunits (regiments + support companies) a
/// division of this type contains (§5.2 `division_template`).
#[derive(Debug, Clone)]
pub struct TemplateData {
    /// Template id from `id = { id = N ... }` (present in real saves).
    pub id: Option<u64>,
    pub name: String,
    /// `division_names_group` tag (real saves; e.g. `GER_Inf_01`) — the
    /// names-group a division of this template draws its auto name from.
    pub division_names_group: Option<String>,
    /// Line regiments expanded to (type, count) pairs.
    pub battalions: Vec<BattalionInfo>,
    pub support_companies: Vec<BattalionInfo>,
}

/// Per-country extracted state (§5.2).
#[derive(Debug, Clone)]
pub struct CountryData {
    pub tag: String,
    pub divisions: Vec<DivisionData>,
    /// Researched technology tokens (incl. doctrine techs; §5.1 tech level).
    pub technologies: Vec<String>,
    /// Active national spirit tokens (both the legacy `active_ideas` and the
    /// 1.19 bare-token `ideas = { tok1 tok2 }` save keys feed this list).
    pub active_ideas: Vec<String>,
    /// Enabled dynamic modifiers: `(definition token, current values)` where
    /// the values are the save's bare float array in the definition's
    /// modifier-key order — resolve org keys by index through
    /// [`crate::ModifierTable::dynamic_keys`]. Only `enabled = yes` entries
    /// are collected.
    pub dynamic_modifiers: Vec<(String, Vec<f32>)>,
    /// Trait tokens of the country's UNEXPIRED country-leader roles: read
    /// from the `character_manager.historical` character blocks carrying
    /// `country = "TAG"` plus a `country_leaders` role block whose `expire`
    /// date lies past the save date. Approximation: ALL unexpired roles'
    /// traits aggregate — the ruling-ideology check that picks a single
    /// in-power leader in HOI4 is not resolved.
    pub leader_traits: Vec<String>,
    /// Idea tokens of the country's APPOINTED advisors: the country block's
    /// `appointed_advisors` entries (slot + character id) are resolved
    /// through the character database's `advisors` role blocks — the
    /// same-slot role's `idea_token` wins, otherwise the first role block's
    /// token (documented fallback). Advisor tokens ARE idea tokens: their
    /// modifiers resolve through the ideas table.
    pub active_advisors: Vec<String>,
}

/// One HOI4 division extracted from the save (§5.2 `division`).
///
/// `organization` and `strength` are ABSOLUTE values in 1.19 saves (ratio
/// form ≤ 1 is also accepted — synthetic/script paths; the builder
/// disambiguates per field pair, see `create_tactical_units`). `strength`
/// carries the manpower side only — equipment fill is NOT blended into it.
/// `battalions`/`support_companies` are resolved from the division's
/// template regiments; `equipment` holds the division's actual on-hand counts.
#[derive(Debug, Clone)]
pub struct DivisionData {
    pub id: u64,
    pub name: String,
    /// Name of the template this division was built from (may be unresolved).
    pub template_name: String,
    /// §5.2 auto-name tokens: the template's `division_names_group` tag and
    /// the division's `name_order` issue number within it — resolve through
    /// [`crate::NameGroups`] for the in-game name. `name_order` is None for
    /// player-renamed divisions (their `override` already won in `name`)
    /// and for synthetic fixtures.
    pub names_group: Option<String>,
    pub name_order: Option<u32>,
    /// Province the division currently stands in (`location` in the save).
    pub location: Option<u32>,
    /// Current organisation ratio (0..=1).
    pub organization: f32,
    /// Current strength ratio (0..=1).
    pub strength: f32,
    pub experience: f32,
    pub entrenchment: f32,
    /// Strategic supply ratio if recorded in the save (§6.7 input).
    pub supply_status: Option<f32>,
    /// Actual equipment on hand: equipment key → count (§5.2).
    pub equipment: HashMap<String, f32>,
    /// Line battalions resolved from the division template's regiments.
    pub battalions: Vec<BattalionInfo>,
    /// Support companies resolved from the division template's support.
    pub support_companies: Vec<BattalionInfo>,
}

/// One subunit entry of a template: `count` battalions of one type.
///
/// `token` is the original HOI4 subunit token as written in the save
/// ("infantry", "light_armor", "artillery_brigade", ...). Unknown tokens fall
/// back to `UnitType::Infantry` but keep their `token` as a log-friendly
/// marker of what was actually found (§5.3 fallback rule). `chassis` and
/// `extra_attrs` complete the battalion classification (type ⊕ chassis ⊕
/// token flags).
#[derive(Debug, Clone)]
pub struct BattalionInfo {
    pub token: String,
    pub unit_type: UnitType,
    pub chassis: tactical_core::Chassis,
    pub extra_attrs: tactical_core::Attrs,
    pub count: usize,
}
