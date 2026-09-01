//! Debug scenario builder: pick map, forces, tactic, and side from the
//! terminal — no HOI4 game session required. A HOI4 *install* is only
//! needed for the "province map" option; the synthetic arena map always works.
//! Presets + tactic list live in scenario.rs (single source).

use std::io::{BufRead, Write};

use bevy::prelude::*;
use tactical3d_render::game::GameController;
use tactical3d_render::state::TacticalState;
use tactical3d_render::TacticalGamePlugin;
use tactical_core::grid::HexGrid;
use tactical_core::hex::HexCoord;
use tactical_core::unit::Side;

use crate::demo::{arena_grid, arena_zones};
use crate::scenario::{deploy_force, TACTICS};

pub fn run() {
    println!("=== Forward Command 3D — Debug Scenario Builder ===");
    println!("(no running HOI4 needed; option 2 only reads map files)\n");

    // --- Map ---
    let map_choice = prompt("Map: [1] Arena (synthetic)  [2] HOI4 province by id", "1");
    let mut location = "Arena (synthetic)".to_string();
    let mut vp_label = None;
    let (grid, zones) = if map_choice == "2" {
        match province_map() {
            Some((g, z, id, label)) => {
                location = format!("Province #{id}");
                vp_label = label;
                (g, z)
            }
            None => {
                eprintln!("province map failed — falling back to the arena");
                let g = arena_grid();
                let z = arena_zones(&g);
                (g, z)
            }
        }
    } else {
        let g = arena_grid();
        let z = arena_zones(&g);
        (g, z)
    };

    // --- Forces ---
    println!("\nForce presets:");
    println!("  [1] Panzer division     (4 MedArmor + 2 LtArmor + 3 Mot + 1 Mech + motArt + motAT + Rec/Eng)");
    println!("  [2] Infantry division   (9 Inf + 1 Cav + 2 Art + 1 AT + 1 AA + Eng/Rec)");
    println!("  [3] Mixed battlegroup   (2 MedArmor + 2 Mot + 1 Inf + 1 Rkt + 1 Nbw + 1 AT + 1 AA + Rec)");
    let atk_preset = prompt("Attacker force [1/2/3]", "1");
    let def_preset = prompt("Defender force [1/2/3]", "2");

    // --- Enemy tactic ---
    println!("\nEnemy (AI) tactic:");
    for (i, t) in TACTICS.iter().enumerate() {
        println!("  [{}] {}", i + 1, t.name());
    }
    let tactic_idx = prompt("Tactic [1-9]", "2")
        .parse::<usize>()
        .ok()
        .filter(|i| (1..=TACTICS.len()).contains(i))
        .unwrap_or(2);
    let enemy_tactic = TACTICS[tactic_idx - 1];

    // --- Player side ---
    let side_choice = prompt("Your side: [1] Attacker  [2] Defender", "1");
    let player_side = if side_choice == "2" {
        Side::Defender
    } else {
        Side::Attacker
    };

    // --- Country tags (theme colors) ---
    let atk_tag = prompt("Attacker country tag", "GER").to_uppercase();
    let def_tag = prompt("Defender country tag", "FRA").to_uppercase();

    // --- Build & deploy ---
    let mut units = Vec::new();
    let mut next_id = 0usize;
    let (az, dz) = (zones.0.clone(), zones.1.clone());
    // Canonical-key terrain adjusters (missing table = zeros).
    let adj_templates = tactical_save::UnitTemplateTable::load(
        crate::dirs::runtime_root().join("data/unit_templates.json"),
    )
    .ok();
    deploy_force(
        &mut units,
        &mut next_id,
        Side::Attacker,
        &atk_preset,
        &az,
        adj_templates.as_ref(),
    );
    deploy_force(
        &mut units,
        &mut next_id,
        Side::Defender,
        &def_preset,
        &dz,
        adj_templates.as_ref(),
    );
    // The player's battalions start OFF the board and are placed from the
    // OOB window (or Auto Deploy).
    crate::scenario::mark_player_undeployed(&mut units, player_side);

    println!(
        "\nLaunching: {} vs {} | enemy tactic: {} | you play {:?}",
        preset_name(&atk_preset),
        preset_name(&def_preset),
        enemy_tactic.name(),
        player_side
    );

    // UI language from settings.json (DESIGN §15).
    let settings = crate::settings::AppSettings::load();
    let loc = tactical_locale::Locale::load(settings.language());
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: loc.tr("window.title.debug").into_owned(),
            resolution: (1440.0_f32, 900.0_f32).into(),
            // Fifo = true vsync (see demo.rs): even frame pacing fixes the
            // orbit judder from unsynced ~190 fps presentation.
            present_mode: bevy::window::PresentMode::Fifo,
            ..default()
        }),
        ..default()
    }));
    app.add_plugins(TacticalGamePlugin);
    // The game plugin registers an English LocaleRes default; insert-after-
    // init wins (locale.rs).
    app.insert_resource(tactical3d_render::locale::LocaleRes(loc));
    crate::window::start_maximized(&mut app);
    // Render quality + idle frame-saver from settings.json.
    crate::window::apply_render_quality(&mut app, &settings);
    // Render-resolution scale (offscreen + upscale when < 100%).
    crate::window::apply_render_scale(&mut app, settings.render_scale_pct());
    if settings.low_power {
        crate::window::apply_low_power(&mut app);
    }
    // Esc menu → Settings (see battle.rs).
    crate::window::init_battle_settings(&mut app, &settings);

    let mut game = GameController::new(player_side, enemy_tactic, 7);
    game.location = location;
    // Surface transition errors instead of swallowing them.
    if let Err(e) = game
        .session
        .start_launching()
        .and_then(|_| game.session.start_deployment())
    {
        warn!("debug: session failed to reach Deployment: {e:?}");
    }
    let state = TacticalState {
        grid: Some(std::sync::Arc::new(grid)),
        units,
        deployment_zones: Some(zones),
        player_side,
        board_colors_dirty: true,
        units_dirty: true,
        ..default()
    };
    // Battle-start checkpoint (restart target until the first sync).
    app.insert_resource(tactical3d_render::game::Checkpoints {
        battle_start: Some(tactical3d_render::game::BattleSnapshot::take(&game, &state)),
        ..default()
    });
    app.insert_resource(game);
    app.insert_resource(crate::theme::side_colors(&atk_tag, &def_tag));
    app.insert_resource(tactical3d_render::fx::VpLabel(vp_label));
    app.insert_resource(state);
    app.run();
}

fn prompt(label: &str, default: &str) -> String {
    print!("{label}  [{default}] > ");
    std::io::stdout().flush().ok();
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).ok();
    let line = line.trim();
    if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    }
}

fn preset_name(p: &str) -> &'static str {
    match p {
        "1" => "Panzer",
        "3" => "Mixed",
        _ => "Infantry",
    }
}

fn province_map() -> Option<(
    HexGrid,
    (Vec<HexCoord>, Vec<HexCoord>),
    u32,
    Option<(String, HexCoord)>,
)> {
    let id: u32 = prompt("Province id (e.g. 3560 for Sedan)", "3560")
        .parse()
        .ok()?;
    let hoi4_dir = tactical_map::detect_hoi4_dir()?;
    let map_dir = hoi4_dir.join("map");
    println!("loading province map data from {} …", map_dir.display());
    let pm = tactical_map::ProvinceMap::load_bmp(&map_dir.join("provinces.bmp")).ok()?;
    let defs = tactical_map::load_definition_csv(&map_dir.join("definition.csv")).ok()?;
    let adj =
        tactical_map::load_adjacencies_csv(&map_dir.join("adjacencies.csv")).unwrap_or_default();
    let mut gen = tactical_map::MapGenerator::new(pm, defs, adj);
    // River hex overlay (rivers.bmp) + VP urban (history/states).
    if let Ok(rivers) = tactical_map::IndexMap::load_indexed_bmp(&map_dir.join("rivers.bmp")) {
        gen.set_rivers(rivers);
    }
    let vps = tactical_map::load_victory_points(&tactical_map::states_dir_of(&hoi4_dir))
        .unwrap_or_default();
    gen.set_victory_points(vps);
    // Unitstacks index-0 positions anchor VP cities at the real city
    // location within the province.
    gen.set_unit_stacks(tactical_map::load_unit_stacks(
        &map_dir.join("unitstacks.txt"),
    ));
    // VP display names for the floating city label, in the UI language
    // (Chinese session → the install's Chinese yml).
    let zh =
        crate::settings::AppSettings::load().language() == tactical_locale::Language::SimpChinese;
    gen.set_vp_names(tactical_map::load_vp_names(&tactical_map::vp_names_path(
        &hoi4_dir, zh,
    )));

    let dirs_raw = prompt(
        "Attack directions (comma list, e.g. E,NE for Sedan 1940)",
        "E,NE",
    );
    let dirs: Vec<tactical_core::hex::HexDirection> = dirs_raw
        .split(',')
        .filter_map(|t| tactical_core::hex::HexDirection::from_token(t.trim()))
        .collect();
    let tmap = gen.generate(id, &dirs).ok()?;
    println!(
        "province {} generated: {}×{} hexes",
        id, tmap.grid.width, tmap.grid.height
    );
    Some((
        tmap.grid,
        (tmap.zones.attacker, tmap.zones.defender),
        id,
        tmap.vp_label,
    ))
}
