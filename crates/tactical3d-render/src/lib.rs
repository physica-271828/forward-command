//! tactical3d-render — Bevy 0.15 3D renderer for Forward Command.
//! Pixel/blocky art: merged vertex-colored meshes, RTS camera, egui overlay.

pub mod board;
pub mod camera;
pub mod fonts;
pub mod fx;
pub mod game;
pub mod gate;
pub mod icons;
pub mod locale;
pub mod mesh_build;
pub mod models;
pub mod picking;
pub mod render_scale;
pub mod settings;
pub mod state;
pub mod trace;
pub mod ui;
pub mod units;

use bevy::prelude::*;
use state::TacticalState;

/// Core rendering plugin (board + camera + units + picking).
pub struct Tactical3dPlugin;

impl Plugin for Tactical3dPlugin {
    fn build(&self, app: &mut App) {
        trace::init_from_env();
        app.init_resource::<TacticalState>()
            .init_resource::<state::RenderQuality>()
            .init_resource::<state::DetailWindow>()
            .init_resource::<gate::RenderGate>()
            .init_resource::<models::SideColors>()
            .init_resource::<models::TagColors>()
            .init_resource::<units::UnitMeshCache>()
            .init_resource::<units::MoveAnims>()
            .init_resource::<board::RouteArrowAnim>()
            .init_resource::<camera::CameraResetReq>()
            .init_resource::<camera::CameraFocusReq>()
            .init_resource::<camera::CameraZoomReq>()
            .init_resource::<camera::CameraGlide>()
            .init_resource::<camera::MapRightClick>()
            .init_resource::<board::BoardAssets>()
            .add_systems(
                Startup,
                (
                    camera::spawn_camera,
                    board::setup_board,
                    // Render-scale offscreen target: builds only when
                    // RenderScaleState exists and pct < 100.
                    render_scale::setup_render_scale,
                )
                    .chain(),
            )
            // In-battle settings: hot-apply BattleSettings
            // edits, then let the render-scale path react to a pct flip
            // (chained so the offscreen (re)build lands the same frame);
            // sync keeps the target matched to the window. All no-op where
            // the resources are absent (menu / previews).
            .add_systems(
                Update,
                (
                    settings::apply_battle_settings,
                    render_scale::apply_render_scale_change,
                    render_scale::sync_render_scale,
                )
                    .chain(),
            )
            // Chained + ordered after `ui_pointer_guard`: the camera
            // and picking read `pointer_over_ui`, and downstream systems read
            // the hover/click state these produce — no frame-stale reads.
            .add_systems(
                Update,
                (
                    camera::rts_camera_controller,
                    // Gate-skipped: with the render gate closed the
                    // cursor hasn't moved, so the hover raycast would return
                    // the same hex anyway — and nothing would be presented.
                    picking::update_hover.run_if(crate::gate::gate_open),
                    board::rebuild_board_mesh,
                    board::sync_zone_border,
                    board::sync_ally_sector_overlay,
                    board::sync_flag_zones,
                    // Pure cosmetics: they only need to run on frames that
                    // actually render (the gate's ~10 fps idle heartbeat
                    // keeps the pulse/ants stepping).
                    board::pulse_zone_borders.run_if(crate::gate::gate_open),
                    board::sync_route_arrows,
                    board::sync_command_lines,
                    board::sync_div_order_markers.run_if(crate::gate::gate_open),
                    // The units pipeline MUST be ordered (slide animation):
                    // respawn first, then attach pending animations to the
                    // fresh entities (attaching to the doomed pre-respawn
                    // entity silently loses the slide = "瞬移").
                    (
                        units::sync_unit_visuals,
                        units::apply_move_anims,
                        units::sync_deploy_ghost,
                        units::animate_moves,
                    )
                        .chain(),
                )
                    .chain()
                    .after(ui::ui_pointer_guard),
            );
        // Perf diagnostics are an opt-in debug feature: set `FC_PERF=1`
        // in the environment before launch to
        // print FPS/frame-time to the console once a second. Off by default
        // so normal play keeps the log quiet. Battle children inherit the
        // parent process env, so one setting covers menu + battles.
        if std::env::var_os("FC_PERF").is_some() {
            app.add_plugins(bevy::diagnostic::FrameTimeDiagnosticsPlugin)
                .add_plugins(bevy::diagnostic::LogDiagnosticsPlugin::filtered(vec![
                    bevy::diagnostic::FrameTimeDiagnosticsPlugin::FPS,
                    bevy::diagnostic::FrameTimeDiagnosticsPlugin::FRAME_TIME,
                ]));
        }
        // One-shot adapter log from the render world (main world never sees
        // RenderAdapterInfo). Answers "is this WARP/CPU rendering?" at a
        // glance in the console.
        if let Some(render_app) = app.get_sub_app_mut(bevy::render::RenderApp) {
            render_app.add_systems(bevy::render::Render, log_adapter_once);
        }
    }
}

fn log_adapter_once(
    info: Option<Res<bevy::render::renderer::RenderAdapterInfo>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    if let Some(info) = info {
        *done = true;
        let a = &info.0;
        eprintln!(
            "[render] adapter: {:?} | device_type={:?} | backend={:?} | driver={} {}",
            a.name, a.device_type, a.backend, a.driver, a.driver_info
        );
    }
}

/// Full game plugin: rendering + game loop + egui UI.
pub struct TacticalGamePlugin;

impl Plugin for TacticalGamePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(bevy_egui::EguiPlugin);
        app.add_plugins(Tactical3dPlugin);
        app.add_plugins(game::GameLoopPlugin);
        app.add_plugins(ui::TacticalUiPlugin);
        app.add_plugins(fx::FxPlugin);
    }
}
