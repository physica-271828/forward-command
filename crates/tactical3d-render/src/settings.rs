//! In-battle render/performance settings: the
//! Esc menu's Settings window adjusts MSAA / shadows / render scale / frame
//! cap / idle frame-saver WITHOUT leaving the battle. `BattleSettings` is
//! the live source of truth once the window is up (`RenderQuality` in
//! state.rs only carries the startup values into `spawn_camera` /
//! `setup_board`); the Settings window writes it and change detection drives
//! [`apply_battle_settings`] here (hot-apply) plus the bin crate's
//! persistence to settings.json.

use bevy::prelude::*;

use crate::gate::RenderGate;
use crate::render_scale::RenderScaleState;

/// Live render/performance knobs for the battle window — a mirror of the
/// settings.json render keys. Inserted by the bin crate at assembly from
/// `AppSettings`; the Esc-menu Settings window edits it. Every consumer
/// reacts through Bevy change detection, so writes must only happen on a
/// real edit (the UI guards `value != current` before assigning).
#[derive(Resource)]
pub struct BattleSettings {
    /// MSAA sample count: 1 (= off) / 2 / 4.
    pub msaa: u32,
    /// Shadow quality: 0 = off, 1 = low (1024 map + one
    /// cascade + Hardware2x2 filtering), 2 = high (2048 + two cascades +
    /// Gaussian 9-tap).
    pub shadow_level: u32,
    /// Render-resolution scale percent: 100 / 85 / 70 / 50.
    pub render_scale: u32,
    /// Frame-rate cap (0 = uncapped; 30 / 60 / 90 / 120 / 144).
    pub max_fps: u32,
    /// Idle frame-saver: drop to ~10 fps when nothing moves.
    pub low_power: bool,
}

/// The sun's cascade coverage, captured by `setup_board` (board.rs) at spawn
/// so [`apply_battle_settings`] can rebuild the cascade config at the same
/// extent when the shadow level flips the cascade count (low = 1, high = 2).
#[derive(Resource)]
pub struct SunShadow {
    pub maximum_distance: f32,
    pub num_cascades: usize,
}

/// Hot-apply `BattleSettings` edits: MSAA + shadow filtering onto the 3D
/// camera, shadow on/off + cascade count onto the sun, the map size into the
/// shared shadow-map resource, the frame cap / idle saver into the render
/// gate, and the render-scale percent into `RenderScaleState` (the offscreen
/// setup/teardown reacts to THAT change — render_scale.rs). Fields are
/// written only when they differ, so downstream change detection never
/// fires spuriously. No-op where the resource is absent (menu, previews).
pub fn apply_battle_settings(
    settings: Option<Res<BattleSettings>>,
    sun: Option<ResMut<SunShadow>>,
    mut q_cam: Query<(&mut Msaa, &mut bevy::pbr::ShadowFilteringMethod), With<Camera3d>>,
    mut q_light: Query<(&mut DirectionalLight, &mut bevy::pbr::CascadeShadowConfig)>,
    mut shadow_map: ResMut<bevy::pbr::DirectionalLightShadowMap>,
    mut gate: ResMut<RenderGate>,
    scale: Option<ResMut<RenderScaleState>>,
) {
    let Some(s) = settings else { return };
    if !s.is_changed() {
        return;
    }
    // Camera: MSAA + the shadow edge filter that matches the level.
    let msaa = match s.msaa {
        1 => Msaa::Off,
        2 => Msaa::Sample2,
        _ => Msaa::Sample4,
    };
    let filtering = if s.shadow_level <= 1 {
        bevy::pbr::ShadowFilteringMethod::Hardware2x2
    } else {
        bevy::pbr::ShadowFilteringMethod::Gaussian
    };
    for (mut m, mut f) in &mut q_cam {
        if *m != msaa {
            *m = msaa;
        }
        if *f != filtering {
            *f = filtering;
        }
    }
    // Sun: on/off + the cascade tier (the map extent is the one setup_board
    // measured — a level flip must not shrink the coverage).
    let shadows_on = s.shadow_level > 0;
    let num_cascades = if s.shadow_level <= 1 { 1 } else { 2 };
    let maximum_distance = sun.as_deref().map(|c| c.maximum_distance);
    for (mut light, mut cascades) in &mut q_light {
        if light.shadows_enabled != shadows_on {
            light.shadows_enabled = shadows_on;
        }
        let current = sun.as_deref().map(|c| c.num_cascades);
        if current != Some(num_cascades) {
            *cascades = bevy::pbr::CascadeShadowConfigBuilder {
                num_cascades,
                first_cascade_far_bound: 60.0,
                maximum_distance: maximum_distance.unwrap_or(1000.0),
                ..default()
            }
            .build();
        }
    }
    if let Some(mut sun) = sun {
        if sun.num_cascades != num_cascades {
            sun.num_cascades = num_cascades;
        }
    }
    let map_size = if s.shadow_level >= 2 { 2048 } else { 1024 };
    if shadow_map.size != map_size {
        shadow_map.size = map_size;
    }
    // Gate: frame cap + idle saver.
    if gate.max_fps != s.max_fps {
        gate.max_fps = s.max_fps;
    }
    if gate.low_power != s.low_power {
        gate.low_power = s.low_power;
    }
    // Render scale: hand the percent over; apply_render_scale_change does
    // the offscreen setup/teardown.
    if let Some(mut scale) = scale {
        if scale.pct != s.render_scale {
            scale.pct = s.render_scale;
        }
    }
}
