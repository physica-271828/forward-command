//! Mouse → hex picking: cast a ray from the camera through the cursor and
//! intersect the board's XZ plane (using local terrain height as the plane).

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use tactical_core::hex::HexCoord;

use crate::board::HEX_SIZE;
use crate::state::TacticalState;

/// Cursor position → the camera's render-target coordinate space.
/// With a render-scale offscreen target the camera viewport is
/// SMALLER than the window, so the cursor (window logical px) is scaled by
/// camera/window; on the normal window-target path the ratio is exactly 1
/// and the cursor passes through unchanged.
pub fn scaled_cursor(cursor: Vec2, camera: &Camera, window_size: Vec2) -> Vec2 {
    let cam = camera.logical_viewport_size().unwrap_or(window_size);
    scale_cursor_to(cursor, cam, window_size)
}

/// The pure ratio math behind [`scaled_cursor`] (split out for tests —
/// Bevy's camera target info is not constructible outside a running app).
pub(crate) fn scale_cursor_to(cursor: Vec2, cam_size: Vec2, window_size: Vec2) -> Vec2 {
    if (cam_size - window_size).length() < 0.5 {
        cursor
    } else {
        cursor * (cam_size / window_size)
    }
}

/// Cursor position → world point on the ground plane (y ≈ 0.4).
/// The camera query MUST be Camera3d-filtered: the render-scale
/// blit camera (Camera2d) would otherwise make `get_single()` fail and kill
/// every projection/pick path when render_scale < 100.
pub fn cursor_world_point(
    windows: &Query<&Window, With<PrimaryWindow>>,
    q_cam: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
) -> Option<Vec3> {
    let window = windows.get_single().ok()?;
    let cursor = window.cursor_position()?;
    let (camera, cam_transform) = q_cam.get_single().ok()?;
    let cursor = scaled_cursor(cursor, camera, window.resolution.size());
    let ray = camera.viewport_to_world(cam_transform, cursor).ok()?;
    // Intersect with the shared picking plane (average board top height).
    let plane_y = crate::board::GROUND_PLANE_Y;
    let denom = ray.direction.y;
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = (plane_y - ray.origin.y) / denom;
    if t < 0.0 {
        return None;
    }
    Some(ray.origin + ray.direction * t)
}

/// Cursor position → hex coordinate.
pub fn cursor_hex(
    windows: &Query<&Window, With<PrimaryWindow>>,
    q_cam: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
) -> Option<HexCoord> {
    let p = cursor_world_point(windows, q_cam)?;
    Some(HexCoord::from_world(p.x, p.z, HEX_SIZE))
}

/// System: keep `state.hover_hex` updated and mark hover highlight.
pub fn update_hover(
    windows: Query<&Window, With<PrimaryWindow>>,
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>, // Camera3d-only: excludes the render-scale blit camera (see cursor_world_point)
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<TacticalState>,
) {
    if state.pointer_over_ui {
        if state.hover_hex.is_some() {
            state.hover_hex = None;
            state.board_colors_dirty = true;
        }
        return;
    }
    // While drag-panning/orbiting the world slides under the cursor, which
    // would dirty the board highlight (→ board palette rewrite) every frame
    // for no benefit (a hover/dirty-mark feedback loop).
    if mouse.pressed(MouseButton::Middle) || mouse.pressed(MouseButton::Right) {
        return;
    }
    // Keyboard camera motion does the same: WASD /
    // arrows pan, Q/E orbit and R/F pitch (the camera.rs bindings) slide the
    // world under a stationary cursor. Freeze the hover while any of them is
    // held; it self-corrects on the first frame after release.
    const CAM_KEYS: [KeyCode; 12] = [
        KeyCode::KeyW,
        KeyCode::KeyA,
        KeyCode::KeyS,
        KeyCode::KeyD,
        KeyCode::ArrowUp,
        KeyCode::ArrowDown,
        KeyCode::ArrowLeft,
        KeyCode::ArrowRight,
        KeyCode::KeyQ,
        KeyCode::KeyE,
        KeyCode::KeyR,
        KeyCode::KeyF,
    ];
    if CAM_KEYS.iter().any(|k| keys.pressed(*k)) {
        return;
    }
    let h = cursor_hex(&windows, &q_cam).filter(|h| {
        state
            .grid
            .as_ref()
            .map(|g| g.in_bounds(*h))
            .unwrap_or(false)
    });
    if h != state.hover_hex {
        state.hover_hex = h;
        state.board_colors_dirty = true;
        // Seize picking's hover-hex highlight follows the cursor.
        if state.div_pick.is_some() {
            crate::game::refresh_command_highlights(&mut state);
        }
    }
}

/// Vertical offset for placing a unit model on its hex (the
/// per-hex ELEVATION top, so models stand on the ridge they occupy).
pub fn unit_y_on_grid(state: &TacticalState, h: HexCoord) -> f32 {
    state
        .grid
        .as_ref()
        .and_then(|g| g.cell(h))
        .map(|c| tactical_core::Terrain::elevation_render_height(c.elevation))
        .unwrap_or(0.3)
}
