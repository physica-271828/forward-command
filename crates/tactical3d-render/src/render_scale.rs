//! Render-resolution scale (settings `render_scale`) — the
//! weak-GPU lever. Below 100% the 3D scene renders into a smaller offscreen
//! target and a `Camera2d` + fullscreen sprite blits it up to the window,
//! cutting fragment/MSAA cost (and the offscreen buffers' VRAM) by the
//! square of the scale. At 100% the module inert: no offscreen target, the
//! camera renders straight to the window exactly as before. The scale can be
//! flipped mid-battle from the Esc-menu Settings window:
//! [`apply_render_scale_change`] builds/tears down the offscreen path on a
//! `pct` edit — the blit `Camera2d` is exactly why every camera query in the
//! crate carries a `With<Camera3d>` / `With<RtsCamera>` filter.
//!
//! Why this and not more quality knobs: MSAA/shadow tweaks barely moved a
//! 90%+-busy GPU because
//! the tax is total pixels × per-pixel cost — with HOI4 + the desktop
//! already pressing a 4 GB card, dropping rendered pixels is the only lever
//! that scales. Cursor/raycast call sites convert window → target
//! coordinates via [`crate::picking::scaled_cursor`]; egui overlays project
//! through NDC and need no conversion.
//!
//! Camera ordering is safe across sub-graphs: Bevy's single
//! `CameraDriverNode` walks `SortedCameras` by `Camera.order` globally, so
//! the 3D pass (order 0, image target) always completes before the blit
//! (order 1, window) samples it; bevy_egui's node runs after the driver, so
//! the UI still lands on top.

use bevy::prelude::*;
use bevy::render::camera::RenderTarget;
use bevy::render::render_asset::RenderAssetUsages;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::window::PrimaryWindow;

use crate::camera::RtsCamera;

/// The render-scale state for this app. `pct >= 100` = disabled (the
/// offscreen path is never built). `image` is Some once [`setup_render_scale`]
/// has created the offscreen target.
#[derive(Resource)]
pub struct RenderScaleState {
    pub pct: u32,
    pub image: Option<Handle<Image>>,
}

impl RenderScaleState {
    pub fn new(pct: u32) -> Self {
        RenderScaleState { pct, image: None }
    }
}

/// Marker on the blit (`Camera2d`) camera.
#[derive(Component)]
pub struct RenderScaleBlitCam;
/// Marker on the fullscreen blit sprite.
#[derive(Component)]
pub struct RenderScaleBlitQuad;

/// physical size × pct, clamped to ≥1 px per axis.
fn scaled_size(physical: UVec2, pct: u32) -> (u32, u32) {
    let w = (physical.x as u64 * pct as u64 / 100).max(1) as u32;
    let h = (physical.y as u64 * pct as u64 / 100).max(1) as u32;
    (w, h)
}

/// The offscreen color target the 3D scene renders into. Same format the
/// swapchain would use; bound as a texture for the blit afterwards.
fn target_image(w: u32, h: u32) -> Image {
    let size = Extent3d {
        width: w.max(1),
        height: h.max(1),
        depth_or_array_layers: 1,
    };
    let mut image = Image::new_fill(
        size,
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::bevy_default(),
        RenderAssetUsages::default(),
    );
    image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::RENDER_ATTACHMENT;
    image
}

/// What the runtime change handler must do for a `pct` edit (pure — unit
/// tested). Sub-100 → sub-100 needs nothing: [`sync_render_scale`] rebuilds
/// the target at the new size on the next frame by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleAction {
    Nothing,
    /// Window path → offscreen: build the target + blit pass.
    Setup,
    /// Offscreen → window path: re-target, despawn the blit pass, free the image.
    Teardown,
}

pub fn scale_action(pct: u32, has_image: bool) -> ScaleAction {
    if pct >= 100 {
        if has_image {
            ScaleAction::Teardown
        } else {
            ScaleAction::Nothing
        }
    } else if has_image {
        ScaleAction::Nothing
    } else {
        ScaleAction::Setup
    }
}

/// Build the offscreen target at the current physical size × pct, point the
/// RTS camera at it, spawn the blit camera + fullscreen sprite. Shared by
/// the startup system and the runtime change handler.
fn setup_offscreen(
    commands: &mut Commands,
    state: &mut RenderScaleState,
    images: &mut Assets<Image>,
    win: &Window,
    q_cam: &mut Query<&mut Camera, With<RtsCamera>>,
) {
    let (w, h) = scaled_size(win.resolution.physical_size(), state.pct);
    let handle = images.add(target_image(w, h));
    for mut cam in q_cam.iter_mut() {
        cam.target = RenderTarget::Image(handle.clone());
    }
    // The blit pass: no MSAA (a textured quad has no edges to smooth) and it
    // must run AFTER the 3D camera (global camera order — see module doc).
    commands.spawn((
        Camera2d,
        Camera {
            order: 1,
            ..default()
        },
        Msaa::Off,
        RenderScaleBlitCam,
    ));
    commands.spawn((
        Sprite {
            image: handle.clone(),
            custom_size: Some(win.resolution.size()),
            ..default()
        },
        RenderScaleBlitQuad,
    ));
    state.image = Some(handle);
}

/// Startup (after `camera::spawn_camera`): build the offscreen target when
/// the initial scale is below 100%. No-op at pct ≥ 100 (plain window path)
/// or without the resource (menu / previews).
pub fn setup_render_scale(
    mut commands: Commands,
    state: Option<ResMut<RenderScaleState>>,
    mut images: ResMut<Assets<Image>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut q_cam: Query<&mut Camera, With<RtsCamera>>,
) {
    let Some(mut state) = state else { return };
    if state.pct >= 100 || state.image.is_some() {
        return;
    }
    let Ok(win) = windows.get_single() else {
        return;
    };
    setup_offscreen(&mut commands, &mut state, &mut images, win, &mut q_cam);
}

/// React to `RenderScaleState.pct` edits from the in-battle Settings window
/// (the scale is not a launch-time-only knob — it can change mid-battle):
/// build the offscreen path when the scale drops below 100%, tear it down on
/// the way back to 100%. Between two sub-100 values the pct write alone
/// suffices — [`sync_render_scale`] re-creates the image at the new size.
pub fn apply_render_scale_change(
    mut commands: Commands,
    state: Option<ResMut<RenderScaleState>>,
    mut images: ResMut<Assets<Image>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut q_cam: Query<&mut Camera, With<RtsCamera>>,
    q_blit_cam: Query<Entity, With<RenderScaleBlitCam>>,
    q_blit_quad: Query<Entity, With<RenderScaleBlitQuad>>,
) {
    let Some(mut state) = state else { return };
    if !state.is_changed() {
        return;
    }
    match scale_action(state.pct, state.image.is_some()) {
        ScaleAction::Nothing => {}
        ScaleAction::Setup => {
            let Ok(win) = windows.get_single() else {
                return;
            };
            setup_offscreen(&mut commands, &mut state, &mut images, win, &mut q_cam);
        }
        ScaleAction::Teardown => {
            let handle = state.image.take().expect("Teardown implies an image");
            for mut cam in &mut q_cam {
                cam.target = RenderTarget::Window(bevy::window::WindowRef::Primary);
            }
            for e in &q_blit_cam {
                commands.entity(e).despawn();
            }
            for e in &q_blit_quad {
                commands.entity(e).despawn();
            }
            images.remove(handle.id());
        }
    }
}

/// Keep the offscreen target and the blit quad matched to the window: the
/// initial 1440×900 gets maximized on the first frames (and the user can
/// resize anytime). The image asset is replaced in place, so the camera
/// target / sprite handles never churn. One Image alloc per resize.
pub fn sync_render_scale(
    state: Option<Res<RenderScaleState>>,
    mut images: ResMut<Assets<Image>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut q_sprite: Query<&mut Sprite, With<RenderScaleBlitQuad>>,
) {
    let Some(state) = state else { return };
    let Some(handle) = &state.image else {
        return;
    };
    let Ok(win) = windows.get_single() else {
        return;
    };
    let (w, h) = scaled_size(win.resolution.physical_size(), state.pct);
    let stale = images
        .get(handle)
        .map(|img| img.width() != w || img.height() != h)
        .unwrap_or(true);
    if stale {
        images.insert(handle.id(), target_image(w, h));
    }
    let logical = win.resolution.size();
    for mut sprite in &mut q_sprite {
        if sprite.custom_size != Some(logical) {
            sprite.custom_size = Some(logical);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_size_math() {
        assert_eq!(scaled_size(UVec2::new(1920, 1080), 100), (1920, 1080));
        assert_eq!(scaled_size(UVec2::new(1920, 1080), 70), (1344, 756));
        assert_eq!(scaled_size(UVec2::new(1920, 1080), 50), (960, 540));
        // Never degenerate.
        assert_eq!(scaled_size(UVec2::new(0, 0), 70), (1, 1));
        assert_eq!(scaled_size(UVec2::new(1, 1), 50), (1, 1));
    }

    #[test]
    fn scale_action_state_machine() {
        // At 100 with nothing built: stay on the plain window path.
        assert_eq!(scale_action(100, false), ScaleAction::Nothing);
        // Back to 100 with the offscreen path live: tear it down.
        assert_eq!(scale_action(100, true), ScaleAction::Teardown);
        // Dropping below 100 with no target yet: build it.
        assert_eq!(scale_action(70, false), ScaleAction::Setup);
        assert_eq!(scale_action(50, false), ScaleAction::Setup);
        // Sub-100 → sub-100: sync_render_scale resizes the existing target.
        assert_eq!(scale_action(70, true), ScaleAction::Nothing);
        assert_eq!(scale_action(85, true), ScaleAction::Nothing);
    }

    #[test]
    fn scaled_cursor_matches_camera_viewport() {
        use crate::picking::{scale_cursor_to, scaled_cursor};
        let win = Vec2::new(1440.0, 900.0);
        let c = Vec2::new(200.0, 100.0);
        // Window-target camera (no computed target info yet): passes through.
        let cam = Camera::default();
        assert_eq!(scaled_cursor(c, &cam, win), c);
        // Same-size viewport → identity (the normal 100% path).
        assert_eq!(scale_cursor_to(c, win, win), c);
        // Offscreen 70% target → cursor scales into the camera's space.
        let out = scale_cursor_to(c, Vec2::new(1008.0, 630.0), win);
        assert!((out.x - 140.0).abs() < 0.01 && (out.y - 70.0).abs() < 0.01);
    }
}
