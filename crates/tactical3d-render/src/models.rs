//! Procedural blocky unit miniatures ("pixel/voxel wargame" style).
//! One merged vertex-colored mesh per (ModelFamily, Side, GunState, tag),
//! cached and shared. Silhouette first: each family must be recognizable
//! from the RTS camera. Towed guns have DISTINCT emplaced / in-tow models
//! (the limbered state shows the horse team or tow truck).

use std::collections::HashMap;

use bevy::prelude::*;
use tactical_core::unit::{GunState, ModelFamily, Side};

use crate::mesh_build::{scale_color, MeshBuilder};

/// Faction uniform colors.
pub fn uniform_color(side: Side) -> [f32; 4] {
    match side {
        // Attacker: feldgrau (German gray-green)
        Side::Attacker => [0.32, 0.36, 0.28, 1.0],
        // Defender: khaki drab
        Side::Defender => [0.48, 0.42, 0.28, 1.0],
    }
}

/// Attacker/defender theme colors: bound to the HOI4 map colors
/// of the battling countries (`data/country_colors.json`) by the bin crate;
/// falls back to classic wargame blue vs red. Inserted once per battle and
/// never mutated afterwards (the unit mesh cache keys on Side only).
#[derive(Resource, Debug, Clone, Copy)]
pub struct SideColors {
    pub attacker: [f32; 4],
    pub defender: [f32; 4],
}

impl SideColors {
    pub fn for_side(&self, side: Side) -> [f32; 4] {
        match side {
            Side::Attacker => self.attacker,
            Side::Defender => self.defender,
        }
    }
}

impl Default for SideColors {
    fn default() -> Self {
        SideColors {
            attacker: [0.20, 0.38, 0.72, 1.0],
            defender: [0.72, 0.24, 0.20, 1.0],
        }
    }
}

/// Per-country base-plate colors (DESIGN §7.5): country tag →
/// its HOI4 map color, filled by the bin crate for every tag the battle
/// fields (script division→tag table). A unit whose tag is empty or absent
/// here renders with the plain side color.
#[derive(Resource, Debug, Clone, Default)]
pub struct TagColors(pub HashMap<String, [f32; 4]>);

impl TagColors {
    pub fn get(&self, tag: &str) -> Option<[f32; 4]> {
        self.0.get(tag).copied()
    }
}

const DARK: [f32; 4] = [0.16, 0.16, 0.17, 1.0]; // tires, tracks, boots
const GUNMETAL: [f32; 4] = [0.28, 0.29, 0.30, 1.0];
const WOOD: [f32; 4] = [0.42, 0.30, 0.18, 1.0];
const CANVAS: [f32; 4] = [0.55, 0.52, 0.40, 1.0];
const SKIN: [f32; 4] = [0.75, 0.60, 0.45, 1.0];
const WHITE: [f32; 4] = [0.85, 0.85, 0.82, 1.0]; // winter / mountain gear
const HORSE: [f32; 4] = [0.38, 0.26, 0.15, 1.0];

fn darker(c: [f32; 4]) -> [f32; 4] {
    scale_color(c, 0.72)
}

/// Unit base plate geometry: round, coin-stack thick
/// (thickness : radius ≈ two stacked 1-yuan coins ≈ 0.3).
pub const BASE_RADIUS: f32 = 0.52;
pub const BASE_THICKNESS: f32 = 0.15;

/// Build the merged mesh (base plate + model) for a family+side+carriage.
/// The base plate takes `plate_color` — the side color, or
/// the unit's country-tag color. Model faces +X; rotate the entity to face
/// travel direction.
pub fn build_unit_mesh(
    family: ModelFamily,
    side: Side,
    plate_color: [f32; 4],
    gun: GunState,
) -> (Mesh, Image) {
    let mut mb = MeshBuilder::new();
    // Base plate: round counter in the side's (or the unit's nation's) color.
    mb.add_cylinder(
        Vec3::ZERO,
        BASE_RADIUS,
        0.0,
        BASE_THICKNESS,
        plate_color,
        28,
    );
    let u = uniform_color(side);
    let y0 = BASE_THICKNESS; // model sits on the plate

    match family {
        ModelFamily::Infantry => infantry(&mut mb, u, y0),
        ModelFamily::Marine => infantry_geared(&mut mb, u, y0, Helmet::Canvas, Pack::Satchel),
        ModelFamily::Mountaineer => infantry_geared(&mut mb, u, y0, Helmet::White, Pack::Tall),
        ModelFamily::Paratrooper => infantry_geared(&mut mb, u, y0, Helmet::Round, Pack::Chute),
        ModelFamily::Bicycle => bicycle(&mut mb, u, y0),
        ModelFamily::Cavalry => cavalry(&mut mb, u, y0),
        // Motorized / mechanized infantry: the carrier AND its dismounts
        // stand side by side.
        ModelFamily::Truck => {
            truck(&mut mb, u, y0);
            dismounts(&mut mb, u, y0);
        }
        ModelFamily::Halftrack => {
            halftrack(&mut mb, u, y0);
            dismounts(&mut mb, u, y0);
        }
        ModelFamily::TankLight => tank(&mut mb, u, y0, 0.42, 0.30, 0.20, 0.26),
        ModelFamily::TankMedium => tank(&mut mb, u, y0, 0.52, 0.34, 0.24, 0.34),
        ModelFamily::TankHeavy => heavy_tank(&mut mb, u, y0),
        ModelFamily::TankSuperHeavy => super_heavy_tank(&mut mb, u, y0),
        ModelFamily::TankModern => modern_tank(&mut mb, u, y0),
        ModelFamily::TankAmphibious => amphibious_tank(&mut mb, u, y0),
        ModelFamily::Artillery => match gun {
            GunState::Emplaced => gun_emplaced(&mut mb, u, y0, 0.40, false),
            _ => gun_limbered(&mut mb, u, y0, 0.40, false, gun),
        },
        ModelFamily::AntiTank => match gun {
            GunState::Emplaced => gun_emplaced(&mut mb, u, y0, 0.44, true),
            _ => gun_limbered(&mut mb, u, y0, 0.44, true, gun),
        },
        ModelFamily::AntiAir => match gun {
            GunState::Emplaced => aa_emplaced(&mut mb, u, y0),
            _ => aa_limbered(&mut mb, u, y0, gun),
        },
        ModelFamily::RocketTowed => match gun {
            GunState::Emplaced => rocket_towed_emplaced(&mut mb, u, y0),
            _ => rocket_towed_limbered(&mut mb, u, y0, gun),
        },
        // Truck-mounted Katyusha: single model, no carriage states.
        ModelFamily::RocketArtillery => rocket_truck(&mut mb, u, y0),
        ModelFamily::ArmoredCar => armored_car(&mut mb, u, y0),
        ModelFamily::Jeep => jeep(&mut mb, u, y0),
    }
    mb.build()
}

// ────────────────────────────────────────────────────────────────────────────
// Soldiers
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Helmet {
    /// Standard helmet with a brim (darker uniform color).
    Brim,
    /// Canvas helmet cover (marines).
    Canvas,
    /// Winter-white helmet (mountain troops).
    White,
    /// Brimless jump helmet (paratroopers).
    Round,
}

#[derive(Clone, Copy)]
enum Pack {
    None,
    /// Canvas backpack (marines).
    Satchel,
    /// Tall mountain rucksack.
    Tall,
    /// Parachute pack.
    Chute,
}

/// One blocky soldier at (dx, dz), facing +X, rifle forward.
fn soldier(
    mb: &mut MeshBuilder,
    dx: f32,
    dz: f32,
    y0: f32,
    u: [f32; 4],
    helmet: Helmet,
    pack: Pack,
) {
    // legs/boots
    mb.add_box_c(
        Vec3::new(dx, y0 + 0.05, dz),
        Vec3::new(0.09, 0.10, 0.09),
        DARK,
    );
    // body
    mb.add_box_c(Vec3::new(dx, y0 + 0.17, dz), Vec3::new(0.11, 0.14, 0.11), u);
    // head
    mb.add_box_c(
        Vec3::new(dx, y0 + 0.27, dz),
        Vec3::new(0.08, 0.07, 0.08),
        SKIN,
    );
    match helmet {
        Helmet::Brim => {
            mb.add_box_c(
                Vec3::new(dx, y0 + 0.31, dz),
                Vec3::new(0.10, 0.03, 0.10),
                darker(u),
            );
        }
        Helmet::Canvas => {
            mb.add_box_c(
                Vec3::new(dx, y0 + 0.31, dz),
                Vec3::new(0.10, 0.03, 0.10),
                CANVAS,
            );
        }
        Helmet::White => {
            mb.add_box_c(
                Vec3::new(dx, y0 + 0.31, dz),
                Vec3::new(0.10, 0.03, 0.10),
                WHITE,
            );
        }
        Helmet::Round => {
            // brimless: taller crown, no overhanging brim
            mb.add_box_c(
                Vec3::new(dx, y0 + 0.30, dz),
                Vec3::new(0.09, 0.05, 0.09),
                darker(u),
            );
        }
    }
    match pack {
        Pack::None => {}
        Pack::Satchel => {
            mb.add_box_c(
                Vec3::new(dx - 0.07, y0 + 0.18, dz),
                Vec3::new(0.05, 0.10, 0.10),
                CANVAS,
            );
        }
        Pack::Tall => {
            mb.add_box_c(
                Vec3::new(dx - 0.07, y0 + 0.20, dz),
                Vec3::new(0.06, 0.16, 0.11),
                CANVAS,
            );
        }
        Pack::Chute => {
            mb.add_box_c(
                Vec3::new(dx - 0.07, y0 + 0.17, dz),
                Vec3::new(0.06, 0.12, 0.12),
                CANVAS,
            );
        }
    }
    // rifle
    mb.add_box_c(
        Vec3::new(dx + 0.08, y0 + 0.19, dz),
        Vec3::new(0.16, 0.025, 0.025),
        WOOD,
    );
}

/// 3 blocky soldiers in triangle formation, rifles forward.
fn infantry(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    infantry_geared(mb, u, y0, Helmet::Brim, Pack::None);
}

/// 3 soldiers with branch-specific gear (helmet + pack).
fn infantry_geared(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, helmet: Helmet, pack: Pack) {
    for (dx, dz) in [(0.10, 0.0), (-0.10, -0.14), (-0.10, 0.14)] {
        soldier(mb, dx, dz, y0, u, helmet, pack);
    }
}

/// 2 soldiers walking their bicycles (frame + wheels beside each man).
fn bicycle(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    for dz in [-0.14, 0.14] {
        soldier(mb, -0.05, dz, y0, u, Helmet::Brim, Pack::None);
        // bicycle to the man's right
        let bz = dz + 0.15;
        // wheels (thin metal plates, rolling along X)
        for bx in [-0.02, 0.14] {
            mb.add_box_c(
                Vec3::new(bx, y0 + 0.065, bz),
                Vec3::new(0.11, 0.11, 0.02),
                GUNMETAL,
            );
        }
        // frame: top tube + down tube
        mb.add_box_c(
            Vec3::new(0.06, y0 + 0.115, bz),
            Vec3::new(0.13, 0.02, 0.02),
            GUNMETAL,
        );
        mb.add_box_rot(
            Vec3::new(0.04, y0 + 0.075, bz),
            Vec3::new(0.11, 0.018, 0.018),
            Quat::from_rotation_z(0.5),
            GUNMETAL,
        );
        // handlebar + seat
        mb.add_box_c(
            Vec3::new(0.15, y0 + 0.15, bz),
            Vec3::new(0.03, 0.05, 0.08),
            GUNMETAL,
        );
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.145, bz),
            Vec3::new(0.04, 0.02, 0.05),
            DARK,
        );
    }
}

/// 2 dismount soldiers standing beside a carrier (motorized/mechanized).
fn dismounts(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    for dz in [-0.30, 0.30] {
        soldier(mb, -0.10, dz, y0, u, Helmet::Brim, Pack::Satchel);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Horses & cavalry
// ────────────────────────────────────────────────────────────────────────────

/// One horse (no rider), facing +X.
fn horse(mb: &mut MeshBuilder, x: f32, z: f32, y0: f32) {
    mb.add_box_c(
        Vec3::new(x, y0 + 0.13, z),
        Vec3::new(0.30, 0.13, 0.10),
        HORSE,
    );
    mb.add_box_c(
        Vec3::new(x + 0.17, y0 + 0.20, z),
        Vec3::new(0.09, 0.12, 0.07),
        HORSE,
    );
    mb.add_box_c(
        Vec3::new(x + 0.10, y0 + 0.03, z),
        Vec3::new(0.05, 0.07, 0.08),
        darker(HORSE),
    );
    mb.add_box_c(
        Vec3::new(x - 0.10, y0 + 0.03, z),
        Vec3::new(0.05, 0.07, 0.08),
        darker(HORSE),
    );
}

/// 2 blocky cavalry mounts with riders.
fn cavalry(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    for dz in [-0.12, 0.12] {
        horse(mb, 0.0, dz, y0);
        // rider
        mb.add_box_c(
            Vec3::new(-0.02, y0 + 0.28, dz),
            Vec3::new(0.09, 0.12, 0.09),
            u,
        );
        mb.add_box_c(
            Vec3::new(-0.02, y0 + 0.37, dz),
            Vec3::new(0.07, 0.06, 0.07),
            SKIN,
        );
        mb.add_box_c(
            Vec3::new(-0.02, y0 + 0.41, dz),
            Vec3::new(0.09, 0.025, 0.09),
            darker(u),
        );
    }
}

/// Horse team pulling a limbered gun (2 horses + driver).
fn horse_team(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, x_front: f32) {
    for dz in [-0.09, 0.09] {
        horse(mb, x_front, dz, y0);
    }
    // driver walking between the gun and the team
    soldier(mb, x_front - 0.24, 0.0, y0, u, Helmet::Brim, Pack::None);
}

// ────────────────────────────────────────────────────────────────────────────
// Wheeled / tracked carriers
// ────────────────────────────────────────────────────────────────────────────

/// Box truck: cab + canvas-covered bed + 6 wheels.
fn truck(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    // chassis
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.10, 0.0),
        Vec3::new(0.58, 0.08, 0.26),
        GUNMETAL,
    );
    // cargo bed + canvas
    mb.add_box_c(
        Vec3::new(-0.08, y0 + 0.19, 0.0),
        Vec3::new(0.38, 0.10, 0.28),
        u,
    );
    mb.add_box_c(
        Vec3::new(-0.08, y0 + 0.30, 0.0),
        Vec3::new(0.36, 0.12, 0.27),
        CANVAS,
    );
    // cab
    mb.add_box_c(
        Vec3::new(0.22, y0 + 0.21, 0.0),
        Vec3::new(0.16, 0.14, 0.27),
        u,
    );
    mb.add_box_c(
        Vec3::new(0.26, y0 + 0.30, 0.0),
        Vec3::new(0.09, 0.06, 0.25),
        darker(u),
    );
    // wheels
    for x in [0.20, -0.02, -0.22] {
        for z in [-0.14, 0.14] {
            mb.add_box_c(
                Vec3::new(x, y0 + 0.04, z),
                Vec3::new(0.09, 0.09, 0.05),
                DARK,
            );
        }
    }
}

/// Halftrack: armored body, front wheels + rear tracks.
fn halftrack(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.13, 0.0),
        Vec3::new(0.52, 0.14, 0.28),
        u,
    );
    mb.add_box_c(
        Vec3::new(0.16, y0 + 0.24, 0.0),
        Vec3::new(0.18, 0.09, 0.26),
        darker(u),
    );
    // rear tracks
    for z in [-0.16, 0.16] {
        mb.add_box_c(
            Vec3::new(-0.12, y0 + 0.06, z),
            Vec3::new(0.28, 0.11, 0.07),
            DARK,
        );
    }
    // front wheels
    for z in [-0.15, 0.15] {
        mb.add_box_c(
            Vec3::new(0.18, y0 + 0.05, z),
            Vec3::new(0.09, 0.09, 0.05),
            DARK,
        );
    }
}

/// Tank with size parameters. len = hull length, wid = hull width.
fn tank(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, len: f32, wid: f32, tur: f32, barrel: f32) {
    let h = 0.14 + (len - 0.42) * 0.25;
    // tracks
    for z in [-(wid / 2.0 + 0.035), wid / 2.0 + 0.035] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.06, z),
            Vec3::new(len + 0.06, 0.12, 0.08),
            DARK,
        );
    }
    // hull
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.12 + h / 2.0, 0.0),
        Vec3::new(len, h, wid),
        u,
    );
    // turret
    mb.add_box_c(
        Vec3::new(-0.02, y0 + 0.14 + h + 0.05, 0.0),
        Vec3::new(tur, 0.10, tur * 0.85),
        darker(u),
    );
    // barrel
    mb.add_box_c(
        Vec3::new(tur / 2.0 + barrel / 2.0 - 0.02, y0 + 0.14 + h + 0.06, 0.0),
        Vec3::new(barrel, 0.04, 0.04),
        GUNMETAL,
    );
}

/// Heavy tank: bigger, side skirts, thicker barrel.
fn heavy_tank(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    // tracks
    for z in [-0.24, 0.24] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.07, z),
            Vec3::new(0.66, 0.14, 0.09),
            DARK,
        );
    }
    // side skirts
    for z in [-0.24, 0.24] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.15, z),
            Vec3::new(0.62, 0.05, 0.10),
            darker(u),
        );
    }
    // hull
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.20, 0.0),
        Vec3::new(0.60, 0.18, 0.40),
        u,
    );
    // turret
    mb.add_box_c(
        Vec3::new(-0.03, y0 + 0.35, 0.0),
        Vec3::new(0.30, 0.12, 0.26),
        darker(u),
    );
    // barrel (thick)
    mb.add_box_c(
        Vec3::new(0.28, y0 + 0.36, 0.0),
        Vec3::new(0.42, 0.055, 0.055),
        GUNMETAL,
    );
    // muzzle brake
    mb.add_box_c(
        Vec3::new(0.48, y0 + 0.36, 0.0),
        Vec3::new(0.06, 0.08, 0.08),
        GUNMETAL,
    );
}

/// Super-heavy tank: the heaviest silhouette on the field — extra-wide
/// hull, dual-thickness barrel, deep side skirts.
fn super_heavy_tank(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    // tracks
    for z in [-0.27, 0.27] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.075, z),
            Vec3::new(0.72, 0.15, 0.10),
            DARK,
        );
    }
    // deep side skirts
    for z in [-0.27, 0.27] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.17, z),
            Vec3::new(0.68, 0.07, 0.11),
            darker(u),
        );
    }
    // hull
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.22, 0.0),
        Vec3::new(0.66, 0.20, 0.44),
        u,
    );
    // turret
    mb.add_box_c(
        Vec3::new(-0.04, y0 + 0.39, 0.0),
        Vec3::new(0.34, 0.14, 0.30),
        darker(u),
    );
    // barrel (extra thick + muzzle brake)
    mb.add_box_c(
        Vec3::new(0.30, y0 + 0.40, 0.0),
        Vec3::new(0.48, 0.065, 0.065),
        GUNMETAL,
    );
    mb.add_box_c(
        Vec3::new(0.52, y0 + 0.40, 0.0),
        Vec3::new(0.07, 0.09, 0.09),
        GUNMETAL,
    );
}

/// Modern tank: sloped glacis + low wedge turret + long smoothbore —
/// deliberately sleeker than the WWII silhouettes.
fn modern_tank(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    // tracks
    for z in [-0.19, 0.19] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.06, z),
            Vec3::new(0.58, 0.11, 0.08),
            DARK,
        );
    }
    // hull
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.16, 0.0),
        Vec3::new(0.54, 0.13, 0.32),
        u,
    );
    // sloped glacis plate
    mb.add_box_rot(
        Vec3::new(0.22, y0 + 0.23, 0.0),
        Vec3::new(0.16, 0.03, 0.30),
        Quat::from_rotation_z(0.55),
        darker(u),
    );
    // low wedge turret
    mb.add_box_c(
        Vec3::new(-0.03, y0 + 0.28, 0.0),
        Vec3::new(0.30, 0.08, 0.24),
        darker(u),
    );
    mb.add_box_rot(
        Vec3::new(0.10, y0 + 0.29, 0.0),
        Vec3::new(0.14, 0.06, 0.20),
        Quat::from_rotation_z(0.35),
        darker(u),
    );
    // long smoothbore barrel
    mb.add_box_c(
        Vec3::new(0.33, y0 + 0.29, 0.0),
        Vec3::new(0.46, 0.035, 0.035),
        GUNMETAL,
    );
}

/// Amphibious tank: light tank wearing a canvas flotation collar.
fn amphibious_tank(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    tank(mb, u, y0, 0.42, 0.30, 0.20, 0.26);
    // flotation screen: canvas rim around the hull top edge
    let yt = y0 + 0.30;
    for z in [-0.17, 0.17] {
        mb.add_box_c(Vec3::new(0.0, yt, z), Vec3::new(0.46, 0.09, 0.03), CANVAS);
    }
    for x in [-0.22, 0.22] {
        mb.add_box_c(Vec3::new(x, yt, 0.0), Vec3::new(0.03, 0.09, 0.34), CANVAS);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Towed guns — emplaced (firing) models
// ────────────────────────────────────────────────────────────────────────────

/// Towed gun EMPLACED: wheels + spread trail legs + barrel at firing angle
/// (+ shield for AT). Field artillery fires at high angle: barrel elevated
/// 45°; AT guns are direct-fire: barrel horizontal behind a gun shield.
fn gun_emplaced(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, barrel: f32, shield: bool) {
    // wheels
    for z in [-0.14, 0.14] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.07, z),
            Vec3::new(0.06, 0.14, 0.06),
            DARK,
        );
    }
    // axle + breech
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.10, 0.0),
        Vec3::new(0.10, 0.10, 0.30),
        GUNMETAL,
    );
    if shield {
        // AT: horizontal barrel + gun shield (direct-fire silhouette)
        mb.add_box_c(
            Vec3::new(barrel / 2.0, y0 + 0.11, 0.0),
            Vec3::new(barrel, 0.04, 0.04),
            GUNMETAL,
        );
        mb.add_box_c(
            Vec3::new(0.10, y0 + 0.13, 0.0),
            Vec3::new(0.03, 0.16, 0.30),
            darker(u),
        );
        // trail legs
        mb.add_box_c(
            Vec3::new(-0.20, y0 + 0.04, -0.08),
            Vec3::new(0.30, 0.04, 0.05),
            u,
        );
        mb.add_box_c(
            Vec3::new(-0.20, y0 + 0.04, 0.08),
            Vec3::new(0.30, 0.04, 0.05),
            u,
        );
    } else {
        // Artillery: 45° elevated barrel, longer and thinner (howitzer silhouette)
        let dir = Vec3::new(
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
        );
        let muzzle = Vec3::new(0.05, y0 + 0.12, 0.0) + dir * (barrel / 2.0);
        mb.add_box_rot(
            muzzle,
            Vec3::new(barrel, 0.035, 0.035),
            Quat::from_rotation_z(std::f32::consts::FRAC_PI_4),
            GUNMETAL,
        );
        // recoil sled under the breech
        mb.add_box_c(
            Vec3::new(-0.02, y0 + 0.06, 0.0),
            Vec3::new(0.16, 0.05, 0.16),
            darker(u),
        );
        // wide-spread trail legs
        mb.add_box_rot(
            Vec3::new(-0.20, y0 + 0.04, -0.13),
            Vec3::new(0.34, 0.04, 0.05),
            Quat::from_rotation_y(0.35),
            u,
        );
        mb.add_box_rot(
            Vec3::new(-0.20, y0 + 0.04, 0.13),
            Vec3::new(0.34, 0.04, 0.05),
            Quat::from_rotation_y(-0.35),
            u,
        );
    }
}

/// AA gun EMPLACED: cruciform firing platform + quad 45° barrels (Flak
/// silhouette — no truck, the carriage is unhitched and leveled).
fn aa_emplaced(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    // cruciform outriggers
    for rot in [std::f32::consts::FRAC_PI_4, -std::f32::consts::FRAC_PI_4] {
        mb.add_box_rot(
            Vec3::new(0.0, y0 + 0.02, 0.0),
            Vec3::new(0.44, 0.04, 0.08),
            Quat::from_rotation_y(rot),
            darker(u),
        );
    }
    // pedestal + turntable
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.09, 0.0),
        Vec3::new(0.12, 0.12, 0.12),
        GUNMETAL,
    );
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.17, 0.0),
        Vec3::new(0.18, 0.05, 0.18),
        darker(u),
    );
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.23, 0.0),
        Vec3::new(0.12, 0.08, 0.16),
        GUNMETAL,
    );
    // quad barrels at 45° elevation, stacked in two pairs
    let elev = Quat::from_rotation_z(std::f32::consts::FRAC_PI_4);
    for dz in [-0.05, 0.05] {
        for dy in [0.0, 0.06] {
            let base = Vec3::new(0.02, y0 + 0.26 + dy, dz);
            let center = base
                + Vec3::new(
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.0,
                ) * 0.16;
            mb.add_box_rot(center, Vec3::new(0.32, 0.028, 0.028), elev, GUNMETAL);
        }
    }
}

/// Towed rocket launcher EMPLACED (Nebelwerfer): wheeled carriage + 6 tubes
/// in two banks of three, elevated 45°, spread trails.
fn rocket_towed_emplaced(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    // wheels
    for z in [-0.14, 0.14] {
        mb.add_box_c(
            Vec3::new(0.0, y0 + 0.07, z),
            Vec3::new(0.06, 0.14, 0.06),
            DARK,
        );
    }
    // carriage frame
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.12, 0.0),
        Vec3::new(0.12, 0.10, 0.28),
        GUNMETAL,
    );
    // 6 tubes: 2 banks (dy) × 3 tubes (dz), 45° elevation
    let elev = Quat::from_rotation_z(std::f32::consts::FRAC_PI_4);
    for dy in [0.0, 0.09] {
        for dz in [-0.09, 0.0, 0.09] {
            let base = Vec3::new(-0.02, y0 + 0.20 + dy, dz);
            let center = base
                + Vec3::new(
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.0,
                ) * 0.15;
            mb.add_box_rot(center, Vec3::new(0.30, 0.05, 0.05), elev, darker(GUNMETAL));
        }
    }
    // wide-spread trail legs
    mb.add_box_rot(
        Vec3::new(-0.20, y0 + 0.04, -0.13),
        Vec3::new(0.34, 0.04, 0.05),
        Quat::from_rotation_y(0.35),
        u,
    );
    mb.add_box_rot(
        Vec3::new(-0.20, y0 + 0.04, 0.13),
        Vec3::new(0.34, 0.04, 0.05),
        Quat::from_rotation_y(-0.35),
        u,
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Towed guns — limbered (in-tow) models: barrel/rack horizontal pointing
// rearward, trails closed forward into a tow bar; horse team (foot) or
// tow truck (TruckTowed) at the front.
// ────────────────────────────────────────────────────────────────────────────

/// Shared limbered-gun skeleton: wheels + horizontal barrel pointing -X +
/// closed trails forming a tow bar toward +X. `x0` shifts the whole piece
/// rearward so the tractor (horse team / truck) still fits on the base.
/// Returns the tow-bar tip x.
fn limber_skeleton(
    mb: &mut MeshBuilder,
    u: [f32; 4],
    y0: f32,
    x0: f32,
    barrel: f32,
    shield: bool,
) -> f32 {
    // wheels
    for z in [-0.14, 0.14] {
        mb.add_box_c(
            Vec3::new(x0, y0 + 0.07, z),
            Vec3::new(0.06, 0.14, 0.06),
            DARK,
        );
    }
    // axle + breech
    mb.add_box_c(
        Vec3::new(x0, y0 + 0.10, 0.0),
        Vec3::new(0.10, 0.10, 0.30),
        GUNMETAL,
    );
    // barrel horizontal, pointing rearward (-X, away from the tractor)
    mb.add_box_c(
        Vec3::new(x0 - barrel / 2.0 + 0.02, y0 + 0.11, 0.0),
        Vec3::new(barrel, 0.035, 0.035),
        GUNMETAL,
    );
    if shield {
        // AT shield stays up in tow
        mb.add_box_c(
            Vec3::new(x0 + 0.02, y0 + 0.13, 0.0),
            Vec3::new(0.03, 0.16, 0.30),
            darker(u),
        );
    }
    // trails closed together, reaching forward to the tow hook
    mb.add_box_c(
        Vec3::new(x0 + 0.20, y0 + 0.06, 0.0),
        Vec3::new(0.34, 0.035, 0.05),
        u,
    );
    // tow hook
    mb.add_box_c(
        Vec3::new(x0 + 0.38, y0 + 0.06, 0.0),
        Vec3::new(0.05, 0.05, 0.05),
        GUNMETAL,
    );
    x0 + 0.40 // tow-bar tip
}

/// Front half of a limbered piece: horse team (foot tow) or tow truck,
/// hooked to the tow-bar tip.
fn limber_front(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, state: GunState, tip: f32) {
    match state {
        GunState::LimberedFoot => horse_team(mb, u, y0, tip + 0.10),
        GunState::LimberedTruck => {
            // light prime mover ahead of the piece (crew rides in it)
            truck_at(mb, u, y0, tip + 0.16);
        }
        _ => {}
    }
}

/// A light prime-mover truck re-centered on x, scaled down so the whole
/// tow composition (gun + tractor) stays on the base plate.
fn truck_at(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, cx: f32) {
    mb.add_box_c(
        Vec3::new(cx, y0 + 0.08, 0.0),
        Vec3::new(0.32, 0.06, 0.19),
        GUNMETAL,
    );
    mb.add_box_c(
        Vec3::new(cx - 0.04, y0 + 0.14, 0.0),
        Vec3::new(0.20, 0.07, 0.20),
        u,
    );
    mb.add_box_c(
        Vec3::new(cx - 0.04, y0 + 0.21, 0.0),
        Vec3::new(0.19, 0.08, 0.19),
        CANVAS,
    );
    mb.add_box_c(
        Vec3::new(cx + 0.12, y0 + 0.15, 0.0),
        Vec3::new(0.10, 0.10, 0.19),
        u,
    );
    mb.add_box_c(
        Vec3::new(cx + 0.14, y0 + 0.21, 0.0),
        Vec3::new(0.06, 0.04, 0.17),
        darker(u),
    );
    for x in [0.10, -0.02, -0.12] {
        for z in [-0.10, 0.10] {
            mb.add_box_c(
                Vec3::new(cx + x, y0 + 0.03, z),
                Vec3::new(0.06, 0.06, 0.04),
                DARK,
            );
        }
    }
}

/// Towed gun LIMBERED (tube artillery / AT).
fn gun_limbered(
    mb: &mut MeshBuilder,
    u: [f32; 4],
    y0: f32,
    barrel: f32,
    shield: bool,
    state: GunState,
) {
    let tip = limber_skeleton(mb, u, y0, -0.20, barrel, shield);
    limber_front(mb, u, y0, state, tip);
}

/// AA gun LIMBERED: barrels locked horizontal (pointing -X) on a low bed.
fn aa_limbered(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, state: GunState) {
    let x0 = -0.20;
    // wheels
    for z in [-0.14, 0.14] {
        mb.add_box_c(
            Vec3::new(x0, y0 + 0.07, z),
            Vec3::new(0.06, 0.14, 0.06),
            DARK,
        );
    }
    // low platform bed
    mb.add_box_c(
        Vec3::new(x0, y0 + 0.12, 0.0),
        Vec3::new(0.30, 0.06, 0.24),
        darker(u),
    );
    // quad barrels horizontal, stacked pairs pointing rearward
    for dz in [-0.05, 0.05] {
        for dy in [0.0, 0.06] {
            mb.add_box_c(
                Vec3::new(x0 - 0.12, y0 + 0.18 + dy, dz),
                Vec3::new(0.30, 0.028, 0.028),
                GUNMETAL,
            );
        }
    }
    // trails closed → tow bar
    mb.add_box_c(
        Vec3::new(x0 + 0.22, y0 + 0.06, 0.0),
        Vec3::new(0.30, 0.035, 0.05),
        u,
    );
    mb.add_box_c(
        Vec3::new(x0 + 0.38, y0 + 0.06, 0.0),
        Vec3::new(0.05, 0.05, 0.05),
        GUNMETAL,
    );
    limber_front(mb, u, y0, state, x0 + 0.40);
}

/// Towed rocket launcher LIMBERED: tube rack rotated horizontal (pointing
/// -X) over the carriage.
fn rocket_towed_limbered(mb: &mut MeshBuilder, u: [f32; 4], y0: f32, state: GunState) {
    let x0 = -0.20;
    // wheels
    for z in [-0.14, 0.14] {
        mb.add_box_c(
            Vec3::new(x0, y0 + 0.07, z),
            Vec3::new(0.06, 0.14, 0.06),
            DARK,
        );
    }
    // carriage frame
    mb.add_box_c(
        Vec3::new(x0, y0 + 0.11, 0.0),
        Vec3::new(0.12, 0.08, 0.28),
        GUNMETAL,
    );
    // 6 tubes horizontal in two banks, pointing rearward
    for dy in [0.0, 0.08] {
        for dz in [-0.09, 0.0, 0.09] {
            mb.add_box_c(
                Vec3::new(x0 - 0.14, y0 + 0.17 + dy, dz),
                Vec3::new(0.28, 0.05, 0.05),
                darker(GUNMETAL),
            );
        }
    }
    // trails closed → tow bar
    mb.add_box_c(
        Vec3::new(x0 + 0.22, y0 + 0.06, 0.0),
        Vec3::new(0.30, 0.035, 0.05),
        u,
    );
    mb.add_box_c(
        Vec3::new(x0 + 0.38, y0 + 0.06, 0.0),
        Vec3::new(0.05, 0.05, 0.05),
        GUNMETAL,
    );
    limber_front(mb, u, y0, state, x0 + 0.40);
}

// ────────────────────────────────────────────────────────────────────────────
// Self-propelled pieces & cars
// ────────────────────────────────────────────────────────────────────────────

/// Truck-mounted rocket launcher (Katyusha BM-13): plain truck + a wide
/// two-tier bank of 8 long launch rails elevated ~30° FORWARD over the
/// cab (firing direction = truck front), pivoting at the tailgate,
/// with ladder cross-struts and an A-frame support truss.
fn rocket_truck(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    truck(mb, u, y0);
    let tilt = Quat::from_rotation_z(std::f32::consts::PI / 6.0); // 30° up toward +X (front)
    let dir = Vec3::new(0.8660, 0.5, 0.0); // unit vector along the rails
    let pivot = Vec3::new(-0.28, y0 + 0.40, 0.0); // mount at the tailgate
    let rail_len = 0.62; // much longer than the bed, overhangs the cab
                         // rail bank: 4 rails × 2 tiers
    for dz in [-0.12, -0.04, 0.04, 0.12] {
        for dy in [0.0, 0.06] {
            let base = pivot + Vec3::new(0.0, dy, dz);
            mb.add_box_rot(
                base + dir * (rail_len / 2.0),
                Vec3::new(rail_len, 0.045, 0.045),
                tilt,
                GUNMETAL,
            );
        }
    }
    // ladder cross-struts across the bank
    for t in [0.12, 0.30, 0.48] {
        mb.add_box_rot(
            pivot + dir * t + Vec3::new(0.0, 0.03, 0.0),
            Vec3::new(0.03, 0.03, 0.32),
            tilt,
            darker(GUNMETAL),
        );
    }
    // pivot block + A-frame truss down to the bed
    mb.add_box_c(
        Vec3::new(-0.26, y0 + 0.36, 0.0),
        Vec3::new(0.08, 0.12, 0.14),
        darker(u),
    );
    for (x1, x2) in [(-0.14, -0.02), (0.10, -0.02)] {
        let (dy1, dy2) = (0.34, 0.52);
        let mid = Vec3::new((x1 + x2) / 2.0, y0 + (dy1 + dy2) / 2.0, 0.0);
        let len = ((x2 - x1).powi(2) + (dy2 - dy1).powi(2)).sqrt();
        let angle = (dy2 - dy1).atan2(x2 - x1);
        mb.add_box_rot(
            mid,
            Vec3::new(len, 0.03, 0.03),
            Quat::from_rotation_z(angle),
            darker(GUNMETAL),
        );
    }
}

/// Armored car: low hull + small turret + 4 wheels.
fn armored_car(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.11, 0.0),
        Vec3::new(0.44, 0.13, 0.24),
        u,
    );
    mb.add_box_c(
        Vec3::new(-0.02, y0 + 0.21, 0.0),
        Vec3::new(0.16, 0.08, 0.16),
        darker(u),
    );
    mb.add_box_c(
        Vec3::new(0.12, y0 + 0.22, 0.0),
        Vec3::new(0.18, 0.03, 0.03),
        GUNMETAL,
    );
    for x in [0.14, -0.14] {
        for z in [-0.13, 0.13] {
            mb.add_box_c(
                Vec3::new(x, y0 + 0.04, z),
                Vec3::new(0.08, 0.08, 0.05),
                DARK,
            );
        }
    }
}

/// Jeep / support car with a bright role-crate on the back.
fn jeep(mb: &mut MeshBuilder, u: [f32; 4], y0: f32) {
    mb.add_box_c(
        Vec3::new(0.0, y0 + 0.08, 0.0),
        Vec3::new(0.36, 0.09, 0.20),
        u,
    );
    mb.add_box_c(
        Vec3::new(0.10, y0 + 0.16, 0.0),
        Vec3::new(0.05, 0.08, 0.18),
        darker(u),
    ); // windshield
    mb.add_box_c(
        Vec3::new(-0.10, y0 + 0.16, 0.0),
        Vec3::new(0.12, 0.10, 0.16),
        [0.85, 0.70, 0.20, 1.0],
    ); // role crate
    for x in [0.12, -0.12] {
        for z in [-0.11, 0.11] {
            mb.add_box_c(
                Vec3::new(x, y0 + 0.035, z),
                Vec3::new(0.07, 0.07, 0.04),
                DARK,
            );
        }
    }
}

/// Selection ring: flat round plate slightly larger than the base plate.
pub fn build_selection_ring_mesh() -> (Mesh, Image) {
    let mut mb = MeshBuilder::new();
    mb.add_cylinder(Vec3::ZERO, 0.62, 0.0, 0.02, [1.0, 0.9, 0.2, 1.0], 28);
    mb.build()
}
