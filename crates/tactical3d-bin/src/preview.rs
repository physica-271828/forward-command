//! Preview mode: renders every terrain hex and every unit model into
//! previews/*.png for individual art review (Bevy 0.15 Screenshot API).

use std::collections::VecDeque;
use std::path::PathBuf;

use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::render::view::window::screenshot::{save_to_disk, Screenshot};
use tactical3d_render::board::{build_board_mesh, build_props_mesh, BoardMesh, BoardProps};
use tactical3d_render::models::{build_unit_mesh, SideColors};
use tactical3d_render::state::TacticalState;
use tactical_core::grid::HexGrid;
use tactical_core::hex::HexCoord;
use tactical_core::terrain::Terrain;
use tactical_core::unit::{GunState, ModelFamily, Side};

/// Every preview station: (shot name, family, carriage state). Towed guns
/// get all three states (emplaced / horse-limbered / truck-limbered).
const STATIONS: &[(&str, ModelFamily, GunState)] = &[
    ("unit_infantry", ModelFamily::Infantry, GunState::NA),
    ("unit_marine", ModelFamily::Marine, GunState::NA),
    ("unit_mountaineer", ModelFamily::Mountaineer, GunState::NA),
    ("unit_paratrooper", ModelFamily::Paratrooper, GunState::NA),
    ("unit_bicycle", ModelFamily::Bicycle, GunState::NA),
    ("unit_cavalry", ModelFamily::Cavalry, GunState::NA),
    ("unit_truck_motorized", ModelFamily::Truck, GunState::NA),
    ("unit_halftrack_mech", ModelFamily::Halftrack, GunState::NA),
    ("unit_tanklight", ModelFamily::TankLight, GunState::NA),
    ("unit_tankmedium", ModelFamily::TankMedium, GunState::NA),
    ("unit_tankheavy", ModelFamily::TankHeavy, GunState::NA),
    (
        "unit_tanksuperheavy",
        ModelFamily::TankSuperHeavy,
        GunState::NA,
    ),
    ("unit_tankmodern", ModelFamily::TankModern, GunState::NA),
    (
        "unit_tankamphibious",
        ModelFamily::TankAmphibious,
        GunState::NA,
    ),
    (
        "unit_rocket_truck",
        ModelFamily::RocketArtillery,
        GunState::NA,
    ),
    ("unit_armoredcar", ModelFamily::ArmoredCar, GunState::NA),
    ("unit_jeep", ModelFamily::Jeep, GunState::NA),
    (
        "unit_artillery_emplaced",
        ModelFamily::Artillery,
        GunState::Emplaced,
    ),
    (
        "unit_artillery_limbered_foot",
        ModelFamily::Artillery,
        GunState::LimberedFoot,
    ),
    (
        "unit_artillery_limbered_truck",
        ModelFamily::Artillery,
        GunState::LimberedTruck,
    ),
    (
        "unit_antitank_emplaced",
        ModelFamily::AntiTank,
        GunState::Emplaced,
    ),
    (
        "unit_antitank_limbered_foot",
        ModelFamily::AntiTank,
        GunState::LimberedFoot,
    ),
    (
        "unit_antitank_limbered_truck",
        ModelFamily::AntiTank,
        GunState::LimberedTruck,
    ),
    (
        "unit_antiair_emplaced",
        ModelFamily::AntiAir,
        GunState::Emplaced,
    ),
    (
        "unit_antiair_limbered_foot",
        ModelFamily::AntiAir,
        GunState::LimberedFoot,
    ),
    (
        "unit_antiair_limbered_truck",
        ModelFamily::AntiAir,
        GunState::LimberedTruck,
    ),
    (
        "unit_rockettowed_emplaced",
        ModelFamily::RocketTowed,
        GunState::Emplaced,
    ),
    (
        "unit_rockettowed_limbered_foot",
        ModelFamily::RocketTowed,
        GunState::LimberedFoot,
    ),
    (
        "unit_rockettowed_limbered_truck",
        ModelFamily::RocketTowed,
        GunState::LimberedTruck,
    ),
];

#[derive(Resource)]
struct PreviewDriver {
    /// (file name, camera eye, camera target)
    queue: VecDeque<(String, Vec3, Vec3)>,
    /// Frames until next action.
    wait: u32,
    /// Currently pending screenshot (waiting one frame after moving camera).
    pending: Option<String>,
}

pub fn run(what: &str) {
    let previews_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("previews");
    if let Err(e) = std::fs::create_dir_all(&previews_dir) {
        eprintln!("--preview: cannot create {}: {e}", previews_dir.display());
        std::process::exit(2);
    }
    std::env::set_var("PREVIEWS_DIR", &previews_dir);

    let (grid, queue) = build_preview_scene_plan(what);
    // An unknown `what` (or nothing matched) used to exit 0 with "all
    // previews captured" and an empty queue.
    if queue.is_empty() {
        eprintln!("--preview: nothing to capture for '{what}' (try terrains, units, or all)");
        std::process::exit(2);
    }

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Forward Command 3D — Model Preview".into(),
            resolution: (900.0_f32, 700.0_f32).into(),
            ..default()
        }),
        ..default()
    }));
    app.insert_resource(TacticalState {
        grid: Some(std::sync::Arc::new(grid)),
        ..default()
    });
    // Previews show the canonical pairing: Germany vs France.
    app.insert_resource(crate::theme::side_colors("GER", "FRA"));
    app.insert_resource(PreviewDriver {
        queue,
        wait: 150, // let shaders/pipelines warm up
        pending: None,
    });
    app.add_systems(Startup, setup_preview);
    app.add_systems(Update, drive_preview);
    app.run();
}

/// Lay out the preview grid and compute the shot queue.
fn build_preview_scene_plan(what: &str) -> (HexGrid, VecDeque<(String, Vec3, Vec3)>) {
    // 29 unit stations in rows of 7 below the terrain islands → need the
    // extra height (units start at r=10, 5 rows × 3).
    let mut grid = HexGrid::new(30, 26, Terrain::Plains);
    let mut queue = VecDeque::new();
    let size = tactical3d_render::board::HEX_SIZE;

    let do_terrains = matches!(what, "terrains" | "all");
    let do_units = matches!(what, "units" | "all");

    if do_terrains {
        // 3×3 terrain islands, two rows of SIX (12 terrains). Five-per-row
        // pushed a third row down to r=11, overlapping the unit stations at
        // r=10 — six-per-row ends cleanly at r=8.
        for (i, t) in Terrain::ALL.iter().enumerate() {
            let q0 = 1 + (i as i32 % 6) * 4;
            let r0 = 1 + (i as i32 / 6) * 5;
            for dq in 0..3 {
                for dr in 0..3 {
                    grid.set_terrain(HexCoord::new(q0 + dq, r0 + dr), *t);
                }
            }
            let (cx, cz) = HexCoord::new(q0 + 1, r0 + 1).to_world(size);
            let center = Vec3::new(cx, 0.4, cz);
            let eye = center + Vec3::new(2.0, 2.4, 3.0);
            queue.push_back((format!("terrain_{}", t.name().to_lowercase()), eye, center));
        }
    }

    if do_units {
        // Unit stations: rows of 7, each station has the attacker piece on
        // the left hex and the defender piece on the right hex (same row).
        // (terrain islands above end at r=8, so no overlap)
        for (i, (name, ..)) in STATIONS.iter().enumerate() {
            let q0 = 1 + (i as i32 % 7) * 4;
            let r0 = 10 + (i as i32 / 7) * 3;
            let (ax, az) = HexCoord::new(q0, r0).to_world(size);
            let (dx, dz) = HexCoord::new(q0 + 1, r0).to_world(size);
            let mid = Vec3::new((ax + dx) / 2.0, 0.3, (az + dz) / 2.0);
            let eye = mid + Vec3::new(0.0, 1.1, 2.3);
            queue.push_back((name.to_string(), eye, mid));
        }
    }

    (grid, queue)
}

fn setup_preview(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    state: Res<TacticalState>,
    colors: Res<SideColors>,
) {
    // Board + props (terrain islands + plains field). Props are ONE merged
    // static mesh, same as the battle scene.
    let (board, palette_img) = build_board_mesh(&state);
    let board_mat = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(images.add(palette_img)),
        perceptual_roughness: 0.95,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(board)),
        MeshMaterial3d(board_mat),
        Transform::default(),
        BoardMesh,
    ));
    if let Some((props_mesh, props_img)) = build_props_mesh(&state) {
        let props_mat = materials.add(StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(images.add(props_img)),
            perceptual_roughness: 0.95,
            ..default()
        });
        commands.spawn((
            Mesh3d(meshes.add(props_mesh)),
            MeshMaterial3d(props_mat),
            Transform::default(),
            BoardProps,
        ));
    }

    // Unit models at their stations.
    let size = tactical3d_render::board::HEX_SIZE;
    for (i, (_, fam, gun)) in STATIONS.iter().enumerate() {
        let q0 = 1 + (i as i32 % 7) * 4;
        let r0 = 10 + (i as i32 / 7) * 3;
        for (q, side) in [(q0, Side::Attacker), (q0 + 1, Side::Defender)] {
            let (mesh, image) = build_unit_mesh(*fam, side, colors.for_side(side), *gun);
            let mat = materials.add(StandardMaterial {
                base_color: Color::WHITE,
                base_color_texture: Some(images.add(image)),
                perceptual_roughness: 0.85,
                ..default()
            });
            let (x, z) = HexCoord::new(q, r0).to_world(size);
            let y = tactical3d_render::picking::unit_y_on_grid(&state, HexCoord::new(q, r0));
            commands.spawn((
                Mesh3d(meshes.add(mesh)),
                MeshMaterial3d(mat),
                Transform::from_xyz(x, y, z),
            ));
        }
    }

    // Lighting (same feel as the battle scene).
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, -0.6, -1.0, 0.0)),
    ));
    commands.insert_resource(AmbientLight {
        color: Color::srgb(0.65, 0.70, 0.80),
        brightness: 320.0,
    });

    // Camera (driven by PreviewDriver).
    commands.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(Color::srgb(0.53, 0.66, 0.82)),
            ..default()
        },
        Transform::from_xyz(0.0, 20.0, 20.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn drive_preview(
    mut commands: Commands,
    mut driver: ResMut<PreviewDriver>,
    mut q_cam: Query<&mut Transform, With<Camera3d>>,
    mut exit: EventWriter<AppExit>,
) {
    if driver.wait > 0 {
        driver.wait -= 1;
        return;
    }
    // Take the pending screenshot (camera has been in place for a few frames).
    if let Some(name) = driver.pending.take() {
        let dir = std::env::var("PREVIEWS_DIR").unwrap_or_else(|_| "previews".into());
        let path = PathBuf::from(dir).join(format!("{name}.png"));
        info!("capturing {}", path.display());
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path));
        driver.wait = 10; // let the capture settle before the next move
        return;
    }
    // Move to the next shot.
    let Some((name, eye, target)) = driver.queue.pop_front() else {
        info!("all previews captured");
        exit.send(AppExit::Success);
        return;
    };
    if let Ok(mut t) = q_cam.get_single_mut() {
        *t = Transform::from_translation(eye).looking_at(target, Vec3::Y);
    }
    driver.pending = Some(name);
    driver.wait = 10;
}
