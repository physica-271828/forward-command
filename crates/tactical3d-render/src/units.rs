//! Unit entity management: spawn/despawn miniatures to match state.units,
//! selection ring, facing, and smooth slide animation along movement paths.

use std::collections::HashMap;

use bevy::pbr::NotShadowCaster;
use bevy::prelude::*;
use tactical_core::fog::VisibilityState;
use tactical_core::hex::HexCoord;
use tactical_core::unit::{BattalionUnit, GunState, ModelFamily, Side, UnitState};
use tactical_sync::BattlePhase;

use crate::game::GameController;
use crate::models::{build_selection_ring_mesh, build_unit_mesh, SideColors, TagColors};
use crate::picking::unit_y_on_grid;
use crate::state::TacticalState;

#[derive(Component)]
pub struct UnitVisual {
    pub unit_id: usize,
}

#[derive(Component)]
pub struct SelectionRing;

/// Max-range circle shown for the selected ranged unit (attack_range > 1).
#[derive(Component)]
pub struct RangeRing;

/// Ghost preview of the unit being drag-deployed. A separate
/// per-frame system owns these — they follow the cursor, so they are not
/// tied to the `units_dirty` rebuild cadence.
#[derive(Component)]
pub struct DeployGhost;

/// Slide animation along a path of world waypoints.
#[derive(Component)]
pub struct MoveAnimation {
    pub waypoints: Vec<Vec3>,
    pub index: usize,
    pub speed: f32,
}

/// Pending slide animations keyed by unit id. Written by
/// `execute_movement` when a unit advances (waypoints = the hexes crossed,
/// in world coords); attached to the freshly respawned model by
/// [`apply_move_anims`]. Living in a resource (not on the entity) means the
/// animation survives the `units_dirty` full respawn.
#[derive(Resource, Default)]
pub struct MoveAnims(pub std::collections::HashMap<usize, Vec<Vec3>>);

/// Attach pending animations to (re)spawned unit models: snap the model
/// back to the first waypoint, then `animate_moves` slides it forward.
/// A unit already sliding keeps its current visual position and redirects
/// into the new path from `index = 0` — dropping its entry here (the old
/// `Without<MoveAnimation>` + `clear()` behaviour) stranded the model at
/// the old hex until the next full respawn.
/// Entries without a matching model are dropped — waypoints are only pushed
/// for units visible at the march's start, so nothing leaks.
pub fn apply_move_anims(
    mut commands: Commands,
    mut anims: ResMut<MoveAnims>,
    mut q_units: Query<(Entity, &UnitVisual, &mut Transform, Has<MoveAnimation>)>,
) {
    if anims.0.is_empty() {
        return;
    }
    let mut attached = Vec::new();
    for (entity, uv, mut transform, mid_slide) in q_units.iter_mut() {
        let Some(waypoints) = anims.0.get(&uv.unit_id) else {
            continue;
        };
        if waypoints.is_empty() {
            attached.push(uv.unit_id);
            continue;
        }
        // Freshly (re)spawned model: snap back to the march's first waypoint
        // and slide forward. Mid-slide model: keep the visual position and
        // redirect into the new path from index 0 (no snap, no teleport).
        let index = if mid_slide {
            0
        } else {
            transform.translation = waypoints[0];
            1
        };
        commands.entity(entity).insert(MoveAnimation {
            waypoints: waypoints.clone(),
            index,
            speed: 4.5, // world units per second (~2.6 hexes/s: step = √3)
        });
        attached.push(uv.unit_id);
    }
    for id in attached {
        anims.0.remove(&id);
    }
    // Anything left over has no rendered model this frame — drop it rather
    // than animate later from a stale (possibly fog-hidden) origin.
    anims.0.clear();
}

/// Cache of shared meshes/materials for unit rendering. The
/// key carries the unit's country tag — the base plate color is baked into
/// the mesh, so each nation's plates need their own entry ("" = side color).
type UnitMeshKey = (ModelFamily, Side, GunState, String);

#[derive(Resource, Default)]
pub struct UnitMeshCache {
    pub models: HashMap<UnitMeshKey, (Handle<Mesh>, Handle<StandardMaterial>)>,
    /// Translucent "spent this turn" material variants keyed like `models`
    /// (acted units render at 35% opacity; the mesh is
    /// shared, only the material differs).
    pub spent_models: HashMap<UnitMeshKey, Handle<StandardMaterial>>,
    pub ring: Option<(Handle<Mesh>, Handle<StandardMaterial>)>,
    /// Shared translucent material for drag-deploy ghosts.
    pub ghost_mat: Option<Handle<StandardMaterial>>,
}

/// The base-plate color of a unit — its country-tag color when
/// the battle knows one, the side color otherwise.
fn plate_color(unit: &BattalionUnit, colors: &SideColors, tags: &TagColors) -> [f32; 4] {
    tags.get(&unit.tag)
        .unwrap_or_else(|| colors.for_side(unit.side))
}

impl UnitMeshCache {
    #[allow(clippy::too_many_arguments)]
    fn model(
        &mut self,
        key: UnitMeshKey,
        plate: [f32; 4],
        meshes: &mut Assets<Mesh>,
        images: &mut Assets<Image>,
        materials: &mut Assets<StandardMaterial>,
    ) -> (Handle<Mesh>, Handle<StandardMaterial>) {
        self.models
            .entry(key.clone())
            .or_insert_with(|| {
                let (mesh, image) = build_unit_mesh(key.0, key.1, plate, key.2);
                let mat = materials.add(StandardMaterial {
                    base_color: Color::WHITE,
                    base_color_texture: Some(images.add(image)),
                    perceptual_roughness: 0.85,
                    ..default()
                });
                (meshes.add(mesh), mat)
            })
            .clone()
    }

    fn ring(
        &mut self,
        meshes: &mut Assets<Mesh>,
        images: &mut Assets<Image>,
        materials: &mut Assets<StandardMaterial>,
    ) -> (Handle<Mesh>, Handle<StandardMaterial>) {
        self.ring
            .get_or_insert_with(|| {
                let (mesh, image) = build_selection_ring_mesh();
                let mat = materials.add(StandardMaterial {
                    base_color: Color::WHITE,
                    base_color_texture: Some(images.add(image)),
                    perceptual_roughness: 0.6,
                    emissive: LinearRgba::new(0.8, 0.7, 0.1, 1.0),
                    ..default()
                });
                (meshes.add(mesh), mat)
            })
            .clone()
    }

    /// Pale-green translucent "shadow" material for the deploy ghost.
    fn ghost_material(
        &mut self,
        materials: &mut Assets<StandardMaterial>,
    ) -> Handle<StandardMaterial> {
        self.ghost_mat
            .get_or_insert_with(|| {
                materials.add(StandardMaterial {
                    base_color: Color::srgba(0.55, 0.95, 0.55, 0.45),
                    alpha_mode: AlphaMode::Blend,
                    emissive: LinearRgba::new(0.1, 0.4, 0.1, 1.0),
                    perceptual_roughness: 0.7,
                    ..default()
                })
            })
            .clone()
    }

    /// Translucent variant of a unit material for units that have already
    /// acted this turn (wargame "spent" dimming). Reuses the same palette
    /// texture; alpha 0.35.
    fn spent_material(
        &mut self,
        key: UnitMeshKey,
        base: &Handle<StandardMaterial>,
        materials: &mut Assets<StandardMaterial>,
    ) -> Handle<StandardMaterial> {
        self.spent_models
            .entry(key)
            .or_insert_with(|| {
                let tex = materials
                    .get(base)
                    .and_then(|m| m.base_color_texture.clone());
                materials.add(StandardMaterial {
                    base_color: Color::srgba(1.0, 1.0, 1.0, 0.35),
                    base_color_texture: tex,
                    alpha_mode: AlphaMode::Blend,
                    perceptual_roughness: 0.85,
                    ..default()
                })
            })
            .clone()
    }
}

/// Rebuild all unit visuals when `units_dirty` is set (full respawn — simple,
/// and battles have ≤ ~40 units).
pub fn sync_unit_visuals(
    mut commands: Commands,
    mut state: ResMut<TacticalState>,
    game: Option<Res<GameController>>,
    colors: Res<SideColors>,
    tags: Res<TagColors>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut cache: ResMut<UnitMeshCache>,
    q_units: Query<(Entity, &UnitVisual, Has<MoveAnimation>)>,
    q_rings: Query<Entity, With<SelectionRing>>,
    q_range_rings: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>), With<RangeRing>>,
) {
    if !state.units_dirty {
        return;
    }
    state.units_dirty = false;

    // Units mid-slide KEEP their entity: despawning them would kill the
    // MoveAnimation and snap the model to its final hex — the persistent
    // "瞬移" (AI-turn actions set units_dirty every 0.35s tick).
    let mut sliding = std::collections::HashSet::new();
    for (e, uv, has_anim) in q_units.iter() {
        if has_anim {
            sliding.insert(uv.unit_id);
        } else {
            commands.entity(e).despawn();
        }
    }
    for e in q_rings.iter() {
        commands.entity(e).despawn();
    }
    // Range rings own per-entity mesh/material/image (built per selection)
    // — reclaim those assets instead of leaking a set per rebuild.
    // (Selection rings and unit models use the shared cache — never freed.)
    crate::board::despawn_visuals(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        q_range_rings
            .iter()
            .map(|(e, m, mat)| (e, m.0.clone(), mat.0.clone())),
    );

    // During Deployment the enemy AI has not placed
    // its units yet — render nothing of the enemy side (fog starts only at
    // BeginBattle, so the fog check below cannot cover this).
    let deploying = game
        .as_deref()
        .map(|g| g.session.phase == BattlePhase::Deployment)
        .unwrap_or(false);

    // Spawn facing: aim each unit at the opposing side's centroid (models
    // face +X; angle audit — defenders used to spawn facing east = away).
    let centroid_of = |side: Side| -> Option<Vec3> {
        let mut sum = Vec3::ZERO;
        let mut n = 0u32;
        for u in &state.units {
            if u.side == side && u.is_combat_effective() {
                let (x, z) = u.position.to_world(crate::board::HEX_SIZE);
                sum += Vec3::new(x, 0.0, z);
                n += 1;
            }
        }
        (n > 0).then(|| sum / n as f32)
    };
    let atk_c = centroid_of(Side::Attacker);
    let def_c = centroid_of(Side::Defender);

    for unit in &state.units {
        // Battle-report deferral: a unit whose engagement report
        // is still unconfirmed keeps rendering at its pre-combat hex even
        // though its resolved state already moved / eliminated / surrendered
        // it. The ghost bypasses the alive check but NOT fog/deploy checks —
        // and the fog check scores the ghost hex (that is what renders).
        let ghost = state.report_ghosts.get(&unit.id).copied();
        let alive = unit.is_combat_effective() || unit.state == UnitState::Retreating;
        if !alive && ghost.is_none() {
            continue;
        }
        if sliding.contains(&unit.id) {
            continue; // mid-slide: its entity (and animation) survived the rebuild
        }
        // Units still in the OOB have no
        // place on the board — nothing renders until placed (OFFBOARD).
        if unit.undeployed {
            continue;
        }
        if deploying && unit.side != state.player_side {
            continue; // enemy AI has not deployed yet
        }
        // Fog: enemy units not currently visible are not rendered.
        let render_pos = ghost.unwrap_or(unit.position);
        if unit.side != state.player_side && state.fog_state(render_pos) != VisibilityState::Visible
        {
            continue;
        }
        let key = (
            unit.model_family(),
            unit.side,
            unit.gun_state(),
            unit.tag.clone(),
        );
        let (mesh, mat) = cache.model(
            key.clone(),
            plate_color(unit, &colors, &tags),
            &mut meshes,
            &mut images,
            &mut materials,
        );
        // Acted units render translucent (wargame "spent" dimming; turn
        // reset re-flags units_dirty so the solid look returns on refresh).
        let mat = if unit.acted {
            cache.spent_material(key, &mat, &mut materials)
        } else {
            mat
        };
        let y = unit_y_on_grid(&state, render_pos);
        let (x, z) = render_pos.to_world(crate::board::HEX_SIZE);
        let mut transform = Transform::from_xyz(x, y, z);
        // Persisted facing wins (a respawn must not snap the model back to
        // the spawn default every turn); only a never-moved unit aims at
        // the enemy centroid.
        if let Some(&ry) = state.unit_facing.get(&unit.id) {
            transform.rotation = Quat::from_rotation_y(ry);
        } else {
            let foe = match unit.side {
                Side::Attacker => def_c,
                Side::Defender => atk_c,
            };
            if let Some(f) = foe {
                let d = f - Vec3::new(x, 0.0, z);
                if d.length_squared() > 1e-4 {
                    transform.rotation = Quat::from_rotation_y(-d.z.atan2(d.x));
                }
            }
        }
        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            transform,
            UnitVisual { unit_id: unit.id },
        ));
    }

    // Selection ring under the selected unit.
    if let Some(id) = state.selected_unit {
        if let Some(unit) = state.unit_by_id(id) {
            let (ring, mat) = cache.ring(&mut meshes, &mut images, &mut materials);
            let y = unit_y_on_grid(&state, unit.position) + 0.01;
            let (x, z) = unit.position.to_world(crate::board::HEX_SIZE);
            commands.spawn((
                Mesh3d(ring),
                MeshMaterial3d(mat),
                Transform::from_xyz(x, y, z),
                SelectionRing,
                NotShadowCaster, // flat ring: casts no visible shadow
            ));
            // Ranged unit selected: show the max-range circle (§6.3). Only
            // hexes whose center is inside it can be fired at / supported.
            // Uniform red regardless of side (deliberate choice over
            // side-theme coloring).
            if unit.attack_range > 1 {
                let color = [0.85, 0.15, 0.12, 1.0];
                let (mesh, image) = crate::board::build_range_ring_mesh(
                    &state,
                    unit.position,
                    unit.attack_range,
                    color,
                    false,
                );
                let mat = materials.add(StandardMaterial {
                    base_color: Color::WHITE,
                    base_color_texture: Some(images.add(image)),
                    perceptual_roughness: 0.6,
                    emissive: LinearRgba::new(color[0] * 0.5, color[1] * 0.5, color[2] * 0.5, 1.0),
                    cull_mode: None,
                    ..default()
                });
                commands.spawn((
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(mat),
                    Transform::default(),
                    RangeRing,
                    NotShadowCaster, // flat ring: casts no visible shadow
                ));
            }
            // Rocket artillery also shows its MINIMUM range — an
            // orange dashed circle through the dead-zone hexes (inside it =
            // "Too close"). Radius min-1 mirrors the max ring convention
            // (through the centers of the boundary hexes).
            if unit.min_attack_range() > 1 {
                let color = [0.95, 0.55, 0.10, 1.0];
                let (mesh, image) = crate::board::build_range_ring_mesh(
                    &state,
                    unit.position,
                    unit.min_attack_range() - 1,
                    color,
                    true,
                );
                let mat = materials.add(StandardMaterial {
                    base_color: Color::WHITE,
                    base_color_texture: Some(images.add(image)),
                    perceptual_roughness: 0.6,
                    emissive: LinearRgba::new(color[0] * 0.5, color[1] * 0.5, color[2] * 0.5, 1.0),
                    cull_mode: None,
                    ..default()
                });
                commands.spawn((
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(mat),
                    Transform::default(),
                    RangeRing,
                    NotShadowCaster, // flat ring: casts no visible shadow
                ));
            }
        }
    }
}

/// Per-frame drag-deploy ghost. Runs independently of the
/// `units_dirty` rebuild: despawns yesterday's ghost, hides the dragged
/// unit's real model, and respawns a translucent copy under the cursor when
/// the hovered hex is a legal drop (own zone, deployable, unoccupied). At
/// invalid hexes no ghost is shown (by design). Also
/// serves OOB placement — `deploy_placing` (unit picked in the OOB window)
/// previews the same ghost without hiding anything (the unit is not on the
/// board yet). Sector deployment: while the player drags a
/// division's rectangle, `state.sector_preview` ghosts every battalion at
/// the position the planner WOULD give it.
#[allow(clippy::too_many_arguments)]
pub fn sync_deploy_ghost(
    mut commands: Commands,
    state: Res<TacticalState>,
    game: Option<Res<GameController>>,
    colors: Res<SideColors>,
    tags: Res<TagColors>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut cache: ResMut<UnitMeshCache>,
    q_ghosts: Query<Entity, With<DeployGhost>>,
    mut q_units: Query<(&UnitVisual, &mut Visibility)>,
) {
    for e in q_ghosts.iter() {
        commands.entity(e).despawn();
    }
    let active_id = state.deploy_drag.or(state.deploy_placing);
    let dragging = game
        .as_deref()
        .map(|g| g.session.phase == BattlePhase::Deployment)
        .unwrap_or(false)
        && active_id.is_some();
    for (uv, mut vis) in q_units.iter_mut() {
        let hide = dragging && state.deploy_drag.is_some() && Some(uv.unit_id) == active_id;
        let want = if hide {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
        if *vis != want {
            *vis = want;
        }
    }
    let Some(grid) = state.grid.as_ref() else {
        return;
    };
    // Shared ghost spawning: same model, ghost material, facing the enemy
    // centroid (mirrors sync_unit_visuals; enemy units exist in state even
    // though they are not rendered during deployment).
    let spawn_ghost = |commands: &mut Commands,
                       cache: &mut UnitMeshCache,
                       meshes: &mut Assets<Mesh>,
                       images: &mut Assets<Image>,
                       materials: &mut Assets<StandardMaterial>,
                       unit: &tactical_core::unit::BattalionUnit,
                       hex: HexCoord| {
        let key = (
            unit.model_family(),
            unit.side,
            unit.gun_state(),
            unit.tag.clone(),
        );
        let (mesh, _) = cache.model(
            key,
            plate_color(unit, &colors, &tags),
            meshes,
            images,
            materials,
        );
        let mat = cache.ghost_material(materials);
        let y = unit_y_on_grid(&state, hex);
        let (x, z) = hex.to_world(crate::board::HEX_SIZE);
        let mut transform = Transform::from_xyz(x, y, z);
        let foe_side = match unit.side {
            Side::Attacker => Side::Defender,
            Side::Defender => Side::Attacker,
        };
        let mut sum = Vec3::ZERO;
        let mut n = 0u32;
        for u in &state.units {
            if u.side == foe_side && u.is_combat_effective() {
                let (fx, fz) = u.position.to_world(crate::board::HEX_SIZE);
                sum += Vec3::new(fx, 0.0, fz);
                n += 1;
            }
        }
        if n > 0 {
            let d = (sum / n as f32) - Vec3::new(x, 0.0, z);
            if d.length_squared() > 1e-4 {
                transform.rotation = Quat::from_rotation_y(-d.z.atan2(d.x));
            }
        }
        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            transform,
            DeployGhost,
            NotShadowCaster,
        )); // translucent ghost: no shadow
    };

    if dragging {
        let (Some(id), Some(h)) = (active_id, state.hover_hex) else {
            return;
        };
        let Some(unit) = state.units.iter().find(|u| u.id == id) else {
            return;
        };
        if !grid.in_bounds(h) || state.unit_at(h).is_some() {
            return;
        }
        let zone_ok = state
            .deployment_zones
            .as_ref()
            .map(|(a, d)| {
                let z = if state.player_side == Side::Attacker {
                    a
                } else {
                    d
                };
                z.contains(&h)
            })
            .unwrap_or(true);
        let deployable = grid
            .cell(h)
            .map(|c| c.is_passable && c.terrain.is_deployable())
            .unwrap_or(false);
        if !zone_ok || !deployable {
            return;
        }
        spawn_ghost(
            &mut commands,
            &mut cache,
            &mut meshes,
            &mut images,
            &mut materials,
            unit,
            h,
        );
    }

    // Sector preview: every battalion of the dragged division at
    // its planner-computed position (the sector was already zone-filtered).
    for (uid, hex) in &state.sector_preview {
        let Some(unit) = state.units.iter().find(|u| u.id == *uid) else {
            continue;
        };
        if !grid.in_bounds(*hex) {
            continue;
        }
        spawn_ghost(
            &mut commands,
            &mut cache,
            &mut meshes,
            &mut images,
            &mut materials,
            unit,
            *hex,
        );
    }
}

/// Advance move animations (slide + face travel direction). The facing is
/// also recorded into `TacticalState::unit_facing` so the next
/// `units_dirty` respawn keeps it instead of snapping to the spawn default.
pub fn animate_moves(
    mut commands: Commands,
    time: Res<Time>,
    mut state: ResMut<TacticalState>,
    mut q: Query<(Entity, &mut Transform, &mut MoveAnimation, &UnitVisual)>,
) {
    for (entity, mut transform, mut anim, vis) in q.iter_mut() {
        let Some(target) = anim.waypoints.get(anim.index) else {
            commands.entity(entity).remove::<MoveAnimation>();
            continue;
        };
        let pos = transform.translation;
        let to = *target - pos;
        let dist = to.length();
        let step = anim.speed * time.delta_secs();
        if dist <= step {
            transform.translation = *target;
            anim.index += 1;
            if anim.index >= anim.waypoints.len() {
                commands.entity(entity).remove::<MoveAnimation>();
            }
        } else {
            let dir = to.normalize();
            transform.translation = pos + dir * step;
            // Face travel direction (model +X forward).
            let ry = -dir.z.atan2(dir.x);
            transform.rotation = Quat::from_rotation_y(ry);
            state.unit_facing.insert(vis.unit_id, ry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slide writes the travel facing into `unit_facing` so the next
    /// respawn keeps it (no per-turn snap-back to the spawn default).
    #[test]
    fn sliding_records_the_travel_facing_into_state() {
        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .insert_resource(TacticalState::default())
            .add_systems(Update, animate_moves);
        let e = app
            .world_mut()
            .spawn((
                Transform::from_xyz(0.0, 0.0, 0.0),
                MoveAnimation {
                    waypoints: vec![Vec3::new(10.0, 0.0, 10.0)],
                    index: 0,
                    speed: 1.0,
                },
                UnitVisual { unit_id: 7 },
            ))
            .id();
        app.update();
        let ry = *app
            .world()
            .resource::<TacticalState>()
            .unit_facing
            .get(&7)
            .expect("facing recorded");
        let want = -std::f32::consts::FRAC_PI_4; // travel dir (10, 10): -atan2(10, 10)
        assert!((ry - want).abs() < 1e-5, "recorded yaw {ry} ≈ {want}");
        let t = app.world().entity(e).get::<Transform>().unwrap();
        let (axis, angle) = t.rotation.to_axis_angle();
        let (_, want_angle) = Quat::from_rotation_y(want).to_axis_angle();
        assert!((angle - want_angle).abs() < 1e-4, "model faces travel: {axis:?} {angle}");
    }
}
