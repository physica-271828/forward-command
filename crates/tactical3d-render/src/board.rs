//! Board rendering: one merged palette-textured mesh for all terrain hexes
//! (river strips baked in), ONE merged static mesh for all props, and
//! palette-only recoloring for fog of war and command highlights.
//!
//! Terrain is static for a battle, so the merged
//! board mesh (millions of verts on big provinces) is built ONCE; palette
//! slots are pinned to a fixed layout (3 river slots + 2 per hex), so a
//! fog/highlight recolor rewrites palette pixels in place instead of
//! rebuilding and re-uploading the whole mesh every time the hover hex
//! changes (the old per-frame full rebuild was the pan/orbit GPU spike).

use bevy::pbr::NotShadowCaster;
use bevy::prelude::*;
use tactical_combat::AttackTarget;
use tactical_core::command::{aura_radius_of, compute_command_links, CommandLink};
use tactical_core::fog::VisibilityState;
use tactical_core::grid::HexGrid;
use tactical_core::hex::{HexCoord, HexDirection};
use tactical_core::movement::next_step_progress;
use tactical_core::params::CombatParams;
use tactical_core::terrain::Terrain;
use tactical_core::unit::{BattalionUnit, Side};
use tactical_sync::BattlePhase;

use crate::game::{DivOrderPick, GameController};
use crate::mesh_build::{color3, hash01, hex_corners, hex_edge, scale_color, MeshBuilder};
use crate::models::SideColors;
use crate::state::{HighlightKind, TacticalState};

/// World size of one hex (circumradius) — 1 unit = 1 km (DESIGN §4 hex scale).
pub const HEX_SIZE: f32 = 1.0;
/// Horizontal shrink so adjacent prisms leave a readable grid gap.
pub const HEX_GAP: f32 = 0.94;

#[derive(Component)]
pub struct BoardMesh;

/// Terrain-flat prism top (legacy: synthetic/preview grids and transient
/// fx whose hex is not at hand). Battles render per-hex ELEVATION instead —
/// see [`hex_top_y_at`].
pub fn hex_top_y(t: Terrain) -> f32 {
    t.render_height()
}

/// Per-hex prism top height: the cell's own ELEVATION level, so
/// the picture matches the LOS ridge rule — a ridge the player SEES is a
/// ridge that blocks sight. Water stays flat on its bed; out-of-bounds and
/// off-grid fall back to 0.0.
pub fn hex_top_y_at(grid: &HexGrid, h: HexCoord) -> f32 {
    grid.cell(h)
        .map(|c| {
            if c.is_passable {
                Terrain::elevation_render_height(c.elevation)
            } else {
                0.0
            }
        })
        .unwrap_or(0.0)
}

/// Average board top height (terrain tops span 0.05–1.70 with per-hex
/// elevation; 0.4 ≈ plains). The cursor-picking ground plane lives at this
/// height — one shared constant for picking.rs and camera.rs (previously two
/// literals; known limitation: tall ridges shift hover by up to ~1 hex).
pub const GROUND_PLANE_Y: f32 = 0.4;

pub fn hex_world(h: HexCoord) -> Vec3 {
    let (x, z) = h.to_world(HEX_SIZE);
    Vec3::new(x, 0.0, z)
}

fn hex_color_jitter(h: HexCoord, rgb: [f32; 3]) -> [f32; 4] {
    let j = 0.92 + 0.16 * hash01(((h.q as u64) << 32) ^ (h.r as u64 & 0xffff_ffff));
    color3([rgb[0] * j, rgb[1] * j, rgb[2] * j])
}

/// Fog veil: an OVERCAST look, not darkness. The terrain keeps its hue,
/// muted toward its own luminance (desat) and pulled to a soft grey-day
/// brightness —
/// Revealed ≈ ⅘ light, Hidden ≈ ½ light, both leaning slightly cool.
/// Visible stays full colour; the vision boundary still reads at a glance.
const REVEALED_MUL: [f32; 3] = [0.78, 0.81, 0.87];
const REVEALED_DESAT: f32 = 0.30;
const HIDDEN_MUL: [f32; 3] = [0.48, 0.51, 0.58];
const HIDDEN_DESAT: f32 = 0.35;
/// Sky haze for the out-of-bounds "pseudo-transparency": the
/// board floats over nothing but the camera clear colour, so blending the
/// backdrop TOWARD that sky reads as semi-transparent distance haze with
/// zero pipeline changes. MUST match camera.rs's clear/DistanceFog colour
/// (0.53, 0.66, 0.82) — keep them in sync by hand.
const SKY_HAZE: [f32; 4] = [0.53, 0.66, 0.82, 1.0];
const BACKDROP_HAZE_BLEND: f32 = 0.55;

/// The fog "overcast" shadow: dim by `mul` per channel (slightly cool),
/// then mute the hue toward its own luminance by `desat`. Hue survives,
/// alpha untouched — unlike `blend`, which greys the terrain out.
fn shadow(c: [f32; 4], mul: [f32; 3], desat: f32) -> [f32; 4] {
    let mut m = [c[0] * mul[0], c[1] * mul[1], c[2] * mul[2]];
    let lum = (m[0] + m[1] + m[2]) / 3.0;
    for v in &mut m {
        *v += (lum - *v) * desat;
    }
    [m[0], m[1], m[2], c[3]]
}

/// Current top-face color of a hex: banded terrain color + jitter +
/// highlight blend + fog shadow. `elevation` feeds the mountain contour
/// bands.
pub fn board_color_of(
    state: &TacticalState,
    h: HexCoord,
    terrain: Terrain,
    elevation: i32,
) -> [f32; 4] {
    let mut color = hex_color_jitter(h, terrain.banded_color(elevation));
    if let Some(kind) = state.highlight_at(h) {
        let hc = match kind {
            HighlightKind::Move => [0.35, 0.85, 0.35, 1.0],
            HighlightKind::Assault => [0.90, 0.25, 0.20, 1.0],
            HighlightKind::Support => [0.30, 0.55, 0.95, 1.0],
            HighlightKind::Hover => [0.95, 0.90, 0.45, 1.0],
            // Burning red for the reported engagement hex.
            HighlightKind::Report => [1.0, 0.12, 0.08, 1.0],
            // No zone WASH — the thick border mesh (DeploymentBorder)
            // carries zone readability. A leftover Deployment mark is a
            // no-op (strength 0 below).
            HighlightKind::Deployment => [0.0, 0.0, 0.0, 1.0],
        };
        let strength = match kind {
            HighlightKind::Deployment => 0.0,
            HighlightKind::Report => 0.70,
            _ => 0.55,
        };
        color = blend(color, hc, strength);
    } else {
        match state.fog_state(h) {
            VisibilityState::Visible => {}
            VisibilityState::Revealed => color = shadow(color, REVEALED_MUL, REVEALED_DESAT),
            VisibilityState::Hidden => color = shadow(color, HIDDEN_MUL, HIDDEN_DESAT),
        }
    }
    color
}

/// Out-of-bounds backdrop (§6.14: passable but never the battlefield):
/// pseudo-transparent DISTANCE HAZE — the pixel's own banded terrain
/// colour blended toward the sky clear colour, so the surrounding world
/// fades into the distance instead of competing with the battlefield
/// (there is no ground plane under the board, only sky — the blend IS the
/// transparency). Supersedes the older mid-grey wash and covers
/// out-of-bounds WATER too (sea/lake outside the province fades the same
/// way). The fog states ride on top as the same shadow
/// multipliers; the hazed base keeps the backdrop's dark a touch lighter
/// and bluer than the battlefield dark, so the province border stays
/// readable in the dark (界外迷雾 vs 战场暗幕).
fn backdrop_color_of(
    state: &TacticalState,
    h: HexCoord,
    terrain: Terrain,
    elevation: i32,
) -> [f32; 4] {
    let foreign = blend(
        hex_color_jitter(h, terrain.banded_color(elevation)),
        SKY_HAZE,
        BACKDROP_HAZE_BLEND,
    );
    match state.fog_state(h) {
        VisibilityState::Visible => foreign,
        VisibilityState::Revealed => shadow(foreign, REVEALED_MUL, REVEALED_DESAT),
        VisibilityState::Hidden => shadow(foreign, HIDDEN_MUL, HIDDEN_DESAT),
    }
}

/// River band color per fog state — the same shadow multipliers as the
/// board, so a river reads as water-in-shadow, not grey tape. These live in
/// the board palette's first THREE fixed slots (see below); hex fog states
/// are baked at mesh build (the recolor fast path rewrites hex slots only —
/// river bands keep their build-time state, pre-existing limitation).
fn river_color(vis: VisibilityState) -> [f32; 4] {
    let water = [0.22, 0.45, 0.78, 1.0];
    match vis {
        VisibilityState::Visible => water,
        VisibilityState::Revealed => shadow(water, REVEALED_MUL, REVEALED_DESAT),
        VisibilityState::Hidden => shadow(water, HIDDEN_MUL, HIDDEN_DESAT),
    }
}

/// Fixed board palette layout: slots 0,1,2 = river colors
/// (visible/revealed/hidden); every in-rect hex then owns exactly TWO slots
/// — top face at `hex_top_slot`, side walls at +1. Because the layout does
/// not depend on colors, [`recolor_board_palette`] can rewrite palette
/// pixels in place while the mesh UVs stay valid.
const HEX_SLOT_BASE: u32 = 3;

fn hex_top_slot(width: usize, h: HexCoord) -> u32 {
    HEX_SLOT_BASE + 2 * (h.r as usize * width + h.q as usize) as u32
}

fn write_palette_px(data: &mut [u8], slot: usize, c: [f32; 4]) {
    debug_assert!(
        slot * 4 + 3 < data.len(),
        "palette slot {slot} out of range"
    );
    for (k, v) in c.iter().enumerate() {
        data[slot * 4 + k] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
}

/// Build the merged board mesh with current fog/highlight colors baked in.
/// Geometry is built ONCE per battle (terrain never changes mid-battle);
/// later color changes go through [`recolor_board_palette`].
pub fn build_board_mesh(state: &TacticalState) -> (Mesh, Image) {
    let grid = state.grid.as_ref().expect("grid must exist");
    let mut mb = MeshBuilder::new();

    // Fixed river slots FIRST so every later hex slot matches hex_top_slot.
    let river_slots = [
        mb.alloc_slot(river_color(VisibilityState::Visible)),
        mb.alloc_slot(river_color(VisibilityState::Revealed)),
        mb.alloc_slot(river_color(VisibilityState::Hidden)),
    ];
    debug_assert_eq!(river_slots, [0, 1, 2]);

    for h in grid.iter_coords() {
        let Some(cell) = grid.cell(h) else { continue };
        // Every sampled cell is drawn — in-bounds Water as a low deep-blue
        // prism, out-of-bounds cells (land AND water alike) as sky-hazed
        // backdrop (§6.14: passable, but never the battlefield) — so the
        // map reads as one continuous landmass/seascape fading into the
        // distance instead of the battlefield floating in skybox void.
        let center = hex_world(h);
        // Per-hex ELEVATION, not the terrain-flat height — the rendered
        // relief IS the LOS relief (ridge rule, §6.6).
        let top = Terrain::elevation_render_height(cell.elevation);
        let color = if !cell.out_of_bounds {
            board_color_of(state, h, cell.terrain, cell.elevation)
        } else {
            backdrop_color_of(state, h, cell.terrain, cell.elevation)
        };
        let top_slot = mb.alloc_slot(color);
        let side_slot = mb.alloc_slot(scale_color(color, 0.55));
        debug_assert_eq!(
            top_slot,
            hex_top_slot(grid.width, h),
            "palette layout drift"
        );
        mb.add_hex_prism_slot(center, HEX_SIZE * HEX_GAP, 0.0, top, top_slot, side_slot);

        // River edge strips (§4.2): blue band along each flagged edge.
        // Vertex order is chosen so the band faces UP (a→inward→b gives a
        // downward normal and gets culled).
        for (i, dir) in HexDirection::ALL.iter().enumerate() {
            if cell.river_edges & (1 << i) == 0 {
                continue;
            }
            let corners = hex_corners(center, HEX_SIZE * HEX_GAP, top + 0.015);
            let (a, b) = hex_edge(&corners, *dir as usize);
            let mid = (a + b) * 0.5;
            let inward = (Vec3::new(center.x, top + 0.015, center.z) - mid).normalize() * 0.16;
            let slot = match state.fog_state(h) {
                VisibilityState::Visible => river_slots[0],
                VisibilityState::Revealed => river_slots[1],
                VisibilityState::Hidden => river_slots[2],
            };
            mb.add_quad_slot(a, a + inward, b + inward, b, Vec3::Y, slot);
        }
    }

    mb.build()
}

/// Recolor the board palette IN PLACE after a fog/highlight change — the
/// mesh, UVs, entity and material are all untouched. This is the hot path
/// for hover moves and fog updates: a few hundred KB of texture upload
/// instead of a full multi-megabyte mesh rebuild + re-upload per frame.
pub fn recolor_board_palette(state: &TacticalState, image: &mut Image) {
    let Some(grid) = state.grid.as_ref() else {
        return;
    };
    let data = &mut image.data;
    for h in grid.iter_coords() {
        let Some(cell) = grid.cell(h) else { continue };
        let color = if !cell.out_of_bounds {
            board_color_of(state, h, cell.terrain, cell.elevation)
        } else {
            backdrop_color_of(state, h, cell.terrain, cell.elevation)
        };
        let slot = hex_top_slot(grid.width, h) as usize;
        write_palette_px(data, slot, color);
        write_palette_px(data, slot + 1, scale_color(color, 0.55));
    }
}

/// Handles of the persistent board entity's mesh + palette image: color
/// rebuilds replace these assets IN PLACE instead of leaking a fresh
/// mesh/image/material set per rebuild (previously one full set per hover
/// change, and the entity respawned every time). The MATERIAL handle rides
/// along because Bevy 0.15 bakes the texture view into the material's bind
/// group at prepare time and does NOT re-prepare the material when only the
/// image asset changes — an in-place palette rewrite alone therefore kept
/// rendering the STALE texture forever (fog veil and hover highlights
/// silently dead after the palette refactor). Touching the material (a
/// Modified event) forces the re-bind.
#[derive(Resource, Default)]
pub struct BoardAssets {
    pub mesh: Option<Handle<Mesh>>,
    pub image: Option<Handle<Image>>,
    pub material: Option<Handle<StandardMaterial>>,
}

impl BoardAssets {
    /// Force the board material to re-bind the (freshly rewritten) palette
    /// texture — see the struct doc. Reassigning the same handle
    /// is a real write, so the asset is marked Modified either way.
    fn touch_material(
        materials: &mut Assets<StandardMaterial>,
        material: &Option<Handle<StandardMaterial>>,
        image: &Handle<Image>,
    ) {
        if let Some(mat_h) = material {
            if let Some(mat) = materials.get_mut(mat_h) {
                mat.base_color_texture = Some(image.clone());
            }
        }
    }
}

/// Rebuild the board mesh entity when colors change (fog/highlights).
/// Large boards reach thousands of hexes; the rebuild updates the existing
/// assets in place so neither the entity nor the handles churn. Colors-only
/// updates (the common case — hover, fog) take the palette fast path and
/// never touch the mesh; `board_mesh_dirty` (snapshot restore) still
/// triggers a full in-place rebuild.
pub fn rebuild_board_mesh(
    mut commands: Commands,
    mut state: ResMut<TacticalState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut board_assets: ResMut<BoardAssets>,
    q_board: Query<Entity, With<BoardMesh>>,
) {
    if !state.board_colors_dirty && !state.board_mesh_dirty {
        return;
    }
    let mesh_dirty = state.board_mesh_dirty;
    state.board_colors_dirty = false;
    state.board_mesh_dirty = false;
    if state.grid.is_none() {
        return;
    }
    // Fast path: colors only — rewrite palette pixels in place (texture
    // re-upload of a few hundred KB); geometry stays on the GPU untouched.
    // The material touch is NOT optional: without it the bind group keeps
    // sampling the pre-rewrite texture.
    if !mesh_dirty {
        if let Some(img_h) = &board_assets.image {
            if let Some(image) = images.get_mut(img_h) {
                recolor_board_palette(&state, image);
                let img_h = img_h.clone();
                BoardAssets::touch_material(&mut materials, &board_assets.material, &img_h);
                return;
            }
        }
        // Handles lost (never built): fall through to a full build.
    }
    let (mesh, image) = build_board_mesh(&state);
    match (board_assets.mesh.clone(), board_assets.image.clone()) {
        (Some(mesh_h), Some(img_h)) => {
            // In-place replace: the entity stays untouched; the material is
            // still re-touched so the bind group picks up the new palette
            // texture.
            meshes.insert(mesh_h.id(), mesh);
            images.insert(img_h.id(), image);
            BoardAssets::touch_material(&mut materials, &board_assets.material, &img_h);
        }
        _ => {
            // First build (or lost handles): spawn the persistent entity and
            // remember its handles for all future rebuilds.
            for e in q_board.iter() {
                commands.entity(e).despawn();
            }
            let img_h = images.add(image);
            let mat = board_material(&mut materials, img_h.clone());
            let mesh_h = meshes.add(mesh);
            commands.spawn((
                Mesh3d(mesh_h.clone()),
                MeshMaterial3d(mat.clone()),
                Transform::default(),
                BoardMesh,
                // The board is near-flat (height steps 0.05–1.4): its
                // self-shadowing is invisible, yet drawing the merged mesh
                // into every shadow cascade was the shadow pass's dominant
                // cost. It still RECEIVES unit/prop shadows.
                NotShadowCaster,
            ));
            board_assets.mesh = Some(mesh_h);
            board_assets.image = Some(img_h);
            board_assets.material = Some(mat);
        }
    }
}

fn blend(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] * (1.0 - t) + b[0] * t,
        a[1] * (1.0 - t) + b[1] * t,
        a[2] * (1.0 - t) + b[2] * t,
        1.0,
    ]
}

// ---------------------------------------------------------------------------
// Deployment zone border
// ---------------------------------------------------------------------------

#[derive(Component)]
pub struct DeploymentBorder;

/// Route arrows for standing move orders (§6.2).
#[derive(Component)]
pub struct RouteArrows;

/// Continuous HOI4-style route ribbon for the **selected** player-side
/// unit's standing move order (enemy orders are never shown; unselected
/// units' ribbons are hidden too): black medium border, pure
/// red SOLID interior (no chevron inlays), big arrowhead. Returns TWO
/// meshes — (border, interior) — because a single material's emissive would
/// tint the black border red; they spawn as two entities with the border
/// slightly lower. `clip` (0..=1) limits the ribbon to that fraction of
/// total length — the grow-from-unit animation on issue. Route-ribbon mesh:
/// the standing move order of the selected unit — or, for a selected HQ
/// whose division has a standing division order, EVERY battalion of that
/// division (`div_filter = Some(division)`):
/// the player reviews the whole division's pre-orders at a glance before
/// End Turn.
pub fn build_route_arrows_mesh(
    state: &TacticalState,
    clip: f32,
    div_filter: Option<&str>,
    params: &CombatParams,
) -> ((Mesh, Image), (Mesh, Image)) {
    const BLACK: [f32; 4] = [0.02, 0.02, 0.02, 1.0];
    const RED: [f32; 4] = [0.85, 0.10, 0.07, 1.0];
    let grid = state.grid.as_ref().expect("grid must exist");
    let top_of = |h: HexCoord, lift: f32| hex_top_y_at(grid, h) + lift;
    let center_of = |h: HexCoord, lift: f32| {
        let (x, z) = h.to_world(HEX_SIZE);
        Vec3::new(x, top_of(h, lift), z)
    };
    let mut mbs = [MeshBuilder::new(), MeshBuilder::new()];
    const W: f32 = 0.11; // red interior half-width
    const BORDER: f32 = 0.065; // black border extra width
    let ribbon_of = |u: &BattalionUnit| -> bool {
        u.side == state.player_side
            && u.is_combat_effective()
            && u.move_order.is_some()
            && match div_filter {
                // Selected HQ with a standing division order → the whole
                // division's march ribbons.
                Some(div) => u.division == div,
                // Otherwise only the selected unit's own ribbon.
                None => state.selected_unit == Some(u.id),
            }
    };
    for u in state.units.iter().filter(|u| ribbon_of(u)) {
        let order = u.move_order.as_ref().unwrap();
        // Draw each layer (black border underneath, red on top) as its own
        // clipped polyline pass into its own builder. Only a tiny y-lift
        // separates them: too much and the viewing angle hides the far-side
        // border behind the red.
        for (pass, (half_w, lift, col)) in [(W + BORDER, 0.052, BLACK), (W, 0.058, RED)]
            .into_iter()
            .enumerate()
        {
            let mb = &mut mbs[pass];
            let mut pts = vec![center_of(u.position, lift)];
            for &h in &order.path {
                pts.push(center_of(h, lift));
            }
            let segs: Vec<f32> = pts.windows(2).map(|w| (w[1] - w[0]).length()).collect();
            let total: f32 = segs.iter().sum();
            if total < 1e-4 {
                continue;
            }
            let limit = total * clip.clamp(0.0, 1.0);
            let mut travelled = 0.0;
            for (i, len) in segs.iter().enumerate() {
                if travelled >= limit {
                    break;
                }
                let (a, b) = (pts[i], pts[i + 1]);
                let fwd = (b - a) / *len;
                let side = Vec3::new(-fwd.z, 0.0, fwd.x).normalize_or(Vec3::X);
                let draw_len = (limit - travelled).min(*len);
                let end = a + fwd * draw_len;
                mb.add_quad(
                    a + side * half_w,
                    end + side * half_w,
                    end - side * half_w,
                    a - side * half_w,
                    Vec3::Y,
                    col,
                );
                if i > 0 {
                    mb.add_tri(
                        a + fwd * half_w,
                        a + side * half_w,
                        a - side * half_w,
                        Vec3::Y,
                        col,
                    );
                    mb.add_tri(
                        a - fwd * half_w,
                        a - side * half_w,
                        a + side * half_w,
                        Vec3::Y,
                        col,
                    );
                }
                travelled += len;
            }
            // Arrowhead at the destination once fully grown (the bigger
            // border pass underneath reads as its outline).
            if clip >= 1.0 - 1e-6 && pts.len() >= 2 {
                let d = *pts.last().unwrap();
                let p = pts[pts.len() - 2];
                let fwd = (d - p).normalize_or(Vec3::X);
                let side = Vec3::new(-fwd.z, 0.0, fwd.x).normalize_or(Vec3::X);
                let tip_l = 0.30 * (half_w / W);
                let back_w = 0.24 * (half_w / W);
                mb.add_tri(
                    d + fwd * tip_l,
                    d - fwd * 0.05 + side * back_w,
                    d - fwd * 0.05 - side * back_w,
                    Vec3::Y,
                    col,
                );
            }
        }
        // HOI4-style per-step progress: a light fill growing
        // from the unit toward the next hex — it reaches the hex exactly
        // when the marching step completes (invested hours / step cost,
        // §6.2). Interior pass only, centred and a hair above the red so
        // it reads as a fill level without z-fighting. Shown only on the
        // fully grown ribbon (the 0.45 s grow is a transient).
        if clip >= 1.0 - 1e-6 {
            if let Some(frac) = next_step_progress(grid, &state.units, u, params) {
                if frac > 0.02 {
                    // Light pink — reads clearly against the red interior.
                    const LIGHT: [f32; 4] = [1.0, 0.70, 0.78, 1.0];
                    let a = center_of(u.position, 0.058 + 0.006);
                    let b = center_of(order.path[0], 0.058 + 0.006);
                    let seg = (b - a).length();
                    if seg > 1e-4 {
                        let end = a + (b - a) * frac;
                        let fwd = (b - a) / seg;
                        let side = Vec3::new(-fwd.z, 0.0, fwd.x).normalize_or(Vec3::X);
                        let hw = W * 0.75;
                        mbs[1].add_quad(
                            a + side * hw,
                            end + side * hw,
                            end - side * hw,
                            a - side * hw,
                            Vec3::Y,
                            LIGHT,
                        );
                    }
                }
            }
        }
    }
    let [b0, b1] = mbs;
    (b0.build(), b1.build())
}

/// Attack-order arrows: one lane per registered order of EVERY
/// player-side unit (not just the selected — the player reviews all lanes
/// before End Turn). Same black-border/two-mesh pattern as the route
/// ribbon; a diamond marks the target hex. Drawn slightly higher than the
/// move ribbon to avoid z-fighting where they overlap.
///
/// Fire-mission arcs TELEGRAPH their resolution — the interior colour
/// names the crest bucket (§6.6):
/// EXPOSED crest (×1.5) burns magenta-red, neutral stays bright red,
/// DEFILADE (×0.5) warns amber; AREA missions (weighted zone — the F-key
/// barrage; rockets always) run DASHED and dimmed, and their whole 7-hex
/// blast zone (target + 6 neighbours) is outlined in the same dim hue.
/// Assault and direct-fire lanes keep the plain bright red (their
/// geometry differs).
pub fn build_attack_arrows_mesh(
    state: &TacticalState,
    params: &CombatParams,
) -> ((Mesh, Image), (Mesh, Image)) {
    const BLACK: [f32; 4] = [0.02, 0.02, 0.02, 1.0];
    const BRIGHT: [f32; 4] = [1.0, 0.25, 0.12, 1.0];
    /// Exposed crest ×1.5 (§6.6): magenta-red — the death line.
    const CREST_EXPOSED: [f32; 4] = [0.85, 0.15, 0.45, 1.0];
    /// Defilade ×0.5: amber — the ridge shoulder eats the shell.
    const CREST_DEFILADE: [f32; 4] = [0.95, 0.65, 0.10, 1.0];
    let grid = state.grid.as_ref().expect("grid must exist");
    let top_of = |h: HexCoord, lift: f32| hex_top_y_at(grid, h) + lift;
    let center_of = |h: HexCoord, lift: f32| {
        let (x, z) = h.to_world(HEX_SIZE);
        Vec3::new(x, top_of(h, lift), z)
    };
    let mut mbs = [MeshBuilder::new(), MeshBuilder::new()];
    const W: f32 = 0.11; // interior half-width (matches the route ribbon)
    const BORDER: f32 = 0.065;
    for o in &state.attack_orders {
        let Some(u) = state.unit_by_id(o.attacker) else {
            continue;
        };
        if u.side != state.player_side || !u.is_combat_effective() {
            continue;
        }
        let (target_hex, interior, area) = match &o.target {
            AttackTarget::Assault(t) | AttackTarget::DirectFire(t) => {
                let Some(tu) = state.unit_by_id(*t) else {
                    continue;
                };
                (tu.position, BRIGHT, false)
            }
            AttackTarget::FireMission { hex, precise } => {
                let crest = tactical_core::los::indirect_crest_mult(
                    grid,
                    u.position,
                    *hex,
                    params.exposed_crest_mult,
                    params.defilade_mult,
                );
                let base = if crest > 1.0 {
                    CREST_EXPOSED
                } else if crest < 1.0 {
                    CREST_DEFILADE
                } else {
                    BRIGHT
                };
                // Area fire (F-key barrage; rockets always) — dashed + dim.
                let area = !precise || u.is_rocket();
                (
                    *hex,
                    if area { scale_color(base, 0.65) } else { base },
                    area,
                )
            }
        };
        let dist_hex = u.position.distance(target_hex);
        for (pass, (half_w, lift, col)) in [(W + BORDER, 0.072, BLACK), (W, 0.078, interior)]
            .into_iter()
            .enumerate()
        {
            let mb = &mut mbs[pass];
            let a = center_of(u.position, lift);
            let d = center_of(target_hex, lift);
            let v = d - a;
            let len = v.length();
            if len < 1e-4 {
                continue;
            }
            let fwd = v / len;
            let side = Vec3::new(-fwd.z, 0.0, fwd.x).normalize_or(Vec3::X);
            if dist_hex > 1 {
                // Parabolic ballistic arc: ranged fire arches above the
                // board while assault lanes stay flat —
                // instantly readable who is firing at whom.
                let arc_h = (0.7 + len * 0.22).min(2.8);
                const SEGS: usize = 14;
                let pt = |t: f32| a + v * t + Vec3::Y * (4.0 * arc_h * t * (1.0 - t));
                let mut prev = pt(0.0);
                for i in 1..=SEGS {
                    let cur = pt(i as f32 / SEGS as f32);
                    // Area missions run dashed: 2 on / 2 off.
                    if area && i % 4 >= 2 {
                        prev = cur;
                        continue;
                    }
                    let seg = cur - prev;
                    let sl = seg.length();
                    if sl > 1e-4 {
                        let sf = seg / sl;
                        let ss = Vec3::new(-sf.z, 0.0, sf.x).normalize_or(Vec3::X);
                        mb.add_quad(
                            prev + ss * half_w,
                            cur + ss * half_w,
                            cur - ss * half_w,
                            prev - ss * half_w,
                            Vec3::Y,
                            col,
                        );
                    }
                    prev = cur;
                }
                // Arrowhead along the terminal tangent.
                let tv = (pt(1.0) - pt(0.93)).normalize_or(Vec3::X);
                let ts = Vec3::new(-tv.z, 0.0, tv.x).normalize_or(Vec3::X);
                let tip_l = 0.30 * (half_w / W);
                let back_w = 0.24 * (half_w / W);
                mb.add_tri(
                    d + tv * tip_l,
                    d - tv * 0.05 + ts * back_w,
                    d - tv * 0.05 - ts * back_w,
                    Vec3::Y,
                    col,
                );
            } else {
                // Adjacent assault: the flat lane bridges the gap between
                // the two pieces — it starts past the attacker's base and
                // the arrowhead stops short of the defender's base so the
                // lane never clips through either model.
                let start = a + fwd * 0.62;
                let end = d - fwd * 0.58;
                mb.add_quad(
                    start + side * half_w,
                    end + side * half_w,
                    end - side * half_w,
                    start - side * half_w,
                    Vec3::Y,
                    col,
                );
                let tip_l = 0.30 * (half_w / W);
                let back_w = 0.24 * (half_w / W);
                mb.add_tri(
                    end + fwd * tip_l,
                    end - fwd * 0.02 + side * back_w,
                    end - fwd * 0.02 - side * back_w,
                    Vec3::Y,
                    col,
                );
            }
            // Target diamond.
            let rw = half_w * 1.3;
            mb.add_quad(
                d + fwd * rw,
                d + side * rw,
                d - fwd * rw,
                d - side * rw,
                Vec3::Y,
                col,
            );
            // An area mission also outlines its whole 7-hex blast zone
            // (target + 6 neighbours) in the same dim hue — the
            // player sees what the weighted zone (4/10-1/10) covers before
            // End Turn — friends included.
            if area {
                let ow = if pass == 0 { 0.070 } else { 0.045 };
                for zh in std::iter::once(target_hex).chain(target_hex.neighbors()) {
                    if !grid.in_bounds(zh) {
                        continue;
                    }
                    let zc = center_of(zh, lift);
                    let corners = hex_corners(zc, HEX_SIZE * 0.92, zc.y);
                    for k in 0..6 {
                        let (za, zb) = (corners[k], corners[(k + 1) % 6]);
                        let ze = zb - za;
                        let zs = Vec3::new(-ze.z, 0.0, ze.x).normalize_or(Vec3::X);
                        mb.add_quad(
                            za + zs * ow,
                            zb + zs * ow,
                            zb - zs * ow,
                            za - zs * ow,
                            Vec3::Y,
                            col,
                        );
                    }
                }
            }
        }
    }
    let [b0, b1] = mbs;
    (b0.build(), b1.build())
}

/// Command-overlay line meshes: solid and dashed lanes are separate mesh
/// pairs so the out-of-reach dashed lanes can be thinner and more
/// transparent than the in-command solid ones.
pub struct CommandLineMeshes {
    pub solid: ((Mesh, Image), (Mesh, Image)),
    pub dashed: ((Mesh, Image), (Mesh, Image)),
    pub any_solid: bool,
    pub any_dashed: bool,
}

/// Chain-of-command lanes (§6.13) for the SELECTED unit's division — dim
/// golden lines from the commanding HQ to a battalion, Steel Division
/// style. Selection modes:
/// - HQ selected → every lane of the division;
/// - plain battalion → only its own lane (to the HQ).
/// Solid = in command, DASHED = out of reach (thinner, shorter period).
pub fn build_command_lines_mesh(
    state: &TacticalState,
    links: &[CommandLink],
    division: &str,
    sel_id: usize,
    sel_hq: bool,
) -> CommandLineMeshes {
    const BLACK: [f32; 4] = [0.02, 0.02, 0.02, 1.0];
    const GOLD: [f32; 4] = [0.62, 0.52, 0.10, 1.0]; // darker gold
    let grid = state.grid.as_ref().expect("grid must exist");
    let top_of = |h: HexCoord, lift: f32| hex_top_y_at(grid, h) + lift;
    let center_of = |h: HexCoord, lift: f32| {
        let (x, z) = h.to_world(HEX_SIZE);
        Vec3::new(x, top_of(h, lift), z)
    };
    // Line source for Direct/OutOfRange: the division's commanding HQ (a
    // destroyed or undeployed HQ draws nothing; NoHq units get no line).
    let hq = state.units.iter().find(|u| {
        u.is_hq()
            && u.is_combat_effective()
            && !u.undeployed
            && u.side == state.player_side
            && u.division == division
    });
    const W: f32 = 0.034;
    const BORDER: f32 = 0.018;
    const DASH_W: f32 = 0.020;
    const DASH_BORDER: f32 = 0.012;
    // [solid border, solid interior, dashed border, dashed interior]
    let mut mbs = [
        MeshBuilder::new(),
        MeshBuilder::new(),
        MeshBuilder::new(),
        MeshBuilder::new(),
    ];
    let mut any = [false, false];
    for (i, u) in state.units.iter().enumerate() {
        if u.is_hq()
            || u.division != division
            || u.side != state.player_side
            || !u.is_combat_effective()
            || u.undeployed
        {
            continue;
        }
        // Selection-mode filter: an HQ selection draws the whole division;
        // otherwise only the selected battalion's own lane.
        if !sel_hq && u.id != sel_id {
            continue;
        }
        let (src, dashed) = match links[i] {
            CommandLink::Direct => match hq {
                Some(h) => (h.position, false),
                None => continue,
            },
            CommandLink::OutOfRange => match hq {
                Some(h) => (h.position, true),
                None => continue,
            },
            CommandLink::NoHq => continue,
        };
        let (half_w, border, base) = if dashed {
            (DASH_W, DASH_BORDER, 2usize)
        } else {
            (W, BORDER, 0usize)
        };
        for (pass, (hw, lift, col)) in [(half_w + border, 0.055, BLACK), (half_w, 0.062, GOLD)]
            .into_iter()
            .enumerate()
        {
            let mb = &mut mbs[base + pass];
            let a = center_of(src, lift);
            let d = center_of(u.position, lift);
            let v = d - a;
            let len = v.length();
            if len < 1e-4 {
                continue;
            }
            any[base / 2] = true;
            let fwd = v / len;
            let side = Vec3::new(-fwd.z, 0.0, fwd.x).normalize_or(Vec3::X);
            let start = a + fwd * 0.45;
            let end = d - fwd * 0.45;
            if dashed {
                // Short-period broken line: 2 on / 2 off, ~0.22 world units.
                let n = (((end - start).length() / 0.22).ceil() as i32).max(1);
                let mut prev = start;
                for k in 1..=n {
                    let cur = start + (end - start) * (k as f32 / n as f32);
                    if k % 4 < 2 {
                        mb.add_quad(
                            prev + side * hw,
                            cur + side * hw,
                            cur - side * hw,
                            prev - side * hw,
                            Vec3::Y,
                            col,
                        );
                    }
                    prev = cur;
                }
            } else {
                mb.add_quad(
                    start + side * hw,
                    end + side * hw,
                    end - side * hw,
                    start - side * hw,
                    Vec3::Y,
                    col,
                );
            }
        }
    }
    let [s0, s1, d0, d1] = mbs;
    CommandLineMeshes {
        solid: (s0.build(), s1.build()),
        dashed: (d0.build(), d1.build()),
        any_solid: any[0],
        any_dashed: any[1],
    }
}

/// Thin, dim ring for the command overlay: a slimmer, shorter-dash sibling
/// of [`build_range_ring_mesh`] so the HQ aura ring sits UNDER the
/// attack-range rings visually. Rides terrain the same way.
pub fn build_command_ring_mesh(
    state: &TacticalState,
    center: HexCoord,
    radius: i32,
    color: [f32; 4],
    dashed: bool,
) -> (Mesh, Image) {
    const SEGMENTS: usize = 96;
    const HALF_W: f32 = 0.032;
    let (cx, cz) = center.to_world(HEX_SIZE);
    let radius = radius as f32 * HEX_SIZE * 3.0_f32.sqrt();
    let ground_y = |x: f32, z: f32| {
        state
            .grid
            .as_ref()
            .and_then(|g| g.cell(HexCoord::from_world(x, z, HEX_SIZE)))
            .map(|c| {
                if c.is_passable {
                    Terrain::elevation_render_height(c.elevation)
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0)
            + 0.05
    };
    let mut mb = MeshBuilder::new();
    let mut prev: Option<(Vec3, Vec3)> = None;
    for i in 0..=SEGMENTS {
        let a = (i % SEGMENTS) as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        let (dx, dz) = (a.cos(), a.sin());
        let y = ground_y(cx + dx * radius, cz + dz * radius);
        let outer = Vec3::new(cx + dx * (radius + HALF_W), y, cz + dz * (radius + HALF_W));
        let inner = Vec3::new(cx + dx * (radius - HALF_W), y, cz + dz * (radius - HALF_W));
        if let Some((po, pi)) = prev {
            // Short-period dash: 2 on / 2 off (the range ring uses 4/2).
            if !dashed || i % 4 < 2 {
                mb.add_quad(po, pi, inner, outer, Vec3::Y, color);
            }
        }
        prev = Some((outer, inner));
    }
    mb.build()
}

/// Command-overlay entities: line mesh pairs plus one ring per influence
/// circle — all private assets, reclaimed via [`despawn_visuals`].
#[derive(Component)]
pub struct CommandLines;

/// Spawn one command-overlay mesh entity with a uniform alpha: vertex
/// colors stay opaque, the transparency lives on the material so
/// solid and dashed lanes can differ. No emissive — it ignores alpha and
/// would defeat the fade.
fn spawn_overlay_mesh(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    images: &mut Assets<Image>,
    materials: &mut Assets<StandardMaterial>,
    mesh: Mesh,
    image: Image,
    alpha: f32,
) {
    let mat = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 1.0, 1.0, alpha),
        base_color_texture: Some(images.add(image)),
        perceptual_roughness: 0.6,
        alpha_mode: AlphaMode::Blend,
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(mesh)),
        MeshMaterial3d(mat),
        Transform::default(),
        CommandLines,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
}

/// Rebuild the chain-of-command overlay on selection change or unit
/// changes (`units_dirty`, read but NOT consumed — the units pipeline owns
/// that flag). Selection modes:
/// - HQ selected → the aura ring + every division lane;
/// - plain battalion → its own lane only (no ring).
#[allow(clippy::too_many_arguments)]
pub fn sync_command_lines(
    mut commands: Commands,
    state: Res<TacticalState>,
    game: Option<Res<GameController>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    q_lines: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>), With<CommandLines>>,
    mut last_selected: Local<Option<usize>>,
) {
    let sel_changed = state.selected_unit != *last_selected;
    if sel_changed {
        *last_selected = state.selected_unit;
    }
    if !sel_changed && !state.units_dirty {
        return;
    }
    despawn_visuals(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        q_lines
            .iter()
            .map(|(e, m, mat)| (e, m.0.clone(), mat.0.clone())),
    );
    let (Some(game), Some(_)) = (game.as_ref(), state.grid.as_ref()) else {
        return;
    };
    let Some(sel) = state.selected_unit.and_then(|id| state.unit_by_id(id)) else {
        return;
    };
    if sel.side != state.player_side || sel.division.is_empty() {
        return;
    }
    let division = sel.division.clone();
    let sel_id = sel.id;
    let sel_hq = sel.is_hq();
    let params = game.combat.params();
    let links = compute_command_links(&state.units, params);
    const GOLD: [f32; 4] = [0.62, 0.52, 0.10, 1.0];
    let m = build_command_lines_mesh(&state, &links, &division, sel_id, sel_hq);
    if m.any_solid {
        let ((b_mesh, b_img), (i_mesh, i_img)) = m.solid;
        spawn_overlay_mesh(
            &mut commands,
            &mut meshes,
            &mut images,
            &mut materials,
            b_mesh,
            b_img,
            0.40,
        );
        spawn_overlay_mesh(
            &mut commands,
            &mut meshes,
            &mut images,
            &mut materials,
            i_mesh,
            i_img,
            0.62,
        );
    }
    if m.any_dashed {
        let ((b_mesh, b_img), (i_mesh, i_img)) = m.dashed;
        spawn_overlay_mesh(
            &mut commands,
            &mut meshes,
            &mut images,
            &mut materials,
            b_mesh,
            b_img,
            0.22,
        );
        spawn_overlay_mesh(
            &mut commands,
            &mut meshes,
            &mut images,
            &mut materials,
            i_mesh,
            i_img,
            0.38,
        );
    }
    // The HQ aura ring is an HQ-selection exclusive; its radius reflects a
    // signal company riding the HQ (§6.13: 3 km base → 6 km with signal).
    if sel_hq {
        if let Some(hq) = state.units.iter().find(|u| {
            u.is_hq()
                && u.is_combat_effective()
                && !u.undeployed
                && u.side == state.player_side
                && u.division == division
        }) {
            let (mesh, image) = build_command_ring_mesh(
                &state,
                hq.position,
                aura_radius_of(hq, params),
                GOLD,
                false,
            );
            spawn_overlay_mesh(
                &mut commands,
                &mut meshes,
                &mut images,
                &mut materials,
                mesh,
                image,
                0.50,
            );
        }
    }
}

/// Division-order marker overlay: one thick marching-ants line per active
/// division order — from the division HQ (or the division
/// centroid when the HQ is gone) to the order's target — plus the target
/// marker (gold hex outline for Seize, red ring for Engage). The dashes
/// FLOW toward the target (`phase` shifts them along the line), Steel
/// Division style. Deliberately thicker than the chain-of-command lanes
/// (≈2× width) but translucent, so the map and the units stay readable.
#[derive(Component)]
pub struct DivOrderMarker;

/// Everything the division-order overlay needs in one pass:
/// two vertex-colored meshes — a dark border pass + a gold core pass —
/// or `None` when no order has a drawable target (Advance draws nothing:
/// its goal is directional doctrine, not a fixed point).
fn build_div_order_marker_mesh(
    state: &TacticalState,
    game: &GameController,
    phase: f32,
) -> Option<(Mesh, Mesh)> {
    const BLACK: [f32; 4] = [0.02, 0.02, 0.02, 1.0];
    const GOLD: [f32; 4] = FLAG_GOLD; // [0.85, 0.70, 0.36, 1.0]
    const RED: [f32; 4] = [0.92, 0.28, 0.18, 1.0];
    const W: f32 = 0.062; // ≈2× the command-lane width (0.034)
    const BORDER: f32 = 0.030;
    let grid = state.grid.as_ref()?;
    let top_of = |h: HexCoord, lift: f32| hex_top_y_at(grid, h) + lift;
    let center_of = |h: HexCoord, lift: f32| {
        let (x, z) = h.to_world(HEX_SIZE);
        Vec3::new(x, top_of(h, lift), z)
    };
    // Marching-ants dash pass: 0.5 on / 0.35 off, phase-shifted forward.
    let dash_pass =
        |mb: &mut MeshBuilder, src: Vec3, dst: Vec3, offset: f32, half_w: f32, col: [f32; 4]| {
            let v = dst - src;
            let len = v.length();
            if len < 1e-3 {
                return;
            }
            let fwd = v / len;
            let side = Vec3::new(-fwd.z, 0.0, fwd.x).normalize_or(Vec3::X);
            let period = 0.85_f32;
            let on = 0.5_f32;
            let mut s = -period + offset;
            while s < len {
                let a = s.max(0.0);
                let b = (s + on).min(len);
                if b > a {
                    let p0 = src + fwd * a;
                    let p1 = src + fwd * b;
                    mb.add_quad(
                        p0 + side * half_w,
                        p1 + side * half_w,
                        p1 - side * half_w,
                        p0 - side * half_w,
                        Vec3::Y,
                        col,
                    );
                }
                s += period;
            }
        };

    let mut border_mb = MeshBuilder::new();
    let mut core_mb = MeshBuilder::new();
    let mut any = false;
    for (division, order) in &game.div_orders {
        let target = match order.pick {
            DivOrderPick::Advance => continue, // directional doctrine, no point
            DivOrderPick::Seize(hex) => hex,
            DivOrderPick::Engage { .. } => order.engage_last_pos.unwrap_or(HexCoord::ZERO),
        };
        // Source: the commanding HQ; once it is gone, the division's
        // centroid (the order outlives the HQ by design).
        let src = state
            .units
            .iter()
            .find(|u| {
                u.is_hq()
                    && u.is_combat_effective()
                    && !u.undeployed
                    && u.side == state.player_side
                    && u.division == *division
            })
            .map(|u| u.position)
            .or_else(|| {
                let members: Vec<&BattalionUnit> = state
                    .units
                    .iter()
                    .filter(|u| {
                        u.side == state.player_side
                            && u.division == *division
                            && u.is_combat_effective()
                    })
                    .collect();
                if members.is_empty() {
                    return None;
                }
                let (sq, sr) = members.iter().fold((0i64, 0i64), |(a, b), u| {
                    (a + u.position.q as i64, b + u.position.r as i64)
                });
                let n = members.len() as f32;
                Some(HexCoord::new(
                    (sq as f32 / n).round() as i32,
                    (sr as f32 / n).round() as i32,
                ))
            });
        let Some(src) = src else { continue };
        any = true;
        let offset = phase * 0.85;
        let a = center_of(src, 0.075);
        let d = center_of(target, 0.075);
        dash_pass(&mut border_mb, a, d, offset, W + BORDER, BLACK);
        dash_pass(&mut core_mb, a, d, offset, W, GOLD);
        // Target marker: Seize = gold hex outline; Engage = red ring at the
        // last known position (fog-honest).
        let (cx, cz) = target.to_world(HEX_SIZE);
        let y = top_of(target, 0.09);
        match order.pick {
            DivOrderPick::Seize(_) => {
                let corners = hex_corners(Vec3::new(cx, y, cz), HEX_SIZE * 1.02, y);
                for k in 0..6 {
                    let (a, b) = (corners[k], corners[(k + 1) % 6]);
                    let e = b - a;
                    let side = Vec3::new(-e.z, 0.0, e.x).normalize_or(Vec3::X);
                    core_mb.add_quad(
                        a + side * 0.05,
                        b + side * 0.05,
                        b - side * 0.05,
                        a - side * 0.05,
                        Vec3::Y,
                        GOLD,
                    );
                }
            }
            DivOrderPick::Engage { .. } => {
                let radius = 1.05;
                const SEGMENTS: usize = 64;
                let mut prev: Option<(Vec3, Vec3)> = None;
                for i in 0..=SEGMENTS {
                    let t = (i % SEGMENTS) as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
                    let (dx, dz) = (t.cos(), t.sin());
                    let outer =
                        Vec3::new(cx + dx * (radius + 0.045), y, cz + dz * (radius + 0.045));
                    let inner =
                        Vec3::new(cx + dx * (radius - 0.045), y, cz + dz * (radius - 0.045));
                    if let Some((po, pi)) = prev {
                        core_mb.add_quad(po, pi, inner, outer, Vec3::Y, RED);
                    }
                    prev = Some((outer, inner));
                }
            }
            DivOrderPick::Advance => {}
        }
    }
    if !any {
        return None;
    }
    // `MeshBuilder::build` also produces the 2×2 palette image — the plain
    // vertex-colored materials below don't need it.
    Some((border_mb.build().0, core_mb.build().0))
}

/// Rebuild the division-order overlay on dirty flags or on the animation
/// timer (the marching ants need a periodic rebuild — one line, cheap).
pub fn sync_div_order_markers(
    mut commands: Commands,
    state: Res<TacticalState>,
    game: Option<Res<GameController>>,
    time: Res<Time>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    q: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>), With<DivOrderMarker>>,
    mut anim: Local<f32>,
) {
    let Some(game) = game.as_deref() else { return };
    *anim += time.delta_secs();
    if !state.units_dirty && !state.orders_dirty && *anim < 0.12 {
        return;
    }
    if *anim >= 0.12 {
        *anim = 0.0;
    }
    despawn_visuals(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        q.iter().map(|(e, m, mat)| (e, m.0.clone(), mat.0.clone())),
    );
    if game.div_orders.is_empty() || state.grid.is_none() {
        return;
    }
    let phase = (time.elapsed_secs() * 0.35) % 1.0; // one cycle ≈ 2.9 s
    let Some((border, core)) = build_div_order_marker_mesh(&state, game, phase) else {
        return;
    };
    let mat_b = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 1.0, 1.0, 0.55),
        perceptual_roughness: 0.6,
        alpha_mode: AlphaMode::Blend,
        cull_mode: None,
        ..default()
    });
    let mat_c = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 1.0, 1.0, 0.80),
        perceptual_roughness: 0.6,
        alpha_mode: AlphaMode::Blend,
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(border)),
        MeshMaterial3d(mat_b),
        Transform::default(),
        DivOrderMarker,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
    commands.spawn((
        Mesh3d(meshes.add(core)),
        MeshMaterial3d(mat_c),
        Transform::default(),
        DivOrderMarker,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
}

/// Grow animation state for the route ribbon: set by a newly issued order,
/// replays the ribbon from the unit to its destination. `last_selected`
/// tracks selection changes — the ribbon is drawn for the selected unit
/// only, so switching selection forces a rebuild. `last_filter` tracks the
/// division-filter mode: selecting an
/// ordered division's HQ shows every battalion's ribbon, and a filter
/// change must rebuild even when no order flag fired.
#[derive(Resource, Default)]
pub struct RouteArrowAnim {
    pub t: f32,
    pub active: bool,
    pub last_selected: Option<usize>,
    pub last_filter: Option<String>,
}

/// Despawn entities and remove their per-entity mesh/material/palette-image
/// assets — spawn-per-rebuild paths (route arrows, range rings) otherwise
/// leak one full asset set into `Assets` per rebuild. Only for visuals
/// whose assets are private: shared/cached assets (unit models, selection
/// ring, ghost material) must NEVER go through here.
pub fn despawn_visuals(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    items: impl Iterator<Item = (Entity, Handle<Mesh>, Handle<StandardMaterial>)>,
) {
    for (e, mesh_h, mat_h) in items {
        if let Some(tex) = materials
            .get(&mat_h)
            .and_then(|m| m.base_color_texture.clone())
        {
            images.remove(&tex);
        }
        materials.remove(&mat_h);
        meshes.remove(&mesh_h);
        commands.entity(e).despawn();
    }
}

/// Rebuild route arrows when orders change (`orders_dirty`, §6.2) or while
/// the grow animation is running.
pub fn sync_route_arrows(
    mut commands: Commands,
    mut state: ResMut<TacticalState>,
    game: Option<Res<GameController>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    q_arrows: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>), With<RouteArrows>>,
    time: Res<Time>,
    mut anim: ResMut<RouteArrowAnim>,
) {
    // A newly issued order restarts the grow animation; plain path
    // advances just rebuild the (shorter) ribbon at full length.
    if state.arrows_grow {
        state.arrows_grow = false;
        anim.t = 0.0;
        anim.active = true;
    }
    // Selecting an HQ whose division has a standing order shows the WHOLE
    // division's march ribbons; any other selection keeps the
    // single-unit ribbon. The filter keys the rebuild gate below too —
    // a filter change must rebuild even without a dirty flag.
    let div_filter = state.selected_unit.and_then(|id| {
        let u = state.unit_by_id(id)?;
        if u.is_hq() && u.side == state.player_side && u.is_combat_effective() {
            let ordered = game
                .as_ref()
                .is_some_and(|g| g.div_orders.contains_key(&u.division));
            ordered.then_some(u.division.clone())
        } else {
            None
        }
    });
    // The ribbon is drawn for the SELECTED unit only — selection changes
    // must rebuild even when no order changed.
    let sel_changed = state.selected_unit != anim.last_selected;
    if sel_changed {
        anim.last_selected = state.selected_unit;
    }
    let filter_changed = anim.last_filter != div_filter;
    if filter_changed {
        anim.last_filter = div_filter.clone();
    }
    let dirty = state.orders_dirty;
    if dirty {
        state.orders_dirty = false;
    }
    if !dirty && !anim.active && !sel_changed && !filter_changed {
        return;
    }
    // Arrows own per-entity mesh/material/image — reclaim them.
    despawn_visuals(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        q_arrows
            .iter()
            .map(|(e, m, mat)| (e, m.0.clone(), mat.0.clone())),
    );
    if state.grid.is_none() {
        return;
    }
    // With a division filter the whole division's march counts; otherwise
    // only the selected unit's own ribbon.
    let has_move = match &div_filter {
        Some(div) => state.units.iter().any(|u| {
            u.side == state.player_side
                && u.division == *div
                && u.is_combat_effective()
                && u.move_order.is_some()
        }),
        None => state
            .selected_unit
            .and_then(|id| state.unit_by_id(id))
            .is_some_and(|u| {
                u.side == state.player_side && u.is_combat_effective() && u.move_order.is_some()
            }),
    };
    // Attack lanes render for EVERY player-side registered order.
    let has_attack = state.attack_orders.iter().any(|o| {
        state
            .unit_by_id(o.attacker)
            .is_some_and(|u| u.side == state.player_side && u.is_combat_effective())
    });
    if !has_move && !has_attack {
        anim.active = false;
        return;
    }
    let clip = if anim.active {
        anim.t = (anim.t + time.delta_secs() / 0.45).min(1.0);
        if anim.t >= 1.0 {
            anim.active = false;
        }
        anim.t
    } else {
        1.0
    };
    // Two passes, two entities: the black border gets NO emissive (a shared
    // red emissive turned it maroon — the reason the border was invisible).
    let params = game
        .as_ref()
        .map(|g| g.combat.params().clone())
        .unwrap_or_default();
    let ((b_mesh, b_img), (i_mesh, i_img)) =
        build_route_arrows_mesh(&state, clip, div_filter.as_deref(), &params);
    let border_mat = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(images.add(b_img)),
        perceptual_roughness: 0.5,
        cull_mode: None,
        ..default()
    });
    let red = [0.85, 0.10, 0.07, 1.0];
    let interior_mat = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(images.add(i_img)),
        perceptual_roughness: 0.5,
        emissive: LinearRgba::new(red[0] * 0.35, red[1] * 0.35, red[2] * 0.35, 1.0),
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(b_mesh)),
        MeshMaterial3d(border_mat),
        Transform::default(),
        RouteArrows,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
    commands.spawn((
        Mesh3d(meshes.add(i_mesh)),
        MeshMaterial3d(interior_mat),
        Transform::default(),
        RouteArrows,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
    // Attack lanes, same two-pass pattern in brighter red. Fire-mission
    // arcs are crest-coloured (orange ×1.5 / purple ×0.5) and area missions
    // dashed — the interior emissive must stay NEUTRAL white, or the red
    // glow swallows the hue difference.
    let crest_params = game
        .as_ref()
        .map(|g| g.combat.params().clone())
        .unwrap_or_default();
    let ((ab_mesh, ab_img), (ai_mesh, ai_img)) = build_attack_arrows_mesh(&state, &crest_params);
    let ab_mat = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(images.add(ab_img)),
        perceptual_roughness: 0.5,
        cull_mode: None,
        ..default()
    });
    let ai_mat = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(images.add(ai_img)),
        perceptual_roughness: 0.5,
        emissive: LinearRgba::new(0.12, 0.12, 0.12, 1.0),
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(ab_mesh)),
        MeshMaterial3d(ab_mat),
        Transform::default(),
        RouteArrows,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
    commands.spawn((
        Mesh3d(meshes.add(ai_mesh)),
        MeshMaterial3d(ai_mat),
        Transform::default(),
        RouteArrows,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
}

/// Thick outline around a zone (deployment zone, flag zone), in the given
/// color. Only boundary edges (zone hex ↔ non-zone hex) are drawn, so the
/// zone reads as one enclosed region instead of tinted cells. `lift` rides
/// the band above the terrain top — stacked outlines (flag gold over the
/// deployment border) need distinct lifts to avoid z-fighting. `width` is
/// the inward band extent; the outward nub into the grid gap scales with it.
pub fn build_zone_border_mesh(
    state: &TacticalState,
    zone: &[HexCoord],
    color: [f32; 4],
    lift: f32,
    width: f32,
) -> (Mesh, Image) {
    let grid = state.grid.as_ref().expect("grid must exist");
    let zone_set: std::collections::HashSet<(i32, i32)> = zone.iter().map(|h| (h.q, h.r)).collect();
    let top_of = |h: HexCoord| hex_top_y_at(grid, h);
    let mut mb = MeshBuilder::new();
    for &h in zone {
        let Some(cell) = grid.cell(h) else { continue };
        if !cell.is_passable {
            continue;
        }
        for dir in HexDirection::ALL {
            let n = h.neighbor(dir);
            if zone_set.contains(&(n.q, n.r)) {
                continue; // internal edge
            }
            // Ride on the taller of the two sides so the line never sinks.
            let y = top_of(h).max(top_of(n)) + lift;
            let center = hex_world(h);
            let corners = hex_corners(center, HEX_SIZE * HEX_GAP, y);
            let (a, b) = hex_edge(&corners, dir as usize);
            let mid = (a + b) * 0.5;
            let inward = (Vec3::new(center.x, y, center.z) - mid).normalize();
            // Thick band: mostly inward, a scaled nub outward into the gap.
            // Vertex order faces UP (the a,b,b+in,a+in order faces down and
            // gets culled from the RTS camera).
            let out_w = width * 0.55;
            mb.add_quad(
                a - inward * out_w,
                a + inward * width,
                b + inward * width,
                b - inward * out_w,
                Vec3::Y,
                color,
            );
        }
    }
    mb.build()
}

/// Max-range ring for a selected ranged unit (artillery etc.): a flat band
/// following the circle of radius `attack_range × hex-center-spacing` around
/// the unit. Rule correspondence: hex distance N puts a hex center at world
/// distance N·√3·s (pointy-top spacing), so this circle passes exactly
/// through the centers of the outermost targetable hexes — "targetable iff
/// the hex center is inside the circle". Rides terrain like the zone border.
/// `dashed` renders a broken band (4 on / 2 off segments) — used for the
/// rocket minimum-range dead-zone circle.
pub fn build_range_ring_mesh(
    state: &TacticalState,
    center: HexCoord,
    attack_range: i32,
    color: [f32; 4],
    dashed: bool,
) -> (Mesh, Image) {
    const SEGMENTS: usize = 96;
    const HALF_W: f32 = 0.05;
    let (cx, cz) = center.to_world(HEX_SIZE);
    let radius = attack_range as f32 * HEX_SIZE * 3.0_f32.sqrt();
    // No grid yet (synthetic/menu states): fall back to a flat ring at
    // ground level instead of panicking.
    let ground_y = |x: f32, z: f32| {
        state
            .grid
            .as_ref()
            .and_then(|g| g.cell(HexCoord::from_world(x, z, HEX_SIZE)))
            .map(|c| {
                if c.is_passable {
                    Terrain::elevation_render_height(c.elevation)
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0)
            + 0.04
    };
    let mut mb = MeshBuilder::new();
    let mut prev: Option<(Vec3, Vec3)> = None;
    for i in 0..=SEGMENTS {
        let a = (i % SEGMENTS) as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        let (dx, dz) = (a.cos(), a.sin());
        let y = ground_y(cx + dx * radius, cz + dz * radius);
        let outer = Vec3::new(cx + dx * (radius + HALF_W), y, cz + dz * (radius + HALF_W));
        let inner = Vec3::new(cx + dx * (radius - HALF_W), y, cz + dz * (radius - HALF_W));
        if let Some((po, pi)) = prev {
            // Dash pattern: draw 4 of every 6 segments (broken circle).
            if !dashed || i % 6 < 4 {
                mb.add_quad(po, pi, inner, outer, Vec3::Y, color);
            }
        }
        prev = Some((outer, inner));
    }
    mb.build()
}

/// Spawn/despawn the zone border to match the Deployment phase (§11.1.5).
pub fn sync_zone_border(
    mut commands: Commands,
    state: Res<TacticalState>,
    game: Option<Res<GameController>>,
    colors: Res<SideColors>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    q_border: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>), With<DeploymentBorder>>,
    mut shown: Local<bool>,
) {
    let want = game
        .as_ref()
        .map(|g| g.session.phase == BattlePhase::Deployment)
        .unwrap_or(false)
        || state.debug_no_fog;
    let want = want && state.deployment_zones.is_some() && state.grid.is_some();
    if want == *shown {
        return;
    }
    *shown = want;
    // Route through despawn_visuals — a bare Query<Entity> despawn leaks
    // one mesh/material/image set per rebuild into `Assets`.
    despawn_visuals(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        q_border
            .iter()
            .map(|(e, m, mat)| (e, m.0.clone(), mat.0.clone())),
    );
    if !want {
        return;
    }
    let Some((a, d)) = state.deployment_zones.clone() else {
        return;
    };
    // Both zones are outlined during deployment and whenever the F8 debug
    // view is up (zone-adherence checks): the player's zone in the player's
    // theme color, the AI's zone in the enemy's — the AI only occupies it
    // at BeginBattle, unseen by the player.
    for (zone, side) in [(a, Side::Attacker), (d, Side::Defender)] {
        let color = colors.for_side(side);
        // Same band width and pulse emphasis as the flag outlines — the
        // side colors keep them distinct.
        let (mesh, image) = build_zone_border_mesh(&state, &zone, color, 0.03, 0.17);
        let mat = materials.add(StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(images.add(image)),
            perceptual_roughness: 0.6,
            // Start mid-pulse; `pulse_zone_borders` rewrites the emissive.
            emissive: LinearRgba::new(color[0] * 0.7, color[1] * 0.7, color[2] * 0.7, 1.0),
            ..default()
        });
        commands.spawn((
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(mat),
            Transform::default(),
            DeploymentBorder,
            BorderPulse {
                base: [color[0], color[1], color[2]],
            },
            NotShadowCaster, // ground-hugging overlay: casts no visible shadow
        ));
    }
}

// ---------------------------------------------------------------------------
// Allied sector-suggestion overlay (DESIGN §7.5)
// ---------------------------------------------------------------------------

/// The allied sector-suggestion outline entity — one outline mesh over the
/// union of all stored suggestion rects (Deployment phase only).
#[derive(Component)]
pub struct AllySectorOverlay;

/// Suggestion steel blue — distinct from both side theme colors and the
/// flag gold: the rect is a deployment HINT for the allied AI, not an order.
const ALLY_SECTOR_COLOR: [f32; 4] = [0.55, 0.78, 0.92, 0.85];

/// Spawn/rebuild/despawn the allied suggestion overlay: shown during the
/// Deployment phase like the zone borders, rebuilt whenever a suggestion is
/// stored / cleared / restored (`state.ally_sectors_dirty`).
pub fn sync_ally_sector_overlay(
    mut commands: Commands,
    mut state: ResMut<TacticalState>,
    game: Option<Res<GameController>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    q_overlay: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>), With<AllySectorOverlay>>,
    mut shown: Local<bool>,
) {
    let want = game
        .as_ref()
        .map(|g| g.session.phase == BattlePhase::Deployment)
        .unwrap_or(false)
        && state.grid.is_some();
    let dirty = state.ally_sectors_dirty;
    if want == *shown && !dirty {
        return;
    }
    *shown = want;
    if dirty {
        state.ally_sectors_dirty = false;
    }
    // Same leak-avoidance route as sync_zone_border.
    despawn_visuals(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        q_overlay
            .iter()
            .map(|(e, m, mat)| (e, m.0.clone(), mat.0.clone())),
    );
    if !want {
        return;
    }
    let Some(game) = game.as_ref() else { return };
    if game.allied_sectors.is_empty() {
        return;
    }
    // Union of every stored rect — adjacent rects merge into one outline
    // (build_zone_border_mesh skips internal edges).
    let mut hexes: Vec<HexCoord> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (anchor, release) in game.allied_sectors.values() {
        for h in anchor.rect_between(*release) {
            if seen.insert((h.q, h.r)) {
                hexes.push(h);
            }
        }
    }
    let (mesh, image) = build_zone_border_mesh(&state, &hexes, ALLY_SECTOR_COLOR, 0.04, 0.13);
    let mat = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(images.add(image)),
        perceptual_roughness: 0.6,
        emissive: LinearRgba::new(
            ALLY_SECTOR_COLOR[0] * 0.7,
            ALLY_SECTOR_COLOR[1] * 0.7,
            ALLY_SECTOR_COLOR[2] * 0.7,
            1.0,
        ),
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(mesh)),
        MeshMaterial3d(mat),
        Transform::default(),
        AllySectorOverlay,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
}

// ---------------------------------------------------------------------------
// Flag-capture zone overlay (§6.11 map highlight)
// ---------------------------------------------------------------------------

/// Flag-zone highlight entities (one fill mesh + one outline per flag).
#[derive(Component)]
pub struct FlagZoneOverlay;

/// A zone outline whose emissive pulses (flag gold, deployment side
/// colors): `base` is the outline's un-pulsed color, `pulse_zone_borders`
/// sweeps `emissive = base × f` on a 4 s sine.
#[derive(Component)]
pub struct BorderPulse {
    pub base: [f32; 3],
}

/// §6.11 objective gold (outline) — the same hue as the right panel's
/// capture bars (ui.rs GOLD), so map and panel read as one system.
pub const FLAG_GOLD: [f32; 4] = [0.85, 0.70, 0.36, 1.0];

/// §6.11 fill amber: the original panel gold washed out on plains green
/// and urban grey (all three share a mid luminance) —
/// the fill is a deeper, more saturated amber that contrasts on BOTH while
/// staying in the gold family.
pub const FLAG_FILL: [f32; 4] = [0.92, 0.58, 0.15, 1.0];

/// Translucent golden fill plates over the flag-zone hexes: the zones must
/// be eye-catching WITHOUT hurting terrain judgment or covering pieces —
/// so the fill is a ground-level wash
/// (the alpha lives on the material) that terrain colors, props and the fog
/// gradient show through, and unit models stand above. Hexes are deduped
/// across flags: doubled plates at the same height would z-fight.
pub fn build_flag_zone_fill_mesh(
    state: &TacticalState,
    zones: &[&[HexCoord]],
    color: [f32; 4],
) -> (Mesh, Image) {
    let grid = state.grid.as_ref().expect("grid must exist");
    let mut mb = MeshBuilder::new();
    let mut seen: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    for zone in zones {
        for &h in *zone {
            if !seen.insert((h.q, h.r)) {
                continue;
            }
            let Some(cell) = grid.cell(h) else { continue };
            if !cell.is_passable {
                continue;
            }
            let center = hex_world(h);
            let top = Terrain::elevation_render_height(cell.elevation);
            // Thin plate riding the terrain top: clears the river strips
            // (+0.015), stays under every order/command overlay (+0.05 up).
            mb.add_hex_plate(center, HEX_SIZE * HEX_GAP, top + 0.02, top + 0.03, color);
        }
    }
    mb.build()
}

/// Spawn/refresh the flag-zone overlay (§6.11): one translucent fill mesh
/// over all zones plus an opaque emissive outline per flag. Zones are static
/// for a battle, so a signature of the flag set gates the rebuild (Restart /
/// Rollback restores a session clone — same zones, no churn; a battle
/// without flags — the annihilation path — shows nothing).
pub fn sync_flag_zones(
    mut commands: Commands,
    state: Res<TacticalState>,
    game: Option<Res<GameController>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    q_overlay: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>), With<FlagZoneOverlay>>,
    mut shown: Local<u64>,
) {
    let flags = game.as_ref().and_then(|g| g.session.flags());
    // Signature of the current flag set (0 = no flags / no grid yet).
    let sig = match flags {
        Some(fs) if state.grid.is_some() => {
            let mut s = 0x9E37_9E37u64 ^ fs.flags.len() as u64;
            for f in &fs.flags {
                s = s
                    .wrapping_mul(31)
                    .wrapping_add(((f.anchor.q as u64) << 32) | (f.anchor.r as u64 & 0xffff_ffff))
                    .wrapping_mul(31)
                    .wrapping_add(f.zone.len() as u64);
            }
            s
        }
        _ => 0,
    };
    if *shown == sig {
        return;
    }
    *shown = sig;
    despawn_visuals(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        q_overlay
            .iter()
            .map(|(e, m, mat)| (e, m.0.clone(), mat.0.clone())),
    );
    let Some(flags) = flags.filter(|_| sig != 0) else {
        return;
    };
    // Translucent fill (Blend): eye-catching wash, terrain/fog show through,
    // pieces stand above it. NO emissive — it ignores the material alpha and
    // would turn the wash opaque (the command-overlay lesson).
    let zones: Vec<&[HexCoord]> = flags.flags.iter().map(|f| f.zone.as_slice()).collect();
    let (fill_mesh, fill_img) = build_flag_zone_fill_mesh(&state, &zones, FLAG_FILL);
    let fill_mat = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 1.0, 1.0, 0.40),
        base_color_texture: Some(images.add(fill_img)),
        perceptual_roughness: 0.6,
        alpha_mode: AlphaMode::Blend,
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(fill_mesh)),
        MeshMaterial3d(fill_mat),
        Transform::default(),
        FlagZoneOverlay,
        NotShadowCaster, // ground-hugging overlay: casts no visible shadow
    ));
    // Opaque golden outline per flag — the "醒目" carrier (the fill alone is
    // deliberately quiet): a THICKER band than the deployment border, its
    // emissive breathed by `pulse_flag_zones`. +0.05 lift clears the
    // deployment border (+0.03) where a flag boundary coincides with the
    // zone's deep edge.
    for f in &flags.flags {
        let (mesh, image) = build_zone_border_mesh(&state, &f.zone, FLAG_GOLD, 0.05, 0.17);
        let mat = materials.add(StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(images.add(image)),
            perceptual_roughness: 0.6,
            // Start mid-pulse; the pulse system rewrites the emissive anyway.
            emissive: LinearRgba::new(
                FLAG_GOLD[0] * 0.7,
                FLAG_GOLD[1] * 0.7,
                FLAG_GOLD[2] * 0.7,
                1.0,
            ),
            ..default()
        });
        commands.spawn((
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(mat),
            Transform::default(),
            FlagZoneOverlay,
            BorderPulse {
                base: [FLAG_GOLD[0], FLAG_GOLD[1], FLAG_GOLD[2]],
            },
            NotShadowCaster, // ground-hugging overlay: casts no visible shadow
        ));
    }
}

/// Smooth pulse for the pulsing zone outlines — flag gold and deployment
/// side colors alike. A gentle sine breath (0.45–0.95×) was invisible
/// under the sun-lit base + tone mapping; a radar strobe was too flashy.
/// The final curve sweeps a 0.15×–1.8× band with a plain sine on a 4 s
/// period: no flash attack, no dwell. Only outlines pulse — a pulsing fill
/// would throb over a large area and fight terrain judgment.
pub fn pulse_zone_borders(
    time: Res<Time>,
    q_borders: Query<(&MeshMaterial3d<StandardMaterial>, &BorderPulse)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut last_write: Local<Option<f32>>,
) {
    if q_borders.is_empty() {
        return;
    }
    // ~15 Hz write cadence: a 4 s sine sampled at 15 Hz is visually
    // continuous, and every write marks the border
    // materials Modified → bind-group rebuild — doing it per frame was a
    // standing per-frame asset churn for zero visible difference. The
    // FIRST call always writes (never leave the spawned default emissive).
    let now = time.elapsed_secs();
    let due = match *last_write {
        None => true,
        Some(t) => now - t >= 1.0 / 15.0,
    };
    if !due {
        return;
    }
    *last_write = Some(now);
    // 4 s sine over the 0.15×..1.8× band (mid 0.975, amplitude 0.825).
    let f = 0.975 + 0.825 * (now * std::f32::consts::FRAC_PI_2).sin();
    for (mat_h, pulse) in q_borders.iter() {
        if let Some(m) = materials.get_mut(&mat_h.0) {
            m.emissive =
                LinearRgba::new(pulse.base[0] * f, pulse.base[1] * f, pulse.base[2] * f, 1.0);
        }
    }
}

/// ONE merged static mesh for every prop on the board: props used to be
/// one ENTITY per tree/building — thousands of draw calls per
/// frame on forested maps, the steady-state GPU cost when panning. They are
/// positionally static for the whole battle, so they bake into a single
/// mesh + palette. Visual parity: props never were fog-tinted and still are
/// not; the single shared material uses the tree/rock roughness (0.95) —
/// the pond's old 0.6 differed only subtly on a flat dark plate. Unlike the
/// board mesh, the props entity KEEPS casting shadows.
#[derive(Component)]
pub struct BoardProps;

/// Merge all props (trees/buildings/rocks/ponds) into one mesh — sparse on
/// purpose: 1–2 pieces per hex, purely for terrain readability. `None` when
/// the board has no prop-bearing terrain at all.
pub fn build_props_mesh(state: &TacticalState) -> Option<(Mesh, Image)> {
    let grid = state.grid.as_ref()?;
    let mut mb = MeshBuilder::new();
    for h in grid.iter_coords() {
        let Some(cell) = grid.cell(h) else { continue };
        if !cell.is_passable {
            continue;
        }
        let center = hex_world(h);
        let top = Terrain::elevation_render_height(cell.elevation);
        let j1 = hash01(((h.q * 7349 + h.r * 15187) as u64) | 0xA5);
        let j2 = hash01(((h.q * 4093 + h.r * 8861) as u64) | 0x3C);
        let j3 = hash01(((h.q * 919 + h.r * 6421) as u64) | 0x77);

        let offset = |seed: f32, mag: f32| -> Vec3 {
            let ang = seed * std::f32::consts::TAU;
            Vec3::new(ang.cos() * mag, 0.0, ang.sin() * mag)
        };
        let at = center + Vec3::Y * top;
        match cell.terrain {
            Terrain::Forest => {
                // Trees hug the hex corners so units standing here stay visible.
                add_tree(&mut mb, at + offset(j1, 0.58));
                add_tree(&mut mb, at + offset(j2, 0.52));
            }
            Terrain::Jungle => {
                add_tree(&mut mb, at + offset(j1, 0.55));
            }
            Terrain::Urban => {
                add_building(&mut mb, at + offset(j1, 0.42));
                if j3 > 0.5 {
                    add_building(&mut mb, at + offset(j2, 0.50));
                }
            }
            Terrain::Village => {
                // One hamlet cluster — villages must read at province zoom
                // (a bare tan texel is invisible on 100+ row maps).
                add_building(&mut mb, at + offset(j1, 0.40));
            }
            Terrain::Mountain => {
                // Spire colour = this hex's elevation band, one step
                // brighter than the ground prism.
                let [r, g, b] = Terrain::Mountain.banded_color(cell.elevation);
                let band = scale_color([r, g, b, 1.0], 1.15);
                add_rock(&mut mb, at + offset(j1, 0.42), band);
            }
            Terrain::Marsh => {
                add_pond(&mut mb, at + offset(j1, 0.40));
            }
            _ => {}
        }
    }
    if mb.positions.is_empty() {
        return None;
    }
    Some(mb.build())
}

/// Board material: double-sided. The hex prism *side* quads are wound to
/// face inward (don't touch the winding), so at a height step between
/// neighbors the trench wall facing the camera is backface-culled and the
/// background shows through. Rendering the board double-sided makes
/// elevation walls visible from every angle.
fn board_material(
    materials: &mut Assets<StandardMaterial>,
    image: Handle<Image>,
) -> Handle<StandardMaterial> {
    materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(image),
        perceptual_roughness: 0.95,
        cull_mode: None,
        ..default()
    })
}

/// Blocky tree: trunk + crown (kept small so it never swallows a unit),
/// baked at world position `at`.
fn add_tree(mb: &mut MeshBuilder, at: Vec3) {
    mb.add_box(
        at + Vec3::new(-0.05, 0.0, -0.05),
        at + Vec3::new(0.05, 0.22, 0.05),
        [0.35, 0.22, 0.12, 1.0],
    );
    mb.add_box(
        at + Vec3::new(-0.16, 0.18, -0.16),
        at + Vec3::new(0.16, 0.44, 0.16),
        [0.14, 0.42, 0.16, 1.0],
    );
}

/// Blocky town: two small houses with roofs.
fn add_building(mb: &mut MeshBuilder, at: Vec3) {
    let wall = [0.72, 0.68, 0.60, 1.0];
    let roof = [0.48, 0.25, 0.20, 1.0];
    mb.add_box(
        at + Vec3::new(-0.16, 0.0, -0.12),
        at + Vec3::new(0.04, 0.18, 0.08),
        wall,
    );
    mb.add_box(
        at + Vec3::new(-0.18, 0.18, -0.14),
        at + Vec3::new(0.06, 0.26, 0.10),
        roof,
    );
    mb.add_box(
        at + Vec3::new(0.08, 0.0, -0.06),
        at + Vec3::new(0.24, 0.13, 0.10),
        wall,
    );
    mb.add_box(
        at + Vec3::new(0.06, 0.13, -0.08),
        at + Vec3::new(0.26, 0.20, 0.12),
        roof,
    );
}

/// Blocky rock: stacked boxes in the hex's own elevation-band colour (the
/// spire rides the same palette as its base prism, slightly brightened so
/// it reads as a peak, not a stain).
fn add_rock(mb: &mut MeshBuilder, at: Vec3, rock: [f32; 4]) {
    mb.add_box(
        at + Vec3::new(-0.18, 0.0, -0.14),
        at + Vec3::new(0.14, 0.16, 0.16),
        rock,
    );
    mb.add_box(
        at + Vec3::new(-0.10, 0.16, -0.08),
        at + Vec3::new(0.06, 0.30, 0.08),
        scale_color(rock, 0.85),
    );
}

/// Marsh pond: flat dark-water plate riding the terrain top.
fn add_pond(mb: &mut MeshBuilder, at: Vec3) {
    mb.add_hex_plate(at, 0.28, at.y, at.y + 0.02, [0.20, 0.32, 0.40, 1.0]);
}

/// Startup system: board + props + lighting.
pub fn setup_board(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut board_assets: ResMut<BoardAssets>,
    state: Res<TacticalState>,
    quality: Res<crate::state::RenderQuality>,
) {
    if state.grid.is_none() {
        return;
    }
    let (board, palette_img) = build_board_mesh(&state);
    let img_h = images.add(palette_img);
    let board_mat = board_material(&mut materials, img_h.clone());
    let mesh_h = meshes.add(board);
    commands.spawn((
        Mesh3d(mesh_h.clone()),
        MeshMaterial3d(board_mat.clone()),
        Transform::default(),
        BoardMesh,
        // See rebuild_board_mesh: the near-flat merged board does not cast
        // shadows (invisible self-shadowing, dominant shadow-pass cost).
        NotShadowCaster,
    ));
    board_assets.mesh = Some(mesh_h);
    board_assets.image = Some(img_h);
    board_assets.material = Some(board_mat);
    // Props: ONE merged static mesh entity for the whole board.
    // Terrain and the grid are static post-startup, so this is built once —
    // snapshot restores swap colors/geometry in place, never the grid.
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

    // Lighting: warm sun from the NW + cool ambient fill.
    //
    // Shadow-pass slimming: Bevy's default CSM config is FOUR cascades out
    // to 1000 m with the first far bound at 5 m — tuned for walking sims,
    // not a top-down RTS board; every caster (props, ~100 units, every
    // overlay) was drawn into up to 4 maps per frame, making the shadow
    // pass ~70% of all draw calls. Two cascades instead: crisp shadows
    // below 60 m view depth, one map covering the whole board at max
    // zoom-out (the default's 1000 m cap actually DROPPED shadows entirely
    // on the biggest provinces). On/off follows the settings.json shadow
    // level (off = no shadow pass at all). The LOW tier goes further — a
    // single cascade covering the whole board (half the shadow-pass draws)
    // plus Hardware2x2 filtering on the camera (see spawn_camera); HIGH
    // keeps the two-cascade Gaussian setup.
    let cam = crate::camera::default_view(&state);
    let (gw, gh) = state
        .grid
        .as_ref()
        .map(|g| (g.width as f32, g.height as f32))
        .unwrap_or((64.0, 64.0));
    // Pointy-top world extents: x ≈ (q + r/2)·√3·HEX_SIZE, z ≈ r·1.5·HEX_SIZE.
    let board_diag = ((gw + gh * 0.5) * 1.732 * HEX_SIZE).hypot(gh * 1.5 * HEX_SIZE);
    let shadow_far = cam.max_distance + board_diag.max(64.0);
    let num_cascades = if quality.shadow_level <= 1 { 1 } else { 2 };
    // Captured for the in-battle Settings window: flipping the shadow
    // level rebuilds the cascade config at this same extent (settings.rs
    // apply_battle_settings).
    commands.insert_resource(crate::settings::SunShadow {
        maximum_distance: shadow_far,
        num_cascades,
    });
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadows_enabled: quality.shadows,
            ..default()
        },
        bevy::pbr::CascadeShadowConfigBuilder {
            num_cascades,
            first_cascade_far_bound: 60.0,
            maximum_distance: shadow_far,
            ..default()
        }
        .build(),
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, -0.6, -1.0, 0.0)),
    ));
    commands.insert_resource(AmbientLight {
        color: Color::srgb(0.65, 0.70, 0.80),
        brightness: 320.0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::App;
    use bevy::render::mesh::VertexAttributeValues;
    use std::sync::Arc;
    use tactical_core::unit::{BattalionUnit, UnitType};
    use tactical_core::{synthesize_hqs, CombatParams, HexGrid, MoveOrder, Terrain};

    fn vertex_count(mesh: &Mesh) -> usize {
        match mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
            Some(VertexAttributeValues::Float32x3(v)) => v.len(),
            _ => 0,
        }
    }

    /// §6.13 selection modes — a plain battalion sees only its own lane,
    /// the HQ sees every division lane, and the out-of-reach outlier's lane
    /// lands in the dashed mesh pair.
    #[test]
    fn command_lines_mesh_selection_modes() {
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(16, 16, Terrain::Plains)));
        let mk = |id, q, r| {
            let mut u = BattalionUnit::new(
                id,
                "1.Inf",
                UnitType::Infantry,
                Side::Attacker,
                HexCoord::new(q, r),
            );
            u.division = "D".to_string();
            u
        };
        // Two battalions near the HQ (solid), one far out (dashed).
        state.units = vec![mk(0, 2, 2), mk(1, 4, 2), mk(2, 12, 2)];
        let mut next_id = 100;
        synthesize_hqs(&mut state.units, &mut next_id, Side::Attacker, |_| {
            HexCoord::new(2, 3)
        });
        let params = CombatParams::default();
        let links = compute_command_links(&state.units, &params);
        // Plain battalion selected: only its own solid lane.
        let m = build_command_lines_mesh(&state, &links, "D", 0, false);
        assert!(m.any_solid && !m.any_dashed);
        assert!(vertex_count(&m.solid.1 .0) > 0);
        // HQ selected: every lane, including the dashed outlier.
        let m = build_command_lines_mesh(&state, &links, "D", 100, true);
        assert!(m.any_solid && m.any_dashed);
        // Out-of-range battalion selected: only its own dashed lane.
        let m = build_command_lines_mesh(&state, &links, "D", 2, false);
        assert!(!m.any_solid && m.any_dashed);
        // Unknown division: nothing.
        let m = build_command_lines_mesh(&state, &links, "nope", 0, false);
        assert!(!m.any_solid && !m.any_dashed);
    }

    /// §6.11 map highlight: the flag-zone fill dedupes overlapping zones
    /// (same-height plates would z-fight) and the lifted
    /// golden outline carries the zone's boundary edges.
    #[test]
    fn flag_zone_overlay_meshes_build() {
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(12, 12, Terrain::Plains)));
        let zone_a = vec![
            HexCoord::new(5, 5),
            HexCoord::new(6, 5),
            HexCoord::new(5, 6),
        ];
        // Overlaps zone_a on (5,5) — the dup must not double-plate.
        let zone_b = vec![HexCoord::new(5, 5), HexCoord::new(9, 9)];
        let (fill, _) = build_flag_zone_fill_mesh(&state, &[&zone_a, &zone_b], FLAG_FILL);
        // 4 unique hexes; a hex plate = 6 top fan tris + 6 side quads = 42 verts.
        assert_eq!(vertex_count(&fill), 4 * 42);
        let (border, _) = build_zone_border_mesh(&state, &zone_a, FLAG_GOLD, 0.05, 0.17);
        assert!(vertex_count(&border) > 0);
        // Wider band → same edge count, more area per quad (4 verts each).
        let (thin, _) = build_zone_border_mesh(&state, &zone_a, FLAG_GOLD, 0.05, 0.11);
        assert_eq!(vertex_count(&border), vertex_count(&thin));
    }

    /// §6.11 pulsing outlines (flag gold, deployment side colors)
    /// sine-sweep `base × 0.15..1.8` on a 4 s period; the emissive scales
    /// from EACH entity's own `BorderPulse.base`.
    #[test]
    fn zone_border_pulse_breathes_in_bounds() {
        const BASE: [f32; 3] = [0.5, 0.25, 0.1];
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins);
        // Deterministic clock: each update() advances the virtual clock by
        // exactly 100 ms (advance_by on Time<Virtual> gets clobbered by the
        // time system every frame).
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(100),
        ));
        app.insert_resource(Assets::<StandardMaterial>::default());
        app.add_systems(Update, pulse_zone_borders);
        let mat = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        app.world_mut()
            .spawn((MeshMaterial3d(mat.clone()), BorderPulse { base: BASE }));
        let emissive_red = |app: &mut App| -> f32 {
            app.world_mut()
                .resource::<Assets<StandardMaterial>>()
                .get(&mat)
                .unwrap()
                .emissive
                .red
        };
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        // 50 frames × 100 ms = 5 s — covers the full 4 s sine.
        for _ in 0..50 {
            app.update();
            let r = emissive_red(&mut app);
            lo = lo.min(r);
            hi = hi.max(r);
        }
        let band_lo = BASE[0] * 0.15 - 1e-3;
        let band_hi = BASE[0] * 1.80 + 1e-3;
        assert!(
            lo >= band_lo && hi <= band_hi,
            "emissive {lo}..{hi} outside {band_lo}..{band_hi}"
        );
        assert!(
            hi - lo > BASE[0] * 0.9,
            "pulse must actually swing (got {lo}..{hi})"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: the route ribbon must PERSIST when the player re-selects
    // the unit that issued the order. Drives the REAL `sync_route_arrows`
    // system headlessly and counts RouteArrows entities + mesh vertices
    // after each step (0 = nothing drawn).
    // -----------------------------------------------------------------------

    /// Count RouteArrows entities and total mesh vertices (0 = nothing drawn).
    fn ribbon_stats(app: &mut App) -> (usize, usize) {
        let world = app.world_mut();
        let mut n = 0usize;
        let mut verts = 0usize;
        let mut q = world.query::<(Entity, Option<&Mesh3d>)>();
        for (e, m) in q.iter(world) {
            if world.get::<RouteArrows>(e).is_none() {
                continue;
            }
            n += 1;
            if let Some(m) = m {
                if let Some(mesh) = world.resource::<Assets<Mesh>>().get(&m.0) {
                    if let Some(VertexAttributeValues::Float32x3(v)) =
                        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
                    {
                        verts += v.len();
                    }
                }
            }
        }
        (n, verts)
    }

    fn step_frames(app: &mut App, n: usize) {
        for _ in 0..n {
            app.world_mut()
                .resource_mut::<Time>()
                .advance_by(std::time::Duration::from_millis(50));
            app.update();
        }
    }

    fn ribbon_app() -> App {
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins);
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(24, 24, Terrain::Plains)));
        state.player_side = Side::Attacker;
        let mut a = BattalionUnit::new(
            1,
            "A",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(3, 3),
        );
        a.move_order = Some(MoveOrder {
            path: vec![
                HexCoord::new(4, 3),
                HexCoord::new(5, 3),
                HexCoord::new(6, 3),
                HexCoord::new(7, 3),
                HexCoord::new(8, 3),
                HexCoord::new(9, 3),
                HexCoord::new(10, 3),
            ],
            hours: 0.0,
        });
        let b = BattalionUnit::new(
            2,
            "B",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(3, 8),
        );
        state.units = vec![a, b];
        app.insert_resource(state);
        app.init_resource::<RouteArrowAnim>();
        app.insert_resource(Assets::<Mesh>::default());
        app.insert_resource(Assets::<bevy::image::Image>::default());
        app.insert_resource(Assets::<StandardMaterial>::default());
        app.add_systems(Update, sync_route_arrows);
        app
    }

    /// Regression: select A → order → select B → re-select A → the ribbon
    /// must still be on the board (full length) after the grow window.
    #[test]
    fn route_ribbon_persists_after_reselect() {
        let mut app = ribbon_app();

        // 1. Player selects A.
        app.world_mut()
            .resource_mut::<TacticalState>()
            .selected_unit = Some(1);
        app.world_mut().resource_mut::<TacticalState>().units_dirty = true;
        step_frames(&mut app, 1);
        // 2. Issue the move order (sets arrows_grow; first grow frame).
        app.world_mut().resource_mut::<TacticalState>().arrows_grow = true;
        app.world_mut().resource_mut::<TacticalState>().orders_dirty = true;
        step_frames(&mut app, 1);
        // 3. Let the grow animation finish (0.45 s @ 50 ms/frame ≈ 9+ frames).
        step_frames(&mut app, 12);
        let (n_done, v_done) = ribbon_stats(&mut app);
        assert!(n_done > 0, "ribbon must be visible right after the order");
        assert!(v_done > 0, "ribbon mesh must have vertices after the order");
        // 4. Select unit B.
        app.world_mut()
            .resource_mut::<TacticalState>()
            .selected_unit = Some(2);
        app.world_mut().resource_mut::<TacticalState>().units_dirty = true;
        step_frames(&mut app, 1);
        // 5. Re-select unit A.
        app.world_mut()
            .resource_mut::<TacticalState>()
            .selected_unit = Some(1);
        app.world_mut().resource_mut::<TacticalState>().units_dirty = true;
        step_frames(&mut app, 1);
        let (n_resel, v_resel) = ribbon_stats(&mut app);
        assert!(
            n_resel > 0,
            "ribbon must be visible right after re-selection"
        );
        assert!(
            v_resel > 0,
            "ribbon mesh must have vertices after re-selection"
        );
        // 6. Wait well past the grow window — the ribbon must PERSIST.
        step_frames(&mut app, 20);
        let (n_after, v_after) = ribbon_stats(&mut app);
        assert!(
            n_after > 0 && v_after == v_resel,
            "ribbon vanished after re-selection: entities {n_after} (was {n_resel}), \
             vertices {v_after} (was {v_resel})"
        );
    }

    /// Regression variant: the re-selection lands while the grow animation
    /// from the original order is STILL RUNNING (select B and re-select A
    /// within the 0.45 s window).
    #[test]
    fn route_ribbon_persists_after_fast_reselect() {
        let mut app = ribbon_app();

        // Order issued, animation started mid-grow (3 frames in).
        app.world_mut()
            .resource_mut::<TacticalState>()
            .selected_unit = Some(1);
        app.world_mut().resource_mut::<TacticalState>().arrows_grow = true;
        app.world_mut().resource_mut::<TacticalState>().orders_dirty = true;
        step_frames(&mut app, 3);
        // Select B mid-animation, then immediately re-select A mid-animation.
        app.world_mut()
            .resource_mut::<TacticalState>()
            .selected_unit = Some(2);
        step_frames(&mut app, 2);
        app.world_mut()
            .resource_mut::<TacticalState>()
            .selected_unit = Some(1);
        step_frames(&mut app, 2);
        // Let the animation finish.
        step_frames(&mut app, 20);
        let (n, v) = ribbon_stats(&mut app);
        assert!(
            n > 0,
            "ribbon must persist after the fast re-select, entities = {n}"
        );
        assert!(
            v > 0,
            "ribbon mesh must have vertices after the fast re-select, verts = {v}"
        );
    }

    /// Invested march hours show as a light progress mark on the
    /// route ribbon — half a plain-infantry step (0.125 h of 0.25 h) must
    /// add the progress quad's geometry to the interior mesh.
    #[test]
    fn route_ribbon_shows_step_progress() {
        let mut app = ribbon_app();
        app.world_mut().resource_mut::<TacticalState>().selected_unit = Some(1);
        app.world_mut().resource_mut::<TacticalState>().orders_dirty = true;
        step_frames(&mut app, 20); // grow window (0.45 s) finishes
        let (n0, v0) = ribbon_stats(&mut app);
        assert!(n0 >= 2 && v0 > 0, "baseline ribbon: {n0} entities {v0} verts");
        {
            let mut state = app.world_mut().resource_mut::<TacticalState>();
            state.units[0].move_order.as_mut().unwrap().hours = 0.125;
            state.orders_dirty = true;
        }
        step_frames(&mut app, 2);
        let (n1, v1) = ribbon_stats(&mut app);
        assert!(
            v1 > v0,
            "the progress mark must add geometry: {v0} -> {v1} verts"
        );
    }

    /// Palette fast path: a colors-only recolor must produce EXACTLY the
    /// palette a full rebuild would — the slot layout is fixed
    /// (3 river slots + 2 per hex), so the mesh never needs rebuilding.
    #[test]
    fn palette_recolor_matches_full_rebuild() {
        let mut state = TacticalState::default();
        let mut grid = HexGrid::new(8, 8, Terrain::Plains);
        grid.cell_mut(HexCoord::new(2, 2)).unwrap().river_edges = 0b0001;
        state.grid = Some(Arc::new(grid));

        let (_, image_a) = build_board_mesh(&state);
        // Fixed layout: 3 river slots + 2 per hex (8×8 = 64 hexes).
        assert_eq!(image_a.data.len(), (3 + 2 * 64) * 4);

        // Change a highlight → recolor in place, then compare against a
        // from-scratch rebuild of the same state.
        let mut changed = TacticalState::default();
        changed.grid = state.grid.clone();
        changed.player_side = state.player_side;
        changed
            .highlights
            .push((HexCoord::new(3, 3), HighlightKind::Hover));
        let mut recolored = image_a.clone();
        recolor_board_palette(&changed, &mut recolored);
        let (_, image_b) = build_board_mesh(&changed);
        assert_eq!(
            recolored.data, image_b.data,
            "palette recolor must equal a full rebuild"
        );
    }

    /// Fire-mission arcs telegraph their resolution — crest bucket hues
    /// (orange exposed / purple defilade / red neutral) and the
    /// dimmed dashed area-fire language must reach the interior palette.
    #[test]
    fn fire_mission_arcs_telegraph_crest_and_area_fire() {
        use tactical_core::unit::UnitType;
        let mut state = TacticalState::default();
        let mut grid = HexGrid::new(12, 12, Terrain::Plains);
        // Exposed target (2,7): a local bump — every approach step is LOWER.
        grid.cell_mut(HexCoord::new(2, 7)).unwrap().elevation = 1;
        // Defilade target (9,7): a pit in a raised block — every approach
        // step is HIGHER.
        for q in 7..=11 {
            for r in 5..=9 {
                grid.cell_mut(HexCoord::new(q, r)).unwrap().elevation = 2;
            }
        }
        grid.cell_mut(HexCoord::new(9, 7)).unwrap().elevation = 0;
        state.grid = Some(Arc::new(grid));
        let gun = |id| {
            BattalionUnit::new(
                id,
                "A",
                UnitType::ArtilleryBrigade,
                Side::Attacker,
                HexCoord::new(2, 2),
            )
        };
        state.units.push(gun(1));
        state.units.push(gun(2));
        state.units.push(gun(3));
        state.units.push(gun(4));
        let fm = |attacker, hex, precise| tactical_combat::AttackOrder {
            attacker,
            target: AttackTarget::FireMission { hex, precise },
        };
        state.attack_orders.push(fm(1, HexCoord::new(2, 7), true)); // exposed
        state.attack_orders.push(fm(2, HexCoord::new(9, 7), true)); // defilade
        state.attack_orders.push(fm(3, HexCoord::new(5, 10), true)); // neutral
        state.attack_orders.push(fm(4, HexCoord::new(5, 10), false)); // area
        let (_, (_, i_img)) = build_attack_arrows_mesh(&state, &CombatParams::default());
        let has = |rgb: [u8; 3], tol: i32| {
            i_img.data.chunks(4).any(|p| {
                (p[0] as i32 - rgb[0] as i32).abs() <= tol
                    && (p[1] as i32 - rgb[1] as i32).abs() <= tol
                    && (p[2] as i32 - rgb[2] as i32).abs() <= tol
            })
        };
        assert!(has([217, 38, 115], 6), "exposed-crest magenta missing");
        assert!(has([242, 166, 26], 6), "defilade amber missing");
        assert!(has([255, 64, 31], 6), "precise neutral red missing");
        assert!(has([166, 41, 20], 8), "area dim red missing");
    }

    /// Direction guard: a PRECISE mission draws a solid arc; the same
    /// mission as AREA fire runs dashed (fewer arc segments) AND
    /// carries the 7-hex blast-zone outlines (extra geometry). Exact
    /// triangle counts pin both directions against regression.
    #[test]
    fn precise_arc_is_solid_area_arc_is_dashed() {
        use tactical_core::unit::UnitType;
        let tri_count = |precise: bool| -> usize {
            let mut state = TacticalState::default();
            state.grid = Some(Arc::new(HexGrid::new(12, 12, Terrain::Plains)));
            state.units.push(BattalionUnit::new(
                1,
                "A",
                UnitType::ArtilleryBrigade,
                Side::Attacker,
                HexCoord::new(2, 2),
            ));
            state.attack_orders.push(tactical_combat::AttackOrder {
                attacker: 1,
                target: AttackTarget::FireMission {
                    hex: HexCoord::new(8, 8),
                    precise,
                },
            });
            let ((b_mesh, _), (i_mesh, _)) =
                build_attack_arrows_mesh(&state, &CombatParams::default());
            let count = |m: &Mesh| m.indices().map(|i| i.len() / 3).unwrap_or(0);
            count(&b_mesh) + count(&i_mesh)
        };
        // Per pass: solid = 14 arc quads + arrowhead + diamond = 31 tris;
        // area = 7 arc quads + arrowhead + diamond + 7 hex outlines
        // (7×6 quads = 84 tris) = 101 tris.
        assert_eq!(tri_count(true), 2 * 31, "precise arc must be solid");
        assert_eq!(
            tri_count(false),
            2 * 101,
            "area arc must be dashed + zone outlines"
        );
    }

    /// Merged props: every prop on the board bakes into ONE mesh (one draw
    /// call instead of one entity per tree/building).
    #[test]
    fn props_merge_into_one_mesh() {
        let mut state = TacticalState::default();
        let mut grid = HexGrid::new(4, 4, Terrain::Plains);
        grid.set_terrain(HexCoord::new(0, 0), Terrain::Forest); // 2 trees = 96 verts
        grid.set_terrain(HexCoord::new(1, 0), Terrain::Mountain); // 2 boxes = 48
        grid.set_terrain(HexCoord::new(2, 0), Terrain::Marsh); // plate = 42
        state.grid = Some(Arc::new(grid));
        let (mesh, _img) = build_props_mesh(&state).expect("props exist");
        assert_eq!(vertex_count(&mesh), 96 + 48 + 42);
        // A plains-only board has no props at all.
        let mut empty = TacticalState::default();
        empty.grid = Some(Arc::new(HexGrid::new(4, 4, Terrain::Plains)));
        assert!(build_props_mesh(&empty).is_none());
    }
}
