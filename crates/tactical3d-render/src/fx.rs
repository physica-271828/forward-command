//! Combat FX (blocky style): tracers, muzzle flashes, impact debris, and
//! egui floating damage numbers. Events are pushed by the game loop.

use std::collections::VecDeque;

use bevy::pbr::NotShadowCaster;
use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};
use tactical_core::fog::VisibilityState;
use tactical_core::hex::HexCoord;

/// One FX request.
#[derive(Debug, Clone)]
pub enum FxEvent {
    /// Projectile from→to (artillery arcs high).
    Tracer {
        from: Vec3,
        to: Vec3,
        artillery: bool,
    },
    /// Rising damage text at a position.
    Floater {
        pos: Vec3,
        text: String,
        kind: FloaterKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloaterKind {
    OrgDamage,
    StrDamage,
    /// Red "Intercepted!" — enemy held the next hex / contact.
    Intercepted,
    /// Orange "Congested" — friendly unit blocks the march.
    Congested,
}

#[derive(Resource, Default)]
pub struct FxQueue(pub VecDeque<FxEvent>);

impl FxQueue {
    pub fn push(&mut self, ev: FxEvent) {
        // Cap queued effects to avoid pileups during long AI turns. 256
        // covers a full battle-report batch (a division-scale engagement
        // easily exceeds the old 64 and silently dropped hit animations).
        if self.0.len() < 256 {
            self.0.push_back(ev);
        }
    }
}

// `pub(crate)`: the render gate (gate.rs) queries these to keep
// the gate open while any combat fx is in flight.
#[derive(Component)]
pub(crate) struct Projectile {
    from: Vec3,
    to: Vec3,
    t: f32,
    duration: f32,
    arc_height: f32,
}

#[derive(Component)]
pub(crate) struct Flash {
    t: f32,
}

#[derive(Component)]
pub(crate) struct Debris {
    velocity: Vec3,
    t: f32,
}

#[derive(Component)]
struct FxMarker;

struct ActiveFloater {
    pos: Vec3,
    text: String,
    kind: FloaterKind,
    t: f32,
}

/// Live floaters. `pub(crate)`: the draw system is registered from the
/// ui.rs chain (paint ordering vs the panels), so its signature — and this
/// resource — must be nameable there.
#[derive(Resource, Default)]
pub(crate) struct Floaters(Vec<ActiveFloater>);

impl Floaters {
    /// Any live floaters? (gate.rs keeps rendering while they fade.)
    pub(crate) fn any(&self) -> bool {
        !self.0.is_empty()
    }
}

/// Floating victory-point label — the VP's real name hovering
/// above its city, HOI4 style (gold pip + white name with black outline).
/// Set once at battle start from `TacticalMap::vp_label`.
#[derive(Resource, Default)]
pub struct VpLabel(pub Option<(String, tactical_core::HexCoord)>);

#[derive(Resource)]
struct FxAssets {
    projectile: Handle<Mesh>,
    flash: Handle<Mesh>,
    debris: Handle<Mesh>,
    tracer_mat: Handle<StandardMaterial>,
    flash_mat: Handle<StandardMaterial>,
    debris_mat: Handle<StandardMaterial>,
}

pub struct FxPlugin;

impl Plugin for FxPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FxQueue>()
            .init_resource::<Floaters>()
            .init_resource::<VpLabel>()
            .add_systems(Startup, setup_fx_assets)
            .add_systems(
                Update,
                (
                    spawn_fx,
                    animate_projectiles,
                    animate_flash,
                    animate_debris,
                    // draw_floaters + draw_vp_label live in the ui.rs chain:
                    // they paint onto the shared panel Background
                    // list and must run BEFORE draw_panels so the sidebar /
                    // notice bar / report modal cover the world-space text.
                ),
            );
    }
}

fn setup_fx_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let projectile = meshes.add(Cuboid::new(0.07, 0.07, 0.07));
    let flash = meshes.add(Cuboid::new(0.16, 0.16, 0.16));
    let debris = meshes.add(Cuboid::new(0.05, 0.05, 0.05));
    let tracer_mat = materials.add(StandardMaterial {
        base_color: Color::srgb(1.0, 0.85, 0.3),
        emissive: LinearRgba::new(1.0, 0.75, 0.2, 1.0),
        unlit: true,
        ..default()
    });
    let flash_mat = materials.add(StandardMaterial {
        base_color: Color::srgb(1.0, 0.95, 0.7),
        emissive: LinearRgba::new(1.0, 0.9, 0.5, 1.0),
        unlit: true,
        ..default()
    });
    let debris_mat = materials.add(StandardMaterial {
        base_color: Color::srgb(0.35, 0.32, 0.30),
        ..default()
    });
    commands.insert_resource(FxAssets {
        projectile,
        flash,
        debris,
        tracer_mat,
        flash_mat,
        debris_mat,
    });
}

fn spawn_fx(
    mut commands: Commands,
    mut queue: ResMut<FxQueue>,
    mut floaters: ResMut<Floaters>,
    assets: Option<Res<FxAssets>>,
) {
    let Some(assets) = assets else { return };
    while let Some(ev) = queue.0.pop_front() {
        match ev {
            FxEvent::Tracer {
                from,
                to,
                artillery,
            } => {
                // Muzzle flash.
                commands.spawn((
                    Mesh3d(assets.flash.clone()),
                    MeshMaterial3d(assets.flash_mat.clone()),
                    Transform::from_translation(from),
                    Flash { t: 0.0 },
                    FxMarker,
                    NotShadowCaster,
                ));
                let duration = if artillery { 0.9 } else { 0.25 };
                let arc_height = if artillery { 2.2 } else { 0.15 };
                commands.spawn((
                    Mesh3d(assets.projectile.clone()),
                    MeshMaterial3d(assets.tracer_mat.clone()),
                    Transform::from_translation(from),
                    Projectile {
                        from,
                        to,
                        t: 0.0,
                        duration,
                        arc_height,
                    },
                    FxMarker,
                    NotShadowCaster,
                ));
            }
            FxEvent::Floater { pos, text, kind } => {
                floaters.0.push(ActiveFloater {
                    pos,
                    text,
                    kind,
                    t: 0.0,
                });
            }
        }
    }
}

fn spawn_debris(commands: &mut Commands, assets: &FxAssets, at: Vec3, seed: u32) {
    for i in 0..5 {
        // Integer division here used to quantize the scatter to 7 angles
        // (angle audit); keep the fraction for a proper radial spread.
        let a = ((seed.wrapping_mul(2654435761).wrapping_add(i * 97)) % 628) as f32 / 100.0;
        let v = Vec3::new(a.cos() * 1.2, 1.6 + (i % 3) as f32 * 0.5, a.sin() * 1.2);
        commands.spawn((
            Mesh3d(assets.debris.clone()),
            MeshMaterial3d(assets.debris_mat.clone()),
            Transform::from_translation(at),
            Debris {
                velocity: v,
                t: 0.0,
            },
            FxMarker,
            NotShadowCaster,
        ));
    }
}

fn animate_projectiles(
    mut commands: Commands,
    time: Res<Time>,
    assets: Option<Res<FxAssets>>,
    mut q: Query<(Entity, &mut Transform, &mut Projectile)>,
) {
    let mut impacts: Vec<(Vec3, u32)> = Vec::new();
    for (entity, mut transform, mut proj) in q.iter_mut() {
        proj.t += time.delta_secs() / proj.duration;
        if proj.t >= 1.0 {
            transform.translation = proj.to;
            impacts.push((proj.to, entity.index()));
            commands.entity(entity).despawn();
            continue;
        }
        let t = proj.t;
        let mut pos = proj.from.lerp(proj.to, t);
        pos.y += proj.arc_height * 4.0 * t * (1.0 - t); // parabola
        transform.translation = pos;
    }
    if let Some(assets) = assets {
        for (at, seed) in impacts {
            spawn_debris(&mut commands, &assets, at, seed);
        }
    }
}

fn animate_flash(
    mut commands: Commands,
    time: Res<Time>,
    mut q: Query<(Entity, &mut Transform, &mut Flash)>,
) {
    for (entity, mut transform, mut flash) in q.iter_mut() {
        flash.t += time.delta_secs() / 0.12;
        if flash.t >= 1.0 {
            commands.entity(entity).despawn();
        } else {
            let s = 1.0 + flash.t * 0.8;
            transform.scale = Vec3::splat(s);
        }
    }
}

fn animate_debris(
    mut commands: Commands,
    time: Res<Time>,
    mut q: Query<(Entity, &mut Transform, &mut Debris)>,
) {
    let dt = time.delta_secs();
    for (entity, mut transform, mut debris) in q.iter_mut() {
        debris.t += dt;
        if debris.t > 0.7 {
            commands.entity(entity).despawn();
            continue;
        }
        debris.velocity.y -= 9.0 * dt;
        transform.translation += debris.velocity * dt;
        if transform.translation.y < 0.02 {
            transform.translation.y = 0.02;
            debris.velocity = Vec3::ZERO;
        }
    }
}

/// Rising damage / event numbers (org, str, contact, congestion). Painted
/// onto the shared panel Background list from the ui.rs chain BEFORE
/// draw_panels: the floaters slide UNDER the command sidebar, minimap,
/// notice bar, hover cards and the battle-report modal instead of running
/// over the info panels (otherwise the org/str numbers climb on top of the
/// info window).
pub(crate) fn draw_floaters(
    mut contexts: EguiContexts,
    time: Res<Time>,
    mut floaters: ResMut<Floaters>,
    state: Res<crate::state::TacticalState>,
    // With<Camera3d>: the render-scale blit adds a Camera2d —
    // an unfiltered get_single() fails outright with two cameras live.
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
) {
    // try_ctx_mut: the egui context may be uninitialized on the very first
    // frame (a known egui first-frame hazard) — ctx_mut panics there.
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    let Ok((camera, cam_tf)) = q_cam.get_single() else {
        return;
    };
    let painter = ctx.layer_painter(egui::LayerId::background());
    let screen = ctx.screen_rect();
    floaters.0.retain_mut(|f| {
        f.t += time.delta_secs() / 2.0;
        if f.t >= 1.0 {
            return false;
        }
        // A floater is a physical map event — never paint it over
        // a hex the player cannot currently see. Push sites stay dumb; this
        // one gate covers enemy congestion/contact markers and barrage
        // damage numbers over fogged intel hexes alike. Kept alive but
        // unpainted — a mid-float reveal shows the tail end legitimately.
        let hex = HexCoord::from_world(f.pos.x, f.pos.z, crate::board::HEX_SIZE);
        if state.fog_state(hex) != VisibilityState::Visible {
            return true;
        }
        let w = f.pos + Vec3::Y * (0.3 + f.t * 0.8);
        let Some(ndc) = camera.world_to_ndc(cam_tf, w) else {
            return false;
        };
        let x = (ndc.x * 0.5 + 0.5) * screen.width();
        let y = (1.0 - (ndc.y * 0.5 + 0.5)) * screen.height();
        let (color, size) = match f.kind {
            FloaterKind::OrgDamage => (egui::Color32::from_rgb(240, 200, 60), 14.0),
            FloaterKind::StrDamage => (egui::Color32::from_rgb(235, 90, 70), 14.0),
            FloaterKind::Intercepted => (egui::Color32::from_rgb(235, 60, 50), 16.0),
            FloaterKind::Congested => (egui::Color32::from_rgb(245, 160, 60), 13.0),
        };
        let fade = ((1.0 - f.t) * 255.0) as u8;
        painter.text(
            egui::pos2(x, y),
            egui::Align2::CENTER_CENTER,
            &f.text,
            egui::FontId::proportional(size),
            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), fade),
        );
        true
    });
}

/// Draw the VP name hovering above its city every frame — plain
/// thick-outlined white text over the city centre (no pip/seal — the city
/// itself is the marker). The label is cosmetic: it ignores fog (the city
/// is terrain, visible on the map) and never blocks input.
/// Layering: painted on `LayerId::background()` — the ONE PaintList
/// all egui 0.29 top-level panels share — and registered in the ui.rs chain
/// BEFORE draw_panels. Within a single PaintList shapes paint in insertion
/// order, so the command panel's frame deterministically covers/frosts the
/// name (slide under the sidebar like under the minimap, never pop out of
/// existence). Windows (Middle), hover cards (Foreground) and tooltips
/// (Tooltip) are all higher Orders and cover it the same way.
pub fn draw_vp_label(
    mut contexts: EguiContexts,
    label: Res<VpLabel>,
    state: Res<crate::state::TacticalState>,
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>, // Camera3d filter (render-scale blit adds a Camera2d)
    loc: Res<crate::locale::LocaleRes>,
    mut cached_center: Local<Option<(f32, f32)>>,
) {
    let Some((name, anchor)) = &label.0 else {
        return;
    };
    // try_ctx_mut: on the very first frame the egui context is not yet
    // initialized (a known egui first-frame hazard) — ctx_mut panics.
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    let Ok((camera, cam_tf)) = q_cam.get_single() else {
        return;
    };
    // Sit over the city centre: the urban blob may drift from the anchor
    // (nearest-first fill), so average the actual Urban hexes — ONCE, then
    // cache (the grid never changes terrain mid-battle).
    let (x, z) = match *cached_center {
        Some(c) => c,
        None => {
            let mut pts: Vec<(f32, f32)> = Vec::new();
            if let Some(grid) = &state.grid {
                for c in grid.iter_coords() {
                    if grid.cell(c).map(|c| c.terrain) == Some(tactical_core::Terrain::Urban) {
                        pts.push(c.to_world(crate::board::HEX_SIZE));
                    }
                }
            }
            let c = if pts.is_empty() {
                anchor.to_world(crate::board::HEX_SIZE)
            } else {
                let n = pts.len() as f32;
                (
                    pts.iter().map(|p| p.0).sum::<f32>() / n,
                    pts.iter().map(|p| p.1).sum::<f32>() / n,
                )
            };
            *cached_center = Some(c);
            c
        }
    };
    let w = Vec3::new(
        x,
        crate::board::hex_top_y(tactical_core::Terrain::Urban) + 1.6,
        z,
    );
    let Some(ndc) = camera.world_to_ndc(cam_tf, w) else {
        return;
    };
    let screen = ctx.screen_rect();
    let sx = (ndc.x * 0.5 + 0.5) * screen.width();
    let sy = (1.0 - (ndc.y * 0.5 + 0.5)) * screen.height();
    let pos = egui::pos2(sx, sy);
    if !screen.expand(60.0).contains(pos) {
        return;
    }
    // Paint onto the shared panel Background list; the ui.rs chain
    // runs this BEFORE draw_panels, so the sidebar paints over the name.
    let painter = ctx.layer_painter(egui::LayerId::background());
    // No pip/seal — the city itself is the marker (a separate VP icon reads
    // as clutter). The name sits at seal height over the city centre,
    // extra-thick 12-way black outline + white fill. CJK glyphs read larger
    // than Latin at the same point size — the Chinese label draws slightly
    // smaller (15 → 13).
    let font_size = if loc.language() == tactical_locale::Language::SimpChinese {
        13.0
    } else {
        15.0
    };
    let font = egui::FontId::proportional(font_size);
    let text_pos = pos;
    for (dx, dy) in [
        (-2.0, 0.0),
        (2.0, 0.0),
        (0.0, -2.0),
        (0.0, 2.0),
        (-1.5, -1.5),
        (1.5, -1.5),
        (-1.5, 1.5),
        (1.5, 1.5),
        (-2.0, -1.0),
        (2.0, -1.0),
        (-2.0, 1.0),
        (2.0, 1.0),
        (-1.0, -2.0),
        (1.0, -2.0),
        (-1.0, 2.0),
        (1.0, 2.0),
    ] {
        painter.text(
            text_pos + egui::vec2(dx, dy),
            egui::Align2::CENTER_CENTER,
            name,
            font.clone(),
            egui::Color32::from_rgba_unmultiplied(15, 15, 15, 240),
        );
    }
    painter.text(
        text_pos,
        egui::Align2::CENTER_CENTER,
        name,
        font,
        egui::Color32::WHITE,
    );
}
