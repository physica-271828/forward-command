//! RTS camera: pan (WASD/arrows/middle-drag), zoom (wheel), orbit (Q/E or
//! right-drag), pitch (R/F). SD2-general-mode style free camera clamped to the
//! board bounds.
//!
//! Mouse drags are driven by **cursor positions**, never by `MouseMotion`
//! deltas: raw motion deltas scale with mouse DPI/driver and can be
//! enormous on gaming mice, which flung the camera into the board clamp.
//! Middle-drag instead grabs the ground point under the cursor and keeps it
//! locked there (SD2/Paradox-style map grab); right-drag orbits by logical
//! cursor-pixel deltas.

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use crate::state::TacticalState;

#[derive(Component)]
pub struct RtsCamera {
    pub target: Vec3,
    pub distance: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub min_distance: f32,
    pub max_distance: f32,
}

impl Default for RtsCamera {
    fn default() -> Self {
        RtsCamera {
            target: Vec3::ZERO,
            distance: 18.0,
            yaw: std::f32::consts::FRAC_PI_2, // looking north
            pitch: -0.9,
            min_distance: 4.0,
            // True-scale provinces need a far zoom-out (Sedan is
            // ~118 hexes tall); 300 covers boards up to ~128 rows, and
            // default_view() raises it for taller ones (cap is now 512).
            max_distance: 300.0,
        }
    }
}

impl RtsCamera {
    pub fn eye_position(&self) -> Vec3 {
        let offset = Vec3::new(
            self.yaw.cos() * self.pitch.cos(),
            -self.pitch.sin(),
            self.yaw.sin() * self.pitch.cos(),
        ) * self.distance;
        self.target + offset
    }
}

/// One-shot request (set by the UI "Reset View" button — 中文名：重置视角，
/// to be applied when the UI is localized; testing UI stays English) to
/// restore the initial camera view; consumed by `rts_camera_controller`.
#[derive(Resource, Default)]
pub struct CameraResetReq(pub bool);

/// Pan the camera target to a world position (keeping yaw / pitch
/// / distance) — used by the Order of Battle window's click-to-locate.
/// Consumed by `rts_camera_controller`.
#[derive(Resource, Default)]
pub struct CameraFocusReq(pub Option<Vec3>);

/// One-shot request to set the camera zoom distance: the
/// battle-report tour pins the magnification to [`REPORT_CAM_DISTANCE`] for
/// the playback and restores the player's own zoom when the queue drains.
/// Consumed by `rts_camera_controller`.
#[derive(Resource, Default)]
pub struct CameraZoomReq(pub Option<f32>);

/// Fixed camera distance for battle-report playback: the
/// defender and everything within 3 hexes (~3·√3 ≈ 5.2 world units; with
/// unit models and rising damage floaters ≈ 6.5) stay clearly in frame.
/// From default_view()'s fit math the NEAR edge binds at the default −0.9
/// pitch (d ≈ 2.55·R → 16.6); flatter pitches only show more ground.
pub const REPORT_CAM_DISTANCE: f32 = 16.5;

/// Smooth camera glides (battle-report tour). A pan+zoom tween
/// owned by the controller — while it plays, user camera input and any
/// stale one-shot focus/zoom/reset requests are ignored (the tour freezes
/// map input anyway). `arrived` pulses true for exactly one frame when the
/// glide lands; the tour gates the report window and its combat FX on it.
#[derive(Resource, Default)]
pub struct CameraGlide {
    active: Option<Glide>,
    arrived: bool,
}

#[derive(Debug, Clone, Copy)]
struct Glide {
    from_target: Vec3,
    from_distance: f32,
    to_target: Vec3,
    to_distance: f32,
    t: f32,
    duration: f32,
}

impl CameraGlide {
    /// Start a glide from the camera's current pose. The duration scales
    /// with the pan length + zoom delta (a cross-map jump reads as a move,
    /// a next-hex hop stays snappy), clamped to a tight band.
    pub fn start(&mut self, cam: &RtsCamera, to_target: Vec3, to_distance: f32) {
        let pan = cam.target.distance(to_target);
        let zoom = (cam.distance - to_distance).abs();
        let duration = (pan / 30.0 + zoom / 45.0).clamp(0.35, 1.2);
        self.active = Some(Glide {
            from_target: cam.target,
            from_distance: cam.distance,
            to_target,
            to_distance,
            t: 0.0,
            duration,
        });
        self.arrived = false;
    }

    /// Advance the tween; returns this frame's (target, distance). On
    /// completion the glide clears and the arrival pulse arms.
    fn tick(&mut self, dt: f32) -> Option<(Vec3, f32)> {
        let g = self.active.as_mut()?;
        g.t = (g.t + dt / g.duration).min(1.0);
        // Smoothstep — slow at both ends.
        let e = g.t * g.t * (3.0 - 2.0 * g.t);
        let pose = (
            g.from_target.lerp(g.to_target, e),
            g.from_distance + (g.to_distance - g.from_distance) * e,
        );
        if g.t >= 1.0 {
            self.active = None;
            self.arrived = true;
        }
        Some(pose)
    }

    /// The one-frame arrival pulse (consumed by the battle-tour tick).
    pub fn take_arrived(&mut self) -> bool {
        std::mem::take(&mut self.arrived)
    }
}

/// A right-CLICK on the map (press + release under the drag threshold),
/// delivered by `rts_camera_controller` and consumed by
/// `game::handle_map_clicks`: right-DRAG orbits the camera,
/// right-CLICK issues the context command (move / attack / stand-by).
#[derive(Resource, Default)]
pub struct MapRightClick(pub Option<Vec2>);

/// Right-drag must exceed this cursor-pixel distance before it counts as an
/// orbit drag; below it the release is a map command click.
const RIGHT_CLICK_PX: f32 = 6.0;

/// The view the battle opens with: default angles, board centered, zoom
/// fitted so the whole board just fills the frame.
pub fn default_view(state: &TacticalState) -> RtsCamera {
    let mut cam = RtsCamera::default();
    if let Some(grid) = &state.grid {
        let (x, z) =
            tactical_core::hex::HexCoord::new(grid.width as i32 / 2, grid.height as i32 / 2)
                .to_world(crate::board::HEX_SIZE);
        cam.target = Vec3::new(x, 0.0, z);

        // Fit the whole board in frame. The binding constraint is the NEAR
        // edge: at pitch -0.9 the view axis hits the board center, so ground
        // points on the eye's side appear near the bottom screen edge — the
        // camera must be far enough that the near rim stays inside the lower
        // half-fov. Then check far edge and cross axis, take the max.
        // (Bevy default Camera3d vertical fov = π/4.)
        let (max_x, max_z) =
            tactical_core::hex::HexCoord::new(grid.width as i32 - 1, grid.height as i32 - 1)
                .to_world(crate::board::HEX_SIZE);
        // Board half-extents along / across the view azimuth.
        let (ux, uz) = (cam.yaw.cos(), cam.yaw.sin());
        let r_along = (ux.abs() * max_x + uz.abs() * max_z) * 0.5;
        let r_cross = (uz.abs() * max_x + ux.abs() * max_z) * 0.5;
        let p = -cam.pitch; // view axis angle below horizontal (0.9 rad)
        let h = std::f32::consts::FRAC_PI_8; // half vertical fov
        let (ch, cv) = (p.cos(), p.sin());
        // Ground point at along-distance R from target: depression angle from
        // the eye is atan(cv·d / (ch·d ∓ R)); solve for it to hit p ± h.
        let k_near = (p + h).tan();
        let d_near = k_near * r_along / (ch * k_near - cv);
        let k_far = (p - h).tan();
        let d_far = if cv > ch * k_far {
            k_far * r_along / (cv - ch * k_far)
        } else {
            0.0
        };
        let d_cross = r_cross / (h.tan() * 1.5); // assume ≥4:3 viewport
        let fit = d_near.max(d_far).max(d_cross) * 1.05;
        // Boards taller than the old 128-row cap (now 512) need a farther
        // zoom-out than the 300 default — give this camera enough range;
        // small boards keep the tuned 300.
        cam.max_distance = 300.0f32.max(fit * 1.5);
        cam.distance = fit.clamp(cam.min_distance, cam.max_distance);
    }
    cam
}

pub fn spawn_camera(
    mut commands: Commands,
    state: Res<TacticalState>,
    quality: Res<crate::state::RenderQuality>,
) {
    let cam = default_view(&state);
    let eye = cam.eye_position();
    // Fog range follows the board's fit distance — a fixed 45–110
    // band (tuned when max zoom was 60) drowned true-scale provinces
    // entirely once the camera went past 110.
    let fit = cam.distance;
    let fog_start = (fit * 1.2).max(45.0);
    let fog_end = (fit * 3.0).max(110.0);
    commands.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(Color::srgb(0.53, 0.66, 0.82)),
            ..default()
        },
        Transform::from_translation(eye).looking_at(cam.target, Vec3::Y),
        DistanceFog {
            color: Color::srgba(0.53, 0.66, 0.82, 1.0),
            falloff: FogFalloff::Linear {
                start: fog_start,
                end: fog_end,
            },
            ..default()
        },
        // MSAA from settings.json (default Sample2 — the balanced default
        // that keeps low-end GPUs comfortable, with FXAA below covering
        // far-zoom moiré; Sample4 remains selectable).
        quality.msaa,
        // FXAA always on: the far-zoom orbit moiré on the hex grid is
        // subpixel edge shimmer — exactly FXAA's job, at a fraction of the
        // 4×→2× MSAA delta. Stacks fine with MSAA (they clean up different
        // artifacts) and applies inside whichever target the camera
        // renders (window at 100%, offscreen under render_scale).
        bevy::core_pipeline::fxaa::Fxaa::default(),
        // Shadow edge filtering follows the shadow level — High
        // keeps Bevy's Gaussian 9-tap look; Low drops to Hardware2x2 (under
        // half the per-pixel shadow sampling cost, slightly harder edges).
        if quality.shadow_level <= 1 {
            bevy::pbr::ShadowFilteringMethod::Hardware2x2
        } else {
            bevy::pbr::ShadowFilteringMethod::Gaussian
        },
        cam,
    ));
}

/// Transient drag bookkeeping for the camera controller.
#[derive(Default)]
pub struct DragState {
    /// Ground point grabbed when the middle-drag started (map-grab panning).
    grab: Option<Vec3>,
    /// Cursor position on the previous frame (right-drag orbit).
    last_cursor: Option<Vec2>,
    /// Cursor position where the right button went down (click/drag
    /// split); None when the press started over a UI panel.
    press_pos: Option<Vec2>,
    /// True once the right-drag exceeded RIGHT_CLICK_PX — orbit mode; a
    /// release below the threshold is a map command click instead.
    dragging: bool,
}

/// When present, all camera input is ignored (the main menu's
/// backdrop orbits on its own — the user must not be able to disturb it).
#[derive(Resource)]
pub struct CameraInputLocked;

/// Ground point (shared picking plane, y = GROUND_PLANE_Y) under the cursor,
/// mirroring `picking`. `window_size` feeds the render-scale cursor
/// conversion (picking::scaled_cursor).
fn ground_under_cursor(
    cursor: Vec2,
    camera: &Camera,
    cam_tf: &GlobalTransform,
    window_size: Vec2,
) -> Option<Vec3> {
    let cursor = crate::picking::scaled_cursor(cursor, camera, window_size);
    let ray = camera.viewport_to_world(cam_tf, cursor).ok()?;
    let denom = ray.direction.y;
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = (crate::board::GROUND_PLANE_Y - ray.origin.y) / denom;
    if t < 0.0 {
        return None;
    }
    Some(ray.origin + ray.direction * t)
}

#[allow(clippy::too_many_arguments)]
pub fn rts_camera_controller(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut wheel: EventReader<MouseWheel>,
    mut q_cam: Query<(
        &mut RtsCamera,
        &mut Transform,
        &Camera,
        &mut GlobalTransform,
    )>,
    state: Res<TacticalState>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut drag: Local<DragState>,
    mut reset: ResMut<CameraResetReq>,
    mut focus: ResMut<CameraFocusReq>,
    mut zoom: ResMut<CameraZoomReq>,
    mut glide: ResMut<CameraGlide>,
    mut rclick: ResMut<MapRightClick>,
    input_lock: Option<Res<CameraInputLocked>>,
) {
    // Main-menu backdrop: input fully locked — but still apply the transform
    // (menu_orbit_camera drives yaw directly; skipping the write freezes the
    // view entirely, which is NOT the intent).
    if input_lock.is_some() {
        let Ok((cam, mut transform, _, mut gtf)) = q_cam.get_single_mut() else {
            return;
        };
        let eye = cam.eye_position();
        *transform = Transform::from_translation(eye).looking_at(cam.target, Vec3::Y);
        *gtf = (*transform).into(); // see the GlobalTransform note at the bottom
        return;
    }
    let Ok((mut cam, mut transform, camera, mut cam_gtf)) = q_cam.get_single_mut() else {
        return;
    };
    let dt = time.delta_secs();

    // --- A glide owns the camera (battle-report tour) ---
    if glide.active.is_some() {
        // Drop stale one-shot requests — they cannot be honored without
        // yanking mid-glide (the tour freezes map input anyway).
        focus.0 = None;
        zoom.0 = None;
        reset.0 = false;
        if let Some((target, distance)) = glide.tick(dt) {
            cam.target = target;
            cam.distance = distance;
        }
        let eye = cam.eye_position();
        *transform = Transform::from_translation(eye).looking_at(cam.target, Vec3::Y);
        *cam_gtf = (*transform).into(); // same GlobalTransform self-write as below
        return;
    }

    // --- Focus request (OOB click-to-locate): pan only, keep the
    // current yaw / pitch / distance ---
    if let Some(t) = focus.0.take() {
        cam.target = t;
    }

    // --- Zoom request (battle-report fixed magnification) ---
    if let Some(d) = zoom.0.take() {
        cam.distance = d.clamp(cam.min_distance, cam.max_distance);
    }

    // --- Reset to the initial view (重置视角 button) ---
    if reset.0 {
        reset.0 = false;
        let def = default_view(&state);
        cam.target = def.target;
        cam.yaw = def.yaw;
        cam.pitch = def.pitch;
        cam.distance = def.distance;
        drag.grab = None;
        drag.last_cursor = None;
        drag.press_pos = None;
        drag.dragging = false;
    }

    // --- Pan (keyboard) ---
    let mut pan = Vec3::ZERO;
    let speed = cam.distance * 0.9 * dt;
    let forward = Vec3::new(-cam.yaw.cos(), 0.0, -cam.yaw.sin());
    let right = Vec3::new(-forward.z, 0.0, forward.x);
    if keys.pressed(KeyCode::KeyW) || keys.pressed(KeyCode::ArrowUp) {
        pan += forward;
    }
    if keys.pressed(KeyCode::KeyS) || keys.pressed(KeyCode::ArrowDown) {
        pan -= forward;
    }
    if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
        pan -= right;
    }
    if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
        pan += right;
    }
    if pan.length_squared() > 0.0 {
        cam.target += pan.normalize() * speed;
    }

    // --- Orbit ---
    if keys.pressed(KeyCode::KeyQ) {
        cam.yaw += 1.6 * dt;
    }
    if keys.pressed(KeyCode::KeyE) {
        cam.yaw -= 1.6 * dt;
    }
    if keys.pressed(KeyCode::KeyR) {
        cam.pitch = (cam.pitch + 1.2 * dt).clamp(-1.45, -0.25);
    }
    if keys.pressed(KeyCode::KeyF) {
        cam.pitch = (cam.pitch - 1.2 * dt).clamp(-1.45, -0.25);
    }

    // --- Mouse drag: middle = grab the map, right = orbit ---
    // Don't drag when the pointer is over an egui panel.
    let pointer_over_ui = state.pointer_over_ui;
    let window_size = windows
        .get_single()
        .map(|w| w.resolution.size())
        .unwrap_or(Vec2::ZERO);
    let cursor = windows.get_single().ok().and_then(|w| w.cursor_position());

    if !pointer_over_ui && mouse.pressed(MouseButton::Middle) {
        if let Some(c) = cursor {
            let ground = ground_under_cursor(c, camera, &cam_gtf, window_size);
            match (drag.grab, ground) {
                // Keep the grabbed ground point glued under the cursor.
                (Some(grab), Some(cur)) => cam.target += grab - cur,
                // First frame of the drag: remember what was grabbed.
                (None, Some(cur)) => drag.grab = Some(cur),
                _ => {}
            }
        }
    } else {
        drag.grab = None;
    }

    // Right button: drag past RIGHT_CLICK_PX orbits; a release below the
    // threshold is a map command click forwarded to the game loop
    // (hover picking also stays frozen while the button is down,
    // see picking.rs).
    if mouse.just_pressed(MouseButton::Right) {
        drag.press_pos = if pointer_over_ui { None } else { cursor };
        drag.dragging = false;
        drag.last_cursor = None;
    }
    if !pointer_over_ui && mouse.pressed(MouseButton::Right) && drag.press_pos.is_some() {
        if !drag.dragging {
            if let (Some(c), Some(p)) = (cursor, drag.press_pos) {
                if c.distance(p) > RIGHT_CLICK_PX {
                    drag.dragging = true;
                    drag.last_cursor = Some(c);
                }
            }
        }
        if drag.dragging {
            if let (Some(c), Some(last)) = (cursor, drag.last_cursor) {
                let d = c - last;
                cam.yaw += d.x * 0.004;
                cam.pitch = (cam.pitch - d.y * 0.003).clamp(-1.45, -0.25);
            }
            drag.last_cursor = cursor;
        }
    }
    if mouse.just_released(MouseButton::Right) {
        if drag.press_pos.is_some() && !drag.dragging && !pointer_over_ui {
            rclick.0 = cursor;
        }
        drag.press_pos = None;
        drag.dragging = false;
        drag.last_cursor = None;
    }

    // --- Zoom ---
    if !pointer_over_ui {
        for ev in wheel.read() {
            let factor = 1.0 - ev.y * 0.12;
            cam.distance = (cam.distance * factor).clamp(cam.min_distance, cam.max_distance);
        }
    }

    // --- Clamp target to board bounds ---
    if let Some(grid) = &state.grid {
        let (max_x, max_z) =
            tactical_core::hex::HexCoord::new(grid.width as i32 - 1, grid.height as i32 - 1)
                .to_world(crate::board::HEX_SIZE);
        cam.target.x = cam.target.x.clamp(-4.0, max_x + 4.0);
        cam.target.z = cam.target.z.clamp(-4.0, max_z + 4.0);
    }

    let eye = cam.eye_position();
    *transform = Transform::from_translation(eye).looking_at(cam.target, Vec3::Y);
    // Write the GlobalTransform HERE, not just Transform. Bevy only
    // propagates Transform → GlobalTransform in PostUpdate, so egui painters
    // projecting world→screen during Update (unit labels / VP name / damage
    // floaters / hover card / minimap view-quad) would otherwise read LAST
    // frame's camera pose — the labels visibly trail the models while the
    // map drags, one full frame's worth (30–50 ms at low fps). The camera
    // has no parent, so PostUpdate recomputes exactly this value (idempotent).
    *cam_gtf = (*transform).into();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glide_tweens_and_pulses_arrival() {
        let cam = RtsCamera {
            target: Vec3::ZERO,
            distance: 20.0,
            ..Default::default()
        };
        let mut glide = CameraGlide::default();
        glide.start(&cam, Vec3::new(30.0, 0.0, 0.0), 10.0);
        // pan 30/30 + zoom 10/45 ≈ 1.22 s → clamped to the 1.2 cap; the
        // smoothstep midpoint lands exactly halfway.
        let (target, dist) = glide.tick(0.6).unwrap();
        assert!((target.x - 15.0).abs() < 0.01 && (dist - 15.0).abs() < 0.01);
        assert!(!glide.take_arrived());
        let (target, dist) = glide.tick(0.6).unwrap();
        assert_eq!(target, Vec3::new(30.0, 0.0, 0.0));
        assert_eq!(dist, 10.0);
        assert!(glide.take_arrived());
        assert!(!glide.take_arrived(), "the pulse is one-shot");
        assert!(glide.active.is_none());
    }

    #[test]
    fn glide_duration_scales_and_clamps() {
        let cam = RtsCamera::default(); // target origin, distance 18
        let mut glide = CameraGlide::default();
        glide.start(&cam, Vec3::new(0.5, 0.0, 0.0), 18.0); // next-hex hop
        assert_eq!(glide.active.unwrap().duration, 0.35);
        glide.start(&cam, Vec3::new(500.0, 0.0, 500.0), 300.0); // cross-map jump
        assert_eq!(glide.active.unwrap().duration, 1.2);
    }
}
