//! Battalion units — attributes per DESIGN §6.1, movement-order model per §6.2.
//!
//! Time & scale (§6.2): 1 hex = 1 km, 6 turns = 1 strategic hour, so a unit
//! advances `speed_kmh / 6` hexes of plains per turn. Movement is continuous:
//! a [`MoveOrder`] accumulates fractional progress across turns, so speeds
//! that do not divide the turn (4 km/h infantry = 2 hexes per 3 turns) work
//! naturally.

use crate::hex::HexCoord;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Attacker,
    Defender,
}

impl Side {
    pub fn opponent(self) -> Side {
        match self {
            Side::Attacker => Side::Defender,
            Side::Defender => Side::Attacker,
        }
    }
}

/// Battalion types, mirroring HOI4 line battalions + support companies (§6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitType {
    Infantry,
    Marine,
    Mountaineer,
    Paratrooper,
    Cavalry,
    Bicycle,
    Motorized,
    Mechanized,
    LightArmor,
    MediumArmor,
    HeavyArmor,
    SuperHeavyArmor,
    ModernArmor,
    AmphibiousArmor,
    ArtilleryBrigade,
    RocketArtillery,
    MotRocketArtillery,
    AntiTankBrigade,
    AntiAirBrigade,
    Engineer,
    Recon,
    Signal,
    Logistics,
    Maintenance,
    FieldHospital,
    MilitaryPolice,
    /// Division headquarters (§6.13): one per division, synthesized
    /// at roster build time — never parsed from a save token or script.
    Headquarters,
}

/// How the unit moves cross-country (§6.6 terrain debuffs). Motor units
/// (wheeled/tracked vehicles, towed gun wagons) suffer larger terrain
/// penalties than legs (infantry/cavalry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MobilityClass {
    Leg,
    Motor,
}

impl UnitType {
    /// Every variant, declaration order (locale-key iteration, tests).
    pub const ALL: [UnitType; 27] = [
        UnitType::Infantry,
        UnitType::Marine,
        UnitType::Mountaineer,
        UnitType::Paratrooper,
        UnitType::Cavalry,
        UnitType::Bicycle,
        UnitType::Motorized,
        UnitType::Mechanized,
        UnitType::LightArmor,
        UnitType::MediumArmor,
        UnitType::HeavyArmor,
        UnitType::SuperHeavyArmor,
        UnitType::ModernArmor,
        UnitType::AmphibiousArmor,
        UnitType::ArtilleryBrigade,
        UnitType::RocketArtillery,
        UnitType::MotRocketArtillery,
        UnitType::AntiTankBrigade,
        UnitType::AntiAirBrigade,
        UnitType::Engineer,
        UnitType::Recon,
        UnitType::Signal,
        UnitType::Logistics,
        UnitType::Maintenance,
        UnitType::FieldHospital,
        UnitType::MilitaryPolice,
        UnitType::Headquarters,
    ];

    /// Support companies move freely and buff co-located battalions (§6.8).
    pub fn is_support_company(self) -> bool {
        matches!(
            self,
            UnitType::Engineer
                | UnitType::Recon
                | UnitType::Signal
                | UnitType::Logistics
                | UnitType::Maintenance
                | UnitType::FieldHospital
                | UnitType::MilitaryPolice
        )
    }

    pub fn is_armor(self) -> bool {
        matches!(
            self,
            UnitType::LightArmor
                | UnitType::MediumArmor
                | UnitType::HeavyArmor
                | UnitType::SuperHeavyArmor
                | UnitType::ModernArmor
                | UnitType::AmphibiousArmor
        )
    }

    /// Attack range in hexes (§6.1, realistic ranges):
    /// 9 tube artillery, 6 rocket, 2 AT / all armor, 1 everything else.
    pub fn attack_range(self) -> i32 {
        match self {
            UnitType::ArtilleryBrigade => 9,
            UnitType::RocketArtillery | UnitType::MotRocketArtillery => 6,
            UnitType::AntiTankBrigade => 2,
            t if t.is_armor() => 2,
            _ => 1,
        }
    }

    /// Attribute flags carried by the TYPE half of a battalion class;
    /// the chassis half contributes its own flags via
    /// [`Chassis::attrs`]. Rules read the combined set on
    /// [`BattalionUnit::attrs`], never this table directly.
    pub fn base_attrs(self) -> Attrs {
        use UnitType::*;
        match self {
            Infantry | Marine | Mountaineer | Paratrooper | Bicycle => Attrs::INFANTRY,
            Cavalry => Attrs::CAVALRY,
            // Motorized / mechanized infantry ARE infantry (they ride between
            // battles, fight on foot) — the chassis adds the mobility flag.
            Motorized | Mechanized => Attrs::INFANTRY,
            LightArmor | MediumArmor | HeavyArmor | SuperHeavyArmor | ModernArmor
            | AmphibiousArmor => Attrs::ARMORED,
            ArtilleryBrigade => Attrs::ARTILLERY,
            RocketArtillery | MotRocketArtillery => Attrs::ARTILLERY | Attrs::ROCKET,
            AntiTankBrigade => Attrs::AT,
            AntiAirBrigade => Attrs::AA,
            Recon => Attrs::RECON | Attrs::SUPPORT,
            Engineer => Attrs::INFANTRY | Attrs::SUPPORT,
            Signal | Logistics | Maintenance | FieldHospital | MilitaryPolice => Attrs::SUPPORT,
            // §6.13: the HQ is its own class — no Hold, no support-company
            // rules; only command-system queries read this flag.
            Headquarters => Attrs::HQ,
        }
    }

    /// Baseline max organisation: HOI4 subunit values where the
    /// original game defines them — infantry 60, cavalry 70, armor 10,
    /// engineer/recon/support 20. Tube/rocket artillery, AT and AA are
    /// support *companies* in HOI4 (org 0); promoted to battalions here with
    /// tactical values: towed 30, towed rocket 25,
    /// self-propelled rocket 20.
    pub fn base_org(self) -> f32 {
        use UnitType::*;
        match self {
            Infantry | Marine | Mountaineer | Paratrooper | Bicycle | Motorized => 60.0,
            Cavalry => 70.0,
            Mechanized => 60.0,
            LightArmor | MediumArmor | HeavyArmor | SuperHeavyArmor | ModernArmor
            | AmphibiousArmor => 10.0,
            ArtilleryBrigade | AntiTankBrigade | AntiAirBrigade => 30.0,
            RocketArtillery => 25.0,
            MotRocketArtillery => 20.0,
            Engineer | Recon | Signal | Logistics | Maintenance | FieldHospital
            | MilitaryPolice => 20.0,
            // §6.13: HQ — support-company-class organisation.
            Headquarters => 20.0,
        }
    }

    /// Baseline max strength, same sourcing as [`Self::base_org`].
    pub fn base_strength(self) -> f32 {
        use UnitType::*;
        match self {
            Infantry | Marine | Mountaineer | Paratrooper | Bicycle | Cavalry | Motorized => 25.0,
            Mechanized => 30.0,
            LightArmor | MediumArmor | HeavyArmor | SuperHeavyArmor | ModernArmor
            | AmphibiousArmor => 2.0,
            ArtilleryBrigade | AntiTankBrigade | AntiAirBrigade => 4.0,
            RocketArtillery | MotRocketArtillery => 3.0,
            Engineer | Recon | Signal | Logistics | Maintenance | FieldHospital
            | MilitaryPolice => 2.0,
            // §6.13: HQ — fragile.
            Headquarters => 3.0,
        }
    }

    /// The chassis a battalion type defaults to when the data source does
    /// not say otherwise (demo scenarios; the save mapper always sets it
    /// explicitly from the HOI4 token).
    pub fn default_chassis(self) -> Chassis {
        use UnitType::*;
        match self {
            ArtilleryBrigade | RocketArtillery | AntiTankBrigade | AntiAirBrigade => Chassis::Towed,
            MotRocketArtillery | Motorized | Recon => Chassis::Wheeled,
            Mechanized => Chassis::Halftrack,
            LightArmor => Chassis::Light,
            MediumArmor | AmphibiousArmor => Chassis::Medium,
            HeavyArmor => Chassis::Heavy,
            SuperHeavyArmor => Chassis::SuperHeavy,
            ModernArmor => Chassis::Modern,
            _ => Chassis::None,
        }
    }

    /// Base sight range (§6.1): recon 4; artillery and buttoned-up armor 1;
    /// default 2.
    pub fn base_sight(self) -> i32 {
        match self {
            UnitType::Recon => 4,
            UnitType::ArtilleryBrigade
            | UnitType::RocketArtillery
            | UnitType::MotRocketArtillery => 1,
            t if t.is_armor() => 1,
            _ => 2,
        }
    }

    /// Road speed in km/h (§6.1, HOI4-aligned): infantry 4, cavalry 6,
    /// motorized 12, mechanized 10, armor 12/10/8/6 by weight, towed guns 3,
    /// self-propelled rocket 12. This table is the FALLBACK for foot-mobile
    /// types — any unit with a chassis takes the chassis speed instead
    /// (unit.rs `BattalionUnit::new`), so e.g. RocketArtillery resolves to
    /// Towed=3 via its default chassis, never to this table.
    pub fn speed_kmh(self) -> f32 {
        match self {
            UnitType::Infantry
            | UnitType::Marine
            | UnitType::Mountaineer
            | UnitType::Paratrooper
            | UnitType::Engineer
            | UnitType::Signal
            | UnitType::Logistics
            | UnitType::Maintenance
            | UnitType::FieldHospital
            | UnitType::MilitaryPolice => 4.0,
            // §6.13: foot HQ 4 km/h; motor variants take the chassis speed.
            UnitType::Headquarters => 4.0,
            UnitType::Cavalry | UnitType::Bicycle => 6.0,
            UnitType::Motorized
            | UnitType::Recon
            | UnitType::LightArmor
            | UnitType::ModernArmor
            | UnitType::MotRocketArtillery => 12.0,
            UnitType::Mechanized | UnitType::MediumArmor => 10.0,
            UnitType::HeavyArmor | UnitType::AmphibiousArmor => 8.0,
            UnitType::SuperHeavyArmor => 6.0,
            // Towed pieces — incl. RocketArtillery (Nebelwerfer-style; the
            // Katyusha is MotRocketArtillery=Wheeled). The 12.0 it used to
            // list here was unreachable (chassis overrides) and contradicted
            // its own default_chassis.
            UnitType::ArtilleryBrigade
            | UnitType::AntiTankBrigade
            | UnitType::AntiAirBrigade
            | UnitType::RocketArtillery => 3.0,
        }
    }

    /// Legs (infantry/cavalry/support) vs motor (vehicles and towed wagons);
    /// motor pays the larger terrain debuff (§6.6).
    pub fn mobility_class(self) -> MobilityClass {
        match self {
            UnitType::Infantry
            | UnitType::Marine
            | UnitType::Mountaineer
            | UnitType::Paratrooper
            | UnitType::Cavalry
            | UnitType::Bicycle
            | UnitType::Engineer
            | UnitType::Signal
            | UnitType::Logistics
            | UnitType::Maintenance
            | UnitType::FieldHospital
            | UnitType::MilitaryPolice => MobilityClass::Leg,
            // §6.13: the type table can't see the chassis — the foot HQ pays
            // leg debuffs; wheeled variants inherit the same class here
            // (pathfinding reads the type table only, pathfinding.rs:54).
            UnitType::Headquarters => MobilityClass::Leg,
            _ => MobilityClass::Motor,
        }
    }

    /// AA umbrella radius in hexes (§6.1 placeholder — air power is not
    /// implemented yet; the mere presence of an AA battalion will debuff
    /// enemy air effects inside this radius once air lands).
    pub fn aa_cover_radius(self) -> i32 {
        match self {
            UnitType::AntiAirBrigade => 3,
            _ => 0,
        }
    }

    /// Coarse family used by the 3D model builder.
    pub fn model_family(self) -> ModelFamily {
        use UnitType::*;
        match self {
            Infantry => ModelFamily::Infantry,
            Marine => ModelFamily::Marine,
            Mountaineer => ModelFamily::Mountaineer,
            Paratrooper => ModelFamily::Paratrooper,
            Bicycle => ModelFamily::Bicycle,
            Cavalry => ModelFamily::Cavalry,
            Motorized => ModelFamily::Truck,
            Mechanized => ModelFamily::Halftrack,
            LightArmor => ModelFamily::TankLight,
            MediumArmor => ModelFamily::TankMedium,
            HeavyArmor => ModelFamily::TankHeavy,
            SuperHeavyArmor => ModelFamily::TankSuperHeavy,
            ModernArmor => ModelFamily::TankModern,
            AmphibiousArmor => ModelFamily::TankAmphibious,
            ArtilleryBrigade => ModelFamily::Artillery,
            RocketArtillery => ModelFamily::RocketTowed,
            MotRocketArtillery => ModelFamily::RocketArtillery,
            AntiTankBrigade => ModelFamily::AntiTank,
            AntiAirBrigade => ModelFamily::AntiAir,
            Recon => ModelFamily::ArmoredCar,
            Engineer | Signal | Logistics | Maintenance | FieldHospital | MilitaryPolice => {
                ModelFamily::Jeep
            }
            // §6.13: fallback only — BattalionUnit::model_family() branches
            // by HQ chassis (foot → Infantry, armored car, wheeled → Jeep).
            Headquarters => ModelFamily::Jeep,
        }
    }
}

/// Unit attribute flags (§6.1): rules attach to attributes, not
/// to battalion types — a battalion class IS an attribute combination,
/// mirroring HOI4's own `type = { motorized, artillery }` multi-labels.
/// A unit's set = `unit_type.base_attrs() | chassis.attrs()` (plus rare
/// token-level extras OR-ed by the save mapper). Hand-rolled bitflags keep
/// tactical-core dependency-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attrs(pub u32);

impl Attrs {
    pub const NONE: Attrs = Attrs(0);
    /// Leg infantry (incl. motorized/mechanized infantry — they fight on
    /// foot): Hold stance eligibility.
    pub const INFANTRY: Attrs = Attrs(1 << 0);
    /// Mounted troops: mobile, but cannot Hold.
    pub const CAVALRY: Attrs = Attrs(1 << 1);
    /// Wheeled motor mobility (trucks, cars): big terrain debuff class.
    pub const MOTORIZED: Attrs = Attrs(1 << 2);
    /// Tracked / armored-chassis mobility.
    pub const MECHANIZED: Attrs = Attrs(1 << 3);
    /// Fighting tank: direct fire at range 2, buttoned-up sight 1.
    pub const ARMORED: Attrs = Attrs(1 << 4);
    /// Tube artillery: delivers fire missions (precision or area), never
    /// assaults point-blank willingly (§6.3).
    pub const ARTILLERY: Attrs = Attrs(1 << 5);
    /// Rocket artillery: area-fire only, min range 3, never assaults; same
    /// accuracy class as tube artillery (§6.3).
    pub const ROCKET: Attrs = Attrs(1 << 6);
    /// Anti-tank guns: direct fire range 2, high piercing.
    pub const AT: Attrs = Attrs(1 << 7);
    /// Anti-air guns: air umbrella placeholder radius 3 (§6.1).
    pub const AA: Attrs = Attrs(1 << 8);
    /// Towed piece (any carriage): must emplace before firing (§6.3).
    pub const TOWED: Attrs = Attrs(1 << 9);
    /// Reconnaissance: sight 4.
    pub const RECON: Attrs = Attrs(1 << 10);
    /// Support company: buffs co-located battalions (§6.8).
    pub const SUPPORT: Attrs = Attrs(1 << 11);
    /// Amphibious-capable (marines, amtracs): defined but not yet consumed —
    /// river-crossing rules arrive with a later water pass.
    pub const AMPHIBIOUS: Attrs = Attrs(1 << 12);
    /// Flame tank (support): defined but not yet consumed — bunker-assault
    /// rules arrive later.
    pub const FLAME: Attrs = Attrs(1 << 13);
    /// Division HQ (§6.13): command-system queries only — no Hold,
    /// no support-company rules, never a victory-sustaining unit.
    pub const HQ: Attrs = Attrs(1 << 14);
    /// Armored-car HQ variant (armored divisions, §6.13): model + label
    /// distinction only, rules never read this. OR-ed in after set_chassis.
    pub const HQ_ARMORED: Attrs = Attrs(1 << 15);

    /// True when ANY flag of `f` is set (single flag or union mask).
    pub fn has(self, f: Attrs) -> bool {
        self.0 & f.0 != 0
    }
}

impl std::ops::BitOr for Attrs {
    type Output = Attrs;
    fn bitor(self, o: Attrs) -> Attrs {
        Attrs(self.0 | o.0)
    }
}

impl std::ops::BitOrAssign for Attrs {
    fn bitor_assign(&mut self, o: Attrs) {
        self.0 |= o.0;
    }
}

/// What carries the weapon: the second dimension of a
/// battalion class. Decides road speed, mobility class, and whether the
/// crew must emplace before firing (towed carriages, §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Chassis {
    /// On foot / pack animals following (infantry, cavalry, support).
    #[default]
    None,
    /// Horse-/hand-towed gun: 3 km/h, must emplace.
    Towed,
    /// Truck-towed gun (mot_artillery & co): 12 km/h, must emplace.
    TruckTowed,
    /// Wheeled self-propelled (truck-mounted guns, Katyusha, armored cars,
    /// motorized infantry): 12 km/h.
    Wheeled,
    /// Halftrack / APC (mechanized infantry): 10 km/h.
    Halftrack,
    /// Armored fighting chassis by weight class: 12/10/8/6/12 km/h.
    Light,
    Medium,
    Heavy,
    SuperHeavy,
    Modern,
}

impl Chassis {
    /// Towed carriages (horse or truck) require emplacement before firing.
    pub fn is_towed(self) -> bool {
        matches!(self, Chassis::Towed | Chassis::TruckTowed)
    }

    /// Engine-driven carriage (truck-towed and everything self-propelled):
    /// the maintenance-company speed bonus (§6.3) applies to these only —
    /// foot and horse-towed hosts have no engine to keep tuned.
    pub fn has_engine(self) -> bool {
        !matches!(self, Chassis::None | Chassis::Towed)
    }

    /// Road speed override in km/h; `None` = fall back to the type table
    /// (foot-mobile units).
    pub fn speed_kmh(self) -> Option<f32> {
        Some(match self {
            Chassis::None => return None,
            Chassis::Towed => 3.0,
            Chassis::TruckTowed | Chassis::Wheeled | Chassis::Light | Chassis::Modern => 12.0,
            Chassis::Halftrack | Chassis::Medium => 10.0,
            Chassis::Heavy => 8.0,
            Chassis::SuperHeavy => 6.0,
        })
    }

    /// Mobility attribute flags contributed by the chassis.
    pub fn attrs(self) -> Attrs {
        match self {
            Chassis::None => Attrs::NONE,
            Chassis::Towed => Attrs::TOWED,
            Chassis::TruckTowed => Attrs::TOWED | Attrs::MOTORIZED,
            Chassis::Wheeled => Attrs::MOTORIZED,
            Chassis::Halftrack
            | Chassis::Light
            | Chassis::Medium
            | Chassis::Heavy
            | Chassis::SuperHeavy
            | Chassis::Modern => Attrs::MECHANIZED,
        }
    }
}

/// Coarse visual family for procedural 3D models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelFamily {
    Infantry,
    Marine,
    Mountaineer,
    Paratrooper,
    Bicycle,
    Cavalry,
    Truck,
    Halftrack,
    TankLight,
    TankMedium,
    TankHeavy,
    TankSuperHeavy,
    TankModern,
    TankAmphibious,
    Artillery,
    /// Truck-mounted rocket launcher (Katyusha; Wheeled mot-rocket).
    RocketArtillery,
    /// Towed rocket launcher (Nebelwerfer; foot- or truck-towed).
    RocketTowed,
    AntiTank,
    AntiAir,
    ArmoredCar,
    Jeep,
}

/// Carriage state of a towed gun for mesh selection (towed guns show
/// distinct emplaced / in-tow models; everything else is `NA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GunState {
    /// Not a towed gun (single mesh, self-propelled pieces included).
    NA,
    /// Towed gun set up to fire (trails spread, barrel at firing angle).
    Emplaced,
    /// Towed gun in horse-towed transport (trails closed, barrel horizontal).
    LimberedFoot,
    /// Towed gun behind its truck (mot_artillery & co).
    LimberedTruck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    /// Normal combat readiness.
    Active,
    /// Involuntary retreat (org hit 0): disordered movement to own edge (§6.8).
    Retreating,
    /// Destroyed / annihilated.
    Eliminated,
    /// Org 0 while fully encircled (§6.4).
    Surrendered,
    /// Reached own deployment edge and left the map (§6.8).
    Withdrawn,
    /// §6.14: lingered `CombatParams::oob_leaving_turns` full
    /// turns out of bounds and LEFT THE BATTLE — org 0, strength frozen
    /// (slipped away, NOT annihilated), removed from the board (position =
    /// OFFBOARD), uncommandable, ignored by the AI, resolved for victory.
    LeftBattle,
}

/// One queued leg of a Shift-click waypoint chain (§6.2): the
/// destination hex plus the preview route computed when the leg was
/// appended. The preview drives the ribbon hint and the ETA sum ONLY —
/// the leg is re-planned against the live board when it activates, so a
/// stale preview never decides an actual march.
#[derive(Debug, Clone)]
pub struct MoveLeg {
    pub dest: HexCoord,
    /// Preview polyline INCLUDING its start hex (the previous waypoint /
    /// chain end at append time) — ready to draw and to sum travel time.
    pub preview: Vec<HexCoord>,
}

/// A standing movement order (§6.2): follow `path` (next hex first),
/// advancing at the unit's speed at the end of each of its side's turns.
/// `hours` is the travel time already invested into the first path hex.
/// `queue` holds later waypoint legs of a Shift-click chain — only the
/// player's right-click issues legs (the AI's re-affirms never queue).
/// The chain lives INSIDE the order so every "drop the order" site
/// (stand-by, contact, interception, Hold, emplacement, attack
/// supersede) clears the whole chain with the existing
/// `move_order = None`, and the manual-override lifecycle keyed on
/// `move_order.is_some()` spans the chain automatically.
#[derive(Debug, Clone)]
pub struct MoveOrder {
    pub path: Vec<HexCoord>,
    pub hours: f32,
    pub queue: VecDeque<MoveLeg>,
}

/// Per-battalion terrain adjusters (§6.6 v3.3): the battalion
/// class's HOI4 unit-file terrain modifiers, baked in at assembly from
/// `unit_templates.json` (per save token, so rangers/marine commandos keep
/// their distinct identities). Indexed by [`crate::Terrain::idx`].
///
/// Semantics (strike-role form): whoever FIRES
/// applies its own `attack` adjuster against the target's hex terrain; the
/// absorber's `defense` adjuster multiplies its D on its own hex. Vanilla
/// data verbatim — line infantry/militia/paratroopers have zero adjusters
/// everywhere (vanilla defines none); specialists get bonuses (mountaineers
/// +0.35 mountain attack, marines +0.30 river/marsh, rangers forest/jungle);
/// vehicle and towed classes get penalties (medium armor −0.40 urban, mech
/// −0.30 jungle, towed guns −0.20 forest…). `amphibious`/`fort` keys are
/// dropped at intake (no counterpart hex). The `movement` stat is not
/// consumed (our mobility classes already price terrain).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TerrainAdjusters {
    pub attack: [f32; 12],
    pub defense: [f32; 12],
}

impl TerrainAdjusters {
    /// The firing-side adjuster against a target standing on `terrain`.
    pub fn attack_on(&self, terrain: crate::Terrain) -> f32 {
        self.attack[terrain.idx()]
    }

    /// The absorbing-side adjuster on the unit's own hex terrain.
    pub fn defense_on(&self, terrain: crate::Terrain) -> f32 {
        self.defense[terrain.idx()]
    }

    /// Record one HOI4 unit-file terrain modifier line
    /// (`forest = { attack = -0.2 }` → key "forest", stat "attack").
    /// Unknown terrain keys (amphibious/fort/…) and unknown stats
    /// (movement) are ignored by design — see the struct doc.
    pub fn set_hoi4(&mut self, terrain_key: &str, stat: &str, value: f32) {
        let Some(t) = crate::Terrain::from_hoi4_key(terrain_key) else {
            return;
        };
        match stat {
            "attack" => self.attack[t.idx()] = value,
            "defense" => self.defense[t.idx()] = value,
            _ => {}
        }
    }
}

/// A support company attached to a battalion. NOT a map unit —
/// the attachment rides with its host, dies with it, and confers stat
/// bonuses / ongoing abilities. One-shot stat bonuses (attack / piercing /
/// sight / armor / speed) are baked into the host's fields at attach time;
/// ongoing effects (damage reduction, engineer terrain craft, military-
/// police discipline) are queried via [`BattalionUnit::has_support`] and
/// the aggregators. Every effect is writeback-safe (§6.3): less damage
/// taken, more output, or a rule read — never mid-battle recovery.
#[derive(Debug, Clone, PartialEq)]
pub struct SupportAttachment {
    pub kind: SupportKind,
    /// Display name for the OOB tree / hover card (e.g. "1. AT").
    pub name: String,
    /// The HOI4 company token from the save (e.g. "light_flame_tank";
    /// §5.3). Empty for hand-built scenarios — effects then resolve by
    /// `kind` alone. The named companies with their own modelled rules
    /// (§6.3) key on this token INSTEAD of the kind family default.
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupportKind {
    AntiTank,
    AntiAir,
    Artillery,
    Engineer,
    Recon,
    FieldHospital,
    Signal,
    Maintenance,
    Logistics,
    MilitaryPolice,
}

impl SupportKind {
    /// Every variant, declaration order (locale-key iteration, tests).
    pub const ALL: [SupportKind; 10] = [
        SupportKind::AntiTank,
        SupportKind::AntiAir,
        SupportKind::Artillery,
        SupportKind::Engineer,
        SupportKind::Recon,
        SupportKind::FieldHospital,
        SupportKind::Signal,
        SupportKind::Maintenance,
        SupportKind::Logistics,
        SupportKind::MilitaryPolice,
    ];
}

impl SupportAttachment {
    /// The named companies with their own modelled rules (§6.3): a
    /// special token takes its own effect row INSTEAD of the kind family
    /// default — a flame tank is no military police, so these tokens never
    /// inherit the MilitaryPolice family effects their kind maps to.
    fn is_token_special(&self) -> bool {
        matches!(
            self.token.as_str(),
            "light_flame_tank"
                | "medium_flame_tank"
                | "heavy_flame_tank"
                | "super_heavy_armor"
                | "land_cruiser"
                | "blackshirt_assault_battalion"
                | "sturmtruppe_battalion"
                | "helicopter_brigade"
        )
    }

    /// Baked into the host at attach time (first-pass values, tune by feel).
    pub fn hard_attack_bonus(&self) -> f32 {
        if self.is_token_special() {
            return 0.0;
        }
        match self.kind {
            SupportKind::AntiTank => 4.0,
            _ => 0.0,
        }
    }
    pub fn piercing_bonus(&self) -> f32 {
        if self.is_token_special() {
            return 0.0;
        }
        match self.kind {
            SupportKind::AntiTank => 10.0,
            _ => 0.0,
        }
    }
    pub fn soft_attack_bonus(&self) -> f32 {
        match self.token.as_str() {
            "land_cruiser" => return 3.0,
            "blackshirt_assault_battalion" | "sturmtruppe_battalion" => return 2.0,
            _ => {}
        }
        if self.is_token_special() {
            return 0.0;
        }
        match self.kind {
            SupportKind::Artillery => 5.0,
            SupportKind::AntiAir => 1.0,
            _ => 0.0,
        }
    }
    pub fn breakthrough_bonus(&self) -> f32 {
        match self.token.as_str() {
            "light_flame_tank" | "medium_flame_tank" | "heavy_flame_tank" => 3.0,
            _ => 0.0,
        }
    }
    pub fn armor_bonus(&self) -> f32 {
        match self.token.as_str() {
            "super_heavy_armor" => 3.0,
            "land_cruiser" => 5.0,
            _ => 0.0,
        }
    }
    pub fn hardness_bonus(&self) -> f32 {
        match self.token.as_str() {
            "super_heavy_armor" => 0.15,
            _ => 0.0,
        }
    }
    pub fn sight_bonus(&self) -> i32 {
        if self.token.as_str() == "helicopter_brigade" {
            return 1;
        }
        if self.is_token_special() {
            return 0;
        }
        match self.kind {
            SupportKind::Recon => 1,
            _ => 0,
        }
    }
    /// Baked road-speed multiplier (maintenance crews keep engines
    /// tuned, §6.3); the engine-carriage condition is judged by the host
    /// at attach time.
    pub fn speed_mult(&self) -> f32 {
        if self.is_token_special() {
            return 1.0;
        }
        match self.kind {
            SupportKind::Maintenance => 1.1,
            _ => 1.0,
        }
    }
    /// Ongoing: multiplier on strength damage the HOST takes (hospital,
    /// maintenance, helicopter escort — stacked multiplicatively).
    pub fn str_damage_taken_mult(&self) -> f32 {
        if self.token.as_str() == "helicopter_brigade" {
            return 0.95;
        }
        if self.is_token_special() {
            return 1.0;
        }
        match self.kind {
            SupportKind::FieldHospital => 0.75,
            SupportKind::Maintenance => 0.85,
            _ => 1.0,
        }
    }
    /// Ongoing: multiplier on org damage the host takes in combat
    /// (military-police discipline, §6.3).
    pub fn org_damage_taken_mult(&self) -> f32 {
        if self.is_token_special() {
            return 1.0;
        }
        match self.kind {
            SupportKind::MilitaryPolice => 0.9,
            _ => 1.0,
        }
    }
    /// Ongoing: multiplier on encirclement attrition (§6.4) — logistics
    /// companies stretch the pocket's supplies, military police keep the
    /// ranks from fraying; both ride the same multiplicative stack.
    pub fn encirclement_attrition_mult(&self) -> f32 {
        if self.is_token_special() {
            return 1.0;
        }
        match self.kind {
            SupportKind::Logistics => 0.5,
            SupportKind::MilitaryPolice => 0.9,
            _ => 1.0,
        }
    }
    /// Ongoing: a host with military police attached never surrenders
    /// (§6.4) — fully encircled at org 0 it routs instead of giving up,
    /// and only annihilation clears it. The host alone is covered, never
    /// the other battalions of its pocket.
    pub fn never_surrenders(&self) -> bool {
        if self.is_token_special() {
            return false;
        }
        matches!(self.kind, SupportKind::MilitaryPolice)
    }
    /// Baked multiplier on the off-road excess-over-baseline movement
    /// cost (§6.6): engineer terrain craft shaves the terrain penalty,
    /// road pricing is untouched.
    pub fn terrain_cost_mult(&self) -> f32 {
        if self.is_token_special() {
            return 1.0;
        }
        match self.kind {
            SupportKind::Engineer => 0.75,
            _ => 1.0,
        }
    }
    /// Ongoing: multiplier on the per-layer entrenchment defense
    /// contribution (§6.8) — engineer digging doubles-down on fieldworks.
    pub fn entrenchment_mult(&self) -> f32 {
        if self.is_token_special() {
            return 1.0;
        }
        match self.kind {
            SupportKind::Engineer => 1.5,
            _ => 1.0,
        }
    }
}

/// A single battalion on the tactical map. All game-data-derived stats are
/// pre-calculated into these base attributes at launch (§5.3); only effects
/// generated inside the wargame become tactical buffs.
#[derive(Debug, Clone)]
pub struct BattalionUnit {
    pub id: usize,
    pub name: String,
    /// Division this battalion belongs to (Order of Battle tree).
    /// Demo/debug scenarios assign a fixed name; live mode carries the HOI4
    /// division's name from the save. Empty = unattached (listed flat).
    pub division: String,
    /// Country tag this battalion fights under (DESIGN §7.5):
    /// the roster's effective division → tag mapping (script battles; empty
    /// for non-script ones). Drives the per-tag base-plate colors — an empty
    /// tag falls back to the side color.
    pub tag: String,
    /// HOI4 province of the parent division at battle start: attacker
    /// divisions carry their SOURCE province, defender divisions the
    /// contested one. Routes this battalion's damage into the
    /// matching `damage_units` province line (DESIGN §3.2). `None` outside
    /// live from-save assemblies (demo/script/battle modes).
    pub hoi4_province: Option<u32>,
    /// The HOI4 division this battalion mirrors (DESIGN §8.2):
    /// the mid-battle roster keys on it — at every live sync, divisions the
    /// save's `land_combat` no longer lists have their battalions marched
    /// off (`UnitState::LeftBattle`), newly listed ones enter at the map
    /// edge. `None` outside live from-save assemblies.
    pub hoi4_division_id: Option<u64>,
    pub unit_type: UnitType,
    /// What carries the weapon: decides speed / emplacement.
    pub chassis: Chassis,
    /// Combined attribute flags (type ⊕ chassis ⊕ token extras) — all rule
    /// queries (Hold / emplacement / fire mission / rocket / recon) read
    /// this set, never the raw type tables.
    pub attrs: Attrs,
    pub side: Side,
    pub state: UnitState,
    pub position: HexCoord,
    /// Not yet placed on the map: the unit starts OFF the board and the
    /// player places it from the OOB window during the Deployment phase
    /// (or Auto Deploy hands the remainder to the AI planner). `position`
    /// is then the sentinel [`Self::OFFBOARD`].
    pub undeployed: bool,

    pub soft_attack: f32,
    pub hard_attack: f32,
    pub defense: f32,
    pub breakthrough: f32,

    pub max_org: f32,
    pub org: f32,
    pub max_strength: f32,
    pub strength: f32,

    /// Road speed in km/h (§6.1); movement consumes hours, not AP (§6.2).
    pub speed_kmh: f32,

    /// Strategic logistics state of the parent division at battle start
    /// (§6.7): supply storage ratio and fuel reserve ratio, 0..=1, already
    /// baked into the stat block at assembly; 1.0 outside from-save
    /// assemblies. Display + future in-battle supply model input.
    pub supply: f32,
    pub fuel: f32,

    pub attack_range: i32,
    pub sight_range: i32,

    pub armor: f32,
    pub piercing: f32,
    pub hardness: f32,

    /// Per-battalion terrain adjusters (§6.3/§6.6 v3.3), baked at
    /// assembly from `unit_templates.json` — zero-filled where no template
    /// resolves (tests, hand-built fixtures, HQ). See [`TerrainAdjusters`].
    pub terrain_adj: TerrainAdjusters,

    /// Entrenchment layers 0..=3 (§6.8) — brought in from the HOI4 save's
    /// division entrenchment value only; battle-time digging as a deployable
    /// map element is a future item.
    pub entrenchment: u8,
    /// Take-cover stance (§6.8): hunker down for +25% defense.
    /// Taking cover costs the turn's action; the stance drops the moment the
    /// unit moves (movement.rs) or resolves an attack (combat) — no assault
    /// restriction, no move-cost penalty.
    pub is_holding: bool,
    /// Towed gun deployed for firing (§6.3): required before fire support for
    /// `requires_emplacement` types; blocks movement until limbered.
    pub is_emplaced: bool,
    /// Has consumed this turn's action (assault / fire support / emplace /
    /// limber). A unit that acted does not advance its move order at the
    /// end of the turn (§6.2).
    pub acted: bool,
    /// Rocket salvo reload (§6.3): turns until the next fire
    /// mission is allowed. Set to `CombatParams::rocket_fire_cooldown_turns`
    /// when a rocket salvo resolves; ticks down in `refresh_turn` (start of
    /// the owner's turn). Always 0 for non-rocket units.
    pub fire_cooldown: i32,
    /// Standing movement order (§6.2).
    pub move_order: Option<MoveOrder>,
    /// Step-progress memory (§6.2): the first-step hex and invested hours
    /// of the most recently ABANDONED march order (re-pointed onto a
    /// different first step, or stood by). Re-pointing back onto that
    /// first step — while the unit still stands next to it — picks the
    /// investment back up, clamped to the step's current cost, and
    /// consumes the memory; a unit that marched away from the remembered
    /// hex lets it lapse. The memory never ticks down on its own: it
    /// survives turn rollovers until consumed or overwritten by a fresher
    /// abandonment.
    pub step_memory: Option<(HexCoord, f32)>,

    /// Shocked by a single hit ≥ 25% of max org (the hard cap is
    /// 40%, so only serious hits trigger). The flag blocks new attack orders;
    /// in-flight move orders are NOT cancelled (design: shock suppresses
    /// attacks only). A shock persists until the END OF
    /// THE NEXT turn-end after the one that inflicted it — see
    /// `CombatEngine::expire_shocks`.
    pub shocked: bool,

    /// Support companies riding with this battalion (attachments
    /// are not map units — see [`SupportAttachment`]).
    pub support: Vec<SupportAttachment>,

    /// The hardness delta each attachment actually applied at attach time
    /// (parallel to `support`, same order): when the 1.0 clamp ate part of
    /// a bonus, detach subtracts exactly what attach added, keeping the
    /// deployment-phase re-assignment round trip lossless (§5.3).
    support_hardness_applied: Vec<f32>,

    /// Baked engineer terrain craft (§6.6): multiplies the off-road
    /// excess-over-baseline movement cost — the terrain penalty shrinks,
    /// the 1 km baseline and road pricing are untouched. Accumulated at
    /// attach time ([`Self::attach`]) and divided back out on detach.
    pub terrain_cost_mult: f32,

    /// Division-order automation: set when the PLAYER personally
    /// issued this battalion a command (move / attack / hold / emplace /
    /// retreat) — the division-order AI must not touch the unit until the
    /// player's command completes. Cleared in [`Self::refresh_turn`] when
    /// the command is done (standing move consumed AND no Hold stance AND
    /// not emplaced), or immediately by the player's stand-by. Attack
    /// orders are turn-scoped: they resolve in the fire phase, so a manual
    /// assault protects the unit for exactly one turn (then the AI resumes).
    /// The settings policy "keep manual command"
    /// ([`Self::refresh_turn_keep_manual`]) skips the command-done expiry:
    /// only the player's stand-by releases the unit then.
    pub manual_override: bool,

    /// §6.14: consecutive full turns this unit has ENDED standing
    /// on an out-of-bounds hex (the shoreline margin ring / any
    /// out-of-province land). Resets to 0 when it ends a full turn back in
    /// bounds. At `CombatParams::oob_leaving_turns` the unit leaves the
    /// battle (`UnitState::LeftBattle`, org 0, strength frozen, OFFBOARD).
    /// Ticked by `tactical_combat::apply_oob_leaving` at each full-turn end.
    pub oob_turns: u8,
}

impl BattalionUnit {
    /// Sentinel position for units that have not been deployed yet: far
    /// outside any real grid, so grid queries, rendering and pathfinding
    /// can never reach it.
    pub const OFFBOARD: HexCoord = HexCoord::new(-1000, -1000);

    /// Minimal constructor with sane defaults; callers set stats afterwards.
    /// Chassis defaults from the type; use [`Self::set_chassis`]
    /// when the data source names a different carriage (SP guns, truck-towed).
    pub fn new(
        id: usize,
        name: impl Into<String>,
        unit_type: UnitType,
        side: Side,
        pos: HexCoord,
    ) -> Self {
        let chassis = unit_type.default_chassis();
        let attrs = unit_type.base_attrs() | chassis.attrs();
        BattalionUnit {
            id,
            name: name.into(),
            division: String::new(),
            tag: String::new(),
            hoi4_province: None,
            hoi4_division_id: None,
            unit_type,
            chassis,
            attrs,
            side,
            state: UnitState::Active,
            position: pos,
            undeployed: false,
            soft_attack: 0.0,
            hard_attack: 0.0,
            defense: 0.0,
            breakthrough: 0.0,
            max_org: unit_type.base_org(),
            org: unit_type.base_org(),
            max_strength: unit_type.base_strength(),
            strength: unit_type.base_strength(),
            speed_kmh: chassis.speed_kmh().unwrap_or_else(|| unit_type.speed_kmh()),
            supply: 1.0,
            fuel: 1.0,
            attack_range: unit_type.attack_range(),
            sight_range: unit_type.base_sight(),
            armor: 0.0,
            piercing: 1.0,
            hardness: 0.0,
            terrain_adj: TerrainAdjusters::default(),
            entrenchment: 0,
            is_holding: false,
            is_emplaced: false,
            acted: false,
            fire_cooldown: 0,
            move_order: None,
            step_memory: None,
            shocked: false,
            support: Vec::new(),
            support_hardness_applied: Vec::new(),
            terrain_cost_mult: 1.0,
            manual_override: false,
            oob_turns: 0,
        }
    }

    pub fn is_combat_effective(&self) -> bool {
        matches!(self.state, UnitState::Active) && self.org > 0.0 && self.strength > 0.0
    }

    /// Division headquarters (§6.13) — synthesized one per division;
    /// fragile, commands but does not count as a fighting unit for victory.
    pub fn is_hq(&self) -> bool {
        self.attrs.has(Attrs::HQ)
    }

    /// Victory bookkeeping (§6.11): whether this battalion keeps its side
    /// alive. Any non-HQ unit still on the field — `Active` or `Retreating`
    /// — counts, regardless of org/strength; HQs are command units, not
    /// fighting units (§6.13). SINGLE SOURCE for both frontends:
    /// tactical-sync `check_victory` and the headless `winner`
    /// both call this. The headless used to mirror the check with
    /// `is_combat_effective() || Retreating`, which diverges on an `Active`
    /// unit with org/strength ≤ 0 (beaten there, alive here).
    ///
    /// An `OFFBOARD` (undeployed) battalion does NOT count: it is not on
    /// the map, cannot be attacked, and can never be resolved by combat. A
    /// side kept alive only by one would make the battle unwinnable for its
    /// opponent (the interactive flow resolves this by never starting a
    /// battle with undeployed units; headless deploys everyone before the
    /// first turn).
    pub fn counts_for_victory(&self) -> bool {
        !self.is_hq()
            && self.position != Self::OFFBOARD
            && matches!(self.state, UnitState::Active | UnitState::Retreating)
    }

    /// Normalization fallback, run by both frontends' full-turn
    /// upkeep. Combat resolution transitions org ≤ 0 to Retreating/
    /// Surrendered and strength ≤ 0 to Eliminated on the spot, so an
    /// `Active` battalion with org/strength ≤ 0 is a transient or forged
    /// state — untargetable, uncommandable, yet keeping its side alive
    /// under [`Self::counts_for_victory`]. Normalize: strength ≤ 0 →
    /// `Eliminated`, org ≤ 0 → `Retreating` (§6.8; the encircled-surrender
    /// refinement of §6.4 stays combat-path-only).
    pub fn normalize_broken_state(&mut self) {
        if self.state != UnitState::Active {
            return;
        }
        if self.strength <= 0.0 {
            self.state = UnitState::Eliminated;
        } else if self.org <= 0.0 {
            self.state = UnitState::Retreating;
        }
    }

    /// Valid as an attack target (§6.8): combat-effective units, plus
    /// Retreating ones — a broken unit keeps taking fire until its strength
    /// is gone (it never counters). NOT a movement blocker: disordered
    /// troops hold no ground.
    /// A Withdrawn remnant — org 0, parked on the deployment-zone rim
    /// (§6.8) — is targetable too: it holds a hex that blocks the advance
    /// corridor, and the only way off the map is to be cleared (otherwise
    /// the remnant stalls the corridor).
    /// §6.14: a LeftBattle unit is OFF the board (OFFBOARD) — gone for good,
    /// never targetable, ignored by every combat/AI scan.
    pub fn is_targetable(&self) -> bool {
        self.is_combat_effective()
            || self.state == UnitState::Retreating
            || self.state == UnitState::Withdrawn
    }

    pub fn org_ratio(&self) -> f32 {
        if self.max_org > 0.0 {
            (self.org / self.max_org).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    pub fn strength_ratio(&self) -> f32 {
        if self.max_strength > 0.0 {
            (self.strength / self.max_strength).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Per-turn reset at the start of the unit's side's turn (§6.2): clears
    /// the action flag and ticks the rocket reload. `shocked` is
    /// cleared separately at the END of the side's turn (game loop) so it
    /// suppresses attack orders for exactly one full friendly turn.
    ///
    /// Division-order automation: a manual override (player-issued
    /// command) expires here when the command is done — no standing move,
    /// not holding, not emplaced. A march in progress keeps it alive; the
    /// Hold stance and an emplaced gun are open-ended player commands and
    /// keep it alive until the player stands the unit by.
    /// [`Self::refresh_turn_keep_manual`] is the same reset under the
    /// settings policy that keeps completed commands manual.
    pub fn refresh_turn(&mut self) {
        self.refresh_turn_impl(false);
    }

    /// Per-turn reset with the "keep manual command" settings policy:
    /// a completed command (arrival / fire-phase resolution) does NOT
    /// release the override — the battalion stays player-commanded until
    /// the player stands it by. Used only by the interactive battle loop,
    /// which reads the setting; scripted drivers (headless) keep
    /// [`Self::refresh_turn`].
    pub fn refresh_turn_keep_manual(&mut self) {
        self.refresh_turn_impl(true);
    }

    fn refresh_turn_impl(&mut self, keep_manual: bool) {
        self.acted = false;
        self.fire_cooldown = (self.fire_cooldown - 1).max(0);
        if self.manual_override
            && !keep_manual
            && self.move_order.is_none()
            && !self.is_holding
            && !self.is_emplaced
        {
            self.manual_override = false;
        }
    }

    /// Bank the standing order's invested first-step progress into the
    /// step memory (§6.2). The memory always holds the MOST RECENTLY
    /// abandoned investment, so the write happens only when real progress
    /// dies (`hours > 0`): an unmarched order abandons nothing and leaves
    /// an older memory in place.
    pub fn bank_step_memory(&mut self) {
        if let Some(o) = &self.move_order {
            if o.hours > 0.0 {
                if let Some(&first) = o.path.first() {
                    self.step_memory = Some((first, o.hours));
                }
            }
        }
    }

    /// Stand the battalion by (§7.4): cancel the standing march and
    /// release it back to its division order — the player's command is
    /// over. The cancelled march's step progress banks into the §6.2
    /// memory first, so a later re-issue toward the same first step picks
    /// the investment back up. Unconditional: this is the release that
    /// ends a manual override under EVERY retention policy. Stance,
    /// emplacement and the acted flag are untouched.
    pub fn stand_by(&mut self) {
        self.bank_step_memory();
        self.move_order = None;
        self.manual_override = false;
    }

    /// Assign a non-default chassis and re-derive attrs + road speed.
    /// Token-level extra flags (AMPHIBIOUS / FLAME) must be
    /// OR-ed into `attrs` AFTER this call — it recomputes the set from
    /// type ⊕ chassis.
    pub fn set_chassis(&mut self, chassis: Chassis) {
        self.chassis = chassis;
        self.attrs = self.unit_type.base_attrs() | chassis.attrs();
        if let Some(s) = chassis.speed_kmh() {
            self.speed_kmh = s;
        }
    }

    /// Precision factor — a linear factor of the attack quality q
    /// (§6.3 v3.2): the share of the HOI4 division-scale attack rating that
    /// is effective fire in one battalion engagement. Flag-driven;
    /// multi-flag units take the first tier:
    /// rocket > tube artillery > AT > AA > armor > default. (Armored cars
    /// map to Recon in the current data pipeline and stay at the 1.0
    /// default — revisit if a dedicated flag is added.)
    /// Rockets use the SAME factor as tube artillery (0.30) — a
    /// salvo's per-hex damage equals a tube strike; the delivery (always
    /// area fire, friendly fire) is the difference, not the punch.
    pub fn accuracy_factor(&self) -> f32 {
        if self.attrs.has(Attrs::ROCKET) || self.attrs.has(Attrs::ARTILLERY) {
            0.30
        } else if self.attrs.has(Attrs::AT) {
            0.80
        } else if self.attrs.has(Attrs::AA) {
            0.70
        } else if self.attrs.has(Attrs::ARMORED) {
            0.50
        } else {
            1.0
        }
    }

    /// May register an attack order (assault / direct fire / fire
    /// mission) in the pre-order phase. Blocked while shocked (a shocked
    /// unit may still move / hold / emplace). Per-kind rules (emplacement,
    /// rocket no-assault, ranges) are checked against the concrete target.
    /// §6.13: an HQ never initiates combat — it commands, defends and
    /// returns counter-fire, but none of its own attack orders register.
    pub fn can_attack_order(&self) -> bool {
        self.is_combat_effective() && !self.shocked && !self.is_hq()
    }

    /// True when an attachment of the given kind rides with this battalion.
    pub fn has_support(&self, kind: SupportKind) -> bool {
        self.support.iter().any(|s| s.kind == kind)
    }

    /// Attach a support company: one-shot stat bonuses are baked
    /// into the host's fields immediately; ongoing effects (damage
    /// reduction, engineer terrain craft, military-police discipline) are
    /// queried via the aggregators. First pass has no detach — attachments
    /// ride until the host dies.
    pub fn attach(&mut self, att: SupportAttachment) {
        self.soft_attack += att.soft_attack_bonus();
        self.hard_attack += att.hard_attack_bonus();
        self.piercing += att.piercing_bonus();
        self.sight_range += att.sight_bonus();
        self.breakthrough += att.breakthrough_bonus();
        self.armor += att.armor_bonus();
        // Record the post-clamp delta actually applied, so detach can
        // subtract exactly that — the raw bonus is unrecoverable once the
        // 1.0 ceiling bites.
        let applied = (self.hardness + att.hardness_bonus()).clamp(0.0, 1.0) - self.hardness;
        self.hardness += applied;
        self.support_hardness_applied.push(applied);
        self.terrain_cost_mult *= att.terrain_cost_mult();
        // Maintenance crews tune engines: a motorised host marches
        // faster; foot and horse-towed hosts gain nothing (§6.3).
        if att.speed_mult() != 1.0 && self.chassis.has_engine() {
            self.speed_kmh *= att.speed_mult();
        }
        self.support.push(att);
    }

    /// Detach by index (deployment-phase re-assignment): the
    /// baked bonuses are subtracted back out and the attachment is returned
    /// for re-attaching to another battalion.
    pub fn detach(&mut self, index: usize) -> Option<SupportAttachment> {
        if index >= self.support.len() {
            return None;
        }
        let att = self.support.remove(index);
        self.soft_attack -= att.soft_attack_bonus();
        self.hard_attack -= att.hard_attack_bonus();
        self.piercing -= att.piercing_bonus();
        self.sight_range -= att.sight_bonus();
        self.breakthrough -= att.breakthrough_bonus();
        self.armor -= att.armor_bonus();
        let applied = self.support_hardness_applied.remove(index);
        self.hardness = (self.hardness - applied).clamp(0.0, 1.0);
        self.terrain_cost_mult /= att.terrain_cost_mult();
        if att.speed_mult() != 1.0 && self.chassis.has_engine() {
            self.speed_kmh /= att.speed_mult();
        }
        Some(att)
    }

    /// One line per attached support company: the company's display name
    /// plus its effect tokens as `(locale key, value)` pairs. Locale-free
    /// by design — the UI layer renders the keys through `Locale::trf`
    /// (a battle unit is also assembled in tests and headless runs with no
    /// locale loaded).
    pub fn support_effect_tokens(&self) -> Vec<(String, Vec<(&'static str, String)>)> {
        let mut parts: Vec<(String, Vec<(&'static str, String)>)> = Vec::new();
        for att in &self.support {
            let mut e: Vec<(&'static str, String)> = Vec::new();
            if att.soft_attack_bonus() != 0.0 {
                e.push(("support.effect.soft", att.soft_attack_bonus().to_string()));
            }
            if att.hard_attack_bonus() != 0.0 {
                e.push(("support.effect.hard", att.hard_attack_bonus().to_string()));
            }
            if att.piercing_bonus() != 0.0 {
                e.push(("support.effect.pierce", att.piercing_bonus().to_string()));
            }
            if att.breakthrough_bonus() != 0.0 {
                e.push((
                    "support.effect.breakthrough",
                    att.breakthrough_bonus().to_string(),
                ));
            }
            if att.armor_bonus() != 0.0 {
                e.push(("support.effect.armor", att.armor_bonus().to_string()));
            }
            if att.hardness_bonus() != 0.0 {
                e.push(("support.effect.hardness", att.hardness_bonus().to_string()));
            }
            if att.sight_bonus() != 0 {
                e.push(("support.effect.sight", att.sight_bonus().to_string()));
            }
            if att.speed_mult() != 1.0 && self.chassis.has_engine() {
                e.push((
                    "support.effect.speed",
                    format!("{:.0}", (att.speed_mult() - 1.0) * 100.0),
                ));
            }
            if att.terrain_cost_mult() != 1.0 {
                e.push((
                    "support.effect.terrain_cost",
                    att.terrain_cost_mult().to_string(),
                ));
            }
            if att.entrenchment_mult() != 1.0 {
                e.push((
                    "support.effect.entrenchment",
                    att.entrenchment_mult().to_string(),
                ));
            }
            if att.str_damage_taken_mult() != 1.0 {
                e.push((
                    "support.effect.str_damage",
                    att.str_damage_taken_mult().to_string(),
                ));
            }
            if att.org_damage_taken_mult() != 1.0 {
                e.push((
                    "support.effect.org_damage",
                    att.org_damage_taken_mult().to_string(),
                ));
            }
            if att.encirclement_attrition_mult() != 1.0 {
                e.push((
                    "support.effect.siege_attrition",
                    att.encirclement_attrition_mult().to_string(),
                ));
            }
            if att.never_surrenders() {
                e.push(("support.effect.never_surrenders", String::new()));
            }
            if !e.is_empty() {
                parts.push((att.name.clone(), e));
            }
        }
        parts
    }

    /// Combined multiplier on strength damage taken (field hospital,
    /// maintenance, helicopter escort).
    pub fn support_str_damage_mult(&self) -> f32 {
        self.support
            .iter()
            .map(|s| s.str_damage_taken_mult())
            .product()
    }

    /// Combined multiplier on org damage taken in combat
    /// (military-police discipline, §6.3).
    pub fn support_org_damage_mult(&self) -> f32 {
        self.support
            .iter()
            .map(|s| s.org_damage_taken_mult())
            .product()
    }

    /// Combined multiplier on encirclement attrition (§6.4): logistics
    /// halves it, military police shave it — both stack multiplicatively.
    pub fn support_encirclement_attrition_mult(&self) -> f32 {
        self.support
            .iter()
            .map(|s| s.encirclement_attrition_mult())
            .product()
    }

    /// Any military-police attachment riding with this battalion keeps
    /// it from surrendering (§6.4) — it routs instead, to the death.
    pub fn support_never_surrenders(&self) -> bool {
        self.support.iter().any(|s| s.never_surrenders())
    }

    /// Combined multiplier on the per-layer entrenchment defense
    /// contribution (§6.8) — engineer fieldworks.
    pub fn support_entrenchment_mult(&self) -> f32 {
        self.support.iter().map(|s| s.entrenchment_mult()).product()
    }

    /// Hold stance eligibility (§6.8): every infantry-attribute unit may dig
    /// in — including motorized/mechanized infantry (they dismount to
    /// fight); cavalry and vehicle crews may not.
    pub fn can_hold(&self) -> bool {
        self.attrs.has(Attrs::INFANTRY)
    }

    /// Must the crew emplace before firing (§6.3)? True for any towed
    /// carriage (horse or truck); self-propelled mounts fire on the stop.
    pub fn requires_emplacement(&self) -> bool {
        self.chassis.is_towed()
    }

    /// Visual model family, chassis-aware: a truck-integrated rocket
    /// launcher (Katyusha, Wheeled) shows the truck+rack, while towed
    /// rocket launchers show the Nebelwerfer carriage. Everything else
    /// delegates to [`UnitType::model_family`].
    pub fn model_family(&self) -> ModelFamily {
        // §6.13: HQ model by chassis — foot → infantry stand, armored-car
        // variant (armored divisions) → armored car, motorized → jeep.
        if self.unit_type == UnitType::Headquarters {
            return if self.attrs.has(Attrs::HQ_ARMORED) {
                ModelFamily::ArmoredCar
            } else if self.chassis == Chassis::Wheeled {
                ModelFamily::Jeep
            } else {
                ModelFamily::Infantry
            };
        }
        if self.unit_type == UnitType::MotRocketArtillery && self.chassis != Chassis::Wheeled {
            ModelFamily::RocketTowed
        } else {
            self.unit_type.model_family()
        }
    }

    /// Carriage state for mesh selection (towed guns show distinct
    /// emplaced / in-tow models; anything else is [`GunState::NA`]).
    pub fn gun_state(&self) -> GunState {
        if !self.requires_emplacement() {
            return GunState::NA;
        }
        if self.is_emplaced {
            GunState::Emplaced
        } else if self.chassis == Chassis::TruckTowed {
            GunState::LimberedTruck
        } else {
            GunState::LimberedFoot
        }
    }

    /// Indirect-fire artillery (tube + rocket, §6.3): delivers fire missions
    /// and never assaults — drives the right-click fire-mission split and
    /// the radial F button.
    pub fn is_indirect_artillery(&self) -> bool {
        self.attrs.has(Attrs::ARTILLERY)
    }

    /// Rocket artillery: area-fire only (7-hex saturation), no assault.
    pub fn is_rocket(&self) -> bool {
        self.attrs.has(Attrs::ROCKET)
    }

    /// Direct-fire support piece (AT/AA). Its fire missions are ALWAYS
    /// precision single-target strikes — a direct-fire gun has no
    /// business saturating a zone (the old area path let a point-blank
    /// mission splash the gun's own hex, §6.3 gesture ruling).
    pub fn is_direct_gun(&self) -> bool {
        self.attrs.has(Attrs::AT) || self.attrs.has(Attrs::AA)
    }

    /// Minimum target distance in hexes (§6.3): rockets cannot fire
    /// point-blank (safety / dispersion), everything else has no minimum.
    /// Raised 2 → 3 — at 2 the 7-hex splash still reached hexes
    /// adjacent to the launcher, so rushing adjacent did not actually
    /// silence the battery; at 3 the blast covers rings 2–4 and only
    /// point-blank (ring 1) is safe.
    pub fn min_attack_range(&self) -> i32 {
        if self.is_rocket() {
            3
        } else {
            1
        }
    }

    /// May this unit be ordered to assault right now (§6.2): one action per
    /// turn; emplaced guns and rocket artillery have no direct assault.
    /// A unit in cover MAY assault — the stance simply drops when
    /// the attack resolves (no permanent "cannot assault while holding").
    /// Towed (emplacement-requiring) units can NEVER assault — a gun crew
    /// deals no melee damage, and a LIMBERED towed unit deals no damage in
    /// any form (no assault, no counter — `add_counter` gates it — and fire
    /// support already requires emplacement).
    /// §6.13: an HQ never assaults — it is a command element that only
    /// defends (an HQ under attack still returns counter-fire there).
    pub fn can_assault(&self) -> bool {
        self.is_combat_effective()
            && !self.acted
            && !self.is_emplaced
            && !self.is_rocket()
            && !self.is_hq()
            && !self.requires_emplacement()
    }

    /// May this unit provide fire support right now (§6.3): one action per
    /// turn; towed guns must be emplaced first; rocket launchers must have
    /// finished reloading (the salvo cooldown). §6.13: never an HQ.
    pub fn can_fire_support(&self) -> bool {
        self.is_combat_effective()
            && !self.acted
            && !self.is_hq()
            && self.fire_cooldown <= 0
            && (!self.requires_emplacement() || self.is_emplaced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_turn_resets_action_flag() {
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.acted = true;
        u.refresh_turn();
        assert!(!u.acted);
    }

    #[test]
    fn manual_override_expires_when_command_done() {
        // The player's command protects the unit from division-
        // order automation until it completes.
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.manual_override = true;
        u.refresh_turn();
        assert!(!u.manual_override, "no standing command -> done");

        // A march in progress keeps the override alive.
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.manual_override = true;
        u.move_order = Some(MoveOrder {
            path: vec![HexCoord::new(1, 0)],
            hours: 0.0,
            queue: Default::default(),
        });
        u.refresh_turn();
        assert!(u.manual_override, "standing march still in progress");

        // The Hold stance is an open-ended player command.
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.manual_override = true;
        u.is_holding = true;
        u.refresh_turn();
        assert!(u.manual_override, "Hold stance keeps the override");

        // An emplaced towed gun likewise (limbered/stood down by the player).
        let mut u = BattalionUnit::new(
            0,
            "1.Art",
            UnitType::ArtilleryBrigade,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.manual_override = true;
        u.is_emplaced = true;
        u.refresh_turn();
        assert!(u.manual_override, "emplacement keeps the override");
    }

    #[test]
    fn keep_manual_policy_retains_override_after_command_done() {
        // The settings option "keep manual command": a completed command
        // (no standing march, no open-ended stance) must NOT release the
        // override — refresh after refresh keeps the battalion manual.
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.manual_override = true; // a march just resolved in the fire phase
        u.refresh_turn_keep_manual();
        assert!(u.manual_override, "completed command stays manual");
        u.refresh_turn_keep_manual();
        u.refresh_turn_keep_manual();
        assert!(
            u.manual_override,
            "still manual after repeated rollovers — only stand-by releases"
        );

        // The action-flag reset still happens under the policy.
        u.acted = true;
        u.refresh_turn_keep_manual();
        assert!(!u.acted, "acted flag resets as usual");
    }

    #[test]
    fn stand_by_releases_manual_battalion_under_every_policy() {
        // The release side of the keep-manual option: right-clicking the
        // unit (stand by) must return it to the division AI no matter how
        // long the override was retained.
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.manual_override = true;
        u.move_order = Some(MoveOrder {
            path: vec![HexCoord::new(1, 0)],
            hours: 0.0,
            queue: Default::default(),
        });
        // Marching under the keep policy: refresh keeps the override…
        u.refresh_turn_keep_manual();
        assert!(u.manual_override, "march in progress stays manual");
        // …and stand-by cancels the march and releases the unit.
        u.stand_by();
        assert!(!u.manual_override, "stand-by releases the override");
        assert!(
            u.move_order.is_none(),
            "stand-by cancels the standing march"
        );

        // The release works for an idle manual battalion too (arrived /
        // attack already resolved — the state keep mode retains it in).
        let mut u = BattalionUnit::new(
            0,
            "2.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.manual_override = true;
        u.stand_by();
        assert!(!u.manual_override, "idle manual battalion stands by");
    }

    #[test]
    fn stand_by_banks_the_step_progress_memory() {
        // §6.2 step memory: stand-by abandons the march but banks its
        // invested first-step progress for a later re-issue; an
        // unmarched order abandons nothing and banks nothing.
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.move_order = Some(MoveOrder {
            path: vec![HexCoord::new(1, 0)],
            hours: 0.3,
            queue: Default::default(),
        });
        u.stand_by();
        assert_eq!(u.step_memory, Some((HexCoord::new(1, 0), 0.3)));

        let mut u = BattalionUnit::new(
            1,
            "2.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.move_order = Some(MoveOrder {
            path: vec![HexCoord::new(1, 0)],
            hours: 0.0,
            queue: Default::default(),
        });
        u.stand_by();
        assert_eq!(u.step_memory, None, "nothing invested — nothing banked");
    }

    #[test]
    fn baseline_org_strength_per_hoi4() {
        // Calibration: original HOI4 subunit values.
        assert_eq!(UnitType::Infantry.base_org(), 60.0);
        assert_eq!(UnitType::Cavalry.base_org(), 70.0);
        assert_eq!(UnitType::MediumArmor.base_org(), 10.0);
        assert_eq!(UnitType::MediumArmor.base_strength(), 2.0);
        assert_eq!(UnitType::Mechanized.base_strength(), 30.0);
        // Artillery is a company in HOI4 (org 0) — tactical promotion here.
        assert_eq!(UnitType::ArtilleryBrigade.base_org(), 30.0);
        let u = BattalionUnit::new(
            0,
            "1.Pz",
            UnitType::MediumArmor,
            Side::Attacker,
            HexCoord::ZERO,
        );
        assert_eq!(u.max_org, 10.0);
        assert_eq!(u.max_strength, 2.0);
    }

    #[test]
    fn stat_table_per_design() {
        // Ranges (§6.1).
        assert_eq!(UnitType::Infantry.attack_range(), 1);
        assert_eq!(UnitType::ArtilleryBrigade.attack_range(), 9);
        assert_eq!(UnitType::RocketArtillery.attack_range(), 6);
        assert_eq!(UnitType::AntiTankBrigade.attack_range(), 2);
        assert_eq!(UnitType::AntiAirBrigade.attack_range(), 1);
        assert_eq!(UnitType::MediumArmor.attack_range(), 2);
        // Sight.
        assert_eq!(UnitType::Recon.base_sight(), 4);
        assert_eq!(UnitType::ArtilleryBrigade.base_sight(), 1);
        assert_eq!(UnitType::MediumArmor.base_sight(), 1);
        assert_eq!(UnitType::Infantry.base_sight(), 2);
        // Foot-mobile speeds (vehicle speeds live in the chassis table).
        assert_eq!(UnitType::Infantry.speed_kmh(), 4.0);
        assert_eq!(UnitType::Cavalry.speed_kmh(), 6.0);
        assert_eq!(UnitType::AntiAirBrigade.aa_cover_radius(), 3);
        assert_eq!(UnitType::Infantry.aa_cover_radius(), 0);
        // Mobility classes.
        assert_eq!(UnitType::Infantry.mobility_class(), MobilityClass::Leg);
        assert_eq!(UnitType::Cavalry.mobility_class(), MobilityClass::Leg);
        assert_eq!(UnitType::Motorized.mobility_class(), MobilityClass::Motor);
        assert_eq!(
            UnitType::ArtilleryBrigade.mobility_class(),
            MobilityClass::Motor
        );
    }

    #[test]
    fn attrs_and_chassis_per_design() {
        // Chassis defaults, derived attrs and speeds.
        let mk = |ut| BattalionUnit::new(0, "t", ut, Side::Attacker, HexCoord::ZERO);
        let u = mk(UnitType::ArtilleryBrigade);
        assert_eq!(u.chassis, Chassis::Towed);
        assert_eq!(u.speed_kmh, 3.0);
        assert!(u.requires_emplacement());
        assert!(u.is_indirect_artillery());
        assert!(u.attrs.has(Attrs::TOWED));
        // Towed rocket (Nebelwerfer): emplace + 3 km/h.
        let u = mk(UnitType::RocketArtillery);
        assert!(u.requires_emplacement());
        assert!(u.is_rocket());
        assert_eq!(u.min_attack_range(), 3); // was 2
        assert_eq!(u.speed_kmh, 3.0);
        // Katyusha (self-propelled wheeled): 12 km/h, no emplacement.
        let u = mk(UnitType::MotRocketArtillery);
        assert_eq!(u.chassis, Chassis::Wheeled);
        assert_eq!(u.speed_kmh, 12.0);
        assert!(!u.requires_emplacement());
        assert!(u.is_rocket());
        assert!(!u.can_assault());
        // Hold: every infantry-attribute unit, incl. mot/mech.
        assert!(mk(UnitType::Infantry).can_hold());
        assert!(mk(UnitType::Motorized).can_hold());
        assert!(mk(UnitType::Mechanized).can_hold());
        assert!(mk(UnitType::Engineer).can_hold());
        assert!(!mk(UnitType::Cavalry).can_hold());
        assert!(!mk(UnitType::ArtilleryBrigade).can_hold());
        // Armor speeds by chassis weight.
        assert_eq!(mk(UnitType::LightArmor).speed_kmh, 12.0);
        assert_eq!(mk(UnitType::MediumArmor).speed_kmh, 10.0);
        assert_eq!(mk(UnitType::HeavyArmor).speed_kmh, 8.0);
        assert_eq!(mk(UnitType::SuperHeavyArmor).speed_kmh, 6.0);
        assert_eq!(mk(UnitType::ModernArmor).speed_kmh, 12.0);
        assert_eq!(mk(UnitType::Mechanized).speed_kmh, 10.0);
        // set_chassis re-derives (SP artillery on a medium chassis).
        let mut spg = mk(UnitType::ArtilleryBrigade);
        spg.set_chassis(Chassis::Medium);
        assert_eq!(spg.speed_kmh, 10.0);
        assert!(!spg.requires_emplacement());
        assert!(spg.attrs.has(Attrs::MECHANIZED));
        assert!(!spg.attrs.has(Attrs::TOWED));
        // Truck-towed: 12 km/h but still emplaces.
        let mut mot_arty = mk(UnitType::ArtilleryBrigade);
        mot_arty.set_chassis(Chassis::TruckTowed);
        assert_eq!(mot_arty.speed_kmh, 12.0);
        assert!(mot_arty.requires_emplacement());
        assert!(mot_arty.attrs.has(Attrs::TOWED | Attrs::MOTORIZED));
    }

    #[test]
    fn action_gates() {
        let mut arty = BattalionUnit::new(
            0,
            "1.Art",
            UnitType::ArtilleryBrigade,
            Side::Attacker,
            HexCoord::ZERO,
        );
        // Towed gun: cannot fire until emplaced.
        assert!(!arty.can_fire_support());
        arty.is_emplaced = true;
        assert!(arty.can_fire_support());
        // Emplaced: cannot assault.
        assert!(!arty.can_assault());
        // Acted: nothing further this turn.
        arty.acted = true;
        assert!(!arty.can_fire_support());
        // Rocket artillery never assaults; self-propelled rockets fire from
        // the stop, towed rockets must emplace first.
        let rkt = BattalionUnit::new(
            1,
            "1.Rkt",
            UnitType::MotRocketArtillery,
            Side::Attacker,
            HexCoord::ZERO,
        );
        assert!(!rkt.can_assault());
        assert!(rkt.can_fire_support());
        let towed_rkt = BattalionUnit::new(
            2,
            "2.Rkt",
            UnitType::RocketArtillery,
            Side::Attacker,
            HexCoord::ZERO,
        );
        assert!(!towed_rkt.can_fire_support());
    }

    #[test]
    fn hold_and_entrench_stack() {
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Defender,
            HexCoord::ZERO,
        );
        u.defense = 100.0;
        u.is_holding = true;
        u.entrenchment = 2;
        // 100 × 1.25 (hold) × 1.20 (2 layers × 10%) — through the resolver
        // the hit step actually calls.
        let d = crate::damage::defense_value(
            &u,
            false,
            crate::Terrain::Plains,
            &crate::params::CombatParams::default(),
        );
        assert!((d - 150.0).abs() < 1e-4);
    }

    #[test]
    fn counts_for_victory_matrix_matches_state_semantics() {
        // The predicate both frontends share — a non-HQ Active or
        // Retreating battalion keeps its side alive, regardless of
        // org/strength (the old headless mirror diverged on Active with
        // org/strength ≤ 0).
        let states = [
            (UnitState::Active, true),
            (UnitState::Retreating, true),
            (UnitState::Eliminated, false),
            (UnitState::Surrendered, false),
            (UnitState::Withdrawn, false),
            (UnitState::LeftBattle, false),
        ];
        for (state, expected) in states {
            for org in [0.0, 50.0] {
                for strength in [0.0, 100.0] {
                    let mut u = BattalionUnit::new(
                        0,
                        "1.Inf",
                        UnitType::Infantry,
                        Side::Attacker,
                        HexCoord::ZERO,
                    );
                    u.state = state;
                    u.org = org;
                    u.strength = strength;
                    assert_eq!(
                        u.counts_for_victory(),
                        expected,
                        "{state:?} org={org} str={strength}"
                    );
                    // HQs never count, whatever the state (§6.13).
                    u.attrs |= Attrs::HQ;
                    assert!(
                        !u.counts_for_victory(),
                        "HQ {state:?} org={org} str={strength}"
                    );
                }
            }
        }
    }

    #[test]
    fn offboard_battalion_does_not_keep_its_side_alive() {
        // An undeployed battalion cannot be attacked or resolved, so
        // counting it would make its side permanently un-beatable.
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Defender,
            BattalionUnit::OFFBOARD,
        );
        assert!(!u.counts_for_victory(), "OFFBOARD keeps nobody alive");
        u.undeployed = false;
        u.position = HexCoord::new(3, 3);
        assert!(u.counts_for_victory(), "deployed and active counts");
        // The flag alone is not the criterion — the position is (a routed
        // unit pushed off the map is already Withdrawn/LeftBattle).
        u.position = BattalionUnit::OFFBOARD;
        u.state = UnitState::Retreating;
        assert!(!u.counts_for_victory());
    }

    #[test]
    fn normalize_broken_state_repairs_transient_zeroes() {
        // Active with org/strength ≤ 0 is transient or forged —
        // upkeep normalizes it so the victory predicate can't stall on a
        // ghost (untargetable yet keeping its side alive).
        let broken = |org: f32, strength: f32| {
            let mut u = BattalionUnit::new(
                0,
                "1.Inf",
                UnitType::Infantry,
                Side::Attacker,
                HexCoord::ZERO,
            );
            u.org = org;
            u.strength = strength;
            u
        };
        let mut u = broken(0.0, 100.0);
        u.normalize_broken_state();
        assert_eq!(u.state, UnitState::Retreating, "org 0 routs (§6.8)");

        let mut u = broken(50.0, 0.0);
        u.normalize_broken_state();
        assert_eq!(u.state, UnitState::Eliminated, "strength 0 is annihilated");

        // Strength wins over org when both are gone.
        let mut u = broken(0.0, 0.0);
        u.normalize_broken_state();
        assert_eq!(u.state, UnitState::Eliminated);

        // Healthy Active and already-resolved states pass through untouched.
        let mut u = broken(50.0, 100.0);
        u.normalize_broken_state();
        assert_eq!(u.state, UnitState::Active);
        let mut u = broken(0.0, 100.0);
        u.state = UnitState::Withdrawn;
        u.normalize_broken_state();
        assert_eq!(u.state, UnitState::Withdrawn);
    }

    fn sup(kind: SupportKind, token: &str) -> SupportAttachment {
        SupportAttachment {
            kind,
            name: "X".to_string(),
            token: token.to_string(),
        }
    }

    #[test]
    fn support_effect_table_per_design() {
        // Kind-family rows (§6.3).
        let eng = sup(SupportKind::Engineer, "engineer");
        assert_eq!(eng.terrain_cost_mult(), 0.75);
        assert_eq!(eng.entrenchment_mult(), 1.5);
        let log = sup(SupportKind::Logistics, "logistics_company");
        assert_eq!(log.encirclement_attrition_mult(), 0.5);
        let mp = sup(SupportKind::MilitaryPolice, "military_police");
        assert_eq!(mp.org_damage_taken_mult(), 0.9);
        assert_eq!(mp.encirclement_attrition_mult(), 0.9);
        assert!(mp.never_surrenders());
        let mnt = sup(SupportKind::Maintenance, "maintenance_company");
        assert_eq!(mnt.str_damage_taken_mult(), 0.85);
        assert_eq!(mnt.speed_mult(), 1.1);
        // Token rows (§6.3 named companies).
        for token in ["light_flame_tank", "medium_flame_tank", "heavy_flame_tank"] {
            assert_eq!(
                sup(SupportKind::MilitaryPolice, token).breakthrough_bonus(),
                3.0,
                "{token}"
            );
        }
        let sh = sup(SupportKind::MilitaryPolice, "super_heavy_armor");
        assert_eq!(sh.armor_bonus(), 3.0);
        assert_eq!(sh.hardness_bonus(), 0.15);
        let lc = sup(SupportKind::MilitaryPolice, "land_cruiser");
        assert_eq!(lc.armor_bonus(), 5.0);
        assert_eq!(lc.soft_attack_bonus(), 3.0);
        for token in ["blackshirt_assault_battalion", "sturmtruppe_battalion"] {
            assert_eq!(
                sup(SupportKind::MilitaryPolice, token).soft_attack_bonus(),
                2.0,
                "{token}"
            );
        }
        let heli = sup(SupportKind::MilitaryPolice, "helicopter_brigade");
        assert_eq!(heli.sight_bonus(), 1);
        assert_eq!(heli.str_damage_taken_mult(), 0.95);
        // An unnamed (script-side) attachment resolves by kind alone.
        let plain = sup(SupportKind::Artillery, "");
        assert_eq!(plain.soft_attack_bonus(), 5.0);
        assert!(!plain.never_surrenders());
    }

    #[test]
    fn special_tokens_never_inherit_the_mp_family() {
        // The named companies map to the MP kind as the no-bonus
        // placeholder, but their own effect row REPLACES the family
        // default — a flame tank must not police anyone.
        for token in [
            "light_flame_tank",
            "medium_flame_tank",
            "heavy_flame_tank",
            "super_heavy_armor",
            "land_cruiser",
            "blackshirt_assault_battalion",
            "sturmtruppe_battalion",
            "helicopter_brigade",
        ] {
            let a = sup(SupportKind::MilitaryPolice, token);
            assert_eq!(a.org_damage_taken_mult(), 1.0, "{token}");
            assert_eq!(a.encirclement_attrition_mult(), 1.0, "{token}");
            assert!(!a.never_surrenders(), "{token}");
        }
        // The HQ placeholder companies carry no token rule of their
        // own — they ride the MP family row like any unnamed MP company.
        for token in ["hq_support_company", "hq_specops"] {
            let a = sup(SupportKind::MilitaryPolice, token);
            assert_eq!(a.org_damage_taken_mult(), 0.9, "{token}");
            assert_eq!(a.encirclement_attrition_mult(), 0.9, "{token}");
            assert!(a.never_surrenders(), "{token}");
        }
    }

    #[test]
    fn attach_and_detach_bake_and_restore_the_new_stats() {
        let mut u = BattalionUnit::new(
            0,
            "1.Pz",
            UnitType::MediumArmor,
            Side::Attacker,
            HexCoord::ZERO,
        );
        let (soft, btk, armor, hardness, sight, speed) = (
            u.soft_attack,
            u.breakthrough,
            u.armor,
            u.hardness,
            u.sight_range,
            u.speed_kmh,
        );
        u.attach(sup(SupportKind::MilitaryPolice, "medium_flame_tank"));
        u.attach(sup(SupportKind::MilitaryPolice, "super_heavy_armor"));
        u.attach(sup(SupportKind::MilitaryPolice, "helicopter_brigade"));
        u.attach(sup(SupportKind::Maintenance, "maintenance_company"));
        u.attach(sup(SupportKind::Engineer, "engineer"));
        assert_eq!(u.soft_attack, soft);
        assert_eq!(u.breakthrough, btk + 3.0);
        assert_eq!(u.armor, armor + 3.0);
        assert!((u.hardness - (hardness + 0.15)).abs() < 1e-6);
        assert_eq!(u.sight_range, sight + 1);
        assert!((u.speed_kmh - speed * 1.1).abs() < 1e-4, "{}", u.speed_kmh);
        assert!((u.terrain_cost_mult - 0.75).abs() < 1e-6);
        // Detaching all five (reverse order) restores every field.
        for _ in 0..5 {
            u.detach(u.support.len() - 1);
        }
        assert!(u.support.is_empty());
        assert_eq!(u.soft_attack, soft);
        assert_eq!(u.breakthrough, btk);
        assert_eq!(u.armor, armor);
        assert!((u.hardness - hardness).abs() < 1e-6);
        assert_eq!(u.sight_range, sight);
        assert!((u.speed_kmh - speed).abs() < 1e-4, "{}", u.speed_kmh);
        assert!((u.terrain_cost_mult - 1.0).abs() < 1e-6);
    }

    #[test]
    fn maintenance_speed_bonus_needs_an_engine() {
        // Horse-towed gun: no engine, no bonus.
        let mut towed = BattalionUnit::new(
            0,
            "1.Art",
            UnitType::ArtilleryBrigade,
            Side::Attacker,
            HexCoord::ZERO,
        );
        let s = towed.speed_kmh;
        towed.attach(sup(SupportKind::Maintenance, "maintenance_company"));
        assert_eq!(towed.speed_kmh, s);
        // Truck-towed: an engine to keep tuned — +10%, reversed on detach.
        let mut truck = BattalionUnit::new(
            1,
            "2.Art",
            UnitType::ArtilleryBrigade,
            Side::Attacker,
            HexCoord::ZERO,
        );
        truck.set_chassis(Chassis::TruckTowed);
        let s = truck.speed_kmh;
        truck.attach(sup(SupportKind::Maintenance, "maintenance_company"));
        assert!((truck.speed_kmh - s * 1.1).abs() < 1e-4);
        truck.detach(0);
        assert!((truck.speed_kmh - s).abs() < 1e-4);
        // Foot infantry: no engine either.
        let mut leg = BattalionUnit::new(
            2,
            "1.Inf",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::ZERO,
        );
        let s = leg.speed_kmh;
        leg.attach(sup(SupportKind::Maintenance, "hq_maintenance"));
        assert_eq!(leg.speed_kmh, s);
    }

    #[test]
    fn hardness_bonus_clamps_at_one() {
        let mut u = BattalionUnit::new(
            0,
            "1.Pz",
            UnitType::MediumArmor,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.hardness = 0.9;
        u.attach(sup(SupportKind::MilitaryPolice, "super_heavy_armor"));
        assert_eq!(u.hardness, 1.0);
    }

    #[test]
    fn attach_detach_round_trips_a_clamped_hardness_bonus() {
        // A 0.9-hardness host + the 0.15 bonus clamps to 1.0 at attach;
        // detach must hand back the pre-attach 0.9, not 0.85 (§5.3).
        let mut u = BattalionUnit::new(
            0,
            "1.Pz",
            UnitType::MediumArmor,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.hardness = 0.9;
        u.attach(sup(SupportKind::MilitaryPolice, "super_heavy_armor"));
        assert_eq!(u.hardness, 1.0);
        u.detach(0);
        assert!((u.hardness - 0.9).abs() < 1e-6, "{}", u.hardness);
        assert!(u.support.is_empty());
    }

    #[test]
    fn engineer_fieldworks_amplify_entrenchment() {
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Defender,
            HexCoord::ZERO,
        );
        u.defense = 100.0;
        u.entrenchment = 2;
        u.attach(sup(SupportKind::Engineer, "engineer"));
        // 100 × (1 + 2 layers × 0.10 × 1.5 engineer fieldworks), resolved
        // by the hit step's own defense function.
        let d = crate::damage::defense_value(
            &u,
            false,
            crate::Terrain::Plains,
            &crate::params::CombatParams::default(),
        );
        assert!((d - 130.0).abs() < 1e-4);
    }

    #[test]
    fn damage_reduction_stacks_multiplicatively() {
        let mut u = BattalionUnit::new(
            0,
            "1.Inf",
            UnitType::Infantry,
            Side::Defender,
            HexCoord::ZERO,
        );
        u.attach(sup(SupportKind::FieldHospital, "field_hospital"));
        u.attach(sup(SupportKind::Maintenance, "maintenance_company"));
        u.attach(sup(SupportKind::MilitaryPolice, "helicopter_brigade"));
        let expect = 0.75 * 0.85 * 0.95;
        assert!((u.support_str_damage_mult() - expect).abs() < 1e-6);
        // Logistics + military police ride the same attrition stack.
        u.attach(sup(SupportKind::MilitaryPolice, "military_police"));
        u.attach(sup(SupportKind::Logistics, "logistics_company"));
        assert!((u.support_encirclement_attrition_mult() - 0.45).abs() < 1e-6);
        assert!((u.support_org_damage_mult() - 0.9).abs() < 1e-6);
        assert!(u.support_never_surrenders());
        assert!((u.support_entrenchment_mult() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn support_effect_tokens_list_the_new_effects() {
        let mut u = BattalionUnit::new(
            0,
            "1.Pz",
            UnitType::MediumArmor,
            Side::Attacker,
            HexCoord::ZERO,
        );
        u.attach(sup(SupportKind::MilitaryPolice, "medium_flame_tank"));
        u.attach(sup(SupportKind::Maintenance, "maintenance_company"));
        u.attach(sup(SupportKind::MilitaryPolice, "military_police"));
        u.attach(sup(SupportKind::Engineer, "engineer"));
        let tokens = u.support_effect_tokens();
        let flat: Vec<(String, &str, String)> = tokens
            .iter()
            .flat_map(|(name, effects)| {
                effects
                    .iter()
                    .map(|(key, value)| (name.clone(), *key, value.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let has = |key: &str, value: &str| {
            flat.iter()
                .any(|(_, k, v)| *k == key && v.as_str() == value)
        };
        // The per-attachment grouping survives (each name carries its own
        // effects), and the values stay raw so the locale layer can format
        // them.
        assert_eq!(tokens.len(), 4, "{tokens:?}");
        assert!(has("support.effect.breakthrough", "3"), "{flat:?}");
        assert!(has("support.effect.speed", "10"), "{flat:?}");
        assert!(has("support.effect.str_damage", "0.85"), "{flat:?}");
        assert!(has("support.effect.org_damage", "0.9"), "{flat:?}");
        assert!(has("support.effect.never_surrenders", ""), "{flat:?}");
        assert!(has("support.effect.terrain_cost", "0.75"), "{flat:?}");
        assert!(has("support.effect.entrenchment", "1.5"), "{flat:?}");
    }
}
