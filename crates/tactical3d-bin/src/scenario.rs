//! Scenario = a forged tac_start: one struct carries the whole
//! battle setup — map, forces, tactic, side, tags — and one assemble path
//! serves the menu's Debug Battle form, the `--battle` CLI (agent
//! automation), and (field-compatible) the live log listener. Forces come
//! from built-in presets or from a real .hoi4 save.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tactical_ai::CombatTactic;
use tactical_core::grid::HexGrid;
use tactical_core::hex::{HexCoord, HexDirection};
use tactical_core::unit::{BattalionUnit, Side, UnitType};
use tactical_map::MapGenerator;
use tactical_save::{
    create_tactical_units_named, leader_bonus, CountryCombatModifier, CountryOrgModifier,
    DoctrineTable, EquipmentTable, ModifierTable, SaveParser, UnitNaming, UnitTemplateTable,
};

use crate::demo::{arena_grid, arena_zones};
use crate::script;
use crate::settings::AppSettings;

/// Enemy tactics offered in the debug form (same list as the CLI builder).
/// The full 16-card library. Index order is contractual: 1-9 are the
/// original nine cards (9 = Default); the seven later cards fill 10-16 so
/// every existing `tactic=N` script/doc keeps its meaning.
pub const TACTICS: [CombatTactic; 16] = [
    CombatTactic::Blitz,               // 1
    CombatTactic::ElasticDefense,      // 2
    CombatTactic::OverwhelmingFire,    // 3
    CombatTactic::InfiltrationAssault, // 4
    CombatTactic::MassCharge,          // 5
    CombatTactic::GuerrillaTactics,    // 6
    CombatTactic::TacticalWithdrawal,  // 7
    CombatTactic::Encirclement,        // 8
    CombatTactic::Default,             // 9 (contractual — original numbering)
    CombatTactic::Counterattack,       // 10
    CombatTactic::Ambush,              // 11
    CombatTactic::RiverDefense,        // 12
    CombatTactic::UrbanDefense,        // 13
    CombatTactic::Delay,               // 14
    CombatTactic::Assault,             // 15
    CombatTactic::RiverAssault,        // 16
];

/// Map source.
#[derive(Debug, Clone)]
pub enum MapChoice {
    /// Flat 64×64 arena (no HOI4 data needed).
    Arena,
    /// Real province by id + attack directions.
    Province { id: u32, dirs: Vec<HexDirection> },
}

/// Force source for one side.
#[derive(Debug, Clone, PartialEq)]
pub enum ForceChoice {
    /// Built-in preset: 1 = Panzer, 2 = Infantry, 3 = Mixed.
    Preset(u8),
    /// Divisions of `tag` located in the battle province, read from a save.
    FromSave { tag: String },
}

/// The forged tac_start: everything a battle needs.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub map: MapChoice,
    pub attacker: ForceChoice,
    pub defender: ForceChoice,
    pub enemy_tactic: CombatTactic,
    pub player_side: Side,
    /// Country tags drive theme colors and the save lookup.
    pub atk_tag: String,
    pub def_tag: String,
    /// Save file for FromSave forces (None = newest in the saves dir).
    pub save_path: Option<PathBuf>,
    /// Battle script file (data/battles/<name>.json): when set,
    /// the script's province/dirs/rosters/tags/tactic/side replace every
    /// other scenario field. The script is loaded by `assemble` so the file
    /// stays the single source of truth for the battle.
    pub file: Option<PathBuf>,
    /// When true and `file` is set, `player_side` overrides the
    /// script's own `player_side` field (the debug form's side toggle on a
    /// script battle — e.g. play the Warsaw script from the defender side).
    /// Without it the script file's side wins.
    pub side_override: bool,
    /// §6.11: explicit field-battle flag anchors from the script
    /// file's `flags:` field (loaded by `assemble`; empty = fallback).
    pub script_flags: Vec<HexCoord>,
    /// Combat RNG seed override (None = deterministic default 7).
    pub seed: Option<u64>,
    /// Per-nation command overrides for a script battle —
    /// country tag → true = player commands its divisions (false = allied
    /// AI), overriding the script's own `divisions:` control values. Empty =
    /// the script file is the single source of truth (all legacy runs).
    pub div_control: std::collections::HashMap<String, bool>,
}

/// Assembled battle, ready for `battle::run`.
pub struct BattleSpec {
    pub grid: HexGrid,
    pub units: Vec<BattalionUnit>,
    pub zones: (Vec<HexCoord>, Vec<HexCoord>),
    pub player_side: Side,
    pub enemy_tactic: CombatTactic,
    pub atk_tag: String,
    pub def_tag: String,
    pub location: String,
    pub vp_label: Option<(String, HexCoord)>,
    /// Battle province (live sync / injection context; None for synthetic).
    pub province: Option<u32>,
    pub template: String,
    /// §6.11: the battle's flag board — zone anchors + capture
    /// progress (city = VP-urban cluster; field = script `flags:` or the
    /// deep-band fallback). `None` = annihilation-only battle.
    pub flags: Option<tactical_core::FlagState>,
    /// Combat RNG seed override: None = the deterministic default (7)
    /// so `--battle` runs stay reproducible; live assembly sets a varying
    /// seed so repeated battles don't replay identical dice.
    pub seed: Option<u64>,
    /// §7.5: every division name → its country tag (both sides;
    /// the side tag for undeclared divisions). Empty for non-script battles.
    pub division_tags: std::collections::HashMap<String, String>,
    /// The PLAYER side's AI-controlled allied contingents, grouped
    /// by country tag. Empty = the player commands her whole side (default).
    pub allies: Vec<tactical3d_render::game::AllyContingent>,
    /// HOI4 battle context for the `damage_units`
    /// sync batches (live from-save assemblies only; None elsewhere).
    pub battle_ctx: Option<tactical_sync::BattleContext>,
    /// Damage writeback mode from settings (§12).
    pub writeback_mode: tactical_sync::WritebackMode,
    /// In-game battle start datetime `(year, month, day, hour)`
    /// from the save's `date` header (live assemblies only) — the battle
    /// clock then displays absolute game time. None = elapsed-only clock.
    pub start_datetime: Option<(i32, u32, u32, u32)>,
    /// §8.2: the mid-battle HOI4 division roster — the live
    /// assembly seeds it from the `land_combat` unit lists; every post-sync
    /// save snapshot then diffs against it (departures march off,
    /// reinforcements enter at the map edge, damage pools re-derive).
    /// Empty outside live battles.
    pub roster: tactical_sync::BattleRoster,
}

// ---------------------------------------------------------------------------
// MapGenerator cache (provinces.bmp load takes ~2 s; reuse across menu trips)

static GENERATOR: OnceLock<Mutex<Option<(String, MapGenerator)>>> = OnceLock::new();

/// Shared generator keyed by the HOI4 dir string; rebuilt when the setting
/// changes. Returns None when the HOI4 install is not usable.
pub fn map_generator(
    settings: &AppSettings,
) -> Option<&'static Mutex<Option<(String, MapGenerator)>>> {
    let cell = GENERATOR.get_or_init(|| Mutex::new(None));
    // The cache key carries the UI language too — the generator
    // bakes VP names from the matching HOI4 localisation yml.
    let key = format!("{}|{}", settings.hoi4_dir, settings.language);
    {
        let guard = cell.lock().ok()?;
        if guard.as_ref().map(|(k, _)| k == &key).unwrap_or(false) {
            return Some(cell);
        }
        drop(guard);
    }
    let hoi4_dir = settings.hoi4_dir()?;
    let gen = build_generator(
        &hoi4_dir,
        settings.language() == tactical_locale::Language::SimpChinese,
    )?;
    *cell.lock().ok()? = Some((key, gen));
    Some(cell)
}

/// Lock the cached generator (caller holds the guard while generating).
pub fn with_generator<R>(settings: &AppSettings, f: impl FnOnce(&MapGenerator) -> R) -> Option<R> {
    let cell = map_generator(settings)?;
    let guard = cell.lock().ok()?;
    let (_, gen) = guard.as_ref().unwrap();
    Some(f(gen))
}

fn build_generator(hoi4_dir: &Path, simp_chinese: bool) -> Option<MapGenerator> {
    let map_dir = hoi4_dir.join("map");
    let pm = tactical_map::ProvinceMap::load_bmp(&map_dir.join("provinces.bmp")).ok()?;
    let defs = tactical_map::load_definition_csv(&map_dir.join("definition.csv")).ok()?;
    let adj =
        tactical_map::load_adjacencies_csv(&map_dir.join("adjacencies.csv")).unwrap_or_default();
    let mut gen = MapGenerator::new(pm, defs, adj);
    if let Ok(rivers) = tactical_map::IndexMap::load_indexed_bmp(&map_dir.join("rivers.bmp")) {
        gen.set_rivers(rivers);
    }
    let vps = tactical_map::load_victory_points(&tactical_map::states_dir_of(hoi4_dir))
        .unwrap_or_default();
    gen.set_victory_points(vps);
    gen.set_unit_stacks(tactical_map::load_unit_stacks(
        &map_dir.join("unitstacks.txt"),
    ));
    // VP names in the UI language (Chinese install → Chinese city
    // names; English / missing folder → the English yml).
    gen.set_vp_names(tactical_map::load_vp_names(&tactical_map::vp_names_path(
        hoi4_dir,
        simp_chinese,
    )));
    Some(gen)
}

// ---------------------------------------------------------------------------
// Assemble

/// §6.11: derive the battle's flag board — city battle (VP
/// urban cluster) first, then the script's explicit `flags:` anchors, then
/// the defender-zone deep-band fallback. Deterministic under `seed`.
fn battle_flags(
    grid: &HexGrid,
    zones: &(Vec<HexCoord>, Vec<HexCoord>),
    script_anchors: &[HexCoord],
    seed: u64,
) -> Option<tactical_core::FlagState> {
    let params = tactical_core::CombatParams::default();
    let mut rng = tactical_core::XorShift64::new(seed ^ 0xF1A6);
    tactical_core::derive_flag_state(grid, &zones.0, &zones.1, script_anchors, &params, &mut rng)
}

/// Assemble the scenario into a runnable battle. Heavy: generates the map,
/// may parse a save. Errors are user-readable strings for the menu form.
pub fn assemble(sc: &Scenario, settings: &AppSettings) -> Result<BattleSpec, String> {
    // --- Script file: the file is the single
    // source of truth — its province/dirs/rosters/tags/tactic/side replace
    // every other scenario field (the CLI k=v pairs must not silently mix
    // with it).
    let mut sc = sc.clone();
    let script_sides: Option<(script::ScriptSide, script::ScriptSide)> =
        if let Some(path) = &sc.file {
            let mut sf = script::load(path)?;
            let dirs: Vec<HexDirection> = sf
                .dirs
                .iter()
                .filter_map(|d| HexDirection::from_token(d))
                .collect();
            sc.map = match sf.map.as_deref() {
                // A script may opt into the flat arena instead of a
                // real province (balance-experiment battles).
                Some("synthetic") => MapChoice::Arena,
                _ => MapChoice::Province {
                    id: sf.province.expect("validated in load"),
                    dirs,
                },
            };
            sc.enemy_tactic = CombatTactic::from_str(&sf.enemy_tactic);
            // The debug form's side toggle overrides the script's
            // own side; otherwise the file wins (single source of truth).
            if !sc.side_override {
                sc.player_side = script::player_side(&sf.player_side).expect("validated in load");
            }
            sc.atk_tag = sf.attacker.tag.clone();
            sc.def_tag = sf.defender.tag.clone();
            // §6.11: script `flags:` anchors for field battles.
            sc.script_flags = sf.flags.iter().map(|f| HexCoord::new(f.q, f.r)).collect();
            // The menu's nation selector overrides the script's
            // per-division control values (tag → player|ai). Applied to the
            // side the PLAYER commands BEFORE the tag map and allied
            // contingent grouping below, so both stay consistent with the
            // overridden command split.
            let player_script_side = match sc.player_side {
                Side::Attacker => &mut sf.attacker,
                Side::Defender => &mut sf.defender,
            };
            script::apply_control_overrides(player_script_side, &sc.div_control);
            Some((sf.attacker, sf.defender))
        } else {
            None
        };
    // §7.5: the script's division→tag table + the PLAYER side's
    // AI-controlled allied contingents (resolved AFTER the side
    // override above, so `sc.player_side` is final). Non-script battles
    // leave both empty — the player commands her whole side.
    let (division_tags, allies) = match &script_sides {
        Some((atk, def)) => {
            let player_script_side = match sc.player_side {
                Side::Attacker => atk,
                Side::Defender => def,
            };
            (
                script::division_tag_map(atk, def),
                script::allied_contingents(player_script_side, sc.player_side),
            )
        }
        None => (std::collections::HashMap::new(), Vec::new()),
    };
    // --- Map + zones ---
    let (grid, zones, location, vp_label, province) = match &sc.map {
        MapChoice::Arena => {
            let g = arena_grid();
            let z = arena_zones(&g);
            (g, z, "Arena (synthetic)".to_string(), None, None)
        }
        MapChoice::Province { id, dirs } => {
            let tmap = with_generator(settings, |gen| {
                // The elevation-noise field keys off the battle
                // seed — each seed fights on its own relief (same seed →
                // identical relief, the determinism contract).
                gen.generate_with_elevation_seed(*id, dirs, sc.seed.unwrap_or(7))
            })
            .ok_or("HOI4 map data unavailable (check Settings → HOI4 install dir)")?
            .map_err(|e| format!("map generation failed for province {id}: {e}"))?;
            let location = tmap
                .vp_label
                .as_ref()
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| format!("Province #{id}"));
            let zones = (tmap.zones.attacker.clone(), tmap.zones.defender.clone());
            let grid = tmap.grid.clone();
            (grid, zones, location, tmap.vp_label.clone(), Some(*id))
        }
    };

    // --- Forces ---
    let needs_save = matches!(sc.attacker, ForceChoice::FromSave { .. })
        || matches!(sc.defender, ForceChoice::FromSave { .. });
    let mut units: Vec<BattalionUnit> = Vec::new();
    let mut next_id = 0usize;
    let mut template = "Debug Division".to_string();
    if let Some((atk, def)) = &script_sides {
        // Canonical-key terrain adjusters for script battalions
        // (a missing table degrades to zero adjusters, §5.3 fallback).
        let adj_templates =
            UnitTemplateTable::load(crate::dirs::runtime_root().join("data/unit_templates.json"))
                .ok();
        script::deploy_script_side(
            &mut units,
            &mut next_id,
            Side::Attacker,
            atk,
            &zones.0,
            adj_templates.as_ref(),
        )?;
        script::deploy_script_side(
            &mut units,
            &mut next_id,
            Side::Defender,
            def,
            &zones.1,
            adj_templates.as_ref(),
        )?;
        template = atk.division.clone();
        // Stamp every battalion's country tag from the
        // division→tag table (HQs included — they carry the division name).
        // Non-script battles leave tags empty → side-colored base plates.
        for u in &mut units {
            u.tag = division_tags.get(&u.division).cloned().unwrap_or_default();
        }
    } else if needs_save {
        let save_path = match &sc.save_path {
            Some(p) => p.clone(),
            None => crate::settings::newest_save_in(
                &settings
                    .saves_dir()
                    .ok_or("saves dir invalid (check Settings)")?,
            )
            .ok_or("no .hoi4 save found in the saves dir")?,
        };
        let save_game =
            SaveParser::parse_save(&save_path).map_err(|e| format!("save parse failed: {e}"))?;
        let data_dir = crate::dirs::runtime_root().join("data");
        let equipment = EquipmentTable::load(data_dir.join("equipment_stats.json"))
            .map_err(|e| format!("equipment_stats.json: {e}"))?;
        let templates = UnitTemplateTable::load(data_dir.join("unit_templates.json"))
            .map_err(|e| format!("unit_templates.json: {e}"))?;
        // National org modifiers (missing/malformed file =
        // neutral — the loader degrades silently).
        let modifiers = ModifierTable::load(data_dir.join("modifiers.json"));
        // Doctrine combat factors — same degrade-silent contract
        // (a fidelity refinement, not a hard dependency).
        let doctrine_table =
            DoctrineTable::load(data_dir.join("doctrine_bonuses.json")).unwrap_or_default();
        // Generated battalion names in the session language.
        let naming = crate::naming::localized_unit_naming(&tactical_locale::Locale::load(
            settings.language(),
        ));
        let province_id = match sc.map {
            MapChoice::Province { id, .. } => id,
            MapChoice::Arena => {
                return Err("From-save forces need a real province map".into())
            }
        };
        for (force, side) in [
            (&sc.attacker, Side::Attacker),
            (&sc.defender, Side::Defender),
        ] {
            match force {
                ForceChoice::Preset(p) => {
                    let zone = match side {
                        Side::Attacker => &zones.0,
                        Side::Defender => &zones.1,
                    };
                    deploy_force(
                        &mut units,
                        &mut next_id,
                        side,
                        &p.to_string(),
                        zone,
                        Some(&templates),
                    );
                }
                ForceChoice::FromSave { tag } => {
                    let divs =
                        tactical_save::find_divisions_in_province(&save_game, tag, province_id);
                    if divs.is_empty() {
                        return Err(format!(
                            "no divisions of {tag} found in province {province_id} (save: {})",
                            save_path.display()
                        ));
                    }
                    if side == sc.player_side && template == "Debug Division" {
                        template = divs[0].template_name.clone();
                    }
                    // The force's country org modifier, computed
                    // once for all its divisions.
                    let org_mod = save_game
                        .countries
                        .get(tag.as_str())
                        .map(|c| CountryOrgModifier::of(c, &modifiers))
                        .unwrap_or_default();
                    // The force's researched doctrine factors +
                    // national combat modifiers.
                    let doctrines = save_game
                        .countries
                        .get(tag.as_str())
                        .map(|c| doctrine_table.researched(&c.technologies));
                    let combat_mod = save_game
                        .countries
                        .get(tag.as_str())
                        .map(|c| CountryCombatModifier::of(c, &modifiers))
                        .unwrap_or_default();
                    let mut made = Vec::new();
                    for d in divs {
                        // The division commander's bonuses
                        // resolve PER DIVISION — each division may sit in a
                        // different army under a different general.
                        let leader = leader_bonus(&save_game, &modifiers, d.id);
                        let us = create_tactical_units_named(
                            d,
                            side,
                            &equipment,
                            &templates,
                            doctrines.as_ref(),
                            org_mod,
                            combat_mod,
                            &leader,
                            next_id,
                            &naming,
                        );
                        // Advance the GLOBAL id counter by this division's
                        // size — `made.len()` is force-local and collided
                        // with earlier forces' ids.
                        next_id += us.len();
                        made.extend(us);
                    }
                    auto_deploy(&mut made, side, &zones);
                    units.extend(made);
                }
            }
        }
    } else {
        // Canonical-key terrain adjusters for preset battalions
        // (a missing table degrades to zero adjusters, §5.3 fallback).
        let adj_templates =
            UnitTemplateTable::load(crate::dirs::runtime_root().join("data/unit_templates.json"))
                .ok();
        deploy_force(
            &mut units,
            &mut next_id,
            Side::Attacker,
            &preset_num(&sc.attacker),
            &zones.0,
            adj_templates.as_ref(),
        );
        deploy_force(
            &mut units,
            &mut next_id,
            Side::Defender,
            &preset_num(&sc.defender),
            &zones.1,
            adj_templates.as_ref(),
        );
        template = format!("{} Division", preset_name(&preset_num(&sc.attacker)));
    }
    if units.is_empty() {
        return Err("scenario produced no battalions".into());
    }
    check_zones(&zones, &units)?;
    // The player's battalions start OFF the
    // board — the Deployment phase places them from the OOB window (or Auto
    // Deploy). The enemy keeps its pre-spread positions; the AI re-plans
    // them at BeginBattle anyway.
    mark_player_undeployed(&mut units, sc.player_side);
    let flags = battle_flags(&grid, &zones, &sc.script_flags, sc.seed.unwrap_or(7));

    Ok(BattleSpec {
        grid,
        units,
        zones,
        player_side: sc.player_side,
        enemy_tactic: sc.enemy_tactic,
        atk_tag: sc.atk_tag.clone(),
        def_tag: sc.def_tag.clone(),
        location,
        vp_label,
        province,
        template,
        flags,
        seed: sc.seed,
        division_tags,
        allies,
        battle_ctx: None,
        writeback_mode: tactical_sync::WritebackMode::default(),
        start_datetime: None,
        roster: tactical_sync::BattleRoster::default(),
    })
}

fn preset_num(f: &ForceChoice) -> String {
    match f {
        ForceChoice::Preset(p) => p.to_string(),
        ForceChoice::FromSave { .. } => "2".into(),
    }
}

fn preset_name(p: &str) -> &'static str {
    match p {
        "1" => "Panzer",
        "3" => "Mixed",
        _ => "Infantry",
    }
}

/// From-save divisions land spread across their deployment zone.
fn auto_deploy(made: &mut [BattalionUnit], side: Side, zones: &(Vec<HexCoord>, Vec<HexCoord>)) {
    let zone = match side {
        Side::Attacker => &zones.0,
        Side::Defender => &zones.1,
    };
    for (i, u) in made.iter_mut().enumerate() {
        if !zone.is_empty() {
            // Distinct hex per unit while capacity lasts; the wrap only
            // stacks when the force outnumbers the zone, and the deployment
            // phase (player drag / AI planner) re-places everyone anyway.
            u.position = zone[i % zone.len()];
        }
    }
}

/// A side with battalions but no deployment zone would pile every unit onto
/// (0,0) — treat it as an assembly error, not a silent misplacement.
fn check_zones(
    zones: &(Vec<HexCoord>, Vec<HexCoord>),
    units: &[BattalionUnit],
) -> Result<(), String> {
    let has = |s: Side| units.iter().any(|u| u.side == s && u.is_combat_effective());
    if has(Side::Attacker) && zones.0.is_empty() {
        return Err("attacker has units but an empty deployment zone".into());
    }
    if has(Side::Defender) && zones.1.is_empty() {
        return Err("defender has units but an empty deployment zone".into());
    }
    Ok(())
}

/// The player's battalions start OFF the
/// board (`undeployed` + the OFFBOARD sentinel) — the Deployment phase
/// places them from the OOB window or hands them to Auto Deploy. The enemy
/// side is untouched (the AI re-plans it at BeginBattle anyway).
pub fn mark_player_undeployed(units: &mut [BattalionUnit], player_side: Side) {
    for u in units {
        if u.side == player_side {
            u.undeployed = true;
            u.position = BattalionUnit::OFFBOARD;
        }
    }
}

// ---------------------------------------------------------------------------
// Force presets (moved from debug.rs so menu/CLI/stdin builders share them)

/// Instantiate a force preset and spread it across its deployment zone.
/// `templates` feeds the per-battalion vanilla terrain adjusters
/// via canonical template keys; None = zero adjusters.
pub fn deploy_force(
    units: &mut Vec<BattalionUnit>,
    next_id: &mut usize,
    side: Side,
    preset: &str,
    zone: &[HexCoord],
    templates: Option<&UnitTemplateTable>,
) {
    let roster: Vec<(&str, UnitType, f32, f32, f32, f32, f32, f32, f32)> = match preset {
        // (name, type, soft, hard, defense, breakthrough, armor, piercing, hardness)
        // HOI4 1939 equipment calibration. Full-division scale:
        // HOI4 divisions run 9-25 battalions; a real battle
        // involves several — the presets approximate one full division each.
        "1" => vec![
            (
                "1.Pz",
                UnitType::MediumArmor,
                19.0,
                14.0,
                5.0,
                36.0,
                60.0,
                61.0,
                0.9,
            ),
            (
                "2.Pz",
                UnitType::MediumArmor,
                19.0,
                14.0,
                5.0,
                36.0,
                60.0,
                61.0,
                0.9,
            ),
            (
                "3.Pz",
                UnitType::MediumArmor,
                19.0,
                14.0,
                5.0,
                36.0,
                60.0,
                61.0,
                0.9,
            ),
            (
                "4.Pz",
                UnitType::MediumArmor,
                19.0,
                14.0,
                5.0,
                36.0,
                60.0,
                61.0,
                0.9,
            ),
            (
                "1.PzL",
                UnitType::LightArmor,
                13.0,
                4.0,
                4.0,
                26.0,
                10.0,
                10.0,
                0.8,
            ),
            (
                "2.PzL",
                UnitType::LightArmor,
                13.0,
                4.0,
                4.0,
                26.0,
                10.0,
                10.0,
                0.8,
            ),
            (
                "1.Mot",
                UnitType::Motorized,
                6.0,
                1.0,
                16.0,
                5.0,
                0.0,
                4.0,
                0.1,
            ),
            (
                "2.Mot",
                UnitType::Motorized,
                6.0,
                1.0,
                16.0,
                5.0,
                0.0,
                4.0,
                0.1,
            ),
            (
                "3.Mot",
                UnitType::Motorized,
                6.0,
                1.0,
                16.0,
                5.0,
                0.0,
                4.0,
                0.1,
            ),
            (
                "1.Mech",
                UnitType::Mechanized,
                7.0,
                2.0,
                14.0,
                6.0,
                0.0,
                8.0,
                0.3,
            ),
            (
                "1.Art",
                UnitType::ArtilleryBrigade,
                25.0,
                2.0,
                10.0,
                6.0,
                0.0,
                5.0,
                0.0,
            ),
            (
                "1.AT",
                UnitType::AntiTankBrigade,
                4.0,
                20.0,
                4.0,
                2.0,
                0.0,
                60.0,
                0.0,
            ),
        ],
        "3" => vec![
            (
                "1.Pz",
                UnitType::MediumArmor,
                19.0,
                14.0,
                5.0,
                36.0,
                60.0,
                61.0,
                0.9,
            ),
            (
                "2.Pz",
                UnitType::MediumArmor,
                19.0,
                14.0,
                5.0,
                36.0,
                60.0,
                61.0,
                0.9,
            ),
            (
                "1.Mot",
                UnitType::Motorized,
                6.0,
                1.0,
                16.0,
                5.0,
                0.0,
                4.0,
                0.1,
            ),
            (
                "2.Mot",
                UnitType::Motorized,
                6.0,
                1.0,
                16.0,
                5.0,
                0.0,
                4.0,
                0.1,
            ),
            (
                "1.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            // Katyusha-style self-propelled rocket (wheeled, no
            // emplacement, min range 3) — the towed Nebelwerfer variant is
            // UnitType::RocketArtillery. Soft 36 (heavier than tube
            // artillery, 3-turn reload balances it).
            (
                "1.Rkt",
                UnitType::MotRocketArtillery,
                36.0,
                1.0,
                12.0,
                9.0,
                0.0,
                2.0,
                0.0,
            ),
            // Towed-gun additions: Nebelwerfer
            // + AT + AA round out the emplacement family test bench.
            (
                "1.Nbw",
                UnitType::RocketArtillery,
                36.0,
                1.0,
                12.0,
                9.0,
                0.0,
                2.0,
                0.0,
            ),
            (
                "1.AT",
                UnitType::AntiTankBrigade,
                4.0,
                20.0,
                4.0,
                2.0,
                0.0,
                60.0,
                0.0,
            ),
            (
                "1.AA",
                UnitType::AntiAirBrigade,
                4.0,
                1.0,
                8.0,
                2.0,
                0.0,
                5.0,
                0.0,
            ),
        ],
        _ => vec![
            (
                "1.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "2.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "3.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "4.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "5.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "6.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "7.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "8.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "9.Inf",
                UnitType::Infantry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            // Regression bench: cavalry must NOT offer Hold.
            (
                "1.Cav",
                UnitType::Cavalry,
                6.0,
                1.0,
                22.0,
                3.0,
                0.0,
                4.0,
                0.0,
            ),
            (
                "1.Art",
                UnitType::ArtilleryBrigade,
                25.0,
                2.0,
                10.0,
                6.0,
                0.0,
                5.0,
                0.0,
            ),
            (
                "2.Art",
                UnitType::ArtilleryBrigade,
                25.0,
                2.0,
                10.0,
                6.0,
                0.0,
                5.0,
                0.0,
            ),
            (
                "1.AT",
                UnitType::AntiTankBrigade,
                4.0,
                20.0,
                4.0,
                2.0,
                0.0,
                60.0,
                0.0,
            ),
            (
                "1.AA",
                UnitType::AntiAirBrigade,
                4.0,
                1.0,
                8.0,
                2.0,
                0.0,
                5.0,
                0.0,
            ),
        ],
    };

    for (i, (name, ut, sa, ha, def, brk, armor, pier, hard)) in roster.into_iter().enumerate() {
        let pos = if zone.is_empty() {
            HexCoord::ZERO
        } else {
            zone[i % zone.len()]
        };
        let mut u = BattalionUnit::new(*next_id, name, ut, side, pos);
        // Vanilla terrain adjusters via the canonical key.
        u.terrain_adj = templates
            .map(|t| t.terrain_adjusters_for(ut))
            .unwrap_or_default();
        // Debug presets form one ad-hoc division each (OOB tree).
        u.division = format!("{} Division", preset_name(preset));
        u.soft_attack = sa;
        u.hard_attack = ha;
        u.defense = def;
        u.breakthrough = brk;
        u.armor = armor;
        u.piercing = pier;
        u.hardness = hard;
        // HOI4: a division marches at its slowest battalion, so fast
        // divisions field truck-drawn guns (mapping.rs: mot_*_brigade →
        // TruckTowed, 12 km/h but still emplaces). Horse guns stay with the
        // infantry preset; 1.Nbw keeps Towed as the towed-rocket showcase.
        if matches!(preset, "1" | "3")
            && matches!(
                ut,
                UnitType::ArtilleryBrigade | UnitType::AntiTankBrigade | UnitType::AntiAirBrigade
            )
        {
            u.set_chassis(tactical_core::unit::Chassis::TruckTowed);
        }
        *next_id += 1;
        units.push(u);
    }

    // Support companies ride with their battalions (same scheme as
    // the demo): recon → lead armor; infantry preset's AT/Eng → 1./2. Inf.
    let attach = |units: &mut Vec<BattalionUnit>,
                  host: &str,
                  kind: tactical_core::SupportKind,
                  name: &str| {
        if let Some(u) = units.iter_mut().find(|u| u.name == host && u.side == side) {
            u.attach(tactical_core::SupportAttachment {
                kind,
                name: name.to_string(),
            });
        }
    };
    match preset {
        "1" => {
            attach(units, "1.Pz", tactical_core::SupportKind::Recon, "Aufkl");
            attach(
                units,
                "1.Mot",
                tactical_core::SupportKind::Engineer,
                "1.Eng",
            );
        }
        "3" => attach(units, "1.Pz", tactical_core::SupportKind::Recon, "1.AC"),
        _ => {
            // AT is a line battalion now; the support companies are Eng + Cav recon.
            attach(
                units,
                "1.Inf",
                tactical_core::SupportKind::Engineer,
                "1.Eng",
            );
            attach(units, "1.Cav", tactical_core::SupportKind::Recon, "1.Rec");
        }
    }

    // §6.13: one synthesized HQ per division (debug presets form
    // one ad-hoc division each, so one HQ per preset force), taking the zone
    // slots right after the roster.
    let base = units
        .iter()
        .filter(|u| u.side == side && !u.is_hq())
        .count();
    tactical_core::synthesize_hqs(units, next_id, side, |n| {
        if zone.is_empty() {
            tactical_core::hex::HexCoord::ZERO
        } else {
            zone[(base + n) % zone.len()]
        }
    });
}

// ---------------------------------------------------------------------------
// Live assembly (tac_start from game.log → battle)

/// Assemble a battle from a real tac_start trigger (live mode / menu listen).
/// The save's `land_combat` record is the battle truth: contested province,
/// side membership (player attack/defend), participating division ids and
/// the enemy tactic card; the mod's placeholders only fill in when NO
/// land_combat involves the player (then the enemy is the other country
/// with the most divisions in the province, allies excluded). Divisions of
/// ALLIED countries fighting on the player's side assemble under the
/// player's direct command with their own country's national modifiers —
/// live battles run whole-side single-planner (no allied-AI split).
/// Heavy: map generation + save parse.
pub fn assemble_live(
    settings: &AppSettings,
    province: u32,
    tag: &str,
    attack_dirs: &[String],
    enemy_tactic: CombatTactic,
    player_is_attacker: bool,
) -> Result<BattleSpec, String> {
    let dirs: Vec<HexDirection> = attack_dirs
        .iter()
        .filter_map(|t| HexDirection::from_token(t))
        .collect();
    if dirs.is_empty() {
        // All-garbage dirs would silently generate a map with no deploy
        // zones — fail loudly instead.
        return Err(format!(
            "no valid attack directions in dirs={} (use E,NE,SE,W,SW,NW)",
            attack_dirs.join(",")
        ));
    }
    let save_path = settings
        .saves_dir()
        .and_then(|d| crate::settings::newest_save_in(&d))
        .ok_or("no .hoi4 save found in the saves dir")?;
    let save_game =
        SaveParser::parse_save(&save_path).map_err(|e| format!("save parse failed: {e}"))?;
    // The mod cannot print its tag through HOI4's log
    // interpolation — an empty tac_start tag means "the played country",
    // taken from the save's root `player="TAG"` key.
    let inferred_tag;
    let tag = if tag.is_empty() {
        inferred_tag = save_game
            .player
            .clone()
            .ok_or("tac_start carried no tag and the save has no player= country")?;
        inferred_tag.as_str()
    } else {
        tag
    };
    // The save's `combat.land_combat` blocks are the authoritative battle
    // record — contested province, participating divisions, sides, tactic
    // ids. The mod's placeholders (province=0, constant is_player_attacker,
    // fixed tactic card) only fill in when NO land_combat involves the
    // player. The early inference premise ("both sides sit in the
    // contested province") is FALSE — attackers stay in their source
    // provinces (tac_probe.hoi4: ITA∩ETH = ∅). A non-zero province comes
    // from the state-target pick (menu/live resolved tac_pick=1 → this
    // location) and LOCKS the battle by location; the largest-battle
    // heuristic only runs when the pick no longer matches a running combat
    // (ended between pick and assembly) or no pick was made (province=0).
    let player_battle = {
        let involves_player = |c: &&tactical_save::LandCombatData| {
            c.attacker.tags.iter().any(|t| t == tag) || c.defender.tags.iter().any(|t| t == tag)
        };
        let by_size = |c: &&tactical_save::LandCombatData| {
            c.attacker.unit_ids.len() + c.defender.unit_ids.len()
        };
        let locked = (province != 0)
            .then(|| {
                save_game
                    .land_combats
                    .iter()
                    .filter(involves_player)
                    .find(|c| c.location == province)
            })
            .flatten();
        locked.or_else(|| {
            save_game
                .land_combats
                .iter()
                .filter(involves_player)
                .max_by_key(by_size)
        })
    };

    // Battle province: the player's land_combat location wins; a non-zero
    // mod province (old logs / future mods) is used as-is.
    let province = match (player_battle, province) {
        (Some(b), _) => b.location,
        (None, 0) => {
            return Err(format!(
                "no battle involving {tag} found in the save — save again while a battle is in progress"
            ))
        }
        (None, p) => p,
    };
    // Side / enemy tactic: the save's truth wins over the mod's constants.
    let player_is_attacker = player_battle
        .map(|b| b.attacker.tags.iter().any(|t| t == tag))
        .unwrap_or(player_is_attacker);
    let enemy_tactic = player_battle
        .and_then(|b| {
            if player_is_attacker {
                Some(b.defender.tactic)
            } else {
                Some(b.attacker.tactic)
            }
            .flatten()
        })
        .and_then(|id| tactical_save::COMBAT_TACTIC_IDS.get(id.saturating_sub(1) as usize))
        .map(|token| CombatTactic::from_str(token))
        .unwrap_or(enemy_tactic);
    // REAL attack directions from the ATTACKER
    // side's source provinces (the attacking divisions' `location` in the
    // save — they stand in their source provinces, not the contested one).
    // Falls back to the mod's placeholder dirs when nothing resolves.
    let source_provinces: Vec<u32> = player_battle
        .map(|b| {
            let ids: std::collections::HashSet<u64> = b.attacker.unit_ids.iter().copied().collect();
            let mut out: Vec<u32> = Vec::new();
            for c in save_game.countries.values() {
                for d in &c.divisions {
                    if ids.contains(&d.id) {
                        if let Some(loc) = d.location {
                            if !out.contains(&loc) {
                                out.push(loc);
                            }
                        }
                    }
                }
            }
            out
        })
        .unwrap_or_default();
    // Vary the dice per live battle while staying deterministic
    // for a given (province, launch-second) pair.
    let live_seed = province as u64
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
    let tmap = with_generator(settings, |gen| {
        // The live battle's own seed also drives the elevation
        // noise — every live battle gets fresh relief (dice-seed parity).
        gen.generate_from_sources(province, &source_provinces, &dirs, live_seed)
            .ok()
    })
    .flatten()
    .ok_or_else(|| "map generation failed (HOI4 dir in Settings?)".to_string())?;

    let data_dir = crate::dirs::runtime_root().join("data");
    let equipment = EquipmentTable::load(data_dir.join("equipment_stats.json"))
        .map_err(|e| format!("equipment_stats.json: {e}"))?;
    let templates = UnitTemplateTable::load(data_dir.join("unit_templates.json"))
        .map_err(|e| format!("unit_templates.json: {e}"))?;
    // National org modifiers (missing/malformed file = neutral —
    // the loader degrades silently).
    let modifiers = ModifierTable::load(data_dir.join("modifiers.json"));
    // Doctrine combat factors — same degrade-silent contract.
    let doctrine_table =
        DoctrineTable::load(data_dir.join("doctrine_bonuses.json")).unwrap_or_default();

    // Forces: the land_combat's unit-id lists when present —
    // the ATTACKER's divisions are NOT in the contested province (they sit
    // in their source provinces), so province filtering would find nothing.
    // Fallback (no land_combat): province-location selection.
    // Every division carries its OWNING tag (roster membership
    // is per country) — a battle line can span several tags. ALLIED
    // divisions (co-belligerents on the player's side) assemble under the
    // player's direct command like her own: live battles run whole-side
    // single-planner, no allied-AI split.
    let mut player_divs: Vec<(String, &tactical_save::DivisionData)>;
    let mut enemy_divs: Vec<(String, &tactical_save::DivisionData)> = Vec::new();
    let mut enemy_tag = String::new();
    // Side tag lists for the damage write-back lines (one line per tag).
    let my_tags: Vec<String>;
    let their_tags: Vec<String>;
    if let Some(b) = player_battle {
        let (my_side, their_side) = if player_is_attacker {
            (&b.attacker, &b.defender)
        } else {
            (&b.defender, &b.attacker)
        };
        let my_ids: std::collections::HashSet<u64> = my_side.unit_ids.iter().copied().collect();
        let their_ids: std::collections::HashSet<u64> =
            their_side.unit_ids.iter().copied().collect();
        player_divs = side_divisions(&save_game, tag, &my_ids);
        if player_divs.is_empty() {
            return Err(format!(
                "the save's land_combat lists no divisions on {tag}'s side (battle resolved before the save was written?) — save again while the battle runs"
            ));
        }
        for (t, c) in &save_game.countries {
            if t == tag {
                continue;
            }
            for d in &c.divisions {
                if their_ids.contains(&d.id) {
                    enemy_divs.push((t.clone(), d));
                }
            }
        }
        if enemy_divs.is_empty() {
            return Err("the save's land_combat lists no enemy divisions".to_string());
        }
        enemy_tag = their_side.tags.first().cloned().unwrap_or_default();
        if enemy_tag.is_empty() {
            // Tags missing (odd save): derive the enemy tag from the owner
            // of the first matched enemy division.
            enemy_tag = enemy_divs[0].0.clone();
        }
        // The land_combat `tags` lists carry every participating country;
        // an empty list (odd save) degrades to the single known tag.
        my_tags = if my_side.tags.is_empty() {
            vec![tag.to_string()]
        } else {
            my_side.tags.clone()
        };
        their_tags = if their_side.tags.is_empty() {
            vec![enemy_tag.clone()]
        } else {
            their_side.tags.clone()
        };
    } else {
        let own = tactical_save::find_divisions_in_province(&save_game, tag, province);
        if own.is_empty() {
            return Err(format!(
                "no divisions of {tag} found in province {province}"
            ));
        }
        player_divs = own.into_iter().map(|d| (tag.to_string(), d)).collect();
        // Alliance evidence from the save's other running combats (1.19.2
        // text saves don't serialize the war objects): co-belligerent
        // divisions standing in the province join the player's command and
        // must NEVER be counted as enemy contenders.
        let (allies, enemies) = war_partners(&save_game, tag);
        let mut allied_divs: Vec<(String, &tactical_save::DivisionData)> = Vec::new();
        for (t, c) in &save_game.countries {
            if !allies.contains(t.as_str()) {
                continue;
            }
            for d in &c.divisions {
                if d.location == Some(province) {
                    allied_divs.push((t.clone(), d));
                }
            }
        }
        allied_divs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.id.cmp(&b.1.id)));
        player_divs.extend(allied_divs);
        // Enemy: the OTHER country with the most divisions in the province
        // (was: the first in HashMap iteration order, which is random
        // per run). Known enemies (fighting the player in another running
        // combat) outrank the biggest-garrison heuristic; remaining ties
        // break on the lexicographically smaller tag so the pick is
        // deterministic for a given save.
        let mut contenders: Vec<(&String, usize)> = save_game
            .countries
            .iter()
            .filter(|(t, _)| *t != tag && !allies.contains(t.as_str()))
            .map(|(t, c)| {
                (
                    t,
                    c.divisions
                        .iter()
                        .filter(|d| d.location == Some(province))
                        .count(),
                )
            })
            .filter(|(_, n)| *n > 0)
            .collect();
        contenders.sort_by(|a, b| {
            enemies
                .contains(b.0.as_str())
                .cmp(&enemies.contains(a.0.as_str()))
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.0.cmp(b.0))
        });
        if let Some((best_tag, _)) = contenders.first() {
            enemy_tag = (*best_tag).clone();
            if let Some(country) = save_game.countries.get(*best_tag) {
                for d in country
                    .divisions
                    .iter()
                    .filter(|d| d.location == Some(province))
                {
                    enemy_divs.push((enemy_tag.clone(), d));
                }
            }
        }
        // Mirror of the player-side check: no enemy in the province means
        // the battle would assemble ONE-sided (auto-win vs nobody) — the
        // old code silently launched it.
        if enemy_divs.is_empty() {
            return Err(format!("no enemy divisions found in province {province}"));
        }
        my_tags = vec![tag.to_string()];
        their_tags = vec![enemy_tag.clone()];
    }
    let template = player_divs[0].1.template_name.clone();

    // §5.2 real division names: the save stores `division_name` token pairs,
    // not literals — resolve them through the names-groups read LIVE from
    // the HOI4 install (`common/units/names_divisions`), fresh across game
    // updates. Player renames carry no token pair (parser) and are left
    // untouched; unresolvable tokens keep the parser's synthesized name.
    let name_groups = settings
        .hoi4_dir()
        .map(|d| {
            tactical_save::NameGroups::load_from_dir(&d.join("common/units/names_divisions"))
        })
        .unwrap_or_default();
    let mut player_divs: Vec<(String, tactical_save::DivisionData)> = player_divs
        .into_iter()
        .map(|(t, d)| (t, d.clone()))
        .collect();
    let mut enemy_divs: Vec<(String, tactical_save::DivisionData)> = enemy_divs
        .into_iter()
        .map(|(t, d)| (t, d.clone()))
        .collect();
    resolve_division_names(player_divs.iter_mut().chain(enemy_divs.iter_mut()), &name_groups);

    let player_side = if player_is_attacker {
        Side::Attacker
    } else {
        Side::Defender
    };
    let mut units = Vec::new();
    let mut next_id = 0usize;
    // The player side's country org modifier, once per battle;
    // the enemy side resolves per division because a battle line can span
    // several tags (land_combat `tags` lists).
    let player_org_mod = save_game
        .countries
        .get(tag)
        .map(|c| CountryOrgModifier::of(c, &modifiers))
        .unwrap_or_default();
    // The player side's researched doctrine factors + national
    // combat modifiers, once per battle (enemy side resolves per division
    // below, same pattern).
    let player_doctrines = save_game
        .countries
        .get(tag)
        .map(|c| doctrine_table.researched(&c.technologies));
    let player_combat_mod = save_game
        .countries
        .get(tag)
        .map(|c| CountryCombatModifier::of(c, &modifiers))
        .unwrap_or_default();
    // Generated battalion names in the session language.
    let naming =
        crate::naming::localized_unit_naming(&tactical_locale::Locale::load(settings.language()));
    // The mid-battle roster seeds one entry per participating
    // division as its battalions come out (live land_combat path only).
    let mut roster_entries: Vec<tactical_sync::RosterEntry> = Vec::new();
    for (owner_tag, d) in player_divs {
        // The division commander's bonuses resolve PER
        // DIVISION — each division may sit in a different army under a
        // different general.
        let leader = leader_bonus(&save_game, &modifiers, d.id);
        // Own divisions ride the side-level national modifier set resolved
        // above; ALLIED divisions resolve their own country's set per
        // division, exactly like the enemy side below.
        let (org_mod, combat_mod, doctrines) = if owner_tag == tag {
            (player_org_mod, player_combat_mod, player_doctrines.clone())
        } else {
            owner_national_mods(&save_game, &modifiers, &doctrine_table, d.id)
        };
        let mut us = create_tactical_units_named(
            &d,
            player_side,
            &equipment,
            &templates,
            doctrines.as_ref(),
            org_mod,
            combat_mod,
            &leader,
            next_id,
            &naming,
        );
        next_id += us.len();
        // Country-tagged base plates: an allied contingent reads as its own
        // color on the board (empty tag would fall back to side colors).
        for u in &mut us {
            u.tag = owner_tag.clone();
        }
        roster_entries.push(roster_entry(
            &d,
            &owner_tag,
            player_side,
            &us,
            &templates,
            org_mod,
        ));
        units.extend(us);
    }
    for (owner_tag, d) in enemy_divs {
        let (org_mod, combat_mod, doctrines) =
            owner_national_mods(&save_game, &modifiers, &doctrine_table, d.id);
        let leader = leader_bonus(&save_game, &modifiers, d.id);
        let mut us = create_tactical_units_named(
            &d,
            player_side.opponent(),
            &equipment,
            &templates,
            doctrines.as_ref(),
            org_mod,
            combat_mod,
            &leader,
            next_id,
            &naming,
        );
        next_id += us.len();
        for u in &mut us {
            u.tag = owner_tag.clone();
        }
        roster_entries.push(roster_entry(
            &d,
            &owner_tag,
            player_side.opponent(),
            &us,
            &templates,
            org_mod,
        ));
        units.extend(us);
    }
    if units.is_empty() {
        return Err("save produced no battalions".into());
    }
    let zones = (tmap.zones.attacker.clone(), tmap.zones.defender.clone());
    check_zones(&zones, &units)?;
    // The PLAYER's battalions start OFF the
    // board like in script battles (`mark_player_undeployed`) — the old
    // dense pre-placement buried the deployment zone in a stacked blob.
    // The enemy side keeps its zone seeding (the AI re-plans at BeginBattle
    // anyway).
    let enemy_zone = match player_side {
        Side::Attacker => &zones.1,
        Side::Defender => &zones.0,
    };
    let mut i = 0;
    for u in units.iter_mut().filter(|u| u.side != player_side) {
        if !enemy_zone.is_empty() {
            u.position = enemy_zone[i % enemy_zone.len()];
            i += 1;
        }
    }
    mark_player_undeployed(&mut units, player_side);

    let location = tmap
        .vp_label
        .as_ref()
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| format!("Province #{province}"));
    let flags = battle_flags(&tmap.grid, &zones, &[], province as u64);
    // atk_tag/def_tag follow the SIDES, not the player: the attacker tag is
    // the player's only when she attacks.
    let (atk_tag, def_tag) = if player_is_attacker {
        (tag.to_string(), enemy_tag)
    } else {
        (enemy_tag, tag.to_string())
    };
    // The damage write-back keys on every participating tag (one
    // damage_units line per tag per province — `limit` filters by owning
    // country), not just the side primaries.
    let (atk_tags, def_tags) = if player_is_attacker {
        (my_tags, their_tags)
    } else {
        (their_tags, my_tags)
    };
    // HOI4 battle context for the damage_units sync
    // batches — contested province, side tags, and the per-province maxima
    // bases (participants for org, every attacker-side division for the
    // str dilution base). Only the land_combat path has real participants.
    let battle_ctx = player_battle.map(|b| {
        build_battle_context(
            &save_game, b, province, &units, &templates, &modifiers, &atk_tags, &def_tags,
        )
    });
    // §8.2: the mid-battle roster rides the same land_combat
    // gate as the battle context (the fallback path has no participant
    // list to diff against). Seeds: one entry per participating division
    // (above), the last-seen province of EVERY division in the save (a
    // mid-battle joiner is by definition not yet a participant), and the
    // contested province's neighbour → direction table for edge placement.
    let roster = if battle_ctx.is_some() {
        let last_seen = save_game
            .countries
            .values()
            .flat_map(|c| c.divisions.iter())
            .filter_map(|d| d.location.map(|p| (d.id, p)))
            .collect();
        let approach_dirs =
            with_generator(settings, |gen| gen.neighbour_dirs(province)).unwrap_or_default();
        tactical_sync::BattleRoster::new(roster_entries, last_seen, approach_dirs)
    } else {
        tactical_sync::BattleRoster::default()
    };
    Ok(BattleSpec {
        grid: tmap.grid.clone(),
        units,
        zones,
        player_side,
        enemy_tactic,
        atk_tag,
        def_tag,
        location,
        vp_label: tmap.vp_label.clone(),
        province: Some(province),
        template,
        flags,
        // Vary the dice per live battle while staying deterministic
        // for a given (province, launch-second) pair.
        seed: Some(live_seed),
        // Live battles carry no script divisions block — the player
        // commands her whole side, allied contingents included.
        division_tags: std::collections::HashMap::new(),
        allies: Vec::new(),
        battle_ctx,
        writeback_mode: settings.writeback_mode(),
        // The battle clock starts from the save's in-game time.
        start_datetime: save_game.date,
        roster,
    })
}

/// The divisions fighting on `tag`'s side of a battle, owner tag
/// attached: the player's OWN divisions first (save order), then allied
/// divisions sorted by owner tag + division id — a deterministic assembly
/// order that leaves the single-country case byte-identical to the old
/// own-only collection. `ids` = the side's `land_combat` unit-id list.
fn side_divisions<'a>(
    save_game: &'a tactical_save::SaveGame,
    tag: &str,
    ids: &std::collections::HashSet<u64>,
) -> Vec<(String, &'a tactical_save::DivisionData)> {
    let mut own = Vec::new();
    let mut allied = Vec::new();
    for (t, c) in &save_game.countries {
        for d in &c.divisions {
            if !ids.contains(&d.id) {
                continue;
            }
            if t == tag {
                own.push((t.clone(), d));
            } else {
                allied.push((t.clone(), d));
            }
        }
    }
    allied.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    own.extend(allied);
    own
}

/// Upgrade the parser's synthesized division names to the real in-game ones
/// (§5.2): a division carrying a `name_order` token and a template
/// names-group resolves through `groups`; everything else (player renames,
/// synthetic names, unknown groups) keeps its current name. The division
/// name is the OOB/HQ grouping key, so a literal collision (allied copies
/// of another country's template sharing a group + issue number) is
/// disambiguated with the division serial.
fn resolve_division_names<'a>(
    divs: impl Iterator<Item = &'a mut (String, tactical_save::DivisionData)>,
    groups: &tactical_save::NameGroups,
) {
    let mut used: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for (_, d) in divs {
        let (Some(order), Some(group)) = (d.name_order, d.names_group.as_deref()) else {
            continue;
        };
        let Some(resolved) = groups.resolve(group, order) else {
            continue;
        };
        d.name = match used.get(&resolved) {
            Some(&other) if other != d.id => format!("{resolved} #{}", d.id),
            _ => resolved,
        };
        used.insert(d.name.clone(), d.id);
    }
}

/// Alliance evidence from the save's running land combats (1.19.2 text
/// saves don't serialize the war objects): countries sharing a side with
/// `tag` in any combat are ALLIES, countries on the far side ENEMIES. A
/// country seen on both sides across different combats is no ally.
fn war_partners(
    save_game: &tactical_save::SaveGame,
    tag: &str,
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
) {
    let mut allies = std::collections::HashSet::new();
    let mut enemies = std::collections::HashSet::new();
    for c in &save_game.land_combats {
        for (mine, theirs) in [(&c.attacker, &c.defender), (&c.defender, &c.attacker)] {
            if mine.tags.iter().any(|t| t == tag) {
                allies.extend(mine.tags.iter().filter(|t| *t != tag).cloned());
                enemies.extend(theirs.tags.iter().cloned());
            }
        }
    }
    for t in &enemies {
        allies.remove(t);
    }
    (allies, enemies)
}

/// One roster entry for a just-assembled division — battalion
/// ids (HQ included) + the damage-ratio pool bases (org = assembled battalion
/// pool, HQ excluded; str = HOI4 division subunit sum).
fn roster_entry(
    d: &tactical_save::DivisionData,
    tag: &str,
    side: Side,
    us: &[BattalionUnit],
    templates: &tactical_save::UnitTemplateTable,
    org_mod: CountryOrgModifier,
) -> tactical_sync::RosterEntry {
    tactical_sync::RosterEntry {
        division_id: d.id,
        side,
        tag: tag.to_string(),
        name: d.name.clone(),
        battalion_ids: us.iter().map(|u| u.id).collect(),
        org_pool: us.iter().filter(|u| !u.is_hq()).map(|u| u.max_org).sum(),
        // Str base only — the doctrine org factor is irrelevant to
        // max_strength (same rule as build_battle_context).
        max_str: tactical_save::division_maxima(d, templates, org_mod, 0.0).1,
        province: d.location,
    }
}

/// Aggregate the maxima bases for the sync batches. `battle` =
/// the save's land_combat record; `province` = contested province; `units`
/// = the assembled battalions; `atk_tags`/`def_tags` = every participating
/// tag per side (the damage lines emit one command per tag).
///
/// The ORG bases are the assembled battalion
/// pools (Σ battalion `max_org`, HQ excluded — HQ damage is never
/// recorded), because the `record_damage` numerator is counted in the same
/// battalion-scale points. The old HOI4 division-level org sums (a
/// count-weighted MEAN per division) under-counted the base by the
/// battalions-per-division factor (~6-8×): two wiped battalions plus
/// suppression chipped every defender division's org to zero within three
/// hours of contact. STR bases stay save-side division sums — a HOI4
/// division's max_strength IS the sum of its subunits, so that currency
/// already matches the numerator. Attacker provinces group participating
/// battalions by their `hoi4_province` (source province), with
/// `all_max_str` covering EVERY attacker-tag division standing there (the
/// dilution base).
#[allow(clippy::too_many_arguments)]
fn build_battle_context(
    save_game: &tactical_save::SaveGame,
    battle: &tactical_save::LandCombatData,
    province: u32,
    units: &[BattalionUnit],
    templates: &tactical_save::UnitTemplateTable,
    modifiers: &tactical_save::ModifierTable,
    atk_tags: &[String],
    def_tags: &[String],
) -> tactical_sync::BattleContext {
    let atk_ids: std::collections::HashSet<u64> =
        battle.attacker.unit_ids.iter().copied().collect();
    let def_ids: std::collections::HashSet<u64> =
        battle.defender.unit_ids.iter().copied().collect();
    // Org pools from the assembled battalions themselves —
    // doctrine factor, country modifier, org-0 baseline promotion and the
    // 1.0 floor all ride along by construction. Every pool is PER TAG:
    // each country's damage line books against its own divisions' maxima.
    let mut def_max: std::collections::HashMap<String, (f32, f32)> =
        std::collections::HashMap::new();
    let mut part_org_by_prov: std::collections::HashMap<
        u32,
        std::collections::HashMap<String, f32>,
    > = std::collections::HashMap::new();
    for u in units.iter().filter(|u| !u.is_hq()) {
        match u.side {
            Side::Defender => def_max.entry(u.tag.clone()).or_insert((0.0, 0.0)).0 += u.max_org,
            Side::Attacker => {
                if let Some(p) = u.hoi4_province {
                    *part_org_by_prov
                        .entry(p)
                        .or_default()
                        .entry(u.tag.clone())
                        .or_insert(0.0) += u.max_org;
                }
            }
        }
    }
    // Strength bases from the save's divisions (Σ subunit strength).
    let mut part_str_by_prov: std::collections::HashMap<
        u32,
        std::collections::HashMap<String, f32>,
    > = std::collections::HashMap::new();
    for (t, c) in &save_game.countries {
        // Divisions of one country share its org modifier.
        let org_mod = CountryOrgModifier::of(c, modifiers);
        for d in &c.divisions {
            // Str bases only — the doctrine org factor (0.0) is irrelevant
            // to max_strength (the org bases come from the assembled
            // battalion pool instead).
            let max_str = tactical_save::division_maxima(d, templates, org_mod, 0.0).1;
            if def_ids.contains(&d.id) {
                def_max.entry(t.clone()).or_insert((0.0, 0.0)).1 += max_str;
            }
            if atk_ids.contains(&d.id) {
                if let Some(loc) = d.location {
                    *part_str_by_prov
                        .entry(loc)
                        .or_default()
                        .entry(t.clone())
                        .or_insert(0.0) += max_str;
                }
            }
        }
    }
    // Dilution base: every division of EVERY attacker-side tag standing in
    // a participant's source province (each tag's damage line hits exactly
    // its own divisions there).
    let mut all_str: std::collections::HashMap<u32, std::collections::HashMap<String, f32>> =
        std::collections::HashMap::new();
    for t in atk_tags {
        if let Some(c) = save_game.countries.get(t) {
            let org_mod = CountryOrgModifier::of(c, modifiers);
            for d in &c.divisions {
                if let Some(loc) = d.location {
                    if part_str_by_prov.contains_key(&loc) {
                        *all_str
                            .entry(loc)
                            .or_default()
                            .entry(t.clone())
                            .or_insert(0.0) +=
                            tactical_save::division_maxima(d, templates, org_mod, 0.0).1;
                    }
                }
            }
        }
    }
    // Province union: a participant division that assembled zero battalions
    // still holds a str base, and vice versa should not silently drop.
    let mut provinces: Vec<u32> = part_str_by_prov.keys().copied().collect();
    provinces.extend(
        part_org_by_prov
            .keys()
            .filter(|p| !part_str_by_prov.contains_key(*p))
            .copied(),
    );
    provinces.sort_unstable();
    let attacker_provinces: Vec<tactical_sync::AttackerProvinceCtx> = provinces
        .into_iter()
        .map(|p| {
            let part_str = part_str_by_prov.get(&p).cloned().unwrap_or_default();
            let mut all = all_str.get(&p).cloned().unwrap_or_default();
            // A tag without bystander coverage dilutes over its own
            // participants only (the old whole-map fallback).
            for (t, ps) in &part_str {
                all.entry(t.clone()).or_insert(*ps);
            }
            tactical_sync::AttackerProvinceCtx {
                province: p,
                participants_max_org: part_org_by_prov.get(&p).cloned().unwrap_or_default(),
                participants_max_str: part_str,
                all_max_str: all,
            }
        })
        .collect();
    tactical_sync::BattleContext {
        contested_province: province,
        attacker_tags: atk_tags.to_vec(),
        defender_tags: def_tags.to_vec(),
        defender_max: def_max,
        attacker_provinces,
    }
}

/// The OWNING country's national modifiers — org, combat,
/// and the researched doctrine table (the enemy side of a live battle can
/// mix tags per the land_combat participant lists, so a single side-level
/// set would be wrong). Unknown owner → neutral defaults.
fn owner_national_mods(
    save_game: &tactical_save::SaveGame,
    modifiers: &tactical_save::ModifierTable,
    doctrines: &DoctrineTable,
    division_id: u64,
) -> (
    CountryOrgModifier,
    CountryCombatModifier,
    Option<DoctrineTable>,
) {
    let owner = save_game
        .countries
        .values()
        .find(|c| c.divisions.iter().any(|d| d.id == division_id));
    (
        owner
            .map(|c| CountryOrgModifier::of(c, modifiers))
            .unwrap_or_default(),
        owner
            .map(|c| CountryCombatModifier::of(c, modifiers))
            .unwrap_or_default(),
        owner.map(|c| doctrines.researched(&c.technologies)),
    )
}

// ---------------------------------------------------------------------------
// Mid-battle roster maintenance at the hourly sync (§8.2)

/// The data tables needed to assemble a division mid-battle (reinforcement
/// path). Loaded once per live battle window; every sync reuses them.
pub struct DivisionTables {
    pub equipment: EquipmentTable,
    pub templates: UnitTemplateTable,
    pub modifiers: ModifierTable,
    pub doctrines: DoctrineTable,
}

impl DivisionTables {
    /// Load the four runtime tables from `<runtime root>/data` (same files
    /// and degrade-silent contracts as the live assembly).
    pub fn load_runtime() -> Result<Self, String> {
        let data_dir = crate::dirs::runtime_root().join("data");
        Ok(DivisionTables {
            equipment: EquipmentTable::load(data_dir.join("equipment_stats.json"))
                .map_err(|e| format!("equipment_stats.json: {e}"))?,
            templates: UnitTemplateTable::load(data_dir.join("unit_templates.json"))
                .map_err(|e| format!("unit_templates.json: {e}"))?,
            modifiers: ModifierTable::load(data_dir.join("modifiers.json")),
            doctrines: DoctrineTable::load(data_dir.join("doctrine_bonuses.json"))
                .unwrap_or_default(),
        })
    }
}

/// One division-level roster change for the player-facing report.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterChange {
    /// HOI4 division name (for the battle log).
    pub name: String,
    pub tag: String,
    pub side: Side,
    /// Battalions affected (HQ included).
    pub battalions: usize,
    /// Joiners only: the map-edge direction the division enters from
    /// (`None` = no bearing resolved, placed at the side's zone).
    pub dir: Option<HexDirection>,
}

/// What one roster sync did (for logging + the notice bar).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RosterSyncReport {
    pub joined: Vec<RosterChange>,
    pub left: Vec<RosterChange>,
}

impl RosterSyncReport {
    pub fn is_empty(&self) -> bool {
        self.joined.is_empty() && self.left.is_empty()
    }
}

/// §8.2: diff the session roster against the fresh save's
/// `land_combat` and apply the result to the board. Called by the battle
/// window after every successful hourly sync whose post-sync battle-alive
/// check said the HOI4 battle is still running (a GONE record is the
/// battle-ended path, not a roster matter).
///
/// * Divisions no longer listed (routed out under the injected damage, or
///   manually retreated HOI4-side) have every battalion marched off:
///   `UnitState::LeftBattle` + OFFBOARD — the §6.14 terminal state, so the
///   board, combat and AI scans need zero special-casing, and
///   `check_victory` stops counting them on the spot.
/// * Newly listed divisions (reinforcements) are assembled from the fresh
///   save and placed at the map edge in their approach direction (NO
///   mid-battle deployment phase: a battle-start deployment zone can be a
///   combat zone by now, and arriving at the edge is the immersive read).
/// * The damage-ratio pools then re-derive from the updated
///   roster so the next hour's writeback books against the real OOB.
///
/// No-ops outside live battles (no battle context) and when the save's
/// own `land_combat` for the contested province is gone.
#[allow(clippy::too_many_arguments)]
pub fn apply_roster_sync(
    session: &mut tactical_sync::BattleSession,
    grid: &HexGrid,
    zones: &(Vec<HexCoord>, Vec<HexCoord>),
    units: &mut Vec<BattalionUnit>,
    save: &tactical_save::SaveGame,
    tables: &DivisionTables,
    naming: &UnitNaming,
) -> RosterSyncReport {
    let mut report = RosterSyncReport::default();
    let contested = match &session.battle_ctx {
        Some(ctx) => ctx.contested_province,
        None => return report,
    };
    // NOTE: an empty roster is NOT a no-op gate — a live battle whose
    // whole OOB just left still diffs (every listed division re-joins);
    // the non-live battles are already excluded by the battle-ctx gate.
    let Some(combat) = save.land_combats.iter().find(|c| c.location == contested) else {
        return report; // battle ended HOI4-side — the savecheck path resolves it
    };
    let atk_ids: std::collections::HashSet<u64> =
        combat.attacker.unit_ids.iter().copied().collect();
    let def_ids: std::collections::HashSet<u64> =
        combat.defender.unit_ids.iter().copied().collect();
    let diff = session.roster.diff(&atk_ids, &def_ids);

    // 1. Departures: every battalion of a division the HOI4 combat no
    //    longer lists marches off (mirrors apply_oob_leaving's effects —
    //    org wiped, strength frozen, off the board for good).
    for e in &diff.left {
        for u in units.iter_mut().filter(|u| e.battalion_ids.contains(&u.id)) {
            u.org = 0.0;
            u.state = tactical_core::UnitState::LeftBattle;
            u.position = BattalionUnit::OFFBOARD;
            u.move_order = None;
            u.is_holding = false;
            u.is_emplaced = false;
            u.oob_turns = 0;
        }
        report.left.push(RosterChange {
            name: e.name.clone(),
            tag: e.tag.clone(),
            side: e.side,
            battalions: e.battalion_ids.len(),
            dir: None,
        });
        session.roster.remove(e.division_id);
    }

    // 2. Joiners: assemble from the fresh save, place at the approach edge.
    let mut next_id = units.iter().map(|u| u.id).max().map_or(0, |m| m + 1);
    for (division_id, side) in &diff.joined {
        let found = save.countries.iter().find_map(|(t, c)| {
            c.divisions
                .iter()
                .find(|d| d.id == *division_id)
                .map(|d| (t.clone(), d))
        });
        // The combat listed the division but no country owns it in this
        // save (destroyed between the two reads?) — skip it; the next sync
        // diffs again.
        let Some((owner_tag, d)) = found else {
            continue;
        };
        let dir = session
            .roster
            .join_direction(d.id, *side, d.location, contested);
        let (org_mod, combat_mod, doctrines) =
            owner_national_mods(save, &tables.modifiers, &tables.doctrines, d.id);
        let leader = leader_bonus(save, &tables.modifiers, d.id);
        let mut us = create_tactical_units_named(
            d,
            *side,
            &tables.equipment,
            &tables.templates,
            doctrines.as_ref(),
            org_mod,
            combat_mod,
            &leader,
            next_id,
            naming,
        );
        next_id += us.len();
        // Same country-tagged base plates as the battle-start assembly.
        for u in &mut us {
            u.tag = owner_tag.clone();
        }
        let entry = roster_entry(d, &owner_tag, *side, &us, &tables.templates, org_mod);
        let occupied: std::collections::HashSet<(i32, i32)> = units
            .iter()
            .filter(|u| u.position != BattalionUnit::OFFBOARD)
            .map(|u| (u.position.q, u.position.r))
            .collect();
        let hexes = entry_hexes(grid, zones, *side, dir, &occupied, us.len());
        let mut us = us;
        for (u, h) in us.iter_mut().zip(hexes.iter()) {
            u.position = *h;
        }
        units.extend(us);
        report.joined.push(RosterChange {
            name: entry.name.clone(),
            tag: entry.tag.clone(),
            side: *side,
            battalions: entry.battalion_ids.len(),
            dir,
        });
        session.roster.insert(entry);
    }

    // 3. Refresh the last-seen provinces from the fresh save (the approach
    //    bearing source for FUTURE joiners — a defender joining next hour
    //    is standing in its marching province right now).
    session.roster.refresh_last_seen(
        save.countries
            .values()
            .flat_map(|c| c.divisions.iter())
            .map(|d| (d.id, d.location)),
    );

    // 4. Re-derive the damage-ratio pools from the updated roster.
    recompute_pools(session, save, tables);

    report
}

/// Re-derive the battle context's damage-ratio pools from the
/// CURRENT roster (bases: org = assembled battalion pools, str =
/// HOI4 division subunit sums — every pool PER TAG, so each country's
/// damage line books against its own divisions). The attacker str dilution
/// base (`all_max_str` = every same-tag division standing in a participant's
/// source province) re-reads the fresh save — bystanders move too. The side
/// tag lists refresh with the roster: a reinforcement can bring a NEW tag
/// into the battle between syncs.
fn recompute_pools(
    session: &mut tactical_sync::BattleSession,
    save: &tactical_save::SaveGame,
    tables: &DivisionTables,
) {
    let mut def_max: std::collections::HashMap<String, (f32, f32)> = Default::default();
    let mut part_org: std::collections::HashMap<u32, std::collections::HashMap<String, f32>> =
        Default::default();
    let mut part_str: std::collections::HashMap<u32, std::collections::HashMap<String, f32>> =
        Default::default();
    // Side tag lists in first-seen roster order (deterministic per
    // session): the damage lines emit one command per tag.
    let mut atk_tags: Vec<String> = Vec::new();
    let mut def_tags: Vec<String> = Vec::new();
    for e in session.roster.entries() {
        let tags = match e.side {
            Side::Attacker => &mut atk_tags,
            Side::Defender => &mut def_tags,
        };
        if !tags.contains(&e.tag) {
            tags.push(e.tag.clone());
        }
        match e.side {
            Side::Defender => {
                let m = def_max.entry(e.tag.clone()).or_insert((0.0, 0.0));
                m.0 += e.org_pool;
                m.1 += e.max_str;
            }
            Side::Attacker => {
                if let Some(p) = e.province {
                    *part_org
                        .entry(p)
                        .or_default()
                        .entry(e.tag.clone())
                        .or_insert(0.0) += e.org_pool;
                    *part_str
                        .entry(p)
                        .or_default()
                        .entry(e.tag.clone())
                        .or_insert(0.0) += e.max_str;
                }
            }
        }
    }
    if session.battle_ctx.is_none() {
        return;
    }
    let mut all_str: std::collections::HashMap<u32, std::collections::HashMap<String, f32>> =
        Default::default();
    for t in &atk_tags {
        if let Some(c) = save.countries.get(t) {
            let org_mod = CountryOrgModifier::of(c, &tables.modifiers);
            for d in &c.divisions {
                if let Some(loc) = d.location {
                    if part_str.contains_key(&loc) || part_org.contains_key(&loc) {
                        *all_str
                            .entry(loc)
                            .or_default()
                            .entry(t.clone())
                            .or_insert(0.0) +=
                            tactical_save::division_maxima(d, &tables.templates, org_mod, 0.0).1;
                    }
                }
            }
        }
    }
    let mut provinces: Vec<u32> = part_str.keys().copied().collect();
    provinces.extend(
        part_org
            .keys()
            .filter(|p| !part_str.contains_key(*p))
            .copied(),
    );
    provinces.sort_unstable();
    let attacker_provinces: Vec<tactical_sync::AttackerProvinceCtx> = provinces
        .into_iter()
        .map(|p| {
            let ps = part_str.get(&p).cloned().unwrap_or_default();
            let mut all = all_str.get(&p).cloned().unwrap_or_default();
            // A tag without bystander coverage dilutes over its own
            // participants only (the old whole-map fallback).
            for (t, s) in &ps {
                all.entry(t.clone()).or_insert(*s);
            }
            tactical_sync::AttackerProvinceCtx {
                province: p,
                participants_max_org: part_org.get(&p).cloned().unwrap_or_default(),
                participants_max_str: ps,
                all_max_str: all,
            }
        })
        .collect();
    if let Some(ctx) = &mut session.battle_ctx {
        ctx.defender_max = def_max;
        ctx.attacker_provinces = attacker_provinces;
        // Degenerate states (a side momentarily unlisted) keep the
        // assembly-time tags rather than dropping the damage lines.
        if !atk_tags.is_empty() {
            ctx.attacker_tags = atk_tags;
        }
        if !def_tags.is_empty() {
            ctx.defender_tags = def_tags;
        }
    }
}

/// Map-edge entry hexes for a reinforcing division. With an
/// approach direction the candidates are the same edge strip the §4.2
/// deployment zones use for that bearing (the division pours in over the
/// border it actually crossed); without one, the side's own deployment
/// zone; then any free deployable in-province hex nearest the grid center.
/// Occupied hexes are skipped while candidates last; if the board is truly
/// jammed the remainder wraps over the side's zone (stacking is tolerated
/// at placement time, same as the battle-start auto-deploy wrap).
fn entry_hexes(
    grid: &HexGrid,
    zones: &(Vec<HexCoord>, Vec<HexCoord>),
    side: Side,
    dir: Option<HexDirection>,
    occupied: &std::collections::HashSet<(i32, i32)>,
    n: usize,
) -> Vec<HexCoord> {
    let deployable = |h: HexCoord| {
        grid.cell(h)
            .map(|c| c.is_passable && !c.out_of_bounds && c.terrain.is_deployable())
            .unwrap_or(false)
    };
    let free = |h: &HexCoord| deployable(*h) && !occupied.contains(&(h.q, h.r));
    let mut out: Vec<HexCoord> = Vec::new();

    // 1. The approach-direction edge strip (§4.2 step 9 rule over the
    //    in-province extent), clustered around the strip centroid.
    if let Some(d) = dir {
        let depth = tactical_map::DEPLOY_STRIP_DEPTH as i32;
        let (mut eq0, mut eq1, mut er0, mut er1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
        for h in grid.iter_coords() {
            if deployable(h) {
                eq0 = eq0.min(h.q);
                eq1 = eq1.max(h.q);
                er0 = er0.min(h.r);
                er1 = er1.max(h.r);
            }
        }
        if eq0 <= eq1 {
            let mid_q2 = eq0 + eq1 + 1;
            let mut strip: Vec<HexCoord> = grid
                .iter_coords()
                .filter(|h| {
                    let in_strip = match d {
                        HexDirection::W => h.q < eq0 + depth,
                        HexDirection::E => h.q + depth > eq1,
                        HexDirection::NW => h.r < er0 + depth && h.q * 2 < mid_q2,
                        HexDirection::NE => h.r < er0 + depth && h.q * 2 >= mid_q2,
                        HexDirection::SW => h.r + depth > er1 && h.q * 2 < mid_q2,
                        HexDirection::SE => h.r + depth > er1 && h.q * 2 >= mid_q2,
                    };
                    in_strip && free(h)
                })
                .collect();
            let c = centroid(&strip);
            strip.sort_by_key(|h| (h.distance(c), h.q, h.r));
            out.extend(strip);
        }
    }

    // 2. The side's own deployment zone (free hexes first).
    let zone = match side {
        Side::Attacker => &zones.0,
        Side::Defender => &zones.1,
    };
    let mut zone_free: Vec<HexCoord> = zone.iter().copied().filter(|h| free(h)).collect();
    let cz = centroid(&zone_free);
    zone_free.sort_by_key(|h| (h.distance(cz), h.q, h.r));
    for h in zone_free {
        if !out.contains(&h) {
            out.push(h);
        }
    }

    // 3. Any free deployable in-province hex, nearest the grid center.
    let center = HexCoord::new(grid.width as i32 / 2, grid.height as i32 / 2);
    let mut rest: Vec<HexCoord> = grid.iter_coords().filter(|h| free(h)).collect();
    rest.sort_by_key(|h| (h.distance(center), h.q, h.r));
    for h in rest {
        if !out.contains(&h) {
            out.push(h);
        }
    }

    out.truncate(n);
    // Last resort: the board is jammed — wrap over the side's zone
    // (stacking is tolerated at placement time).
    if out.len() < n && !zone.is_empty() {
        let mut i = 0;
        while out.len() < n {
            out.push(zone[i % zone.len()]);
            i += 1;
        }
    }
    out
}

/// Centroid of a hex set as a rounded hex (for distance-sorted placement);
/// the zero hex for an empty set.
fn centroid(hexes: &[HexCoord]) -> HexCoord {
    if hexes.is_empty() {
        return HexCoord::ZERO;
    }
    let (sq, sr) = hexes.iter().fold((0i64, 0i64), |(aq, ar), h| {
        (aq + h.q as i64, ar + h.r as i64)
    });
    let n = hexes.len() as i64;
    HexCoord::new((sq / n) as i32, (sr / n) as i32)
}

// ---------------------------------------------------------------------------
// CLI serialization: the menu launches battles as CHILD
// PROCESSES — winit allows only one EventLoop per process (RecreationAttempt
// panic), so an in-process menu↔battle loop is impossible on Windows.

impl Scenario {
    /// Serialize to `--battle` key=value args (inverse of parse_cli).
    pub fn to_cli_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        // Script files carry the whole battle; only the RNG seed and an
        // explicit side toggle may ride along (the debug form's side switch
        // on a script battle).
        if let Some(f) = &self.file {
            let name = f.file_stem().and_then(|s| s.to_str()).unwrap_or("script");
            args.push(format!("file={name}"));
            if self.side_override {
                args.push(format!(
                    "side={}",
                    if self.player_side == Side::Defender {
                        2
                    } else {
                        1
                    }
                ));
            }
            if let Some(s) = self.seed {
                args.push(format!("seed={s}"));
            }
            // Per-nation command overrides (tag → player|ai),
            // serialized alphabetically for stable child-process args.
            let mut tags: Vec<&String> = self.div_control.keys().collect();
            tags.sort();
            for tag in tags {
                args.push(format!(
                    "div_control={tag}:{}",
                    if self.div_control[tag] {
                        "player"
                    } else {
                        "ai"
                    }
                ));
            }
            return args;
        }
        match &self.map {
            MapChoice::Arena => args.push("map=synthetic".into()),
            MapChoice::Province { id, dirs } => {
                args.push(format!("province={id}"));
                let d = dirs
                    .iter()
                    .map(|d| format!("{d:?}").to_uppercase())
                    .collect::<Vec<_>>()
                    .join(",");
                args.push(format!("dirs={d}"));
            }
        }
        let force = |f: &ForceChoice| match f {
            ForceChoice::Preset(p) => p.to_string(),
            ForceChoice::FromSave { tag } => format!("save:{tag}"),
        };
        args.push(format!("atk={}", force(&self.attacker)));
        args.push(format!("def={}", force(&self.defender)));
        let tactic_idx = TACTICS
            .iter()
            .position(|t| *t == self.enemy_tactic)
            .unwrap_or(1)
            + 1;
        args.push(format!("tactic={tactic_idx}"));
        args.push(format!(
            "side={}",
            if self.player_side == Side::Defender {
                2
            } else {
                1
            }
        ));
        args.push(format!("atk_tag={}", self.atk_tag));
        args.push(format!("def_tag={}", self.def_tag));
        if let Some(p) = &self.save_path {
            args.push(format!("save={}", p.display()));
        }
        if let Some(s) = self.seed {
            args.push(format!("seed={s}"));
        }
        args
    }
}

// ---------------------------------------------------------------------------
// CLI parser (agent automation): --battle province=306 dirs=E,NE atk=1 def=2
//   tactic=2 side=1 atk_tag=GER def_tag=FRA
// Force values: 1/2/3 presets, or save / save:TAG (tag defaults to the
// side's *_tag). map=synthetic selects the arena grid.

/// Parse `--battle` key=value args into a Scenario. Missing keys take the
/// debug-builder defaults (Sedan synthetic is chosen via map=synthetic).
pub fn parse_cli(args: &[String]) -> Result<Scenario, String> {
    let mut map_synthetic = false;
    let mut province: Option<u32> = None;
    let mut dirs: Vec<HexDirection> = Vec::new();
    let mut atk = ForceChoice::Preset(1);
    let mut def = ForceChoice::Preset(2);
    let mut tactic: Option<CombatTactic> = None;
    let mut side = Side::Attacker;
    // Whether `side=` was given at all — the script-side toggle.
    let mut side_given = false;
    let mut atk_tag = "GER".to_string();
    let mut def_tag = "FRA".to_string();
    let mut save_path = None;
    let mut seed = None;
    let mut file = None;
    // Per-nation command overrides for script battles.
    let mut div_control: std::collections::HashMap<String, bool> = std::collections::HashMap::new();

    for arg in args {
        // Bare positionals (--headless turns/seed) ride along and are
        // consumed elsewhere; unknown k=v keys are REJECTED — a typo like
        // `provice=306` used to silently build the default scenario and
        // exit 0.
        let Some((k, v)) = arg.split_once('=') else {
            continue;
        };
        match k {
            // Record the token, resolve at the end — an unknown map value
            // must fail loudly, not fall through to the catch-all.
            "map" => {
                if v != "synthetic" {
                    return Err(format!(
                        "unknown map '{v}' (only map=synthetic; province maps use province=)"
                    ));
                }
                map_synthetic = true;
            }
            "province" => {
                let id: u32 = v.parse().map_err(|_| format!("bad province id '{v}'"))?;
                province = Some(id);
            }
            "dirs" => {
                let mut parsed = Vec::new();
                for t in v.split(',') {
                    let t = t.trim();
                    parsed.push(
                        HexDirection::from_token(t)
                            .ok_or_else(|| format!("bad direction '{t}' in dirs="))?,
                    );
                }
                dirs = parsed;
            }
            "atk" => atk = parse_force(v)?,
            "def" => def = parse_force(v)?,
            "tactic" => {
                let i: usize = v.parse().map_err(|_| format!("bad tactic '{v}'"))?;
                tactic = Some(
                    TACTICS
                        .get(i.wrapping_sub(1))
                        .copied()
                        .filter(|_| (1..=TACTICS.len()).contains(&i))
                        .ok_or_else(|| format!("tactic index out of range '{v}'"))?,
                );
            }
            "side" => {
                side_given = true;
                side = match v {
                    "1" => Side::Attacker,
                    "2" => Side::Defender,
                    _ => return Err(format!("bad side '{v}' (use 1=attacker, 2=defender)")),
                };
            }
            "atk_tag" => atk_tag = v.to_uppercase(),
            "def_tag" => def_tag = v.to_uppercase(),
            "save" => save_path = Some(PathBuf::from(v)),
            "seed" => {
                seed = Some(v.parse().map_err(|_| format!("bad seed '{v}'"))?);
            }
            "file" => {
                if v.trim().is_empty() {
                    return Err("bad file= (empty script name)".into());
                }
                file = Some(script::resolve(v));
            }
            // div_control=TAG:player|ai (repeatable) — the menu
            // nation selector's per-nation command overrides for script
            // battles. Unknown tags are ignored at assembly (no match).
            "div_control" => {
                let Some((tag, who)) = v.split_once(':') else {
                    return Err(format!("bad div_control '{v}' (use TAG:player or TAG:ai)"));
                };
                let tag = tag.trim().to_uppercase();
                if tag.is_empty() {
                    return Err("bad div_control (empty tag)".into());
                }
                let player = match who.trim().to_ascii_lowercase().as_str() {
                    "player" => true,
                    "ai" => false,
                    _ => return Err(format!("bad div_control '{v}' (use TAG:player or TAG:ai)")),
                };
                div_control.insert(tag, player);
            }
            // Path overrides ride along to the parent (applied separately by
            // apply_path_overrides) — whitelisted so the strict arm below
            // doesn't reject them.
            "hoi4_dir" | "saves_dir" | "log_path" => {}
            other => return Err(format!("unknown key '{other}' (typo?)")),
        }
    }
    // file= is exclusive: the script carries map+forces+tactic+side+tags,
    // so a half-specified k=v mix would silently drop half the spec. The
    // side toggle and the RNG seed are the only legal riders.
    if file.is_some() {
        let clashed: Vec<&str> = args
            .iter()
            .filter_map(|a| a.split_once('=').map(|(k, _)| k))
            .filter(|k| {
                !matches!(
                    *k,
                    "file"
                        | "seed"
                        | "side"
                        | "div_control"
                        | "hoi4_dir"
                        | "saves_dir"
                        | "log_path"
                )
            })
            .collect();
        if !clashed.is_empty() {
            return Err(format!(
                "file= is exclusive (the script carries everything); remove {}",
                clashed.join(", ")
            ));
        }
    }
    if let ForceChoice::FromSave { tag } = &atk {
        if tag.is_empty() {
            atk = ForceChoice::FromSave {
                tag: atk_tag.clone(),
            };
        }
    }
    if let ForceChoice::FromSave { tag } = &def {
        if tag.is_empty() {
            def = ForceChoice::FromSave {
                tag: def_tag.clone(),
            };
        }
    }
    let map = if let Some(id) = province {
        if map_synthetic {
            // Contradictory spec — don't silently drop one half.
            return Err("map=synthetic conflicts with province= (pick one)".into());
        }
        if dirs.is_empty() {
            return Err("province map needs dirs= (e.g. dirs=E,NE)".into());
        }
        MapChoice::Province { id, dirs }
    } else {
        // map_synthetic or no map key at all → the arena default.
        MapChoice::Arena
    };
    Ok(Scenario {
        map,
        attacker: atk,
        defender: def,
        enemy_tactic: tactic.unwrap_or(CombatTactic::ElasticDefense),
        player_side: side,
        atk_tag,
        def_tag,
        save_path,
        file,
        side_override: side_given,
        script_flags: Vec::new(),
        seed,
        div_control,
    })
}

fn parse_force(v: &str) -> Result<ForceChoice, String> {
    match v {
        "1" | "2" | "3" => Ok(ForceChoice::Preset(v.parse().unwrap())),
        "save" => Ok(ForceChoice::FromSave { tag: String::new() }),
        s if s.starts_with("save:") => Ok(ForceChoice::FromSave {
            tag: s[5..].to_uppercase(),
        }),
        _ => Err(format!("bad force '{v}' (use 1|2|3|save|save:TAG)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_match_debug_builder() {
        let sc = parse_cli(&[]).unwrap();
        assert!(matches!(sc.map, MapChoice::Arena));
        assert_eq!(sc.attacker, ForceChoice::Preset(1));
        assert_eq!(sc.defender, ForceChoice::Preset(2));
        assert_eq!(sc.atk_tag, "GER");
        assert_eq!(sc.def_tag, "FRA");
        assert_eq!(sc.player_side, Side::Attacker);
    }

    #[test]
    fn cli_province_and_save_forces() {
        let args: Vec<String> = [
            "province=306",
            "dirs=E,NE",
            "atk=save:GER",
            "def=save",
            "def_tag=SOV",
            "side=2",
            "tactic=3",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let sc = parse_cli(&args).unwrap();
        match sc.map {
            MapChoice::Province { id, ref dirs } => {
                assert_eq!(id, 306);
                assert_eq!(dirs.len(), 2);
            }
            _ => panic!("expected province map"),
        }
        assert_eq!(sc.attacker, ForceChoice::FromSave { tag: "GER".into() });
        assert_eq!(sc.defender, ForceChoice::FromSave { tag: "SOV".into() });
        assert_eq!(sc.player_side, Side::Defender);
        assert!(matches!(sc.enemy_tactic, CombatTactic::OverwhelmingFire));
    }

    #[test]
    fn cli_rejects_bad_force_and_missing_dirs() {
        assert!(parse_cli(&["atk=9".to_string()]).is_err());
        assert!(parse_cli(&["province=306".to_string()]).is_err());
    }

    #[test]
    fn cli_rejects_unknown_keys() {
        // A typo'd key must fail loudly — it used to silently build the
        // default scenario.
        let err = parse_cli(&["provice=306".to_string()]).unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
    }

    #[test]
    fn cli_rejects_bad_direction_token() {
        let err = parse_cli(&["province=306".to_string(), "dirs=E,XX".to_string()]).unwrap_err();
        assert!(err.contains("bad direction"), "{err}");
    }

    #[test]
    fn cli_file_arg_is_exclusive_and_resolves() {
        let sc = parse_cli(&["file=1939_warsaw".to_string()]).unwrap();
        let stem = sc
            .file
            .as_ref()
            .unwrap()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(stem, "1939_warsaw");
        // The script carries everything — mixing k=v with it is a half-spec.
        let err =
            parse_cli(&["file=1939_warsaw".to_string(), "province=306".to_string()]).unwrap_err();
        assert!(err.contains("exclusive"), "{err}");
        let err = parse_cli(&["file=1939_warsaw".to_string(), "atk=1".to_string()]).unwrap_err();
        assert!(err.contains("exclusive"), "{err}");
        // seed= rides along (RNG override is orthogonal to the battle data).
        assert!(parse_cli(&["file=1939_warsaw".to_string(), "seed=3".to_string()]).is_ok());
        assert!(parse_cli(&["file=".to_string()]).is_err());
    }

    /// side= rides along with file= — the debug form's side
    /// toggle overrides the script's own player_side.
    #[test]
    fn cli_file_side_override_rides_along() {
        let sc = parse_cli(&["file=1939_warsaw".to_string(), "side=2".to_string()]).unwrap();
        assert_eq!(sc.player_side, Side::Defender);
        assert!(sc.side_override, "explicit side= must flag the override");
        let args = sc.to_cli_args();
        assert_eq!(args, vec!["file=1939_warsaw", "side=2"]);
        // Without side= the script's own side wins (override stays off).
        let sc = parse_cli(&["file=1939_warsaw".to_string()]).unwrap();
        assert!(!sc.side_override);
        assert_eq!(sc.to_cli_args(), vec!["file=1939_warsaw"]);
    }

    #[test]
    fn cli_file_serializes_back_to_file_arg() {
        let sc = parse_cli(&["file=1939_warsaw".to_string()]).unwrap();
        let args = sc.to_cli_args();
        assert_eq!(args, vec!["file=1939_warsaw"]);
    }

    /// div_control= rides along with file= — the menu nation
    /// selector's per-tag command overrides.
    #[test]
    fn cli_div_control_rides_along_with_file() {
        let sc = parse_cli(&[
            "file=1940_narvik".to_string(),
            "div_control=ENG:player".to_string(),
            "div_control=POL:ai".to_string(),
            "seed=7".to_string(),
        ])
        .unwrap();
        assert_eq!(sc.div_control.get("ENG"), Some(&true));
        assert_eq!(sc.div_control.get("POL"), Some(&false));
        assert_eq!(sc.div_control.len(), 2);
        // Round-trips through the CLI (stable order).
        let args = sc.to_cli_args();
        assert_eq!(
            args,
            vec![
                "file=1940_narvik",
                "seed=7",
                "div_control=ENG:player",
                "div_control=POL:ai"
            ]
        );
        let sc2 = parse_cli(&args).unwrap();
        assert_eq!(sc2.div_control, sc.div_control);
    }

    #[test]
    fn cli_div_control_rejects_garbage() {
        let err = parse_cli(&[
            "file=1940_narvik".to_string(),
            "div_control=ENG".to_string(),
        ])
        .unwrap_err();
        assert!(err.contains("div_control"), "{err}");
        let err = parse_cli(&[
            "file=1940_narvik".to_string(),
            "div_control=:player".to_string(),
        ])
        .unwrap_err();
        assert!(err.contains("div_control"), "{err}");
        let err = parse_cli(&[
            "file=1940_narvik".to_string(),
            "div_control=ENG:robot".to_string(),
        ])
        .unwrap_err();
        assert!(err.contains("div_control"), "{err}");
    }

    /// The player's battalions start OFF the
    /// board (sentinel position + flag), the enemy's stay pre-spread.
    #[test]
    fn mark_player_undeployed_flags_only_the_player_side() {
        let mut units = vec![
            BattalionUnit::new(
                1,
                "A1".to_string(),
                UnitType::Infantry,
                Side::Attacker,
                HexCoord::new(2, 3),
            ),
            BattalionUnit::new(
                2,
                "D1".to_string(),
                UnitType::Infantry,
                Side::Defender,
                HexCoord::new(8, 3),
            ),
            BattalionUnit::new(
                3,
                "A2".to_string(),
                UnitType::ArtilleryBrigade,
                Side::Attacker,
                HexCoord::new(1, 1),
            ),
        ];
        mark_player_undeployed(&mut units, Side::Attacker);
        assert!(units[0].undeployed);
        assert_eq!(units[0].position, BattalionUnit::OFFBOARD);
        assert!(units[2].undeployed);
        assert_eq!(units[2].position, BattalionUnit::OFFBOARD);
        assert!(
            !units[1].undeployed,
            "enemy side keeps its pre-spread position"
        );
        assert_eq!(units[1].position, HexCoord::new(8, 3));
    }

    /// Regression: the org bases of the sync battle context are
    /// the ASSEMBLED battalion pools (Σ battalion max_org, HQ excluded),
    /// not the HOI4 division-level org means — the two differ by the
    /// battalions-per-division factor (here: 60 mean vs 6×54 pool), which
    /// used to saturate the defender's org writeback within a few hours.
    #[test]
    fn battle_context_org_bases_use_the_assembled_battalion_pool() {
        use tactical_save::{CountryData, DivisionData, LandCombatData, LandCombatSideData};

        let templates = UnitTemplateTable::from_str(
            r#"{"line_battalions": {"infantry": {"max_organisation": 60.0, "max_strength": 25.0}}}"#,
        )
        .unwrap();
        let modifiers = ModifierTable::default();
        let inf = || tactical_save::BattalionInfo {
            token: "infantry".to_string(),
            unit_type: UnitType::Infantry,
            chassis: tactical_core::unit::Chassis::None,
            extra_attrs: tactical_core::unit::Attrs(0),
            count: 6,
        };
        let div = |id: u64, location: u32| DivisionData {
            id,
            name: format!("Div-{id}"),
            template_name: "T".to_string(),
            names_group: None,
            name_order: None,
            location: Some(location),
            organization: 1.0,
            strength: 1.0,
            experience: 0.0,
            entrenchment: 0.0,
            supply_status: None,
            equipment: std::collections::HashMap::new(),
            battalions: vec![inf()],
            support_companies: Vec::new(),
        };
        let country = |tag: &str, divisions| CountryData {
            tag: tag.to_string(),
            divisions,
            technologies: Vec::new(),
            active_ideas: Vec::new(),
            dynamic_modifiers: Vec::new(),
            leader_traits: Vec::new(),
            active_advisors: Vec::new(),
        };
        let mut countries = std::collections::HashMap::new();
        // ITA: divisions 1 (13251) and 3 (13250) attack; division 2 stands
        // in 13251 WITHOUT participating (str dilution base only).
        countries.insert(
            "ITA".to_string(),
            country("ITA", vec![div(1, 13251), div(2, 13251), div(3, 13250)]),
        );
        countries.insert(
            "ETH".to_string(),
            country("ETH", vec![div(10, 13237), div(11, 13237)]),
        );
        let save_game = tactical_save::SaveGame {
            countries,
            ..Default::default()
        };
        let battle = LandCombatData {
            location: 13237,
            attacker: LandCombatSideData {
                unit_ids: vec![1, 3],
                ..Default::default()
            },
            defender: LandCombatSideData {
                unit_ids: vec![10, 11],
                ..Default::default()
            },
        };
        // Assembled-battalion stand-ins: 6 per division at battalion-scale
        // max_org 54, plus one HQ per side whose org must NOT enter the
        // pool (HQ damage is never recorded). Tags are stamped like the
        // live assembly does — the pools key on the owning country.
        let mut units = Vec::new();
        let mut next_id = 0usize;
        for (side, prov, n, tag) in [
            (Side::Defender, 13237, 12, "ETH"),
            (Side::Attacker, 13251, 6, "ITA"),
            (Side::Attacker, 13250, 6, "ITA"),
        ] {
            for _ in 0..n {
                let mut u = BattalionUnit::new(
                    next_id,
                    "U".to_string(),
                    UnitType::Infantry,
                    side,
                    HexCoord::ZERO,
                );
                u.max_org = 54.0;
                u.hoi4_province = Some(prov);
                u.tag = tag.to_string();
                units.push(u);
                next_id += 1;
            }
        }
        for (side, prov, tag) in [
            (Side::Defender, 13237, "ETH"),
            (Side::Attacker, 13251, "ITA"),
        ] {
            let mut hq = BattalionUnit::new(
                next_id,
                "HQ".to_string(),
                UnitType::Infantry,
                side,
                HexCoord::ZERO,
            );
            hq.max_org = 20.0;
            hq.hoi4_province = Some(prov);
            hq.tag = tag.to_string();
            hq.attrs = tactical_core::unit::Attrs::HQ;
            units.push(hq);
            next_id += 1;
        }

        let ctx = build_battle_context(
            &save_game,
            &battle,
            13237,
            &units,
            &templates,
            &modifiers,
            &["ITA".to_string()],
            &["ETH".to_string()],
        );
        // Org pools: battalion sums (12/6 × 54), NOT division means (2×60).
        let (def_org, def_str) = ctx.defender_max["ETH"];
        assert!((def_org - 648.0).abs() < 1e-3, "defender org={def_org}");
        // Str bases: division strength sums (6×25 per division).
        assert!((def_str - 300.0).abs() < 1e-3, "defender str={def_str}");
        assert_eq!(ctx.attacker_provinces.len(), 2);
        let (p50, p51) = (&ctx.attacker_provinces[0], &ctx.attacker_provinces[1]);
        assert_eq!(p50.province, 13250);
        assert!((p50.participants_max_org["ITA"] - 324.0).abs() < 1e-3);
        assert!((p50.participants_max_str["ITA"] - 150.0).abs() < 1e-3);
        assert!((p50.all_max_str["ITA"] - 150.0).abs() < 1e-3);
        assert_eq!(p51.province, 13251);
        assert!((p51.participants_max_org["ITA"] - 324.0).abs() < 1e-3);
        assert!((p51.participants_max_str["ITA"] - 150.0).abs() < 1e-3);
        // Dilution: the non-participating ITA division 2 joins the base.
        assert!((p51.all_max_str["ITA"] - 300.0).abs() < 1e-3);
    }

    // ── Live assembly: allied divisions join the player's side ──────────

    fn coalition_save() -> tactical_save::SaveGame {
        use tactical_save::{CountryData, DivisionData, LandCombatData, LandCombatSideData};
        let div = |id: u64, location: u32| DivisionData {
            id,
            name: format!("Div-{id}"),
            template_name: "T".to_string(),
            names_group: None,
            name_order: None,
            location: Some(location),
            organization: 1.0,
            strength: 1.0,
            experience: 0.0,
            entrenchment: 0.0,
            supply_status: None,
            equipment: std::collections::HashMap::new(),
            battalions: Vec::new(),
            support_companies: Vec::new(),
        };
        let country = |tag: &str, divisions| CountryData {
            tag: tag.to_string(),
            divisions,
            technologies: Vec::new(),
            active_ideas: Vec::new(),
            dynamic_modifiers: Vec::new(),
            leader_traits: Vec::new(),
            active_advisors: Vec::new(),
        };
        let mut countries = std::collections::HashMap::new();
        // Player ENG defends 13237 with divisions 1+2; ally FRA co-defends
        // with 20; a second combat elsewhere pits ENG+FRA against GER.
        countries.insert(
            "ENG".to_string(),
            country("ENG", vec![div(1, 13237), div(2, 13237)]),
        );
        countries.insert("FRA".to_string(), country("FRA", vec![div(20, 13237)]));
        countries.insert("GER".to_string(), country("GER", vec![div(30, 13237), div(31, 999)]));
        countries.insert("ITA".to_string(), country("ITA", vec![div(40, 13237)]));
        tactical_save::SaveGame {
            countries,
            land_combats: vec![
                LandCombatData {
                    location: 13237,
                    attacker: LandCombatSideData {
                        unit_ids: vec![30],
                        tags: vec!["GER".to_string()],
                        ..Default::default()
                    },
                    defender: LandCombatSideData {
                        unit_ids: vec![1, 2, 20],
                        tags: vec!["ENG".to_string(), "FRA".to_string()],
                        ..Default::default()
                    },
                },
                LandCombatData {
                    location: 4995,
                    attacker: LandCombatSideData {
                        unit_ids: vec![31],
                        tags: vec!["GER".to_string()],
                        ..Default::default()
                    },
                    defender: LandCombatSideData {
                        unit_ids: vec![1],
                        tags: vec!["ENG".to_string(), "FRA".to_string()],
                        ..Default::default()
                    },
                },
            ],
            ..Default::default()
        }
    }

    /// The side's participating divisions come back own-first (save
    /// order) then allied (owner tag + id order) — the assembly order is
    /// deterministic and the single-country case is unchanged.
    #[test]
    fn side_divisions_collects_allied_divisions_after_own() {
        let save = coalition_save();
        let ids: std::collections::HashSet<u64> = [1, 2, 20].into_iter().collect();
        let picked = side_divisions(&save, "ENG", &ids);
        let ids: Vec<u64> = picked.iter().map(|(_, d)| d.id).collect();
        assert_eq!(ids, vec![1, 2, 20]);
        assert_eq!(picked[2].0, "FRA");
        // Own-only side: unchanged shape, no allied tail.
        let ids: std::collections::HashSet<u64> = [30].into_iter().collect();
        let picked = side_divisions(&save, "GER", &ids);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].0, "GER");
    }

    /// Alliance evidence from running combats: same-side tags are allies,
    /// far-side tags enemies; a country met on BOTH sides is no ally.
    #[test]
    fn war_partners_split_from_running_combats() {
        let save = coalition_save();
        let (allies, enemies) = war_partners(&save, "ENG");
        assert!(allies.contains("FRA"));
        assert!(enemies.contains("GER"));
        assert!(!allies.contains("GER"));
        assert!(!allies.contains("ITA")); // neutral bystander
        // GER's mirror view: ENG+FRA are both enemies.
        let (allies, enemies) = war_partners(&save, "GER");
        assert!(allies.is_empty());
        assert!(enemies.contains("ENG") && enemies.contains("FRA"));
    }

    /// §5.2 division names: token pairs resolve through the names-groups;
    /// renamed/tokenless divisions keep their names; a literal collision
    /// (shared group + issue number across owners) is disambiguated by id.
    #[test]
    fn resolve_division_names_upgrades_tokens_and_guards_collisions() {
        let mut groups = tactical_save::NameGroups::default();
        groups.merge_str(
            "GER_Inf_01 = {\n\
            \tfallback_name = \"%d. Infanterie-Division\"\n\
            \tordered = { 2 = { \"%d. Infanterie-Division 'Großdeutschland'\" } }\n\
            }",
        );
        let div = |id: u64, name_order: Option<u32>| tactical_save::DivisionData {
            id,
            name: format!("Infanterie-Division #{id}"),
            template_name: "Infanterie-Division".to_string(),
            names_group: name_order.map(|_| "GER_Inf_01".to_string()),
            name_order,
            location: None,
            organization: 1.0,
            strength: 1.0,
            experience: 0.0,
            entrenchment: 0.0,
            supply_status: None,
            equipment: std::collections::HashMap::new(),
            battalions: Vec::new(),
            support_companies: Vec::new(),
        };
        let mut divs: Vec<(String, tactical_save::DivisionData)> = vec![
            ("GER".to_string(), div(1, Some(2))),   // scripted entry
            ("GER".to_string(), div(2, Some(25))),  // fallback entry
            ("GER".to_string(), div(3, Some(2))),   // same group+order → collision
            ("GER".to_string(), div(4, None)),      // no token pair (renamed)
        ];
        divs[3].1.name = "Meine Division".to_string();
        resolve_division_names(divs.iter_mut(), &groups);
        assert_eq!(divs[0].1.name, "2. Infanterie-Division 'Großdeutschland'");
        assert_eq!(divs[1].1.name, "25. Infanterie-Division");
        assert_eq!(
            divs[2].1.name, "2. Infanterie-Division 'Großdeutschland' #3",
            "collision disambiguated by the division serial"
        );
        assert_eq!(divs[3].1.name, "Meine Division", "tokenless name untouched");
    }

    /// The attacker str dilution base covers EVERY attacker-side tag's
    /// divisions in a participant's source province — each tag's damage
    /// line hits exactly its own divisions there.
    #[test]
    fn battle_context_dilutes_over_every_attacker_tag() {
        use tactical_save::{LandCombatData, LandCombatSideData};
        let templates = UnitTemplateTable::from_str(
            r#"{"line_battalions": {"infantry": {"max_organisation": 60.0, "max_strength": 25.0}}}"#,
        )
        .unwrap();
        let modifiers = ModifierTable::default();
        let inf = || tactical_save::BattalionInfo {
            token: "infantry".to_string(),
            unit_type: UnitType::Infantry,
            chassis: tactical_core::unit::Chassis::None,
            extra_attrs: tactical_core::unit::Attrs(0),
            count: 6,
        };
        let div = |id: u64, location: u32| tactical_save::DivisionData {
            id,
            name: format!("Div-{id}"),
            template_name: "T".to_string(),
            names_group: None,
            name_order: None,
            location: Some(location),
            organization: 1.0,
            strength: 1.0,
            experience: 0.0,
            entrenchment: 0.0,
            supply_status: None,
            equipment: std::collections::HashMap::new(),
            battalions: vec![inf()],
            support_companies: Vec::new(),
        };
        let country = |tag: &str, divisions| tactical_save::CountryData {
            tag: tag.to_string(),
            divisions,
            technologies: Vec::new(),
            active_ideas: Vec::new(),
            dynamic_modifiers: Vec::new(),
            leader_traits: Vec::new(),
            active_advisors: Vec::new(),
        };
        let mut countries = std::collections::HashMap::new();
        // ITA division 1 attacks from 13251 where the non-participating
        // GER division 50 (an attacker-side ALLY) also stands.
        countries.insert("ITA".to_string(), country("ITA", vec![div(1, 13251)]));
        countries.insert("GER".to_string(), country("GER", vec![div(50, 13251)]));
        countries.insert("ETH".to_string(), country("ETH", vec![div(10, 13237)]));
        let save_game = tactical_save::SaveGame {
            countries,
            ..Default::default()
        };
        let battle = LandCombatData {
            location: 13237,
            attacker: LandCombatSideData {
                unit_ids: vec![1],
                tags: vec!["ITA".to_string(), "GER".to_string()],
                ..Default::default()
            },
            defender: LandCombatSideData {
                unit_ids: vec![10],
                tags: vec!["ETH".to_string()],
                ..Default::default()
            },
        };
        let mut u = BattalionUnit::new(
            0,
            "U".to_string(),
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.max_org = 54.0;
        u.hoi4_province = Some(13251);
        u.tag = "ITA".to_string();
        let units = vec![u];
        let ctx = build_battle_context(
            &save_game,
            &battle,
            13237,
            &units,
            &templates,
            &modifiers,
            &["ITA".to_string(), "GER".to_string()],
            &["ETH".to_string()],
        );
        assert_eq!(ctx.attacker_provinces.len(), 1);
        let p = &ctx.attacker_provinces[0];
        // Participants: ITA division 1 only (150 str). Each tag dilutes
        // over ITS OWN divisions in the province: ITA → itself (150), the
        // GER bystander → GER's own 150 (never pooled across tags).
        assert!((p.participants_max_str["ITA"] - 150.0).abs() < 1e-3);
        assert!(!p.participants_max_str.contains_key("GER"));
        assert!((p.all_max_str["ITA"] - 150.0).abs() < 1e-3);
        assert!((p.all_max_str["GER"] - 150.0).abs() < 1e-3);
    }

    // ── Mid-battle roster sync (§8.2) ────────────────────────────────────

    /// Forged roster-sync world: ITA division 1 (13251) attacks ETH
    /// division 10 (13237) at contested 13237. The fresh save then lists
    /// attacker divisions 1+2 (2 reinforces from 13250) and defender
    /// division 20 only (10 routed out; 20 marched into 13237 from 2072).
    struct RosterWorld {
        session: tactical_sync::BattleSession,
        grid: HexGrid,
        zones: (Vec<HexCoord>, Vec<HexCoord>),
        units: Vec<BattalionUnit>,
        save: tactical_save::SaveGame,
        tables: DivisionTables,
    }

    fn roster_world() -> RosterWorld {
        use tactical_save::{CountryData, DivisionData, LandCombatData, LandCombatSideData};

        let templates = UnitTemplateTable::from_str(
            r#"{"line_battalions": {"infantry": {"max_organisation": 60.0, "max_strength": 25.0}}}"#,
        )
        .unwrap();
        let inf = || tactical_save::BattalionInfo {
            token: "infantry".to_string(),
            unit_type: UnitType::Infantry,
            chassis: tactical_core::unit::Chassis::None,
            extra_attrs: tactical_core::unit::Attrs(0),
            count: 6,
        };
        let div = |id: u64, name: &str, location: u32| DivisionData {
            id,
            name: name.to_string(),
            template_name: "T".to_string(),
            names_group: None,
            name_order: None,
            location: Some(location),
            organization: 1.0,
            strength: 1.0,
            experience: 0.0,
            entrenchment: 0.0,
            supply_status: None,
            equipment: std::collections::HashMap::new(),
            battalions: vec![inf()],
            support_companies: Vec::new(),
        };
        let country = |tag: &str, divisions| CountryData {
            tag: tag.to_string(),
            divisions,
            technologies: Vec::new(),
            active_ideas: Vec::new(),
            dynamic_modifiers: Vec::new(),
            leader_traits: Vec::new(),
            active_advisors: Vec::new(),
        };
        let mut countries = std::collections::HashMap::new();
        countries.insert(
            "ITA".to_string(),
            country(
                "ITA",
                vec![div(1, "1a Divisione", 13251), div(2, "2a Divisione", 13250)],
            ),
        );
        countries.insert(
            "ETH".to_string(),
            country(
                "ETH",
                vec![
                    div(10, "10th Division", 13237),
                    div(20, "20th Division", 13237),
                ],
            ),
        );
        let save = tactical_save::SaveGame {
            countries,
            land_combats: vec![LandCombatData {
                location: 13237,
                attacker: LandCombatSideData {
                    unit_ids: vec![1, 2],
                    ..Default::default()
                },
                defender: LandCombatSideData {
                    unit_ids: vec![20],
                    ..Default::default()
                },
            }],
            ..Default::default()
        };

        // The battle-start OOB: divisions 1 (attacker) and 10 (defender).
        let naming = UnitNaming::default();
        let mut units = Vec::new();
        let mut next_id = 0usize;
        let mut entries = Vec::new();
        let neutral = CountryOrgModifier::default();
        let neutral_combat = CountryCombatModifier::default();
        for (d, tag, side) in [
            (&save.countries["ITA"].divisions[0], "ITA", Side::Attacker),
            (&save.countries["ETH"].divisions[0], "ETH", Side::Defender),
        ] {
            let us = create_tactical_units_named(
                d,
                side,
                &EquipmentTable::default(),
                &templates,
                None,
                neutral,
                neutral_combat,
                &tactical_save::LeaderBonus::default(),
                next_id,
                &naming,
            );
            next_id += us.len();
            entries.push(roster_entry(d, tag, side, &us, &templates, neutral));
            units.extend(us);
        }
        // On-board positions (the sync only touches OFFBOARD/departed ones).
        let grid = HexGrid::new(12, 12, tactical_core::Terrain::Plains);
        let zones = (
            vec![HexCoord::new(0, 5), HexCoord::new(1, 5)],
            vec![HexCoord::new(8, 6), HexCoord::new(9, 6)],
        );
        for (i, u) in units.iter_mut().enumerate() {
            u.position = HexCoord::new((i % 10) as i32 + 1, 5);
        }

        let mut session = tactical_sync::BattleSession::new();
        session.battle_ctx = Some(tactical_sync::BattleContext {
            contested_province: 13237,
            attacker_tags: vec!["ITA".to_string()],
            defender_tags: vec!["ETH".to_string()],
            defender_max: Default::default(),
            attacker_provinces: Vec::new(),
        });
        let last_seen = [(20u64, 2072u32)].into_iter().collect();
        let approach_dirs = [
            (13250u32, HexDirection::W),
            (13251, HexDirection::W),
            (2072, HexDirection::NE),
        ]
        .into_iter()
        .collect();
        session.roster = tactical_sync::BattleRoster::new(entries, last_seen, approach_dirs);

        RosterWorld {
            session,
            grid,
            zones,
            units,
            save,
            tables: DivisionTables {
                equipment: EquipmentTable::default(),
                templates,
                modifiers: ModifierTable::default(),
                doctrines: DoctrineTable::default(),
            },
        }
    }

    /// One roster sync — the routed defender's battalions march
    /// off (LeftBattle + OFFBOARD), both reinforcements enter at their
    /// approach edges, and the damage pools re-derive from the new OOB.
    #[test]
    fn roster_sync_applies_departures_and_reinforcements() {
        let mut w = roster_world();
        let report = apply_roster_sync(
            &mut w.session,
            &w.grid,
            &w.zones,
            &mut w.units,
            &w.save,
            &w.tables,
            &UnitNaming::default(),
        );
        // Report: one division left, two joined.
        assert_eq!(report.left.len(), 1);
        assert_eq!(report.left[0].name, "10th Division");
        assert_eq!(report.left[0].tag, "ETH");
        assert_eq!(report.joined.len(), 2);
        assert_eq!(report.joined[0].name, "2a Divisione");
        assert_eq!(report.joined[0].dir, Some(HexDirection::W));
        assert_eq!(report.joined[1].name, "20th Division");
        assert_eq!(report.joined[1].dir, Some(HexDirection::NE));

        // Departed: every battalion of division 10 is LeftBattle + OFFBOARD.
        for u in w.units.iter().filter(|u| u.hoi4_division_id == Some(10)) {
            assert_eq!(u.state, tactical_core::UnitState::LeftBattle);
            assert_eq!(u.position, BattalionUnit::OFFBOARD);
            assert_eq!(u.org, 0.0);
        }
        // Joined: 7 units each (6 battalions + HQ), on board at their edge.
        let div2: Vec<_> = w
            .units
            .iter()
            .filter(|u| u.hoi4_division_id == Some(2))
            .collect();
        assert_eq!(div2.len(), 7);
        for u in &div2 {
            assert!(u.position != BattalionUnit::OFFBOARD);
            assert!(u.position.q < 3, "W edge strip, got {:?}", u.position);
        }
        let div20: Vec<_> = w
            .units
            .iter()
            .filter(|u| u.hoi4_division_id == Some(20))
            .collect();
        assert_eq!(div20.len(), 7);
        for u in &div20 {
            assert!(u.position != BattalionUnit::OFFBOARD);
            assert!(u.position.r < 3, "NE edge strip, got {:?}", u.position);
            assert!(u.position.q >= 6, "NE edge strip, got {:?}", u.position);
        }
        // Roster now mirrors the combat: 1, 2, 20.
        assert_eq!(w.session.roster.entries().len(), 3);
        assert!(w.session.roster.get(10).is_none());
        assert_eq!(w.session.roster.get(2).unwrap().tag, "ITA");
        assert_eq!(w.session.roster.get(20).unwrap().tag, "ETH");

        // Pools re-derived: defender = division 20 only (6×60 org, 150 str).
        let ctx = w.session.battle_ctx.as_ref().unwrap();
        assert!((ctx.defender_max["ETH"].0 - 360.0).abs() < 1e-3);
        assert!((ctx.defender_max["ETH"].1 - 150.0).abs() < 1e-3);
        assert_eq!(ctx.attacker_provinces.len(), 2);
        assert_eq!(ctx.attacker_provinces[0].province, 13250);
        assert!((ctx.attacker_provinces[0].participants_max_org["ITA"] - 360.0).abs() < 1e-3);
        assert_eq!(ctx.attacker_provinces[1].province, 13251);
        // Division 1 is the only ITA division at 13251 → no dilution.
        assert!((ctx.attacker_provinces[1].all_max_str["ITA"] - 150.0).abs() < 1e-3);
    }

    /// Fail-open no-ops — no land_combat in the save, or no
    /// battle context (script/demo battles), leaves the board untouched.
    #[test]
    fn roster_sync_noops_without_land_combat_or_battle_context() {
        let mut w = roster_world();
        let before = w.units.len();
        // (a) The save lost the combat record — the battle-ended path
        // owns that case; the roster does nothing.
        w.save.land_combats.clear();
        let report = apply_roster_sync(
            &mut w.session,
            &w.grid,
            &w.zones,
            &mut w.units,
            &w.save,
            &w.tables,
            &UnitNaming::default(),
        );
        assert!(report.is_empty());
        assert_eq!(w.units.len(), before);

        // (b) No battle context (non-live battle) — even with a combat
        // record and a roster, nothing runs.
        let mut w = roster_world();
        w.session.battle_ctx = None;
        let report = apply_roster_sync(
            &mut w.session,
            &w.grid,
            &w.zones,
            &mut w.units,
            &w.save,
            &w.tables,
            &UnitNaming::default(),
        );
        assert!(report.is_empty());
        assert_eq!(w.units.len(), before);
    }

    /// An EMPTIED roster is not a disable flag — a live battle
    /// whose divisions all left still diffs the next save (every listed
    /// division joins fresh).
    #[test]
    fn roster_sync_rejoins_into_an_empty_roster() {
        let mut w = roster_world();
        w.session.roster = tactical_sync::BattleRoster::new(
            Vec::new(),
            [(20u64, 2072u32)].into_iter().collect(),
            [(13250u32, HexDirection::W), (2072, HexDirection::NE)]
                .into_iter()
                .collect(),
        );
        let report = apply_roster_sync(
            &mut w.session,
            &w.grid,
            &w.zones,
            &mut w.units,
            &w.save,
            &w.tables,
            &UnitNaming::default(),
        );
        // The combat lists 1, 2 (attack) and 20 (defense) — all three join.
        assert_eq!(report.joined.len(), 3);
        assert!(report.left.is_empty());
        assert_eq!(w.session.roster.entries().len(), 3);
    }

    /// Entry placement prefers the approach-direction edge strip
    /// and skips occupied hexes.
    #[test]
    fn entry_hexes_prefer_the_approach_edge_and_skip_occupied() {
        let grid = HexGrid::new(12, 12, tactical_core::Terrain::Plains);
        let zones = (vec![HexCoord::new(6, 6)], vec![HexCoord::new(6, 7)]);
        let occupied: std::collections::HashSet<(i32, i32)> =
            [(0, 5), (1, 5)].into_iter().collect();
        let hexes = entry_hexes(
            &grid,
            &zones,
            Side::Attacker,
            Some(HexDirection::W),
            &occupied,
            4,
        );
        assert_eq!(hexes.len(), 4);
        for h in &hexes {
            assert!(h.q < 3, "W edge strip, got {h:?}");
            assert!(!occupied.contains(&(h.q, h.r)));
        }
        // No direction → the side's own zone.
        let hexes = entry_hexes(&grid, &zones, Side::Defender, None, &occupied, 2);
        assert_eq!(hexes.len(), 2);
        assert!(hexes.contains(&HexCoord::new(6, 7)));
    }
}
