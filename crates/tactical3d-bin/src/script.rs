//! Battle scripts: battle setup as DATA FILES
//! instead of hardcoded presets. A `.json` file in `data/battles/` carries
//! the whole battle — province, attack directions, both sides' complete
//! battalion rosters with historical names/stats, tags, enemy tactic and
//! player side. This mirrors the HOI4→mod file channel (JSON via
//! `game.log`) in reverse: the game READS a file to generate a battle, so
//! test battles are data, not code. The shipped battles are one script
//! file each (further drafted battles were shelved — files removed, git
//! history can restore them);
//! `--battle file=<name>` or the menu Debug Battle form start them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use tactical3d_render::game::AllyContingent;
use tactical_ai::CombatTactic;
use tactical_core::hex::HexCoord;
use tactical_core::hex::HexDirection;
use tactical_core::unit::{BattalionUnit, Chassis, Side, SupportAttachment, SupportKind, UnitType};

/// `data/battles/` under the runtime root (next to the exe in shipped
/// packages, workspace root in dev — see [`crate::dirs`]).
pub fn scripts_dir() -> PathBuf {
    crate::dirs::runtime_root().join("data").join("battles")
}

/// Resolve a script name (`1939_warsaw` or `1939_warsaw.json`) to a path.
/// Sub-paths are allowed so scripts can be organized (still under
/// `data/battles/`).
pub fn resolve(name: &str) -> PathBuf {
    let trimmed = name.trim();
    let p = if trimmed.ends_with(".json") {
        scripts_dir().join(trimmed)
    } else {
        scripts_dir().join(format!("{trimmed}.json"))
    };
    p
}

/// All script files under `data/battles/`, sorted by name (menu dropdown).
pub fn list_scripts() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(scripts_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Schema

#[derive(Debug, Deserialize)]
pub struct ScriptFile {
    /// Metadata for human readers / future menu display; the menu lists
    /// scripts by file stem, and `assemble` uses province/dirs/rosters
    /// only — so these are read by validate(), not by the battle.
    #[allow(dead_code)]
    pub name: String,
    /// Optional display title (e.g. "Siege of Warsaw, Sept 1939").
    #[allow(dead_code)]
    #[serde(default)]
    pub title: String,
    /// Free-form historical notes (read by humans, ignored by the game).
    #[allow(dead_code)]
    #[serde(default)]
    pub notes: String,
    /// Battle province id (HOI4 definition.csv, verify with battle_scan.py).
    /// Absent when `map` selects a synthetic map.
    #[serde(default)]
    pub province: Option<u32>,
    /// "synthetic" = the flat 64×64 arena instead of a real
    /// province (balance-experiment battles). Absent = province map.
    #[serde(default)]
    pub map: Option<String>,
    /// Attack directions, e.g. ["W", "SW"] (HexDirection tokens). May be
    /// empty for `map = "synthetic"` (the arena always attacks W → E).
    pub dirs: Vec<String>,
    /// Which side the player commands: "attacker" | "defender".
    #[serde(default = "default_player_side")]
    pub player_side: String,
    /// Enemy (AI) tactic token — the CombatTactic token set ("blitz",
    /// "elastic_defense", "overwhelming_fire", …).
    #[serde(default = "default_tactic")]
    pub enemy_tactic: String,
    /// §6.11: explicit flag anchors for FIELD battles (battles
    /// without a VP-city urban cluster) — per-battle historical key points,
    /// e.g. the fort belt outside a town. City battles derive their single
    /// flag from the urban cluster and ignore this field; absent → the
    /// deep-band fallback sampling.
    #[serde(default)]
    pub flags: Vec<ScriptFlag>,
    pub attacker: ScriptSide,
    pub defender: ScriptSide,
}

/// One field-battle flag anchor (axial grid hex; the flag zone is the
/// radius-`flag_cluster_radius` cluster around it, clipped to the defender
/// zone).
#[derive(Debug, Deserialize)]
pub struct ScriptFlag {
    pub q: i32,
    pub r: i32,
}

#[derive(Debug, Deserialize)]
pub struct ScriptSide {
    /// Country tag (theme colors via country_colors.json).
    pub tag: String,
    /// Division/formation name shown in the Order of Battle tree.
    pub division: String,
    /// Per-division control/tag declarations. Absent = every
    /// division player-commanded under the side tag (all 18 legacy scripts).
    #[serde(default)]
    pub divisions: Vec<ScriptDivision>,
    /// Full battalion roster — every stat is explicit (no preset fallback).
    pub units: Vec<ScriptUnit>,
}

/// DESIGN §7.5: optional per-division declaration — country tag,
/// control owner, and (for AI divisions) the tactic card its planner uses.
#[derive(Debug, Deserialize)]
pub struct ScriptDivision {
    pub name: String,
    /// Country tag; absent = the side's `tag`.
    #[serde(default)]
    pub tag: Option<String>,
    /// "player" (default) | "ai" — who commands this division.
    #[serde(default = "default_control_player")]
    pub control: String,
    /// CombatTactic token for AI-controlled divisions; absent = side-based
    /// default (attacker → assault, defender → elastic_defense).
    #[serde(default)]
    pub tactic: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ScriptUnit {
    /// Unit name for the counter / OOB tree (e.g. "1./Pz.Rgt.36").
    pub name: String,
    /// Battalion type token (see `unit_type_from_token`).
    #[serde(rename = "type")]
    pub unit_type: String,
    /// Per-unit division override: a side may field several
    /// divisions — e.g. the Warsaw attacker carries both the 4. Panzer
    /// Vorhut and the 31. Infanterie-Division; absent = side.division.
    #[serde(default)]
    pub division: Option<String>,
    // Combat stats (HOI4 1939 calibration, same scale as the
    // built-in presets; tune freely per battle).
    pub soft: f32,
    pub hard: f32,
    pub defense: f32,
    pub breakthrough: f32,
    pub armor: f32,
    pub piercing: f32,
    pub hardness: f32,
    /// Chassis token (see `chassis_from_token`); absent = the type default.
    #[serde(default)]
    pub chassis: Option<String>,
    /// Under-strength battalion: a fraction of the type's full
    /// org/strength — used for battle-weary remnants and militia.
    /// Absent = full strength.
    #[serde(default)]
    pub org: Option<f32>,
    #[serde(default)]
    pub strength: Option<f32>,
    /// Support companies riding with this battalion.
    #[serde(default)]
    pub supports: Vec<ScriptSupport>,
}

#[derive(Debug, Deserialize)]
pub struct ScriptSupport {
    /// Support kind token (see `support_kind_from_token`).
    pub kind: String,
    /// Display name in the OOB tree / hover tooltip (e.g. "Aufkl").
    pub name: String,
}

fn default_player_side() -> String {
    "attacker".into()
}
fn default_tactic() -> String {
    "default".into()
}
fn default_control_player() -> String {
    "player".into()
}

// ---------------------------------------------------------------------------
// Token tables (Rust enum names in snake_case; unknown tokens are errors so
// a typo in a script fails loudly at load, not silently mid-battle)

fn unit_type_from_token(tok: &str) -> Option<UnitType> {
    Some(match tok.trim().to_ascii_lowercase().as_str() {
        "infantry" => UnitType::Infantry,
        "marine" => UnitType::Marine,
        "mountaineer" => UnitType::Mountaineer,
        "paratrooper" => UnitType::Paratrooper,
        "cavalry" => UnitType::Cavalry,
        "bicycle" => UnitType::Bicycle,
        "motorized" => UnitType::Motorized,
        "mechanized" => UnitType::Mechanized,
        "light_armor" => UnitType::LightArmor,
        "medium_armor" => UnitType::MediumArmor,
        "heavy_armor" => UnitType::HeavyArmor,
        "super_heavy_armor" => UnitType::SuperHeavyArmor,
        "modern_armor" => UnitType::ModernArmor,
        "amphibious_armor" => UnitType::AmphibiousArmor,
        "artillery" => UnitType::ArtilleryBrigade,
        "rocket_artillery" => UnitType::RocketArtillery,
        "mot_rocket_artillery" => UnitType::MotRocketArtillery,
        "anti_tank" => UnitType::AntiTankBrigade,
        "anti_air" => UnitType::AntiAirBrigade,
        "engineer" => UnitType::Engineer,
        "recon" => UnitType::Recon,
        "signal" => UnitType::Signal,
        "logistics" => UnitType::Logistics,
        "maintenance" => UnitType::Maintenance,
        "field_hospital" => UnitType::FieldHospital,
        "military_police" => UnitType::MilitaryPolice,
        _ => return None,
    })
}

fn chassis_from_token(tok: &str) -> Option<Chassis> {
    Some(match tok.trim().to_ascii_lowercase().as_str() {
        "none" => Chassis::None,
        "towed" => Chassis::Towed,
        "truck_towed" => Chassis::TruckTowed,
        "wheeled" => Chassis::Wheeled,
        "halftrack" => Chassis::Halftrack,
        "light" => Chassis::Light,
        "medium" => Chassis::Medium,
        "heavy" => Chassis::Heavy,
        "super_heavy" => Chassis::SuperHeavy,
        "modern" => Chassis::Modern,
        _ => return None,
    })
}

fn support_kind_from_token(tok: &str) -> Option<SupportKind> {
    Some(match tok.trim().to_ascii_lowercase().as_str() {
        "anti_tank" => SupportKind::AntiTank,
        "anti_air" => SupportKind::AntiAir,
        "artillery" => SupportKind::Artillery,
        "engineer" => SupportKind::Engineer,
        "recon" => SupportKind::Recon,
        "field_hospital" => SupportKind::FieldHospital,
        "signal" => SupportKind::Signal,
        "maintenance" => SupportKind::Maintenance,
        "logistics" => SupportKind::Logistics,
        "military_police" => SupportKind::MilitaryPolice,
        _ => return None,
    })
}

/// "attacker" | "defender" → Side (None for anything else).
pub fn player_side(tok: &str) -> Option<Side> {
    match tok.trim().to_ascii_lowercase().as_str() {
        "attacker" => Some(Side::Attacker),
        "defender" => Some(Side::Defender),
        _ => None,
    }
}

// The whitelist covers every vanilla HOI4 tactic token plus
// the card names that have no vanilla token (mass_charge /
// infiltration_assault / default) — a script may name either the card
// ("counterattack") or the exact HOI4 roll ("backhand_blow"); both land
// on the same card. CombatTactic::from_str is an infallible mapping with
// a Default fallback, so strict script validation happens against THIS
// list (used by enemy_tactic and per-division tactics alike).
const TACTIC_TOKENS: [&str; 58] = [
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
    "infiltration_assault",
    "mass_charge",
    "default",
];

// ---------------------------------------------------------------------------
// Loading + validation

/// Load and fully validate a script file (token mapping, direction
/// tokens, tactic token). All errors are user-readable strings.
pub fn load(path: &Path) -> Result<ScriptFile, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("script {}: {e}", path.display()))?;
    let sf: ScriptFile =
        serde_json::from_str(&text).map_err(|e| format!("script {}: {e}", path.display()))?;
    // Validate everything that has a token table; also convert to the
    // final types so assemble() cannot fail mid-deployment.
    validate(&sf, path)?;
    Ok(sf)
}

/// Token checks only (no file I/O): used by tests and by `load`.
pub fn validate(sf: &ScriptFile, path: &Path) -> Result<(), String> {
    let where_ = |what: &str| format!("script {} ({what})", path.display());
    // Exactly one map source — province for a province battle, or
    // map = "synthetic" for the flat arena. The arena always attacks W → E,
    // so dirs may be empty there.
    let synthetic = match sf.map.as_deref() {
        None => false,
        Some("synthetic") => true,
        Some(other) => {
            return Err(where_(&format!("bad map '{other}' (only \"synthetic\")")));
        }
    };
    if synthetic != sf.province.is_none() {
        return Err(where_(
            "exactly one map source: province for a province battle, or map = \"synthetic\"",
        ));
    }
    for (i, d) in sf.dirs.iter().enumerate() {
        if HexDirection::from_token(d).is_none() {
            return Err(where_(&format!(
                "bad dirs[{i}] '{d}' (use E/NE/NW/W/SW/SE)"
            )));
        }
    }
    if sf.dirs.is_empty() && !synthetic {
        return Err(where_("no attack directions (dirs must be non-empty)"));
    }
    if player_side(&sf.player_side).is_none() {
        return Err(where_(&format!(
            "bad player_side '{}' (use \"attacker\"|\"defender\")",
            sf.player_side
        )));
    }
    // Flag anchors are grid hexes — a coordinate outside any
    // plausible grid (grids are capped at 512×512) is a typo, not a flag.
    // Off-grid-but-valid coords are dropped gracefully at derivation.
    for (i, f) in sf.flags.iter().enumerate() {
        if f.q.abs() > 4096 || f.r.abs() > 4096 {
            return Err(where_(&format!("bad flags[{i}] hex ({}, {})", f.q, f.r)));
        }
    }
    let t = sf.enemy_tactic.trim().to_ascii_lowercase();
    if !TACTIC_TOKENS.contains(&t.as_str()) {
        return Err(where_(&format!(
            "bad enemy_tactic '{}' (use one of {})",
            sf.enemy_tactic,
            TACTIC_TOKENS.join("|")
        )));
    }
    for (side_name, side) in [("attacker", &sf.attacker), ("defender", &sf.defender)] {
        if side.units.is_empty() {
            return Err(where_(&format!("{side_name} has no units")));
        }
        for u in &side.units {
            if unit_type_from_token(&u.unit_type).is_none() {
                return Err(where_(&format!(
                    "{side_name} unit '{}' has unknown type '{}'",
                    u.name, u.unit_type
                )));
            }
            if let Some(c) = &u.chassis {
                if chassis_from_token(c).is_none() {
                    return Err(where_(&format!(
                        "{side_name} unit '{}' has unknown chassis '{}'",
                        u.name, c
                    )));
                }
            }
            for s in &u.supports {
                if support_kind_from_token(&s.kind).is_none() {
                    return Err(where_(&format!(
                        "{side_name} unit '{}' has unknown support kind '{}'",
                        u.name, s.kind
                    )));
                }
            }
        }
        // Per-division declarations must name a division this
        // side actually fields (typo guard — each unit's effective division
        // is unit.division, falling back to side.division), carry a valid
        // control owner, and use a real tactic token.
        let roster_divs: std::collections::HashSet<String> = side
            .units
            .iter()
            .map(|u| u.division.clone().unwrap_or_else(|| side.division.clone()))
            .collect();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (i, d) in side.divisions.iter().enumerate() {
            let c = d.control.trim().to_ascii_lowercase();
            if c != "player" && c != "ai" {
                return Err(where_(&format!(
                    "bad {side_name} divisions[{i}] control '{}' (use \"player\"|\"ai\")",
                    d.control
                )));
            }
            if let Some(tok) = &d.tactic {
                let t = tok.trim().to_ascii_lowercase();
                if !TACTIC_TOKENS.contains(&t.as_str()) {
                    return Err(where_(&format!(
                        "bad {side_name} divisions[{i}] tactic '{tok}' (use one of {})",
                        TACTIC_TOKENS.join("|")
                    )));
                }
            }
            if !roster_divs.contains(&d.name) {
                return Err(where_(&format!(
                    "{side_name} divisions[{i}] names unknown division '{}' (no roster unit carries it)",
                    d.name
                )));
            }
            if !seen.insert(d.name.as_str()) {
                return Err(where_(&format!(
                    "{side_name} divisions[{i}] duplicates division '{}'",
                    d.name
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deployment

/// Spread a script roster across its deployment zone (same wrap rule as
/// the built-in presets: one hex per unit while capacity lasts).
/// `templates` feeds the per-battalion vanilla terrain adjusters
/// via canonical template keys — scripts carry no HOI4 token;
/// None (tests, missing table) = zero adjusters.
pub fn deploy_script_side(
    units: &mut Vec<BattalionUnit>,
    next_id: &mut usize,
    side: Side,
    script: &ScriptSide,
    zone: &[HexCoord],
    templates: Option<&tactical_save::UnitTemplateTable>,
) -> Result<(), String> {
    for (i, su) in script.units.iter().enumerate() {
        let pos = if zone.is_empty() {
            HexCoord::ZERO
        } else {
            zone[i % zone.len()]
        };
        let ut = unit_type_from_token(&su.unit_type).expect("validated in load");
        let mut u = BattalionUnit::new(*next_id, su.name.clone(), ut, side, pos);
        u.terrain_adj = templates
            .map(|t| t.terrain_adjusters_for(ut))
            .unwrap_or_default();
        u.division = su
            .division
            .clone()
            .unwrap_or_else(|| script.division.clone());
        u.soft_attack = su.soft;
        u.hard_attack = su.hard;
        u.defense = su.defense;
        u.breakthrough = su.breakthrough;
        u.armor = su.armor;
        u.piercing = su.piercing;
        u.hardness = su.hardness;
        if let Some(c) = &su.chassis {
            u.set_chassis(chassis_from_token(c).expect("validated in load"));
        }
        // Under-strength override: scale max AND current so the ratio stays
        // consistent (a 40-org remnant can't regen above its reduced max).
        if let Some(o) = su.org {
            let o = o.clamp(1.0, u.max_org);
            u.max_org = o;
            u.org = o;
        }
        if let Some(s) = su.strength {
            let s = s.clamp(1.0, u.max_strength);
            u.max_strength = s;
            u.strength = s;
        }
        for ss in &su.supports {
            u.attach(SupportAttachment {
                kind: support_kind_from_token(&ss.kind).expect("validated in load"),
                name: ss.name.clone(),
            });
        }
        *next_id += 1;
        units.push(u);
    }
    // §6.13: one synthesized HQ per division, taking the zone
    // slots right after the roster (same wrap rule).
    let base = script.units.len();
    tactical_core::synthesize_hqs(units, next_id, side, |n| {
        if zone.is_empty() {
            HexCoord::ZERO
        } else {
            zone[(base + n) % zone.len()]
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Allied contingents (DESIGN §7.5)

/// Division name → country tag for BOTH sides (block tag wins, side tag
/// default). The roster is the source of truth for the division set: every
/// effective division (unit.division, falling back to side.division) maps to
/// the side tag unless a `divisions:` block entry re-tags it.
pub fn division_tag_map(attacker: &ScriptSide, defender: &ScriptSide) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for side in [attacker, defender] {
        for u in &side.units {
            let div = u.division.clone().unwrap_or_else(|| side.division.clone());
            out.entry(div).or_insert_with(|| side.tag.clone());
        }
        for d in &side.divisions {
            if let Some(tag) = &d.tag {
                out.insert(d.name.clone(), tag.clone());
            }
        }
    }
    out
}

/// The player side's control=="ai" divisions grouped by resolved tag, in
/// first-appearance order (the first division of a tag carries the
/// contingent's tactic). Tactic: the explicit token via
/// CombatTactic::from_str; absent → Assault when the player attacks,
/// ElasticDefense when she defends.
pub fn allied_contingents(side: &ScriptSide, player_side: Side) -> Vec<AllyContingent> {
    let default_tactic = match player_side {
        Side::Attacker => CombatTactic::Assault,
        Side::Defender => CombatTactic::ElasticDefense,
    };
    let mut out: Vec<AllyContingent> = Vec::new();
    for d in &side.divisions {
        if !d.control.trim().eq_ignore_ascii_case("ai") {
            continue;
        }
        let tag = d.tag.clone().unwrap_or_else(|| side.tag.clone());
        match out.iter_mut().find(|a| a.tag == tag) {
            Some(a) => a.divisions.push(d.name.clone()),
            None => out.push(AllyContingent {
                tag,
                tactic: d
                    .tactic
                    .as_deref()
                    .map(CombatTactic::from_str)
                    .unwrap_or(default_tactic),
                divisions: vec![d.name.clone()],
            }),
        }
    }
    out
}

/// Apply the menu nation selector's per-tag command overrides
/// onto a script side — every division whose RESOLVED tag (block tag, side
/// tag fallback) appears in `overrides` (tag → player-controlled) gets its
/// `control` rewritten to "player"/"ai". Unknown tags are a silent no-op
/// (the selector only offers tags the script actually fields). Must run
/// BEFORE `division_tag_map` / `allied_contingents` so the command split,
/// the tag table and the contingent grouping all agree.
pub fn apply_control_overrides(side: &mut ScriptSide, overrides: &HashMap<String, bool>) {
    if overrides.is_empty() {
        return;
    }
    for d in &mut side.divisions {
        let tag = d.tag.clone().unwrap_or_else(|| side.tag.clone());
        if let Some(player) = overrides.get(&tag) {
            d.control = if *player {
                "player".to_string()
            } else {
                "ai".to_string()
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "name": "1939_warsaw",
        "title": "Siege of Warsaw, Sept 1939",
        "province": 3544,
        "dirs": ["W", "SW"],
        "player_side": "attacker",
        "enemy_tactic": "elastic_defense",
        "attacker": {
            "tag": "GER",
            "division": "4. Panzer-Division Vorhut",
            "units": [
                { "name": "1.Pz.III", "type": "medium_armor", "soft": 19.0, "hard": 14.0,
                  "defense": 5.0, "breakthrough": 36.0, "armor": 60.0, "piercing": 61.0,
                  "hardness": 0.9, "supports": [{ "kind": "recon", "name": "Aufkl" }] }
            ]
        },
        "defender": {
            "tag": "POL",
            "division": "Armia Warszawa (Czuma)",
            "units": [
                { "name": "1.Inf", "type": "infantry", "soft": 6.0, "hard": 1.0,
                  "defense": 22.0, "breakthrough": 3.0, "armor": 0.0, "piercing": 4.0,
                  "hardness": 0.0, "chassis": "none" }
            ]
        }
    }"#;

    fn sample_script() -> ScriptFile {
        serde_json::from_str(SAMPLE).unwrap()
    }

    #[test]
    fn sample_parses_and_validates() {
        let sf = sample_script();
        validate(&sf, Path::new("1939_warsaw.json")).unwrap();
        assert_eq!(sf.province, Some(3544));
        assert_eq!(sf.attacker.division, "4. Panzer-Division Vorhut");
        assert_eq!(sf.defender.units[0].name, "1.Inf");
    }

    #[test]
    fn unknown_type_token_fails_validation() {
        let mut sf = sample_script();
        sf.attacker.units[0].unit_type = "pzvi_tiger".into();
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(err.contains("unknown type 'pzvi_tiger'"), "{err}");
    }

    #[test]
    fn unknown_direction_token_fails_validation() {
        let mut sf = sample_script();
        sf.dirs = vec!["XX".into()];
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(err.contains("bad dirs[0] 'XX'"), "{err}");
    }

    #[test]
    fn empty_side_fails_validation() {
        let mut sf = sample_script();
        sf.defender.units.clear();
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(err.contains("defender has no units"), "{err}");
    }

    /// A script may fight on the flat arena instead of a province.
    #[test]
    fn synthetic_map_parses_and_validates() {
        let mut sf = sample_script();
        sf.province = None;
        sf.map = Some("synthetic".into());
        sf.dirs.clear(); // the arena always attacks W → E
        validate(&sf, Path::new("x.json")).unwrap();
    }

    #[test]
    fn province_and_map_conflict_fails_validation() {
        let mut sf = sample_script();
        sf.map = Some("synthetic".into()); // province still set
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(err.contains("exactly one map source"), "{err}");
    }

    #[test]
    fn missing_map_source_fails_validation() {
        let mut sf = sample_script();
        sf.province = None;
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(err.contains("exactly one map source"), "{err}");
    }

    #[test]
    fn unknown_map_token_fails_validation() {
        let mut sf = sample_script();
        sf.province = None;
        sf.map = Some("flat_earth".into());
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(err.contains("bad map 'flat_earth'"), "{err}");
    }

    /// A side may declare per-division control — here the second
    /// division is an AI-controlled ally with its own tag and tactic card.
    const SAMPLE_DIVISIONS: &str = r#"{
        "name": "enh0055_allied",
        "province": 306,
        "dirs": ["E"],
        "player_side": "attacker",
        "enemy_tactic": "elastic_defense",
        "attacker": {
            "tag": "GER",
            "division": "1. Infanterie-Division",
            "divisions": [
                { "name": "1. Infanterie-Division" },
                { "name": "1re Division Légère", "tag": "FRA", "control": "ai", "tactic": "assault" }
            ],
            "units": [
                { "name": "1.Inf", "type": "infantry", "soft": 6.0, "hard": 1.0,
                  "defense": 22.0, "breakthrough": 3.0, "armor": 0.0, "piercing": 4.0,
                  "hardness": 0.0 },
                { "name": "1.Pz", "type": "light_armor", "division": "1re Division Légère",
                  "soft": 10.0, "hard": 6.0, "defense": 4.0, "breakthrough": 18.0,
                  "armor": 30.0, "piercing": 30.0, "hardness": 0.6 }
            ]
        },
        "defender": {
            "tag": "POL",
            "division": "Armia Poznań",
            "units": [
                { "name": "2.Inf", "type": "infantry", "soft": 6.0, "hard": 1.0,
                  "defense": 22.0, "breakthrough": 3.0, "armor": 0.0, "piercing": 4.0,
                  "hardness": 0.0 }
            ]
        }
    }"#;

    fn sample_with_divisions() -> ScriptFile {
        serde_json::from_str(SAMPLE_DIVISIONS).unwrap()
    }

    #[test]
    fn divisions_block_parses_with_defaults() {
        let sf = sample_with_divisions();
        validate(&sf, Path::new("x.json")).unwrap();
        assert_eq!(sf.attacker.divisions.len(), 2);
        let own = &sf.attacker.divisions[0];
        assert_eq!(own.name, "1. Infanterie-Division");
        assert_eq!(own.control, "player", "absent control defaults to player");
        assert!(own.tag.is_none(), "absent tag inherits the side tag");
        assert!(own.tactic.is_none(), "absent tactic = side-based default");
        let ally = &sf.attacker.divisions[1];
        assert_eq!(ally.control, "ai");
        assert_eq!(ally.tag.as_deref(), Some("FRA"));
        assert_eq!(ally.tactic.as_deref(), Some("assault"));
    }

    #[test]
    fn unknown_division_name_fails_validation() {
        let mut sf = sample_with_divisions();
        sf.attacker.divisions[0].name = "99. Gespenster-Division".into();
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(
            err.contains("attacker divisions[0] names unknown division '99. Gespenster-Division'"),
            "{err}"
        );
    }

    #[test]
    fn bad_control_fails_validation() {
        let mut sf = sample_with_divisions();
        sf.attacker.divisions[1].control = "skynet".into();
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(
            err.contains("bad attacker divisions[1] control 'skynet'"),
            "{err}"
        );
    }

    #[test]
    fn bad_division_tactic_fails_validation() {
        let mut sf = sample_with_divisions();
        sf.attacker.divisions[1].tactic = Some("zerg_rush".into());
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(
            err.contains("bad attacker divisions[1] tactic 'zerg_rush'"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_division_name_fails_validation() {
        let mut sf = sample_with_divisions();
        sf.attacker.divisions[1].name = "1. Infanterie-Division".into();
        let err = validate(&sf, Path::new("x.json")).unwrap_err();
        assert!(
            err.contains("attacker divisions[1] duplicates division '1. Infanterie-Division'"),
            "{err}"
        );
    }

    #[test]
    fn script_without_divisions_still_valid() {
        // Back-compat: all 18 legacy scripts carry no divisions block.
        let sf = sample_script();
        validate(&sf, Path::new("x.json")).unwrap();
        assert!(sf.attacker.divisions.is_empty());
        assert!(sf.defender.divisions.is_empty());
    }

    // --- The pure script → battle mapping functions (§7.5) ---

    #[test]
    fn division_tag_map_block_tag_wins_side_tag_default() {
        let sf = sample_with_divisions();
        let map = division_tag_map(&sf.attacker, &sf.defender);
        // Block tag wins for the declared French ally…
        assert_eq!(
            map.get("1re Division Légère").map(String::as_str),
            Some("FRA")
        );
        // …the side tag defaults for the block division declared without a
        // tag…
        assert_eq!(
            map.get("1. Infanterie-Division").map(String::as_str),
            Some("GER")
        );
        // …and for the defender's wholly undeclared division.
        assert_eq!(map.get("Armia Poznań").map(String::as_str), Some("POL"));
        assert_eq!(map.len(), 3, "one entry per effective roster division");
    }

    #[test]
    fn allied_contingents_player_only_and_explicit_tactic() {
        let sf = sample_with_divisions();
        let allies = allied_contingents(&sf.attacker, Side::Attacker);
        assert_eq!(allies.len(), 1, "the GER player division is excluded");
        assert_eq!(allies[0].tag, "FRA");
        assert_eq!(allies[0].divisions, vec!["1re Division Légère".to_string()]);
        // The block's explicit tactic token is honored.
        assert!(matches!(allies[0].tactic, CombatTactic::Assault));
    }

    #[test]
    fn allied_contingents_groups_by_tag_first_appearance_order() {
        let mut sf = sample_with_divisions();
        // A second French AI division joins the roster…
        sf.attacker.units.push(ScriptUnit {
            name: "2.BL".into(),
            unit_type: "infantry".into(),
            division: Some("2e Division Légère".into()),
            soft: 6.0,
            hard: 1.0,
            defense: 22.0,
            breakthrough: 3.0,
            armor: 0.0,
            piercing: 4.0,
            hardness: 0.0,
            chassis: None,
            org: None,
            strength: None,
            supports: Vec::new(),
        });
        // …declared BEFORE the 1re in the block — grouping follows the
        // block's first-appearance order, and the first division of a tag
        // carries the contingent's tactic (blitz, not the 1re's assault).
        sf.attacker.divisions.insert(
            1,
            ScriptDivision {
                name: "2e Division Légère".into(),
                tag: Some("FRA".into()),
                control: "ai".into(),
                tactic: Some("blitz".into()),
            },
        );
        validate(&sf, Path::new("x.json")).unwrap();
        let allies = allied_contingents(&sf.attacker, Side::Attacker);
        assert_eq!(
            allies.len(),
            1,
            "two FRA AI divisions group into one contingent"
        );
        assert_eq!(allies[0].tag, "FRA");
        assert_eq!(
            allies[0].divisions,
            vec![
                "2e Division Légère".to_string(),
                "1re Division Légère".to_string()
            ]
        );
        assert!(matches!(allies[0].tactic, CombatTactic::Blitz));
    }

    #[test]
    fn allied_contingents_default_tactics_follow_player_side() {
        let mut sf = sample_with_divisions();
        sf.attacker.divisions[1].tactic = None; // strip the explicit card
        let atk = allied_contingents(&sf.attacker, Side::Attacker);
        assert!(
            matches!(atk[0].tactic, CombatTactic::Assault),
            "player attacks → assault card"
        );
        let def = allied_contingents(&sf.attacker, Side::Defender);
        assert!(
            matches!(def[0].tactic, CombatTactic::ElasticDefense),
            "player defends → elastic-defense card"
        );
    }

    // --- Per-tag command overrides (menu nation selector) ---

    #[test]
    fn control_overrides_rewrite_resolved_tags() {
        let mut sf = sample_with_divisions();
        // The sample: GER player division (no tag) + FRA AI division.
        let mut overrides = HashMap::new();
        overrides.insert("FRA".to_string(), true);
        apply_control_overrides(&mut sf.attacker, &overrides);
        assert_eq!(sf.attacker.divisions[0].control, "player", "GER untouched");
        assert_eq!(sf.attacker.divisions[1].control, "player", "FRA flipped");

        let mut overrides = HashMap::new();
        overrides.insert("GER".to_string(), false);
        apply_control_overrides(&mut sf.attacker, &overrides);
        assert_eq!(
            sf.attacker.divisions[0].control, "ai",
            "the side-tag division (GER) flips to AI"
        );
        // Unknown tags are a silent no-op.
        let mut overrides = HashMap::new();
        overrides.insert("XXX".to_string(), false);
        apply_control_overrides(&mut sf.attacker, &overrides);
        assert_eq!(sf.attacker.divisions[0].control, "ai");
    }

    #[test]
    fn control_overrides_empty_map_is_a_noop() {
        let mut sf = sample_with_divisions();
        let before: Vec<String> = sf
            .attacker
            .divisions
            .iter()
            .map(|d| d.control.clone())
            .collect();
        apply_control_overrides(&mut sf.attacker, &HashMap::new());
        let after: Vec<String> = sf
            .attacker
            .divisions
            .iter()
            .map(|d| d.control.clone())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn control_overrides_then_contingents_agree() {
        let mut sf = sample_with_divisions();
        // Flip the FRA AI division to player command: the contingent list
        // must empty out (the override is applied before the grouping).
        let mut overrides = HashMap::new();
        overrides.insert("FRA".to_string(), true);
        apply_control_overrides(&mut sf.attacker, &overrides);
        let allies = allied_contingents(&sf.attacker, Side::Attacker);
        assert!(allies.is_empty(), "no AI divisions left after the override");
        // And flipping the GER player division to AI creates a contingent
        // under the SIDE tag with the player-side default tactic.
        let mut overrides = HashMap::new();
        overrides.insert("GER".to_string(), false);
        apply_control_overrides(&mut sf.attacker, &overrides);
        let allies = allied_contingents(&sf.attacker, Side::Attacker);
        assert_eq!(allies.len(), 1);
        assert_eq!(allies[0].tag, "GER");
        assert_eq!(
            allies[0].divisions,
            vec!["1. Infanterie-Division".to_string()]
        );
        assert!(
            matches!(allies[0].tactic, CombatTactic::Assault),
            "player-side default card"
        );
    }

    #[test]
    fn allied_contingents_empty_block_empty_vec() {
        // Legacy scripts carry no divisions block → no allied contingents,
        // whichever side the player commands.
        let sf = sample_script();
        assert!(allied_contingents(&sf.attacker, Side::Attacker).is_empty());
        assert!(allied_contingents(&sf.defender, Side::Defender).is_empty());
        // A block with only player-controlled divisions also yields none.
        let mut sf = sample_with_divisions();
        sf.attacker.divisions[1].control = "player".into();
        assert!(allied_contingents(&sf.attacker, Side::Attacker).is_empty());
    }

    #[test]
    fn deploy_spreads_roster_across_zone() {
        let sf = sample_script();
        let zone = vec![HexCoord::new(1, 1), HexCoord::new(2, 2)];
        let mut units = Vec::new();
        let mut next_id = 0usize;
        deploy_script_side(
            &mut units,
            &mut next_id,
            Side::Attacker,
            &sf.attacker,
            &zone,
            None,
        )
        .unwrap();
        // +1 synthesized HQ for the side's division (§6.13) — the
        // roster's tank makes it the armored-car variant.
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].name, "1.Pz.III");
        assert_eq!(units[0].division, "4. Panzer-Division Vorhut");
        assert!(units[0].has_support(SupportKind::Recon));
        assert_eq!(units[0].soft_attack, 19.0);
        assert!(units[1].is_hq());
        assert_eq!(units[1].division, "4. Panzer-Division Vorhut");
        assert!(units[1].attrs.has(tactical_core::Attrs::HQ_ARMORED));
        assert_eq!(next_id, 2);
    }

    #[test]
    fn resolve_adds_json_suffix() {
        assert_eq!(
            resolve("1939_warsaw").file_name().unwrap(),
            "1939_warsaw.json"
        );
        assert_eq!(
            resolve("1939_warsaw.json").file_name().unwrap(),
            "1939_warsaw.json"
        );
    }

    /// End-to-end against the real shipped script: load,
    /// validate, deploy both sides — the exact path `--battle file=…` takes.
    #[test]
    fn real_1939_warsaw_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1939_warsaw.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1939_warsaw script: {e}"));
        assert_eq!(sf.province, Some(3544));
        assert_eq!(sf.attacker.tag, "GER");
        assert_eq!(sf.defender.tag, "POL");
        // Historical battalion names + the full
        // 26 Sep western assault: 4.Pz Vorhut (10) + 10/18/19/31/46 ID x13
        // (9 IR bns + 2 AR + Pz.Jg + Pi.Btl) = 75; Armia Warszawa 44.
        assert_eq!(sf.attacker.units.len(), 75, "4.Pz Vorhut (10) + 5 ID x13");
        assert_eq!(sf.defender.units.len(), 44, "Armia Warszawa: 44 battalions");
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 6 German + 15 Polish
        // divisions → 21 HQs appended after each side's roster (the DOW
        // Artillery Group merged into 8. DP — no org-less gun divisions).
        assert_eq!(units.len(), 140);
        assert_eq!(nid, 140);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 21);
        // German divisions in the OOB tree: 4.Pz (side default) + per-unit.
        assert_eq!(units[0].division, "4. Panzer-Division Vorhut");
        assert_eq!(units[10].division, "31. Infanterie-Division");
        assert_eq!(
            units[10].name, "I./IR 12",
            "real 31. ID regimental battalions"
        );
        // The Polish garrison groups into its historical
        // divisions instead of one all-encompassing "Armia Warszawa".
        // Defender units start after the 6 attacker HQs (75 + 6).
        assert_eq!(units[81].division, "13. Dywizja Piechoty (remnant)");
        let zbior = units
            .iter()
            .find(|u| u.name == "Zbiorcza (Pomorska)")
            .unwrap();
        assert_eq!(zbior.division, "Zbiorcza Cavalry Bde (Abraham)");
        // Historical touches: truck-towed 4.Pz guns vs horse-towed ID ARs.
        let pz_art = units.iter().find(|u| u.name == "I./AR.103").unwrap();
        assert_eq!(pz_art.chassis, Chassis::TruckTowed);
        let id_art = units.iter().find(|u| u.name == "I./AR 31").unwrap();
        assert_eq!(
            id_art.chassis,
            Chassis::Towed,
            "1939 ID artillery was horse-drawn"
        );
        // 46. ID components per Mitcham: IR 42/72/97,
        // AR 114, Pz.Jg.52, Pi.88 — the non-self-numbered auxiliaries.
        let pzjg52 = units.iter().find(|u| u.name == "Pz.Jg.Abt.52").unwrap();
        assert_eq!(pzjg52.division, "46. Infanterie-Division");
        // Polish order of battle per Wikipedia's Opposing forces table:
        // 7 DP remnants x3 under-strength bns (org 40
        // = 2/3 of the 60 baseline — real regimental names now, e.g. 44. pp
        // of 13. DP, 59/61/62. pp of 15. DP, 26. pp of 5. DP, 21. pp of 8. DP),
        // Zbiorcza Cavalry Brigade (Abraham), the 33-tank city force, and
        // militia filling the line (43. Bayonne Volunteer Bde, improvised
        // 1./2. 'Defenders of Praga', Straż Obywatelska).
        let pol_cav = units
            .iter()
            .find(|u| u.name == "Zbiorcza (Pomorska)")
            .unwrap();
        assert_eq!(pol_cav.unit_type, UnitType::Cavalry);
        assert!(pol_cav.has_support(SupportKind::Recon));
        let tp = units.iter().find(|u| u.name == "1.7TP").unwrap();
        assert_eq!(tp.unit_type, UnitType::LightArmor);
        let dp = units.iter().find(|u| u.name == "I./44. pp").unwrap();
        assert_eq!(dp.org, 40.0, "remnant DP battalions are under-strength");
        assert_eq!(dp.max_org, 40.0);
        // Militia: the Volunteer Workers' Bde = 43rd Bayonne Legion rgt
        // (3 battalions), the two improvised 'Defenders of Praga' rgt
        // (6), and Straż Obywatelska civil guard (3).
        let bayonnes: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.name.starts_with("43.Bay"))
            .collect();
        assert_eq!(bayonnes.len(), 3, "43rd Bayonne Legion rgt = 3 battalions");
        let pragas: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.name.starts_with("1.Pragi") || u.name.starts_with("2.Pragi"))
            .collect();
        assert_eq!(pragas.len(), 6, "1./2. 'Defenders of Praga' = 6 battalions");
        let strazs: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.name.starts_with("Straz"))
            .collect();
        assert_eq!(strazs.len(), 3);
        assert!(bayonnes.iter().all(|u| u.org == 30.0));
        assert!(pragas.iter().all(|u| u.org == 30.0));
        assert!(strazs.iter().all(|u| u.org == 25.0));
        assert!(strazs.iter().all(|u| u.unit_type == UnitType::Infantry));
        // Full-strength battalion untouched by the org override.
        let schtz = units.iter().find(|u| u.name == "1./Schtz.Rgt.12").unwrap();
        assert_eq!(schtz.org, UnitType::Motorized.base_org());
        // No org-less gun divisions: the DOW
        // Artillery Group (64 guns) merged into the 8. DP — the Warsaw
        // division artillery the group actually supported.
        for art in ["1.Art", "2.Art", "3.Art"] {
            let u = units.iter().find(|u| u.name == art).unwrap();
            assert_eq!(u.unit_type, UnitType::ArtilleryBrigade, "{art}");
            assert_eq!(u.division, "8. DP / 21. pp Dzieci Warszawy", "{art}");
        }
        assert!(
            !units
                .iter()
                .any(|u| u.division == "DOW Artillery Group (64 guns)"),
            "the pure-artillery division is gone"
        );
    }

    /// End-to-end against the real shipped script (rebuilt with verified
    /// Winter War battalion names): load, validate, deploy both sides —
    /// the exact path `--battle file=…` takes.
    #[test]
    fn real_1940_viipuri_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1940_viipuri.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1940_viipuri script: {e}"));
        assert_eq!(sf.province, Some(9206));
        assert_eq!(sf.attacker.tag, "SOV");
        assert_eq!(sf.defender.tag, "FIN");
        // Historical battalion names + strength:
        // 7. Armiya shock group 44 = 123./100./138. SD x12 (9 RR bns + 2 AP
        // + PTAB) + 35. TB (2x T-28 + 112. OTB T-26) + 39. TB (2x BT-7) +
        // army artillery 2 + AA; Armija Kannas 31 = 3. Div (JR 6/7/8, org 40)
        // + 5. Div (JR 13/14/15, org 48) + III./JR-67 (23. Div det.) + Lahde
        // fortress 4 (Poppius/Million/Bunker Line/Eng) + KTR 3 + PST 2 +
        // sissi 2 + coastal 1.
        assert_eq!(
            sf.attacker.units.len(),
            44,
            "7. Armiya shock group: 44 battalions"
        );
        assert_eq!(sf.defender.units.len(), 31, "Armija Kannas: 31 battalions");
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 6 Soviet + 6 Finnish
        // formations → 12 HQs appended after each side's roster
        // (pure-artillery divisions merged — 7. Armiya Art.
        // Gruppa into the 123rd SD, II AK KTR split to its own 3.D/5.D
        // artillery regiments, coastal battery into the Lahde fortress).
        assert_eq!(units.len(), 87);
        assert_eq!(nid, 87);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 12);
        // Per en/ru Wikipedia: the 123rd SD = 245/255/272
        // RR + 323 AP + 229th PTAB — the pre-rebuild script wrongly listed
        // "I.323.RR" (323 was the ARTILLERY regiment) and skipped the 245th.
        let art = units.iter().find(|u| u.name == "I./323.AP").unwrap();
        assert_eq!(art.unit_type, UnitType::ArtilleryBrigade);
        assert_eq!(art.division, "123. Strelkovaya Diviziya");
        assert!(
            units.iter().any(|u| u.name == "I./245.RR"),
            "real 245th RR of the 123rd"
        );
        assert!(
            !units.iter().any(|u| u.name == "I.323.RR"),
            "323 was the AP, not a RR"
        );
        // Elite tip: the Poppius assault battalion carries sappers, the
        // 'Million' assault regiment the division recce (103.Rec).
        let poppius_assault = units.iter().find(|u| u.name == "I./245.RR").unwrap();
        assert!(poppius_assault.has_support(SupportKind::Engineer));
        let million_assault = units.iter().find(|u| u.name == "I./255.RR").unwrap();
        assert!(million_assault.has_support(SupportKind::Recon));
        // Tank brigades: the 35. TB (T-28/T-26, with 112. OTB) rode with the
        // 123rd at Lahde (Honkaniemi 26 Feb: 35th Light Tank Brigade + 245th
        // IR in the same sector); the 39. TB (BT-7) supported the Muolaa
        // push. T-28 = armor 35 (Bofors 37mm PST piercing 60 can kill it).
        let t28 = units.iter().find(|u| u.name == "1./35.TB (T-28)").unwrap();
        assert_eq!(t28.unit_type, UnitType::MediumArmor);
        assert_eq!(t28.armor, 35.0);
        let otb = units.iter().find(|u| u.name == "112.OTB (T-26)").unwrap();
        assert_eq!(otb.unit_type, UnitType::LightArmor);
        assert_eq!(otb.division, "35. Tankovaya Brigada");
        // Finnish order of battle per Pettibone/Trotter + the 6th Division
        // article (ex-6.D renumbered 3.D in Jan 1940: JR 16/17/18 → 6/7/8;
        // KTR 6 → 3): the 3.D held Summa-Lahde at org 40, the 5.D (JR
        // 13/14/15, KTR 5) counterattacked on 13 Feb at org 48.
        let jr6 = units.iter().find(|u| u.name == "I./JR-6").unwrap();
        assert_eq!(jr6.org, 40.0, "3.D battalions are battle-worn");
        assert_eq!(jr6.max_org, 40.0);
        assert_eq!(jr6.division, "3. Divisioona (Summa-Lahde)");
        let jr13 = units.iter().find(|u| u.name == "I./JR-13").unwrap();
        assert_eq!(jr13.org, 48.0, "5.D counterattack force");
        assert_eq!(jr13.division, "5. Divisioona (Isakson)");
        let jr67 = units.iter().find(|u| u.name == "III./JR-67").unwrap();
        assert_eq!(
            jr67.division, "23. Divisioona (det.)",
            "JR 67 on loan to the 5.D"
        );
        // Lahde fortress: the real bunker names (Poppius = Sj4 per the ru
        // breakthrough article caption; 'Million' next door), defense 26.
        let poppius = units
            .iter()
            .find(|u| u.name == "Sk.10 Poppius (Sj4)")
            .unwrap();
        assert_eq!(poppius.defense, 26.0);
        assert_eq!(poppius.org, 55.0);
        assert!(units.iter().any(|u| u.name == "Sk.11 Million"));
        // KTR battalions documented at Honkaniemi ('1st Battalions of the
        // 5th and 21st Artillery Regiments') — KTR 3 follows the pattern.
        // Each rides its own division (3.D holds KTR 3 + the
        // 21st's battalion, 5.D its KTR 5) — no org-less gun divisions.
        for ktr in ["I./KTR 3", "I./KTR 5", "I./KTR 21"] {
            let u = units.iter().find(|u| u.name == ktr).unwrap();
            assert_eq!(u.unit_type, UnitType::ArtilleryBrigade, "{ktr}");
        }
        let ktr3 = units.iter().find(|u| u.name == "I./KTR 3").unwrap();
        assert_eq!(
            ktr3.division, "3. Divisioona (Summa-Lahde)",
            "KTR 3 is 3.D's own guns"
        );
        let ktr5 = units.iter().find(|u| u.name == "I./KTR 5").unwrap();
        assert_eq!(
            ktr5.division, "5. Divisioona (Isakson)",
            "KTR 5 is 5.D's own guns"
        );
        let ktr21 = units.iter().find(|u| u.name == "I./KTR 21").unwrap();
        assert_eq!(
            ktr21.division, "3. Divisioona (Summa-Lahde)",
            "21st Rgt's bn rides the 3.D"
        );
        // The 7. Armiya artillery group rides the 123rd (the shock division
        // that broke the Summa line), the coastal battery sits in the
        // fortress sector it defended.
        let arm_art = units.iter().find(|u| u.name == "1.Arm.Art").unwrap();
        assert_eq!(arm_art.division, "123. Strelkovaya Diviziya");
        let coastal = units.iter().find(|u| u.name == "1.Coastal").unwrap();
        assert_eq!(coastal.division, "Lahde Fortress Sector");
        assert!(
            !units.iter().any(|u| u.division == "7. Armiya Art. Gruppa"
                || u.division == "II AK KTR (Corps Artillery)"
                || u.division == "Viipurinlahti Rannikkotykisto"),
            "pure-artillery divisions are gone"
        );
        let sissi = units.iter().find(|u| u.name == "1.Sissi").unwrap();
        assert_eq!(sissi.org, 65.0, "elite ski scouts");
        assert!(sissi.has_support(SupportKind::Recon));
    }

    /// End-to-end against the real shipped script (rebuilt with verified
    /// Narvik campaign battalion names): load, validate, deploy both
    /// sides — the exact path `--battle file=…` takes.
    #[test]
    fn real_1940_narvik_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1940_narvik.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1940_narvik script: {e}"));
        assert_eq!(sf.province, Some(192));
        assert_eq!(sf.attacker.tag, "ENG");
        assert_eq!(sf.defender.tag, "GER");
        // Historical battalion names + strength:
        // Allied Expeditionary Force 27 = 24th Guards Bde (3) + 27e DBCA
        // (6e/12e/14e BCA) + 13e DBMLE (1er/2e) + SBSP (I-IV Podhale) +
        // 6. Divisjon 6 (II/IR 14, Lv.bat./IR 14, II/IR 15, I/II/IR 16, Alta)
        // + 5 artillery + 342e CACC H-39 + 31e SES + AT + AA; Gruppe Dietl
        // 15 = GJR 139 x3 + Marine-Regiment Berger x7 + FJR 1 coy + GJR 138
        // coy + I./GAR 112 + NavyGun + Schiffs-Flak.
        assert_eq!(
            sf.attacker.units.len(),
            27,
            "Allied Expeditionary Force: 27 battalions"
        );
        assert_eq!(sf.defender.units.len(), 15, "Gruppe Dietl: 15 battalions");
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 6 Allied + 6 German
        // divisions → 12 HQs appended after each side's roster (the three
        // pure-artillery divisions were merged into their infantry
        // divisions — HOI4 has no org-less gun divisions, and an
        // anchorless battery deployed at the zone's far end).
        assert_eq!(units.len(), 54);
        assert_eq!(nid, 54);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 12);
        // Per fr/no/de Wikipedia + Derry App. B: the 27e
        // Demi-Brigade de Chasseurs Alpins = 6e/12e/14e BCA (three separate
        // bataillons) — the old draft's "1./2.ChassAlp (27e BCA)" confused
        // the bataillon with its brigade, and the 27e BCA proper never left
        // France.
        for bca in ["6e BCA", "12e BCA", "14e BCA"] {
            let u = units.iter().find(|u| u.name == bca).unwrap();
            assert_eq!(u.unit_type, UnitType::Mountaineer, "{bca}");
            assert_eq!(u.division, "27e Demi-Brigade de Chasseurs Alpins");
        }
        assert!(
            !units.iter().any(|u| u.name.contains("ChassAlp")),
            "old draft's fake 27e BCA battalions removed"
        );
        // 13e DBMLE had TWO battalions in 1940 (no III) — the 1er carries
        // the sapper support of the 13 May Bjerkvik landing.
        let db1 = units.iter().find(|u| u.name == "1er/13e DBMLE").unwrap();
        assert_eq!(db1.unit_type, UnitType::Marine);
        assert!(db1.has_support(SupportKind::Engineer));
        assert!(!units
            .iter()
            .any(|u| u.name.starts_with("III.13") || u.name.contains("III/13")));
        // The Polish Podhale Brigade = FOUR battalions (two half-brigades).
        let podhale: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.name.ends_with("Podhale"))
            .collect();
        assert_eq!(podhale.len(), 4, "SBSP: I-IV batalion");
        // Norwegian 6. Divisjon order per lokalhistoriewiki: the 28 May
        // assault battalion is II/IR 15; Alta is the division's elite
        // (org 60), the Landvern battalion is the weakest (org 45).
        let jr15 = units.iter().find(|u| u.name == "II./IR 15").unwrap();
        assert_eq!(jr15.division, "6. Divisjon (Fleischer)");
        assert_eq!(jr15.org, 50.0, "mobilized battalions at org 50");
        let alta = units.iter().find(|u| u.name == "Alta bataljon").unwrap();
        assert_eq!(alta.org, 60.0, "Alta = the division's elite");
        let lv = units.iter().find(|u| u.name == "Lv.bat./IR 14").unwrap();
        assert_eq!(lv.org, 45.0, "Landvern territorials");
        assert!(
            !units
                .iter()
                .any(|u| u.name.contains("IR-12") || u.name.contains("IR-13")),
            "I/IR 12 (Gratangen 25 Apr) and I/IR 13 (Bjornfjell 16 Apr) were destroyed pre-battle"
        );
        // Company-scale French tank company (Hotchkiss H-39, ~10 tanks).
        let cacc = units.iter().find(|u| u.name == "342e CACC (H-39)").unwrap();
        assert_eq!(cacc.unit_type, UnitType::LightArmor);
        assert_eq!(cacc.division, "CEFS Supports");
        // NO pure-artillery divisions — a gun unit belongs to the
        // infantry division it supports (deploy anchor,
        // §6.13 HQ count): 2e GAC into the French CEFS supports, 203 Fd Bty
        // into the 24th Guards, the Norwegian batteries into 6. Divisjon.
        let fr_art = units.iter().find(|u| u.name == "1.FrArt").unwrap();
        assert_eq!(fr_art.division, "CEFS Supports");
        let eng_art = units.iter().find(|u| u.name == "203.Fd Bty").unwrap();
        assert_eq!(eng_art.division, "24th Guards Brigade");
        let nor_art = units.iter().find(|u| u.name == "1.NorArt").unwrap();
        assert_eq!(nor_art.division, "6. Divisjon (Fleischer)");
        let nor_mot = units.iter().find(|u| u.name == "10.MotBtr").unwrap();
        assert_eq!(nor_mot.division, "6. Divisjon (Fleischer)");
        // German order per de.wiki Schlacht um Narvik: the S-Staffel = GJR
        // 139 (reinforced with Geb.Aufkl.112 — hence the recon support) and
        // SEVEN named Marine-Regiment Berger battalions of destroyer crews.
        let gjr = units.iter().find(|u| u.name == "II./GJR 139").unwrap();
        assert!(gjr.has_support(SupportKind::Recon));
        let marin: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.name.starts_with("Marine-Btl."))
            .collect();
        assert_eq!(marin.len(), 7, "Marine-Regiment Berger: 7 named battalions");
        assert!(marin.iter().all(|u| u.org == 30.0 && u.max_org == 30.0));
        for name in [
            "Holtorf",
            "Thiele",
            "Zenker",
            "Arnim",
            "Erdmenger",
            "Freytag-L.",
            "Kothe",
        ] {
            assert!(
                units
                    .iter()
                    .any(|u| u.name == format!("Marine-Btl. {name}")),
                "{name}"
            );
        }
        assert_eq!(marin[0].division, "Marine-Regiment Berger");
        // GJR 137 of the 2. Gebirgs-Division never reached Narvik during the
        // battle (symbolic arrival 13 June) — removed from the old draft.
        assert!(
            !units.iter().any(|u| u.name.contains("137")),
            "GJR 137 did not fight at Narvik"
        );
        // Air-landed reinforcements: a company-strength FJR 1 drop (org 40)
        // and air-lifted GJR 138 companies (org 35).
        let fjr = units.iter().find(|u| u.name == "1./FJR 1").unwrap();
        assert_eq!(fjr.unit_type, UnitType::Paratrooper);
        assert_eq!(fjr.org, 40.0);
        let g138 = units.iter().find(|u| u.name == "1./GJR 138").unwrap();
        assert_eq!(g138.org, 35.0);
        // No German armor/AT at Narvik — the old draft's 1.GebAT is gone.
        assert_eq!(
            units
                .iter()
                .filter(|u| u.unit_type == UnitType::AntiTankBrigade)
                .count(),
            1,
            "only the French AT coy remains (Germans had no AT guns documented)"
        );
    }

    /// §7.5: Narvik is the first script with a divisions block —
    /// the player commands the French CEFS contingent (11 battalions), the
    /// British/Polish/Norwegian divisions are AI-controlled allies, one
    /// contingent per tag, all on the assault card.
    #[test]
    fn real_1940_narvik_divisions_block_splits_command() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1940_narvik.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1940_narvik script: {e}"));
        assert_eq!(
            sf.attacker.divisions.len(),
            6,
            "every Allied division declared"
        );
        assert!(
            sf.defender.divisions.is_empty(),
            "Gruppe Dietl stays monolithic"
        );

        // Player = the three French divisions (control defaults to "player").
        let french = [
            "27e Demi-Brigade de Chasseurs Alpins",
            "13e Demi-Brigade de Legion Etrangere",
            "CEFS Supports",
        ];
        for name in french {
            let d = sf
                .attacker
                .divisions
                .iter()
                .find(|d| d.name == name)
                .unwrap();
            assert_eq!(d.control, "player", "{name}");
            assert_eq!(d.tag.as_deref(), Some("FRA"), "{name}");
        }

        // Allies = ENG + POL + NOR, grouped in the block's first-appearance
        // order, every contingent on the explicit assault card.
        let allies = allied_contingents(&sf.attacker, Side::Attacker);
        assert_eq!(allies.len(), 3);
        assert_eq!(allies[0].tag, "ENG");
        assert_eq!(allies[0].divisions, vec!["24th Guards Brigade".to_string()]);
        assert_eq!(allies[1].tag, "POL");
        assert_eq!(
            allies[1].divisions,
            vec!["Brygada Podhalanska (SBSP)".to_string()]
        );
        assert_eq!(allies[2].tag, "NOR");
        assert_eq!(
            allies[2].divisions,
            vec!["6. Divisjon (Fleischer)".to_string()]
        );
        for c in &allies {
            assert!(
                matches!(c.tactic, CombatTactic::Assault),
                "{} assaults",
                c.tag
            );
        }

        // Tag map: block tags win (French/Norwegian/Polish divisions), the
        // side tag defaults for the Germans (no defender block).
        let map = division_tag_map(&sf.attacker, &sf.defender);
        assert_eq!(
            map.get("27e Demi-Brigade de Chasseurs Alpins")
                .map(String::as_str),
            Some("FRA")
        );
        assert_eq!(
            map.get("6. Divisjon (Fleischer)").map(String::as_str),
            Some("NOR")
        );
        assert_eq!(
            map.get("Brygada Podhalanska (SBSP)").map(String::as_str),
            Some("POL")
        );
        assert_eq!(
            map.get("139. Gebirgs-Jaeger-Regiment").map(String::as_str),
            Some("GER")
        );
        assert_eq!(map.len(), 12, "6 Allied + 6 German divisions");
    }

    /// End-to-end against the real shipped script (rebuilt with verified
    /// Greco-Italian War battalion names): load, validate, deploy both
    /// sides — the exact path `--battle file=…` takes.
    #[test]
    fn real_1940_ioannina_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1940_ioannina.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1940_ioannina script: {e}"));
        assert_eq!(sf.province, Some(3914));
        assert_eq!(sf.dirs, vec!["NW"], "Ciamuria axis via Gjirokaster (single direction: the NW+NE double strip strands the lines 8+ hexes apart)");
        assert_eq!(sf.attacker.tag, "ITA");
        assert_eq!(sf.defender.tag, "GRE");
        assert_eq!(sf.player_side, "attacker");
        assert_eq!(sf.enemy_tactic, "guerrilla_tactics");
        // Historical battalion names + strength:
        // XXV Corpo 'Ciamuria' + Julia, 60 = Julia 13 (5 regular + 4 Val +
        // 2 art + Genio + AT) + Ferrara 12 (47/48 Rgt x3 + 2 art + Mortaisti
        // + AT + 2 CC.NN) + Siena 10 + Bari 10 + Centauro 10 (4 L3 + 3
        // Bersaglieri + 2 art + AT) + corps 5; Army of Epirus 44 = 8. Div 14
        // (10/15/24 Rgt x3 + 3/40 Evzones x3 + 2 art) + 3rd Bde 2 + Pindos
        // Detachment 5 + 1. Div 7 + Cav Div 9 + Cav Bde 2 + 2/39 Evzones 3
        // + Epirus det 2.
        assert_eq!(
            sf.attacker.units.len(),
            60,
            "XXV Corpo + Julia: 60 battalions"
        );
        assert_eq!(sf.defender.units.len(), 44, "Army of Epirus: 44 battalions");
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 7 Italian + 8 Greek
        // divisions → 15 HQs appended after each side's roster (the XXV
        // Corpo 'Ciamuria' corps artillery rode into the Julia — a gun
        // group with no infantry line deploys at the zone's far end and
        // never coordinates).
        assert_eq!(units.len(), 119);
        assert_eq!(nid, 119);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 15);
        // Per en/it/el Wikipedia: Julia 1940 = 8 Rgt
        // (Cividale/Gemona/Tolmezzo) + 9 Rgt (L'Aquila/Vicenza) + Val
        // mobilization bns — Bassano/Feltre left the division in 1937
        // (Tridentina/Pusteria), Pieve.Cadore belongs to the 7 Rgt Pusteria.
        for bn in ["Cividale", "Gemona", "Tolmezzo", "L'Aquila", "Vicenza"] {
            let u = units.iter().find(|u| u.name == bn).unwrap();
            assert_eq!(u.unit_type, UnitType::Mountaineer, "{bn}");
            assert_eq!(u.division, "3. Div. Alpina 'Julia'");
        }
        for bn in ["Val Tagliamento", "Val Fella", "Val Natisone", "Val Cismon"] {
            let u = units.iter().find(|u| u.name == bn).unwrap();
            assert_eq!(u.org, 50.0, "Val mobilization battalions at org 50");
        }
        assert!(
            !units
                .iter()
                .any(|u| u.name.contains("Bassano") || u.name.contains("Feltre")),
            "Bassano/Feltre were not in Julia 1940"
        );
        assert!(
            !units.iter().any(|u| u.name.contains("Pieve")),
            "Pieve.Cadore is Pusteria, not Julia"
        );
        // The 1937-draft "M13/40 Centauro" is corrected to L3/35 tankettes:
        // Centauro had NO M13 battalions in Oct-Nov 1940 (they arrived Jan
        // 1941); only I-IV Btg. Carri L of the 31 Rgt Carristi (163 tankettes,
        // 90 usable → org 6 on the armor-10 baseline).
        let l3 = units.iter().find(|u| u.name == "I. Btg. Carri L").unwrap();
        assert_eq!(l3.unit_type, UnitType::LightArmor);
        assert_eq!(l3.armor, 2.0, "L3/35 was a 2-MG tankette, armor ~14mm");
        assert_eq!(l3.soft_attack, 4.0);
        assert_eq!(l3.org, 6.0, "90/163 tankettes usable");
        assert!(
            !units.iter().any(|u| u.name.contains("M13")),
            "no M13/40 in Oct-Nov 1940"
        );
        assert!(units.iter().any(|u| u.name == "XIV Bersaglieri"));
        assert!(units.iter().any(|u| u.name == "XXII Bersaglieri"));
        assert!(units.iter().any(|u| u.name == "XXIV Bersaglieri"));
        // Acqui (33.) reached Albania only on 18 Dec (coast) — outside the
        // battle window, excluded; Bari (47., 2 Nov arrivals) is included.
        assert!(
            !units.iter().any(|u| u.division.contains("Acqui")),
            "Acqui never fought here"
        );
        assert!(units.iter().any(|u| u.name == "I./139 Rgt"));
        // The sole in-sector Blackshirt legion = 82 Legione 'Benito Mussolini'
        // (LXVIII/LXXXII, late Nov, militia org 40).
        let ccnn = units.iter().find(|u| u.name == "LXVIII CC.NN").unwrap();
        assert_eq!(ccnn.org, 40.0);
        assert_eq!(ccnn.division, "82. Legione CC.NN 'Benito Mussolini'");
        // Italian binary divisions: 2 infantry regiments x3 battalions each.
        for (rgt, org) in [
            ("47", 58.0),
            ("48", 58.0),
            ("31", 50.0),
            ("32", 50.0),
            ("139", 55.0),
            ("140", 55.0),
        ] {
            for bn in ["I", "II", "III"] {
                let u = units
                    .iter()
                    .find(|u| u.name == format!("{bn}./{rgt} Rgt"))
                    .unwrap();
                assert_eq!(u.org, org, "{bn}./{rgt}");
            }
        }
        // Greek order per Gedeon/Battle of Pindus: 8. Div = 10/15/24 Rgt +
        // 3/40 Evzone Rgt (NOT 12/17/22), 15 bns 16 batteries incl. the 3rd
        // Inf Bde; the Pindos Detachment (Davakis ~2,000) = I/II/51 with
        // III/51 arriving piecemeal (org 40); Evzones are the elite
        // mountain troops (def 24).
        for rgt in ["10", "15"] {
            for bn in ["I", "II", "III"] {
                let u = units
                    .iter()
                    .find(|u| u.name == format!("{bn}./{rgt} Rgt"))
                    .unwrap();
                assert_eq!(u.org, 58.0, "{bn}./{rgt} — 8. Div at the Kalamas line");
            }
        }
        let evzone = units.iter().find(|u| u.name == "I./3/40 Evzones").unwrap();
        assert_eq!(evzone.unit_type, UnitType::Mountaineer);
        assert_eq!(evzone.defense, 24.0);
        assert!(units.iter().any(|u| u.name == "II./51 Rgt"));
        assert_eq!(
            units.iter().find(|u| u.name == "III./51 Rgt").unwrap().org,
            40.0
        );
        let pindos = units.iter().find(|u| u.name == "I./51 Rgt").unwrap();
        assert_eq!(pindos.division, "Pindos Detachment (Davakis)");
        assert!(
            units.iter().any(|u| u.name == "Pindos Cav"),
            "Davakis' cavalry troop"
        );
        // The 1. Div counterattack (Vrachnos, from 1 Nov) + Cavalry Division
        // (Stanotas: 3 Cav Rgt + Mech Cav Rgt + 4 Inf Rgt + 7 Rgt bn) and
        // 2/39 Evzones (from the 4. Div) close the battle window roster.
        assert!(units.iter().any(|u| u.name == "I./1 Rgt"));
        assert!(units.iter().any(|u| u.name == "1./Mech Cav Rgt"));
        assert!(units.iter().any(|u| u.name == "I./4 Rgt"));
        assert!(units.iter().any(|u| u.name == "7 Rgt bn"));
        assert!(units.iter().any(|u| u.name == "I./2/39 Evzones"));
        // Centauro's guns were truck-towed (armored division) unlike the
        // horse/mule-drawn division art.
        let cent_art = units
            .iter()
            .find(|u| u.name == "Gr. I (75/27)" && u.division == "131. Div. Corazzata 'Centauro'")
            .unwrap();
        assert_eq!(cent_art.chassis, Chassis::TruckTowed);
        // The Genio sapper battalion rides with Julia (Pi.Btl pattern).
        let genio = units.iter().find(|u| u.name == "III Btg. Genio").unwrap();
        assert!(genio.has_support(SupportKind::Engineer));
        // No org-less gun divisions: the XXV Corpo 'Ciamuria' corps
        // artillery rides the Julia — the corps group had no infantry line
        // of its own, so its guns deployed at the zone's far end and never
        // coordinated.
        for cor in ["Cor.Art I", "Cor.Art II", "Cor.AT"] {
            let u = units.iter().find(|u| u.name == cor).unwrap();
            assert_eq!(u.division, "3. Div. Alpina 'Julia'", "{cor}");
        }
        assert!(
            !units.iter().any(|u| u.division == "XXV Corpo 'Ciamuria'"),
            "the corps-artillery-only division is gone"
        );
    }

    /// End-to-end against the real shipped script (rebuilt with verified
    /// Operation Compass battalion names): load, validate, deploy both
    /// sides — the exact path `--battle file=…` takes.
    #[test]
    fn real_1940_sidi_barrani_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1940_sidi_barrani.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1940_sidi_barrani script: {e}"));
        assert_eq!(sf.province, Some(9989));
        assert_eq!(sf.attacker.tag, "ENG");
        assert_eq!(sf.defender.tag, "ITA");
        // Playfair Vol.I + Wikipedia OOB per Montanari 1990/Christie 1999:
        // WDF 25 = 4th Indian Division 13
        // (5th Ind Bde 3 + 11th Ind Bde 3 + 16th Br Bde 3 + 7.RTR Matildas +
        // 1st/25th Field + 7th Medium RA) + 7th Armoured Division 12 (4th
        // Armd Bde 7.Hussars/2.RTR/6.RTR + 7th Armd Bde 3.KOH/8.KRIH/1.RTR +
        // Support Group 1.KRRC/2.Rifle Bde + 11.Hussars + 3.RHA +
        // 106.LancsYeo AT + 2.HAA); 10. Armata 29 = Raggruppamento Maletti 8
        // (I/V/XVII/XIX Libyan mot + I Saharian + II Carri M M11/39 + 65/17 +
        // 75/27) + 2nd Libyan Division 10 (II/VI/VII + XIV/XV/XVI bns +
        // IX Carri L + 2 Libyan art groups + II Genio) + 4th CC.NN.
        // '3 Gennaio' 8 (228th/250th Legions + 204th Art) + 1st Libyan
        // Division remnants 3 (VIII/X/XII, org 35, town pocket).
        assert_eq!(sf.attacker.units.len(), 25, "WDF: 25 battalions");
        assert_eq!(
            sf.defender.units.len(),
            29,
            "10. Armata camp line: 29 battalions"
        );
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 2 British + 4 Italian
        // divisions → 6 HQs appended after each side's roster (no org-less
        // gun divisions — every artillery/AT/AA unit rides a line division).
        assert_eq!(units.len(), 60);
        assert_eq!(nid, 60);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 6);
        // 7.RTR led every break-in (47 Matilda IIs, the wire-cutting
        // sappers attached); Matilda II = heavy armor 70 — the 47/32 AT
        // (piercing 45 support) cannot crack it.
        let matilda = units
            .iter()
            .find(|u| u.name == "7.RTR (Matilda II)")
            .unwrap();
        assert_eq!(matilda.unit_type, UnitType::HeavyArmor);
        assert_eq!(matilda.armor, 70.0);
        assert_eq!(matilda.piercing, 55.0, "2-pdr AP");
        assert!(matilda.has_support(SupportKind::Engineer));
        assert_eq!(matilda.division, "4th Indian Division");
        // The 2-pdr cruisers (A13) carry no HE shell — low soft, high hard;
        // the Mk VI light tanks and L3 tankettes are machine-gun carriers.
        let cruiser = units.iter().find(|u| u.name == "2.RTR (Cruiser)").unwrap();
        assert_eq!(cruiser.unit_type, UnitType::LightArmor);
        assert_eq!(cruiser.soft_attack, 12.0, "2-pdr had no HE round");
        assert_eq!(cruiser.hard_attack, 15.0);
        assert_eq!(cruiser.piercing, 55.0);
        let mkvi = units
            .iter()
            .find(|u| u.name == "7.Hussars (Mk VI)")
            .unwrap();
        assert_eq!(mkvi.piercing, 5.0, "Mk VI = MGs only");
        // The armoured-car regiment = 11th Hussars (only AC regiment at the
        // battle — the 1st KDG arrived in the Middle East in 1941).
        let hussars = units.iter().find(|u| u.name == "11.Hussars (AC)").unwrap();
        assert!(hussars.has_support(SupportKind::Recon));
        assert!(
            !units.iter().any(|u| u.name.contains("KDG")),
            "1st KDG was not in Egypt in Dec 1940"
        );
        // Support Group motor battalions ride trucks.
        let krrc = units.iter().find(|u| u.name == "1.KRRC (Mot)").unwrap();
        assert_eq!(krrc.unit_type, UnitType::Motorized);
        // Maletti Group = the only mobile Italian force: truck-borne Libyan
        // battalions + the M11/39 battalion (armor 30 — vulnerable to the
        // 2-pdr) with its 47/32 AT companies attached.
        let maletti: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.division == "Raggruppamento Maletti" && !u.is_hq())
            .collect();
        assert_eq!(
            maletti.len(),
            8,
            "Maletti Group: 8 battalions (+1 synthesized HQ)"
        );
        assert!(
            maletti
                .iter()
                .filter(|u| u.unit_type == UnitType::Motorized)
                .count()
                == 5,
            "I/V/XVII/XIX Libyan + I Saharian = 5 motorized battalions"
        );
        let m11 = units
            .iter()
            .find(|u| u.name == "II.Carri M (M11/39)")
            .unwrap();
        assert_eq!(m11.armor, 30.0, "M11/39 max armor");
        assert_eq!(
            m11.piercing, 40.0,
            "37mm gun 30 + baked-in 47/32 AT support +10"
        );
        assert!(
            m11.has_support(SupportKind::AntiTank),
            "Maletti's 47/32 AT companies"
        );
        // Colonial under-strength: Libyan bns org 45, CC.NN. org 30, the
        // 1st Libyan remnants trapped in the town pocket org 35.
        let offella = units.iter().find(|u| u.name == "II.Lib 'Offella'").unwrap();
        assert_eq!(offella.org, 45.0);
        assert_eq!(offella.division, "2nd Libyan Division");
        let ccnn = units
            .iter()
            .find(|u| u.name == "I./228.Leg CC.NN.")
            .unwrap();
        assert_eq!(ccnn.org, 30.0);
        assert_eq!(ccnn.division, "4th CC.NN. '3 Gennaio'");
        let rem = units.iter().find(|u| u.name == "VIII.Lib (rem)").unwrap();
        assert_eq!(rem.org, 35.0);
        assert_eq!(rem.division, "1st Libyan Division (remnants)");
        // The IX L3/35 tankette battalion rode the 2nd Libyan Division.
        let l3 = units
            .iter()
            .find(|u| u.name == "IX.Carri L (L3/35)")
            .unwrap();
        assert_eq!(l3.unit_type, UnitType::LightArmor);
        assert_eq!(l3.piercing, 5.0, "L3 = twin MGs");
        // Battlefield position: 63rd Cirene (Rabia/
        // Sofafi — never attacked, withdrew to Halfaya overnight) and 64th
        // Catanzaro (Alam Salamus/Buq Buq) are OUTSIDE the Sidi Barrani
        // province and escaped — not modeled; the 132nd Ariete stayed in
        // Italy (Playfair explicit).
        assert!(
            !units
                .iter()
                .any(|u| u.division.contains("Cirene") || u.division.contains("Catanzaro")),
            "Cirene/Catanzaro did not fight at Sidi Barrani"
        );
        assert!(
            !units.iter().any(|u| u.division.contains("Ariete")),
            "the Ariete division stayed in Italy in Dec 1940"
        );
        // No pure-artillery divisions: the British guns ride the two line
        // divisions; Italian guns ride Maletti / 2nd Libyan / 3 Gennaio.
        for art in ["1.Fd Rgt RA", "25.Fd Rgt RA", "7.Med Rgt RA"] {
            let u = units.iter().find(|u| u.name == art).unwrap();
            assert_eq!(u.division, "4th Indian Division", "{art}");
        }
        assert_eq!(
            units.iter().find(|u| u.name == "3.RHA").unwrap().division,
            "7th Armoured Division"
        );
        assert_eq!(
            units
                .iter()
                .find(|u| u.name == "106.LancsYeo (AT)")
                .unwrap()
                .division,
            "7th Armoured Division"
        );
        assert_eq!(
            units
                .iter()
                .find(|u| u.name == "2.HAA Rgt (AA)")
                .unwrap()
                .division,
            "7th Armoured Division"
        );
        let mal_art = units.iter().find(|u| u.name == "II Grp 75/27").unwrap();
        assert_eq!(
            mal_art.chassis,
            Chassis::TruckTowed,
            "Maletti was the motorized group"
        );
        assert!(
            mal_art.has_support(SupportKind::AntiAir),
            "Maletti's 20mm AA batteries"
        );
        let lib_art = units.iter().find(|u| u.name == "I.LibArt Grp").unwrap();
        assert_eq!(
            lib_art.chassis,
            Chassis::Towed,
            "colonial guns were horse-drawn"
        );
    }

    /// End-to-end against the real shipped script (rebuilt with verified
    /// Battle-of-Heraklion battalion names): load, validate, deploy both
    /// sides — the exact path `--battle file=…` takes.
    #[test]
    fn real_1941_heraklion_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1941_heraklion.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1941_heraklion script: {e}"));
        assert_eq!(sf.province, Some(9940));
        assert_eq!(sf.dirs, vec!["W"], "Rethymno coast-road axis: the west group blocked the road W of town, all follow-up attacks came from the W/SW gates");
        assert_eq!(sf.attacker.tag, "GER");
        assert_eq!(sf.defender.tag, "ENG");
        // The script does not prescribe the player's side — the field is
        // absent, the schema default (attacker) applies, and the menu/CLI
        // side toggle flips freely.
        assert_eq!(
            sf.player_side, "attacker",
            "absent player_side = schema default"
        );
        assert_eq!(
            sf.enemy_tactic, "elastic_defense",
            "the garrison AI card for the default (attacker) player"
        );
        // OOB per Davin App. IV, Playfair Vol II,
        // en/el Wikipedia: Kampfgruppe Brauer 9 = FJR 1 x3 (org 55) +
        // II./FJR 2 (2 co, org 40) + 1 co Fsch-MG-Btl.7 (org 35) + Fsch-Art.
        // 7 det + Fsch-Pz.Jg. 7 det + 24.5/27.5 parachute drops (org 40);
        // Heraklion Garrison 17 = 14th Inf Bde 9 (2/BW 2/Y&L 2/Leic +
        // 1/A&SH org 45 + 7 Med Rgt as infantry org 30 + 234 Med Bty +
        // Sec 15 Coast Rgt + 2 Matilda II + 6 Mk VI) + 2/4th Aust 3 (the bn
        // + 10 Bofors + 4x3-inch RM) + 3rd Gr Regt 2 + 7th Gr Regt 1 +
        // Garrison Bn 1 + Gendarmerie 1 (Greek recruits org 40/30).
        assert_eq!(
            sf.attacker.units.len(),
            9,
            "Kampfgruppe Brauer: 9 battalions"
        );
        assert_eq!(
            sf.defender.units.len(),
            17,
            "Heraklion Garrison: 17 battalions"
        );
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 2 German + 6
        // Allied/Greek divisions → 8 HQs appended after each side's roster.
        assert_eq!(units.len(), 34);
        assert_eq!(nid, 34);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 8);
        // The 1941 FJ regiment = THREE battalions (I./Walther, II./Burckhardt,
        // III./Schulz — there was no IV. heavy bn in 1941), plus the two
        // companies of II./FJR 2 under Pietzonka (the other two went to
        // Maleme) — the old draft's "Luftlande-Sturmtruppe 1.II/KG2" was a
        // phantom (KG2 was a bomber wing; the Sturmregiment fought at
        // Maleme/Chania).
        for bn in ["I./FJR 1", "II./FJR 1", "III./FJR 1"] {
            let u = units.iter().find(|u| u.name == bn).unwrap();
            assert_eq!(u.unit_type, UnitType::Paratrooper, "{bn}");
            assert_eq!(u.org, 55.0, "{bn}");
            assert_eq!(u.division, "Kampfgruppe Brauer (FJR 1)");
        }
        assert!(
            !units.iter().any(|u| u.name.contains("KG2")),
            "no Luftlande-Sturmtruppe at Heraklion"
        );
        let fjr2 = units.iter().find(|u| u.name == "II./FJR 2 (2 Co)").unwrap();
        assert_eq!(fjr2.org, 40.0, "half-battalion");
        let mg = units.iter().find(|u| u.name == "1./Fsch-MG-Btl.7").unwrap();
        assert_eq!(mg.org, 35.0, "company-scale AA-MG company");
        // 5. Gebirgs-Division NEVER reached Heraklion: GJR 100 airlanded at
        // Maleme (21.5 onwards) and the seaborne 2nd Motor Sailing Flotilla
        // (II./GJR 85 + the division's guns/AA, ~4,000 men) was turned back
        // by Force C off Milos on 22.5 — the old draft's 100.GJR x3 and
        // Seetransportgruppe battalions are gone.
        assert_eq!(
            units
                .iter()
                .filter(|u| u.unit_type == UnitType::Mountaineer)
                .count(),
            0,
            "no GJR 100 at Heraklion (airlanded at Maleme)"
        );
        assert!(
            !units
                .iter()
                .any(|u| u.name.contains("SeaJgd") || u.division.contains("Seetransport")),
            "the seaborne convoy never landed"
        );
        // The 3.7cm PaK 36 (air-dropped, most containers lost) cannot crack
        // the Matilda II (armor 70) — it only threatens the 6 Mk VI (10).
        let pak = units
            .iter()
            .find(|u| u.name == "Fsch-Pz.Jg-Abt.7 (det)")
            .unwrap();
        assert_eq!(pak.unit_type, UnitType::AntiTankBrigade);
        assert_eq!(pak.piercing, 45.0, "PaK 36 AP ~34mm");
        assert_eq!(
            pak.chassis,
            Chassis::Towed,
            "parachuted guns, no prime movers"
        );
        // The defender: 2/4th Australian (NOT 2/7th — Davin App. IV puts
        // 2/7th/2/8th in the Central Sector; only 2/4th defended Heraklion),
        // riding the 'Charlies' hills over the airfield with the AA.
        let aus = units
            .iter()
            .find(|u| u.name == "2/4th Aust Inf Bn")
            .unwrap();
        assert_eq!(aus.division, "2/4th Australian Inf Bn (19 Aust Bde)");
        assert!(!units
            .iter()
            .any(|u| u.name.contains("2/7th") || u.name.contains("2/8th")));
        assert_eq!(
            units
                .iter()
                .filter(|u| u.unit_type == UnitType::AntiAirBrigade)
                .count(),
            2,
            "10 Bofors + 4x3-inch RM"
        );
        // 7 Med Regt had NO guns on Crete (fought as infantry) and the
        // garrison's only field artillery were the 13 captured guns of
        // 234 Med Bty plus the 2x4-inch coast guns — all riding the 14 Bde
        // line division (no pure-artillery divisions).
        let med = units
            .iter()
            .find(|u| u.name == "7 Med Rgt (as inf)")
            .unwrap();
        assert_eq!(med.unit_type, UnitType::Infantry);
        assert_eq!(med.org, 30.0);
        assert_eq!(med.division, "14th Infantry Brigade (Chappel)");
        for gun in ["234 Med Bty (13 guns)", "Sec 15 Coast Rgt (2x4in)"] {
            let u = units.iter().find(|u| u.name == gun).unwrap();
            assert_eq!(u.unit_type, UnitType::ArtilleryBrigade, "{gun}");
            assert_eq!(u.division, "14th Infantry Brigade (Chappel)", "{gun}");
            assert_eq!(u.chassis, Chassis::Towed, "{gun}");
        }
        assert!(
            !units.iter().any(|u| u.division == "Heraklion Coast Defence"
                || u.division == "7th Medium Regiment RA"),
            "pure-artillery divisions are gone"
        );
        // The two Matilda IIs (one at each runway end, the same vehicles
        // that broke the II./FJR 1 east group on 20.5) and the 6 Mk VI light
        // tanks SE of the airfield — the garrison's only armor.
        let matilda = units
            .iter()
            .find(|u| u.name == "B Sqn 7 RTR (2 Matilda II)")
            .unwrap();
        assert_eq!(matilda.unit_type, UnitType::HeavyArmor);
        assert_eq!(matilda.armor, 70.0);
        assert_eq!(matilda.division, "14th Infantry Brigade (Chappel)");
        let mkvi = units
            .iter()
            .find(|u| u.name == "3rd Hussars det (6 Mk VI)")
            .unwrap();
        assert_eq!(mkvi.unit_type, UnitType::LightArmor);
        assert_eq!(mkvi.piercing, 5.0, "Mk VI = machine guns only");
        // The Greek battalions were battalion-strength recruit units (3rd
        // Gr Regt ~1,100, 7th ~800, Garrison Bn ~800 — Davin App. IV) at
        // org 40; the gendarmerie org 30. No AT company is modeled — none
        // is documented in the Heraklion sector.
        assert_eq!(
            units
                .iter()
                .filter(|u| u.division == "3rd Greek Regiment" && !u.is_hq())
                .count(),
            2
        );
        let gr3 = units.iter().find(|u| u.name == "I./3rd Gr Rgt").unwrap();
        assert_eq!(gr3.org, 40.0);
        assert_eq!(gr3.defense, 18.0, "recruit battalions, weak line");
        assert_eq!(
            units
                .iter()
                .find(|u| u.name == "Cretan Gendarmerie")
                .unwrap()
                .org,
            30.0
        );
        assert_eq!(
            units
                .iter()
                .filter(|u| u.unit_type == UnitType::AntiTankBrigade)
                .count(),
            1,
            "only the German PaK 36 (no defender AT documented)"
        );
    }

    /// End-to-end against the real shipped script (rebuilt with verified
    /// Kiev pocket battalion names): load, validate, deploy both sides —
    /// the exact path `--battle file=…` takes.
    #[test]
    fn real_1941_kiev_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1941_kiev.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1941_kiev script: {e}"));
        assert_eq!(sf.province, Some(525));
        assert_eq!(
            sf.dirs,
            vec!["NW", "E", "SE"],
            "the 6. Armee KUR-belt assault (NW), the 2. PzGr inner E face, the 1. PzGr Sula-valley SE face"
        );
        assert_eq!(sf.attacker.tag, "GER");
        assert_eq!(sf.defender.tag, "SOV");
        // The script does not prescribe the player's side — absent
        // player_side = schema default (attacker); the menu/CLI side
        // toggle flips freely.
        assert_eq!(
            sf.player_side, "attacker",
            "absent player_side = schema default"
        );
        assert_eq!(
            sf.enemy_tactic, "encirclement",
            "the pocket AI card for the default (attacker) player"
        );
        // Roster per en/de/ru Wikipedia: attacker 82 =
        // 71. ID 13 + 296. ID 13 + 95. ID 13 (9 IR I/II/III + 2 AR Abt +
        // Pz.Jg + Pi.Btl) + 10. ID (mot) 12 + 3./4. Pz 10 each + 16. Pz 11;
        // defender 63 = 147. SD 8 (2 SP x2 + 379. AP + 231. OATD + 132. TP
        // T-26 x2) + 175./206./284. SD 11 each + KUR 8 + NKVD 3 + Militia 3
        // + 97. SD 8 (26. Armiya).
        assert_eq!(
            sf.attacker.units.len(),
            82,
            "6. Armee city assault + panzer pincers"
        );
        assert_eq!(
            sf.defender.units.len(),
            63,
            "37. Armiya + KUR + garrison + 97. SD"
        );
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 7 German + 8
        // Soviet formations → 15 HQs appended after each side's roster.
        assert_eq!(units.len(), 160);
        assert_eq!(nid, 160);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 15);
        // German 1941 ID: real regimental battalions, horse-towed ARs (the
        // 1941 ID had no organic Flak — corps-level only), Pi.Btl as the
        // sapper line bn, Aufkl as recon support on the first IR.
        let ir191 = units.iter().find(|u| u.name == "I./IR 191").unwrap();
        assert_eq!(ir191.division, "71. Infanterie-Division");
        assert!(ir191.has_support(SupportKind::Recon));
        let ar171 = units.iter().find(|u| u.name == "I./AR 171").unwrap();
        assert_eq!(ar171.unit_type, UnitType::ArtilleryBrigade);
        assert_eq!(
            ar171.chassis,
            Chassis::Towed,
            "1941 ID artillery was horse-drawn"
        );
        let pzjg296 = units.iter().find(|u| u.name == "Pz.Jg.Abt.296").unwrap();
        assert_eq!(pzjg296.unit_type, UnitType::AntiTankBrigade);
        let pi171 = units.iter().find(|u| u.name == "Pi.Btl.171").unwrap();
        assert_eq!(pi171.unit_type, UnitType::Infantry);
        assert!(pi171.has_support(SupportKind::Engineer));
        // Panzer divisions: truck-towed ARs, the Kradschtz motorized bn, and
        // the Aufkl/Pi supports (3. Pz = Pz.Rgt. 6, Schtz.Rgt. 3, AR 75,
        // Pz.Jg. 39 — Model's division at Lokhvitsa).
        let pz6 = units.iter().find(|u| u.name == "I./Pz.Rgt.6").unwrap();
        assert_eq!(pz6.unit_type, UnitType::MediumArmor);
        assert_eq!(pz6.division, "3. Panzer-Division");
        assert!(pz6.has_support(SupportKind::Recon));
        let pzii6 = units.iter().find(|u| u.name == "Pz.II (Pz.Rgt.6)").unwrap();
        assert_eq!(pzii6.unit_type, UnitType::LightArmor);
        let krad3 = units.iter().find(|u| u.name == "Kradschtz.Btl.3").unwrap();
        assert_eq!(krad3.unit_type, UnitType::Motorized);
        let ar75 = units.iter().find(|u| u.name == "I./AR 75").unwrap();
        assert_eq!(
            ar75.chassis,
            Chassis::TruckTowed,
            "panzer ARs were truck-towed"
        );
        // 16. Pz had TWO grenadier regiments (64/79, Pz.Rgt. 2, AR 16) — the
        // other panzer divisions one Schützen regiment each.
        assert!(units.iter().any(|u| u.name == "I./Pz.Gren.Rgt. 64"));
        assert!(units.iter().any(|u| u.name == "I./Pz.Gren.Rgt. 79"));
        // Soviet defenders: worn rifle divisions with the real SP/AP/OATD
        // numbers (1941 shtat 04/100). The 147. SD lost its 551. SP on
        // 1 Aug (2 regiments only, org 35 — the most-worn) and rides with
        // the 132. TP T-26 remnants.
        let sp600 = units.iter().find(|u| u.name == "I./SP 600").unwrap();
        assert_eq!(sp600.division, "147. Strelkovaya Diviziya");
        assert_eq!(sp600.org, 35.0);
        assert!(
            !units.iter().any(|u| u.name.contains("551. SP")),
            "551. SP destroyed at Novyi Myropil 1 Aug"
        );
        let t26 = units.iter().find(|u| u.name == "I./132. TP").unwrap();
        assert_eq!(t26.unit_type, UnitType::LightArmor);
        assert_eq!(t26.division, "147. Strelkovaya Diviziya");
        assert_eq!(
            t26.org,
            UnitType::LightArmor.base_org(),
            "tanks keep their low armor org"
        );
        let sp69 = units.iter().find(|u| u.name == "I./SP 69").unwrap();
        assert_eq!(sp69.division, "97. Strelkovaya Diviziya (26. Armiya)");
        assert_eq!(sp69.org, 40.0);
        // KUR: machine-gun battalions with fortress defense; the fortress's
        // own guns ride the KUR division (no pure-artillery divisions).
        let opb = units.iter().find(|u| u.name == "161. OPB").unwrap();
        assert_eq!(opb.defense, 26.0);
        assert_eq!(opb.org, 45.0);
        for art in ["377. GAP", "344. GAP"] {
            let u = units.iter().find(|u| u.name == art).unwrap();
            assert_eq!(u.unit_type, UnitType::ArtilleryBrigade, "{art}");
            assert_eq!(u.division, "Kiev Fortified Region (KUR)", "{art}");
        }
        assert!(
            !units.iter().any(|u| u.division == "KUR Artillery Group"
                || u.division == "5th Tank Brigade (Remnant)"),
            "no pure-artillery divisions; the 5th TB (Sakhno) fought at Naro-Fominsk, not Kiev"
        );
        // Exclusions: the 3rd VDV Corps left for Konotop 29 Aug,
        // the 227. SD escaped 5 Sep, the 4th/10th NKVD divisions did not
        // exist in 1941, and the 164. SD was with the 18. Army in the south.
        assert!(
            !units.iter().any(|u| {
                u.division.contains("Airborne")
                    || u.division.contains("227.")
                    || u.division.contains("164.")
                    || u.division.contains("NKVD Div")
            }),
            "non-Kiev formations excluded"
        );
        let mil = units.iter().find(|u| u.name == "1. Opolchenie").unwrap();
        assert_eq!(mil.org, 25.0);
        let nkvd = units.iter().find(|u| u.name == "I./4. NKVD MSR").unwrap();
        assert_eq!(nkvd.org, 35.0);
    }

    /// Stalingrad (sources: Glantz & House
    /// "Armageddon in Stalingrad" via de-wiki citations + lexikon-der-wehrmacht
    /// + en/ru Wikipedia unit pages). The script covers the decisive city-center
    /// phase 13-26 Sep 1942.
    #[test]
    fn real_1942_stalingrad_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1942_stalingrad.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1942_stalingrad script: {e}"));
        assert_eq!(sf.province, Some(3529), "Stalingrad (VP30, urban)");
        assert_eq!(
            sf.dirs,
            vec!["NW", "SW"],
            "the German W side: northern factory axis (NW, Gorodishche-Orlovka) + south-city axis (SW, Tsaritsa-Minina); the Volga is the E edge"
        );
        assert_eq!(sf.attacker.tag, "GER");
        assert_eq!(sf.defender.tag, "SOV");
        assert_eq!(
            sf.player_side, "attacker",
            "absent player_side = schema default"
        );
        assert_eq!(sf.enemy_tactic, "urban_defense", "the city is held street by street (the Sept assault reached ~90% of the city historically, but the 62. Armiya held the Volga strip - the urban_defense card keeps the AI in the ruins; guerrilla_tactics spread the line and collapsed)");
        // Attacker 77 = 71. ID 13 (8 IR bns - III./IR 191
        // dissolved - + Pi.Btl. 171 + Pz.Jg. 171 + 2 AR + 88mm) + 295. ID 14
        // (9 IR + Pi.Btl. 295 + Pz.Jg. 295 + 2 AR + Pi.Btl. 50) + 94. ID 14
        // (9 IR + Pi.Btl. 194 + Schnelle Abt. + 2 AR + 88mm) + 389. ID 13
        // (9 IR + Pi.Btl. 389 + Pz.Jg. 389 + 2 AR) + 24. Pz 11 + 14. Pz 12
        // (Pz.Rgt + 4 Pz.Gren + Krad + Pz.Pi + Pz.Jg + 2 AR + StuG/Pi rides);
        // defender 91 = 13 GvSD 12 + 95 SD 12 + 112 SD 12 + 193 SD 11 +
        // 284 SD 13 + 10 NKVD 10 + 124 OSBR 5 + 115 OSBR 4 + 92 MSBR 3 +
        // 23 TC 4 + Proletariat Bde 2 + Workers Militia 3.
        assert_eq!(
            sf.attacker.units.len(),
            77,
            "6. Armee city assault: 77 battalions"
        );
        assert_eq!(sf.defender.units.len(), 91, "62. Armiya: 91 battalions");
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 6 German + 12
        // Soviet formations → 18 HQs appended after each side's roster.
        assert_eq!(units.len(), 186);
        assert_eq!(nid, 186);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 18);
        // 71. ID was the weakest division (300-400 men per bn on the 14 Sep
        // census): III./IR 191 dissolved, 8 line battalions at org 35; the
        // Aufkl.Abt. 171 rides I./IR 191 (Kiev convention).
        let ir191 = units.iter().find(|u| u.name == "I./IR 191").unwrap();
        assert_eq!(ir191.division, "71. Infanterie-Division");
        assert_eq!(ir191.org, 35.0);
        assert!(
            ir191.has_support(SupportKind::Recon),
            "Aufkl.Abt. 171 on I./IR 191"
        );
        assert!(
            !units.iter().any(|u| u.name == "III./IR 191"),
            "III./IR 191 dissolved by Sept 1942"
        );
        let pi171 = units.iter().find(|u| u.name == "Pi.Btl. 171").unwrap();
        assert!(pi171.has_support(SupportKind::Engineer));
        let ar171 = units.iter().find(|u| u.name == "I./AR 171").unwrap();
        assert_eq!(
            ar171.chassis,
            Chassis::Towed,
            "1942 ID artillery was horse-drawn"
        );
        // The LI Korps assault pioneers (Pi.Btl. 50) and the 88mm flak ride
        // line divisions (no pure-artillery divisions).
        let pi50 = units.iter().find(|u| u.name == "Pi.Btl. 50").unwrap();
        assert_eq!(pi50.division, "295. Infanterie-Division");
        assert_eq!(
            pi50.org, 60.0,
            "Sturm-Pionier at the game's fresh-infantry cap"
        );
        assert_eq!(pi50.soft_attack, 12.0, "Sturm-Pionier firepower");
        assert!(pi50.has_support(SupportKind::Engineer));
        let flak94 = units
            .iter()
            .find(|u| u.name == "88mm Flak (94. ID)")
            .unwrap();
        assert_eq!(flak94.division, "94. Infanterie-Division");
        assert_eq!(
            flak94.piercing, 100.0,
            "the grain-elevator 88s - the best AT in the city"
        );
        let schnelle = units
            .iter()
            .find(|u| u.name == "Schnelle Abt. 194")
            .unwrap();
        assert_eq!(schnelle.unit_type, UnitType::AntiTankBrigade);
        assert!(schnelle.has_support(SupportKind::Recon));
        // 1942 armor calibration: the Pz.III L/42 mix cannot frontally pen the
        // T-34 (50 vs 65), the StuG III F L/43 can (92), and the 88mm is 100.
        let pz24 = units.iter().find(|u| u.name == "I./Pz.Rgt.24").unwrap();
        assert_eq!(pz24.armor, 50.0);
        assert_eq!(pz24.piercing, 65.0);
        let stug244 = units.iter().find(|u| u.name == "StuG.Abt. 244").unwrap();
        assert_eq!(stug244.division, "24. Panzer-Division");
        assert_eq!(stug244.piercing, 92.0);
        let pib162 = units.iter().find(|u| u.name == "Pi.Btl. 162").unwrap();
        assert_eq!(
            pib162.division, "14. Panzer-Division",
            "XIV PzK assault pioneers"
        );
        assert!(units.iter().any(|u| u.name == "I./Pz.Gren.Rgt. 103"));
        // Soviet defenders with the real SP numbers (62. Armiya Sept roster).
        let gv34 = units.iter().find(|u| u.name == "I./34 Gv.SP").unwrap();
        assert_eq!(gv34.division, "13. Gv. Strelkovaya Diviziya");
        assert_eq!(gv34.org, 55.0, "13 GvSD crossed fresh 14/15 Sep");
        let pavlov = units.iter().find(|u| u.name == "III./42 Gv.SP").unwrap();
        assert_eq!(pavlov.org, 55.0);
        let sp1045 = units.iter().find(|u| u.name == "I./1045 SP").unwrap();
        assert_eq!(sp1045.division, "284. Strelkovaya Diviziya");
        assert_eq!(sp1045.org, 50.0, "the Siberian 284th crossed 20-22 Sep");
        // Army support rides line divisions: the Katyusha rgt rides 95. SD,
        // the 1077 ZAP (STZ AA) rides 112. SD, the 20. IPTABR rides 284. SD.
        let katyusha = units
            .iter()
            .find(|u| u.name == "92 Gv. Mortar Rgt (Katyusha)")
            .unwrap();
        assert_eq!(katyusha.unit_type, UnitType::MotRocketArtillery);
        assert_eq!(katyusha.division, "95. Strelkovaya Diviziya");
        let zap = units.iter().find(|u| u.name == "1077 ZAP").unwrap();
        assert_eq!(zap.division, "112. Strelkovaya Diviziya");
        let zis3 = units
            .iter()
            .find(|u| u.name == "I./20. IPTABR (76mm)")
            .unwrap();
        assert_eq!(zis3.division, "284. Strelkovaya Diviziya");
        assert_eq!(zis3.piercing, 85.0, "ZiS-3");
        // The 10. NKVD division (Saraev, in the city since 12 Sep): the 282nd
        // (north, untouched) strongest, 269/270 in the thick of it.
        let nkvd282 = units.iter().find(|u| u.name == "I./282 NKVD SP").unwrap();
        assert_eq!(nkvd282.org, 60.0, "freshest of the NKVD regts (game cap)");
        let nkvd269 = units.iter().find(|u| u.name == "I./269 NKVD SP").unwrap();
        assert_eq!(nkvd269.org, 60.0);
        let nkvd272 = units.iter().find(|u| u.name == "I./272 NKVD SP").unwrap();
        assert_eq!(
            nkvd272.org, 50.0,
            "worn by the encircled drama-theatre fight"
        );
        // The Volga-Flotilla marines ride the Gorokhov group; the naval 92
        // MSBR is its own marine brigade (org 30, destroyed by ~25 Sep).
        let mar32 = units
            .iter()
            .find(|u| u.name == "32. Btl. Morskoy Pekhoty")
            .unwrap();
        assert_eq!(mar32.unit_type, UnitType::Marine);
        assert_eq!(mar32.division, "124. OSBR (Gorokhov Group)");
        assert_eq!(mar32.org, 60.0);
        let nb92 = units.iter().find(|u| u.name == "I./92 MSBR").unwrap();
        assert_eq!(nb92.unit_type, UnitType::Marine);
        assert_eq!(nb92.org, 30.0);
        // T-34s keep the 1941 armor 80/piercing 81; the 189 TB rides T-70s.
        let t34 = units.iter().find(|u| u.name == "I./99 TB (T-34)").unwrap();
        assert_eq!(t34.armor, 80.0);
        assert_eq!(t34.piercing, 81.0);
        let t70 = units
            .iter()
            .find(|u| u.name == "II./189 TB (T-70)")
            .unwrap();
        assert_eq!(t70.unit_type, UnitType::LightArmor);
        assert_eq!(t70.piercing, 45.0);
        let mil = units.iter().find(|u| u.name == "STZ Destroyer Bn").unwrap();
        assert_eq!(mil.org, 25.0);
        // Exclusions (Glantz): 76. ID never fought in the city
        // (VIII. Korps, Kotluban corridor - p.741), 100. Jager arrived 26 Sep
        // for the kurgan relief, 308. SD crossed 1/2 Oct, 37./39. GvSD and the
        // 88 Gv. heavy tank rgt are October, Pi.Rgt. 672 is unverifiable.
        assert!(
            !units
                .iter()
                .any(|u| u.division.contains("76.") || u.division.contains("308")),
            "76. ID (Kotluban) and 308. SD (crossed 1/2 Oct) excluded"
        );
        assert!(
            !units
                .iter()
                .any(|u| u.division.contains("Jager") || u.division.contains("672")),
            "100. Jager arrived 26 Sep; Pi.Rgt. 672 unverifiable"
        );
        // No pure-artillery divisions: every gun/AA/AT rides a line division.
        for art in [
            "I./AR 171",
            "II./AR 171",
            "I./AR 89",
            "I./AR 4",
            "32 Gv.AP",
            "57 AP",
            "436 AP",
            "384 AP",
            "820 AP",
            "85 Gv. How. AP",
        ] {
            let u = units.iter().find(|u| u.name == art).unwrap();
            assert!(
                u.division.contains("Infanterie")
                    || u.division.contains("Panzer")
                    || u.division.contains("Strelkovaya"),
                "{art} must ride a line division, not a gun-only one"
            );
        }
    }

    /// El Alamein (sources: Wikipedia Second
    /// Battle of El Alamein + OOB article per Joslen/Playfair Vol. IV, de/it
    /// Wikipedia battle + OOB articles, Defence of Outpost Snipe, 9th/22nd/23rd
    /// Armoured Brigade + 19th Flak Division articles). The province is the
    /// coastal El Alamein station (the 8th Army's forward railhead); the battle
    /// is the NORTHERN sector fight (Lightfoot/Supercharge corridor).
    #[test]
    fn real_1942_el_alamein_script_loads_and_deploys() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("battles")
            .join("1942_el_alamein.json");
        let sf = load(&p).unwrap_or_else(|e| panic!("1942_el_alamein script: {e}"));
        assert_eq!(sf.province, Some(1071), "El-Alamein (VP1 coastal town)");
        assert_eq!(
            sf.dirs,
            vec!["E", "SE"],
            "the 8th Army attacked due west from the Alexandria railhead: E = coast road/rail corridor, SE = inland flank tracks"
        );
        assert_eq!(sf.attacker.tag, "ENG");
        assert_eq!(sf.defender.tag, "GER");
        assert_eq!(
            sf.player_side, "attacker",
            "the Montgomery offensive is the player's side"
        );
        assert_eq!(
            sf.enemy_tactic, "elastic_defense",
            "the 15. Pz/Littorio counterattack reserve = delay + counterattack"
        );
        // Attacker 84 = 51st Highland 14 (9 bns + 126/127/
        // 128 Fd + 61 AT + 40 LAA) + 9th Australian 14 (9 bns + 2/7, 2/8, 2/12 Fd
        // + 3 AT + 4 LAA) + 2nd NZ 12 (7 bns + 4/5/6 Fd + 7 AT + 14 LAA) + 1st SA
        // 14 (org 55) + 1st Armd 10 + 10th Armd 13 + 9th Armd Bde 3 + 23rd Armd
        // Bde 4 Valentines; defender 59 = 164. leichte 12 (org 55) + Trento 9
        // (org 50, motorized) + Bologna 7 (org 50) + 15. Pz 11 + Littorio 9
        // (org 50) + 90. leichte 11.
        assert_eq!(
            sf.attacker.units.len(),
            84,
            "8th Army northern sector: 84 battalions"
        );
        assert_eq!(
            sf.defender.units.len(),
            59,
            "Panzerarmee Afrika northern line: 59 battalions"
        );
        let zone = vec![HexCoord::ZERO];
        let mut units = Vec::new();
        let mut nid = 0usize;
        deploy_script_side(&mut units, &mut nid, Side::Attacker, &sf.attacker, &zone, None).unwrap();
        deploy_script_side(&mut units, &mut nid, Side::Defender, &sf.defender, &zone, None).unwrap();
        // §6.13: +1 synthesized HQ per division — 8 British + 6
        // Axis formations → 14 HQs appended after each side's roster.
        assert_eq!(units.len(), 157);
        assert_eq!(nid, 157);
        assert_eq!(units.iter().filter(|u| u.is_hq()).count(), 14);
        // Real battalion names: the 51st (Highland) fresh
        // from the UK (152/153/154 Bdes), the Point 29 assault bn 2/48 of the
        // Australians, the 23rd Armd Bde's gap-breaching Valentines with
        // sappers riding.
        assert!(units.iter().any(|u| u.name == "2 Seaforth (152 Bde)"));
        assert!(units.iter().any(|u| u.name == "1 Black Watch (154 Bde)"));
        let p29 = units.iter().find(|u| u.name == "2/48 Bn (26 Bde)").unwrap();
        assert_eq!(p29.division, "9th Australian Division");
        assert!(
            p29.has_support(SupportKind::Engineer),
            "2/48 took Point 29 on 25/26 Oct"
        );
        let val = units
            .iter()
            .find(|u| u.name == "40 RTR (Valentine)")
            .unwrap();
        assert_eq!(val.unit_type, UnitType::LightArmor);
        assert_eq!(val.armor, 60.0);
        assert_eq!(
            val.piercing, 55.0,
            "Alamein Valentines still carried the 2-pdr"
        );
        assert_eq!(val.org, 10.0, "armor battalions sit at the base org-10 cap (the org override only models under-strength)");
        assert!(
            val.has_support(SupportKind::Engineer),
            "the gap-breaching sappers"
        );
        // 1942 tank calibration: the Sherman M4 75mm (80/81) beats the Pz.III
        // L/42-L/60 mix (50/70) frontally, the Pz.IV F2 (92) and the 88mm (100)
        // are the killer guns, the M13/40 (40/45) is hopeless vs the Grants —
        // all historical.
        let sherman = units
            .iter()
            .find(|u| u.name == "Queen's Bays (2 Armd Bde)")
            .unwrap();
        assert_eq!(sherman.unit_type, UnitType::MediumArmor);
        assert_eq!(sherman.armor, 80.0);
        assert_eq!(sherman.piercing, 81.0);
        let grant = units
            .iter()
            .find(|u| u.name == "3 RTR (8 Armd Bde)")
            .unwrap();
        assert_eq!(grant.armor, 65.0, "Grant 75mm in the hull, weaker front");
        assert_eq!(grant.piercing, 81.0);
        let pz3 = units
            .iter()
            .find(|u| u.name == "I./Pz.Rgt.8 (Pz.III)")
            .unwrap();
        assert_eq!(pz3.armor, 50.0);
        assert_eq!(pz3.piercing, 70.0, "5cm L/42-L/60 mix");
        let pz4 = units
            .iter()
            .find(|u| u.name == "II./Pz.Rgt.8 (Pz.IV F2)")
            .unwrap();
        assert_eq!(
            pz4.piercing, 92.0,
            "7.5cm L/43 - the Sherman killer besides the 88"
        );
        let m13 = units
            .iter()
            .find(|u| u.name == "IV Btg Carristi (M13/40)")
            .unwrap();
        assert_eq!(m13.unit_type, UnitType::LightArmor);
        assert_eq!(m13.org, 10.0, "armor battalions sit at the base org-10 cap");
        assert_eq!(
            m13.piercing, 45.0,
            "the 47/32 could not hurt the Grants - historical"
        );
        // The 88mm flak rides line divisions — the Kidney/Snipe
        // tank-killers with piercing 100.
        for flak in [
            "88mm Flak (19. Flak-Div)",
            "88mm Flak (19. Flak-Div)",
            "XXIX Grp 8.8cm",
        ] {
            let u = units.iter().find(|u| u.name == flak).unwrap();
            assert_eq!(u.piercing, 100.0, "{flak}");
        }
        assert!(
            units
                .iter()
                .filter(|u| u.name == "88mm Flak (19. Flak-Div)")
                .count()
                == 2
        );
        // Italian orgs: Trento/Bologna/Littorio battle-worn at 50, the fresh
        // 90. leichte (best Axis infantry) and 15. Pz at full 60.
        let trento = units.iter().find(|u| u.name == "I./61 Rgt").unwrap();
        assert_eq!(trento.division, "Div. Trento (Motorizzata)");
        assert_eq!(trento.org, 50.0);
        assert_eq!(trento.unit_type, UnitType::Motorized);
        let bologna = units.iter().find(|u| u.name == "I./39 Rgt").unwrap();
        assert_eq!(bologna.unit_type, UnitType::Infantry);
        assert_eq!(bologna.org, 50.0);
        let g90 = units
            .iter()
            .find(|u| u.name == "I./Pz.Gren.Rgt.155")
            .unwrap();
        assert_eq!(g90.division, "90. leichte Afrika-Division");
        assert_eq!(g90.org, 60.0);
        // Exclusions (historically out of the NORTHERN sector):
        // 21. Pz + Ariete (southern reserve), Ramcke/Folgore/Brescia/Pavia
        // (central/southern line), Trieste (Fuka reserve), 4th Indian (held
        // Ruweisat), 7th Armd + 50th/44th Divs (XIII Corps south).
        for out in [
            "21.",
            "Ariete",
            "Ramcke",
            "Folgore",
            "Brescia",
            "Pavia",
            "Trieste",
            "Indian",
            "7th Armoured",
        ] {
            assert!(
                !units.iter().any(|u| u.division.contains(out)),
                "'{out}' must not appear (out of the northern sector)"
            );
        }
        // No pure-artillery divisions: every gun/AT/AA rides a line division.
        for art in [
            "126 Fd Rgt RA",
            "2/7 Fd Rgt RAA",
            "4 Fd Rgt NZA",
            "1 Cape Fd Rgt SAA",
            "2 RHA",
            "1 RHA",
            "I./AR 220",
            "I./46 Rgt Art",
            "I./205 Rgt Art",
            "I./Art.Rgt.33",
            "I./3 Rgt Art Celere",
            "I./Art.Rgt.190",
        ] {
            let u = units.iter().find(|u| u.name == art).unwrap();
            assert!(
                u.division.contains("Division")
                    || u.division.contains("leichte")
                    || u.division.contains("Bologna")
                    || u.division.contains("Motorizzata")
                    || u.division.contains("Corazzata"),
                "{art} must ride a line division, not a gun-only one"
            );
        }
    }
}
