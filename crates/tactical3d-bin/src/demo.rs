//! Built-in demo scenario: panzer division vs infantry division on the flat
//! 64×64 arena (the hand-crafted Sedan grid was retired as too crude).
//! No HOI4 data required.

use bevy::prelude::*;
use tactical3d_render::game::GameController;
use tactical3d_render::state::TacticalState;
use tactical3d_render::TacticalGamePlugin;
use tactical_ai::CombatTactic;
use tactical_core::grid::HexGrid;
use tactical_core::hex::HexCoord;
use tactical_core::terrain::Terrain;
use tactical_core::unit::{BattalionUnit, Side, SupportAttachment, SupportKind, UnitType};

pub fn run() {
    let shot_mode = std::env::args().any(|a| a == "--shot");
    let (grid, mut units, zones) = demo_scenario();
    // Deployment flow: the player (GER attacker) starts OFF the board —
    // placed from the OOB window or Auto Deploy.
    crate::scenario::mark_player_undeployed(&mut units, Side::Attacker);

    // UI language from settings.json (DESIGN §15).
    let settings = crate::settings::AppSettings::load();
    let loc = tactical_locale::Locale::load(settings.language());
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: loc.tr("window.title.demo").into_owned(),
            resolution: (1440.0_f32, 900.0_f32).into(),
            // Fifo = true vsync: lock frame pacing to the display refresh.
            // AutoVsync fell back to mailbox on this Vulkan/NVIDIA setup
            // (~190 fps unsynced) and the uneven pick of presented frames
            // read as judder during camera orbits.
            present_mode: bevy::window::PresentMode::Fifo,
            // Center explicitly — OS cascade placement drifts down-right.
            position: bevy::window::WindowPosition::Centered(
                bevy::window::MonitorSelection::Primary,
            ),
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

    // Session starts in Deployment; the French AI defends with elastic defense.
    let mut game = GameController::new(Side::Attacker, CombatTactic::ElasticDefense, 42);
    game.location = "Arena".to_string();
    // Surface transition errors instead of swallowing them.
    if let Err(e) = game
        .session
        .start_launching()
        .and_then(|_| game.session.start_deployment())
    {
        warn!("demo: session failed to reach Deployment: {e:?}");
    }
    let state = TacticalState {
        grid: Some(std::sync::Arc::new(grid)),
        units,
        deployment_zones: Some(zones),
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
    // Germany (attacker) vs France (defender) — HOI4 country colors.
    app.insert_resource(crate::theme::side_colors("GER", "FRA"));
    app.insert_resource(state);
    if shot_mode {
        app.insert_resource(ShotDriver {
            timer: 0.0,
            stage: 0,
        });
        // Run after the hover picker so staged drag-deploy hover hexes stick.
        app.add_systems(
            Update,
            shot_driver.after(tactical3d_render::picking::update_hover),
        );
    }
    app.add_systems(Update, (crate::splash::auto_close, crate::app_icon::apply));
    app.run();
}

/// Screenshot smoke driver: shoot the untouched opening view (default_view
/// fit / Reset View target) at t=0.5s, a simulated drag-deploy ghost at
/// t=2.2s, the deployment phase (zone borders) at t=2.6s, begin battle, end
/// two turns, shoot the attacker at t=9s, the plains zoom at t=10.5s (was
/// the Sedan eastern-hills elevation-wall check — the arena is flat), the
/// artillery range ring at t=11.6s, exit at t=13s.
/// Saves previews/battle_{default,ghost,deploy,demo,hills,artring}.png.
#[derive(Resource)]
struct ShotDriver {
    timer: f32,
    stage: u32,
}

fn shot_driver(
    mut driver: ResMut<ShotDriver>,
    time: Res<Time>,
    mut pending: ResMut<tactical3d_render::game::PendingCommands>,
    mut commands: Commands,
    mut exit: EventWriter<bevy::app::AppExit>,
    q_units: Query<&tactical3d_render::units::UnitVisual>,
    q_mesh: Query<&Mesh3d>,
    mut q_cam: Query<&mut tactical3d_render::camera::RtsCamera>,
    mut state: ResMut<tactical3d_render::state::TacticalState>,
) {
    use bevy::render::view::window::screenshot::{save_to_disk, Screenshot};
    use tactical3d_render::game::PlayerCommand;

    let shot = |commands: &mut Commands, name: &str| {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("previews")
            .join(name);
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path));
    };

    driver.timer += time.delta_secs();
    match driver.stage {
        0 if driver.timer > 0.5 => {
            // The untouched opening view — exactly what Reset View
            // restores. Shot BEFORE any camera move below.
            shot(&mut commands, "battle_default.png");
            // NB: stage numbers must stay unique across the whole match —
            // reusing one (e.g. 10 here vs the hills shot's stage 10) loops
            // the driver and the camera bounces forever (shot-flicker bug).
            driver.stage = 90;
        }
        90 if driver.timer > 1.0 => {
            // Aim at the attacker deployment zone (west edge) for the shot.
            if let Some((a, _)) = state.deployment_zones.clone() {
                if let Some(&h) = a.first() {
                    let (x, z) = h.to_world(1.0);
                    for mut cam in q_cam.iter_mut() {
                        cam.target = Vec3::new(x + 3.0, 0.0, z);
                        cam.distance = 18.0;
                        cam.pitch = -1.0;
                    }
                }
            }
            driver.stage = 1;
        }
        1 if driver.timer > 1.6 => {
            // Ghost-deploy shot: simulate an OOB placement — the picked
            // unit's ghost previews on the staged hover hex (valid,
            // unoccupied zone hex). Re-arms every frame until the shot: the
            // hover picker would otherwise overwrite hover_hex (driver runs
            // after it).
            if let Some(u) = state
                .units
                .iter()
                .find(|u| u.side == tactical_core::unit::Side::Attacker)
            {
                let id = u.id;
                let staged = state.deployment_zones.as_ref().and_then(|(a, _)| {
                    a.iter()
                        .copied()
                        .filter(|h| state.unit_at(*h).is_none())
                        .min_by_key(|h| h.distance(BattalionUnit::OFFBOARD))
                        .or_else(|| a.first().copied())
                });
                if let Some(h) = staged {
                    state.deploy_placing = Some(id);
                    state.hover_hex = Some(h);
                }
            }
            if driver.timer > 2.2 {
                driver.stage = 2;
            }
        }
        2 if driver.timer > 2.2 => {
            shot(&mut commands, "battle_ghost.png");
            // Drop the previewed unit, then spread the rest of the player's
            // force across the zone so the deployment-overview shot shows
            // the full lineup (the deployment flow starts with an empty board).
            let (placing, hover) = (state.deploy_placing, state.hover_hex);
            if let (Some(id), Some(h)) = (placing, hover) {
                if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
                    u.position = h;
                    u.undeployed = false;
                }
            }
            state.deploy_placing = None;
            // Spread the rest of the player's force across the zone (the
            // deployment flow starts with an empty board; the overview shot needs
            // the full lineup). Occupied hexes are tracked in a local set
            // to keep the &mut iteration borrow-clean.
            if let Some((a, d)) = state.deployment_zones.clone() {
                let z: Vec<HexCoord> = if state.player_side == Side::Attacker {
                    a
                } else {
                    d
                };
                let player = state.player_side;
                let mut used: std::collections::HashSet<(i32, i32)> = state
                    .units
                    .iter()
                    .filter(|u| !u.undeployed)
                    .map(|u| (u.position.q, u.position.r))
                    .collect();
                let mut i = 0usize;
                for u in &mut state.units {
                    if u.side == player && u.undeployed && !z.is_empty() {
                        let start = i % z.len();
                        let mut placed = false;
                        for step in 0..z.len() {
                            let cand = z[(start + step) % z.len()];
                            if !used.contains(&(cand.q, cand.r)) {
                                u.position = cand;
                                u.undeployed = false;
                                used.insert((cand.q, cand.r));
                                i = start + step + 1;
                                placed = true;
                                break;
                            }
                        }
                        if !placed {
                            i += 1; // zone fully packed — leave it in the OOB
                        }
                    }
                }
            }
            state.units_dirty = true;
            driver.stage = 3;
        }
        3 if driver.timer > 2.6 => {
            // Deployment phase: zone border + unit bases before the battle.
            shot(&mut commands, "battle_deploy.png");
            driver.stage = 4;
        }
        4 if driver.timer > 3.1 => {
            pending.0.push(PlayerCommand::BeginBattle);
            driver.stage = 5;
        }
        5 if driver.timer > 3.6 => {
            // Route-arrow shot: issue a standing move order to 1.Pz — route
            // arrows must appear along the path — then end the turn so the
            // AI acts for the later shots.
            if let Some(u) = state
                .units
                .iter()
                .find(|u| {
                    u.side == tactical_core::unit::Side::Attacker
                        && u.unit_type == tactical_core::unit::UnitType::MediumArmor
                })
                .cloned()
            {
                if let Some(grid) = state.grid.clone() {
                    let params = tactical_core::CombatParams::default();
                    let dest = tactical_core::hex::HexCoord::new(10, 6);
                    if let Some((path, _)) = tactical_core::pathfinding::find_path(
                        &grid,
                        &u,
                        &state.units,
                        dest,
                        &params,
                    ) {
                        if let Some(mu) = state.units.iter_mut().find(|x| x.id == u.id) {
                            mu.move_order = Some(tactical_core::MoveOrder { path, hours: 0.0 });
                            state.orders_dirty = true;
                        }
                    }
                }
            }
            pending.0.push(PlayerCommand::EndTurn); // let the AI act for the shot
            driver.stage = 6;
        }
        6 if driver.timer > 4.4 => {
            // The ribbon renders for the SELECTED unit only — re-select
            // 1.Pz (End Turn cleared the selection), then give the rebuild a
            // few frames before shooting (stage 60).
            if let Some(u) = state.units.iter().find(|u| {
                u.side == tactical_core::unit::Side::Attacker
                    && u.unit_type == tactical_core::unit::UnitType::MediumArmor
            }) {
                state.selected_unit = Some(u.id);
                state.orders_dirty = true;
            }
            driver.stage = 60;
        }
        60 if driver.timer > 5.2 => {
            shot(&mut commands, "battle_arrows.png");
            driver.stage = 7;
        }
        7 if driver.timer > 8.0 => {
            let visible = state
                .units
                .iter()
                .filter(|u| u.is_combat_effective())
                .count();
            info!(
                "DIAG: units_in_state={} alive={} unit_visuals={} mesh_entities={} units_dirty={}",
                state.units.len(),
                visible,
                q_units.iter().count(),
                q_mesh.iter().count(),
                state.units_dirty
            );
            for h in [
                tactical_core::hex::HexCoord::new(3, 6),
                tactical_core::hex::HexCoord::new(18, 8),
            ] {
                info!("DIAG fog {:?} = {:?}", h, state.fog_state(h));
            }
            // Move the RTS camera close above the first attacker unit.
            if let Some(u) = state
                .units
                .iter()
                .find(|u| u.side == tactical_core::unit::Side::Attacker)
            {
                let (x, z) = u.position.to_world(1.0);
                for mut cam in q_cam.iter_mut() {
                    cam.target = Vec3::new(x, 0.0, z);
                    cam.distance = 26.0;
                    cam.pitch = -1.0;
                }
                info!("DIAG camera moved to unit {} at ({x:.1}, {z:.1})", u.name);
            }
            driver.stage = 8;
        }
        8 if driver.timer > 9.0 => {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("previews")
                .join("battle_demo.png");
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path));
            driver.stage = 9;
        }
        9 if driver.timer > 10.0 => {
            // Terrain close-up (was the Sedan eastern-hills elevation-wall
            // check — the arena is flat).
            let (x, z) = tactical_core::hex::HexCoord::new(14, 7).to_world(1.0);
            for mut cam in q_cam.iter_mut() {
                cam.target = Vec3::new(x, 0.0, z);
                cam.distance = 10.0;
                cam.pitch = -0.55;
            }
            driver.stage = 10;
        }
        10 if driver.timer > 10.5 => {
            shot(&mut commands, "battle_hills.png");
            driver.stage = 11;
        }
        11 if driver.timer > 11.0 => {
            // Select the attacker artillery (1.Art): the max-range circle
            // must appear around it (targetable = hex center inside circle).
            if let Some(u) = state
                .units
                .iter()
                .find(|u| u.side == tactical_core::unit::Side::Attacker && u.attack_range > 1)
            {
                let id = u.id;
                let (x, z) = u.position.to_world(1.0);
                state.selected_unit = Some(id);
                state.units_dirty = true;
                for mut cam in q_cam.iter_mut() {
                    cam.target = Vec3::new(x + 2.0, 0.0, z);
                    cam.distance = 15.0;
                    cam.pitch = -0.9;
                }
            }
            driver.stage = 12;
        }
        12 if driver.timer > 11.6 => {
            shot(&mut commands, "battle_artring.png");
            driver.stage = 13;
        }
        13 if driver.timer > 13.0 => {
            exit.send(bevy::app::AppExit::Success);
            driver.stage = 14;
        }
        _ => {}
    }
}

/// The arena terrain grid (shared by demo and debug scenario builder; the
/// hand-crafted Sedan grid was retired as too crude): a flat 64×64
/// all-plains proving ground. No rivers, no elevation, no cover — every
/// outcome is attributable to the combat model, not terrain luck.
pub fn arena_grid() -> HexGrid {
    HexGrid::new(64, 64, Terrain::Plains)
}

/// Deployment zones: mirrored edge strips — attacker west, defender east,
/// both depth 16 (start closer — the original depth-4 strips left 56 hexes
/// of no-man's land that ate ~70 turns of approach march from a 144-turn
/// budget; 32 hexes puts contact at ~T35-40 so combat dominates the
/// window). Symmetric by construction, so experiments measure side-policy
/// asymmetry rather than terrain asymmetry.
pub fn arena_zones(grid: &HexGrid) -> (Vec<HexCoord>, Vec<HexCoord>) {
    const DEPTH: i32 = 16;
    let w = grid.width as i32;
    let mut attacker_zone = Vec::new();
    let mut defender_zone = Vec::new();
    for hex in grid.iter_coords() {
        // Zones hold only deployable hexes (passable, not water).
        let deployable = grid
            .cell(hex)
            .map(|c| c.is_passable && c.terrain.is_deployable())
            .unwrap_or(false);
        if !deployable {
            continue;
        }
        if hex.q < DEPTH {
            attacker_zone.push(hex);
        }
        if hex.q >= w - DEPTH {
            defender_zone.push(hex);
        }
    }
    // The two armies never start in contact (a formality on the
    // arena — the strips are 32 hexes apart).
    let defender_zone = tactical_core::grid::filter_min_distance(
        defender_zone,
        &attacker_zone,
        tactical_core::grid::MIN_ZONE_DISTANCE,
    );
    (attacker_zone, defender_zone)
}

/// The demo battle: German panzers (attacker, west strip) vs French infantry
/// (defender, east strip) on the flat arena. Returns grid, units, and
/// deployment zones.
pub fn demo_scenario() -> (HexGrid, Vec<BattalionUnit>, (Vec<HexCoord>, Vec<HexCoord>)) {
    let grid = arena_grid();
    // Canonical-key terrain adjusters from the shipped table
    // (missing table degrades to zero adjusters, §5.3 fallback).
    let adj_templates = tactical_save::UnitTemplateTable::load(
        crate::dirs::runtime_root().join("data/unit_templates.json"),
    )
    .ok();

    let mut units = Vec::new();
    let mut id = 0usize;
    let mut mk = |name: &str,
                  ut: UnitType,
                  side: Side,
                  q: i32,
                  r: i32,
                  sa: f32,
                  ha: f32,
                  def: f32,
                  brk: f32,
                  armor: f32,
                  pier: f32,
                  hard: f32| {
        let mut u = BattalionUnit::new(id, name, ut, side, HexCoord::new(q, r));
        // Single-division demo OOB (no corps troops split).
        u.division = match side {
            Side::Attacker => "2. Panzer-Division".to_string(),
            Side::Defender => "55e Division d'Infanterie".to_string(),
        };
        u.soft_attack = sa;
        u.hard_attack = ha;
        u.defense = def;
        u.breakthrough = brk;
        u.armor = armor;
        u.piercing = pier;
        u.hardness = hard;
        u.terrain_adj = adj_templates
            .as_ref()
            .map(|t| t.terrain_adjusters_for(ut))
            .unwrap_or_default();
        id += 1;
        units.push(u);
    };

    // German panzer division (attacker, west of the Meuse). Stats are
    // calibrated to HOI4 1939 equipment values; org/str come from the
    // UnitType baseline table (tanks 10/2, infantry 60/25, guns 30/4).
    mk(
        "1.Pz",
        UnitType::MediumArmor,
        Side::Attacker,
        3,
        6,
        19.0,
        14.0,
        5.0,
        36.0,
        60.0,
        61.0,
        0.9,
    );
    mk(
        "2.Pz",
        UnitType::MediumArmor,
        Side::Attacker,
        3,
        8,
        19.0,
        14.0,
        5.0,
        36.0,
        60.0,
        61.0,
        0.9,
    );
    mk(
        "3.Pz",
        UnitType::LightArmor,
        Side::Attacker,
        2,
        7,
        13.0,
        4.0,
        4.0,
        26.0,
        10.0,
        10.0,
        0.8,
    );
    mk(
        "1.Mot",
        UnitType::Motorized,
        Side::Attacker,
        2,
        5,
        6.0,
        1.0,
        16.0,
        5.0,
        0.0,
        4.0,
        0.1,
    );
    mk(
        "2.Mot",
        UnitType::Motorized,
        Side::Attacker,
        2,
        9,
        6.0,
        1.0,
        16.0,
        5.0,
        0.0,
        4.0,
        0.1,
    );
    mk(
        "1.Art",
        UnitType::ArtilleryBrigade,
        Side::Attacker,
        1,
        7,
        25.0,
        2.0,
        10.0,
        6.0,
        0.0,
        5.0,
        0.0,
    );

    // French infantry division (defender, east strip).
    mk(
        "1.Inf",
        UnitType::Infantry,
        Side::Defender,
        61,
        6,
        6.0,
        1.0,
        22.0,
        3.0,
        0.0,
        4.0,
        0.0,
    );
    mk(
        "2.Inf",
        UnitType::Infantry,
        Side::Defender,
        60,
        8,
        6.0,
        1.0,
        22.0,
        3.0,
        0.0,
        4.0,
        0.0,
    );
    mk(
        "3.Inf",
        UnitType::Infantry,
        Side::Defender,
        61,
        9,
        6.0,
        1.0,
        22.0,
        3.0,
        0.0,
        4.0,
        0.0,
    );
    mk(
        "4.Inf",
        UnitType::Infantry,
        Side::Defender,
        62,
        10,
        6.0,
        1.0,
        22.0,
        3.0,
        0.0,
        4.0,
        0.0,
    );
    mk(
        "1.FArt",
        UnitType::ArtilleryBrigade,
        Side::Defender,
        60,
        11,
        25.0,
        2.0,
        10.0,
        6.0,
        0.0,
        5.0,
        0.0,
    );

    // German recon company rides with the lead panzer battalion
    // (sight 1→2), French support companies ride inside their battalions —
    // the AT guns back 1.Inf (+4 hard / +10 piercing baked in), the
    // engineers back 2.Inf. No independent company units on the map.
    for u in units.iter_mut() {
        if u.name == "1.Pz" {
            u.attach(SupportAttachment {
                kind: SupportKind::Recon,
                name: "Aufkl".to_string(),
            });
        } else if u.name == "1.Inf" {
            u.attach(SupportAttachment {
                kind: SupportKind::AntiTank,
                name: "1. AT".to_string(),
            });
        } else if u.name == "2.Inf" {
            u.attach(SupportAttachment {
                kind: SupportKind::Engineer,
                name: "1. Eng".to_string(),
            });
        }
    }

    // Deployment zones: attacker west strip, defender east strip.
    let zones = arena_zones(&grid);
    snap_into_zones(&mut units, &zones);
    (grid, units, zones)
}

/// The hand-placed coordinates above predate the dynamic zones and a few
/// may fall outside them after any zone-shape change. Move any out-of-zone
/// unit to the nearest free zone hex, preserving the rest of the formation.
fn snap_into_zones(units: &mut [BattalionUnit], zones: &(Vec<HexCoord>, Vec<HexCoord>)) {
    let mut taken: std::collections::HashSet<(i32, i32)> =
        units.iter().map(|u| (u.position.q, u.position.r)).collect();
    for unit in units.iter_mut() {
        let zone = match unit.side {
            Side::Attacker => &zones.0,
            Side::Defender => &zones.1,
        };
        let cur = unit.position;
        if zone.contains(&cur) {
            continue;
        }
        let best = zone
            .iter()
            .filter(|h| !taken.contains(&(h.q, h.r)))
            .min_by_key(|h| cur.distance(**h))
            .or_else(|| zone.iter().min_by_key(|h| cur.distance(**h)));
        if let Some(&h) = best {
            taken.remove(&(cur.q, cur.r));
            taken.insert((h.q, h.r));
            unit.position = h;
        }
    }
}
