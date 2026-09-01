//! Battalion units — attributes per DESIGN §6.1, movement-order model per §6.2.
//!
//! Time & scale (§6.2): 1 hex = 1 km, 6 turns = 1 strategic hour, so a unit
//! advances `speed_kmh / 6` hexes of plains per turn. Movement is continuous:
//! a [`MoveOrder`] accumulates fractional progress across turns, so speeds
//! that do not divide the turn (4 km/h infantry = 2 hexes per 3 turns) work
//! naturally.

use crate::hex::HexCoord;

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

/// A standing movement order (§6.2): follow `path` (next hex first),
/// advancing at the unit's speed at the end of each of its side's turns.
/// `hours` is the travel time already invested into the first path hex.
#[derive(Debug, Clone)]
pub struct MoveOrder {
    pub path: Vec<HexCoord>,
    pub hours: f32,
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
/// sight) are baked into the host's fields at attach time; ongoing effects
/// (maintenance regen, hospital damage reduction) are queried via
/// [`BattalionUnit::has_support`] and the aggregators.
#[derive(Debug, Clone, PartialEq)]
pub struct SupportAttachment {
    pub kind: SupportKind,
    /// Display name for the OOB tree / hover card (e.g. "1. AT").
    pub name: String,
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
    /// Baked into the host at attach time (first-pass values, tune by feel).
    pub fn hard_attack_bonus(&self) -> f32 {
        match self.kind {
            SupportKind::AntiTank => 4.0,
            _ => 0.0,
        }
    }
    pub fn piercing_bonus(&self) -> f32 {
        match self.kind {
            SupportKind::AntiTank => 10.0,
            _ => 0.0,
        }
    }
    pub fn soft_attack_bonus(&self) -> f32 {
        match self.kind {
            SupportKind::Artillery => 5.0,
            SupportKind::AntiAir => 1.0,
            _ => 0.0,
        }
    }
    pub fn sight_bonus(&self) -> i32 {
        match self.kind {
            SupportKind::Recon => 1,
            _ => 0,
        }
    }
    /// Ongoing: per-turn strength regeneration (maintenance crews).
    pub fn str_regen(&self) -> f32 {
        match self.kind {
            SupportKind::Maintenance => 0.5,
            _ => 0.0,
        }
    }
    /// Ongoing: multiplier on strength damage the HOST takes (hospital).
    pub fn str_damage_taken_mult(&self) -> f32 {
        match self.kind {
            SupportKind::FieldHospital => 0.75,
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

    /// Division-order automation: set when the PLAYER personally
    /// issued this battalion a command (move / attack / hold / emplace /
    /// retreat) — the division-order AI must not touch the unit until the
    /// player's command completes. Cleared in [`Self::refresh_turn`] when
    /// the command is done (standing move consumed AND no Hold stance AND
    /// not emplaced), or immediately by the player's stand-by. Attack
    /// orders are turn-scoped: they resolve in the fire phase, so a manual
    /// assault protects the unit for exactly one turn (then the AI resumes).
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
            shocked: false,
            support: Vec::new(),
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
    pub fn counts_for_victory(&self) -> bool {
        !self.is_hq() && matches!(self.state, UnitState::Active | UnitState::Retreating)
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
    pub fn refresh_turn(&mut self) {
        self.acted = false;
        self.fire_cooldown = (self.fire_cooldown - 1).max(0);
        if self.manual_override
            && self.move_order.is_none()
            && !self.is_holding
            && !self.is_emplaced
        {
            self.manual_override = false;
        }
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
    pub fn can_attack_order(&self) -> bool {
        self.is_combat_effective() && !self.shocked
    }

    /// True when an attachment of the given kind rides with this battalion.
    pub fn has_support(&self, kind: SupportKind) -> bool {
        self.support.iter().any(|s| s.kind == kind)
    }

    /// Attach a support company: one-shot stat bonuses are baked
    /// into the host's fields immediately; ongoing effects (maintenance
    /// regen, hospital damage reduction) are queried via the aggregators.
    /// First pass has no detach — attachments ride until the host dies.
    pub fn attach(&mut self, att: SupportAttachment) {
        self.soft_attack += att.soft_attack_bonus();
        self.hard_attack += att.hard_attack_bonus();
        self.piercing += att.piercing_bonus();
        self.sight_range += att.sight_bonus();
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
        Some(att)
    }

    /// One-line effect summary for tooltips ("hard +4, piercing +10").
    pub fn support_effects_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for att in &self.support {
            let mut e: Vec<String> = Vec::new();
            if att.soft_attack_bonus() != 0.0 {
                e.push(format!("soft +{}", att.soft_attack_bonus()));
            }
            if att.hard_attack_bonus() != 0.0 {
                e.push(format!("hard +{}", att.hard_attack_bonus()));
            }
            if att.piercing_bonus() != 0.0 {
                e.push(format!("pierce +{}", att.piercing_bonus()));
            }
            if att.sight_bonus() != 0 {
                e.push(format!("sight +{}", att.sight_bonus()));
            }
            if att.str_regen() != 0.0 {
                e.push(format!("str +{}/turn", att.str_regen()));
            }
            if att.str_damage_taken_mult() != 1.0 {
                e.push(format!("str dmg ×{}", att.str_damage_taken_mult()));
            }
            if !e.is_empty() {
                parts.push(format!("{}: {}", att.name, e.join(", ")));
            }
        }
        parts.join(" | ")
    }

    /// Total per-turn strength regen from attachments (maintenance).
    pub fn support_str_regen(&self) -> f32 {
        self.support.iter().map(|s| s.str_regen()).sum()
    }

    /// Combined multiplier on strength damage taken (field hospital).
    pub fn support_str_damage_mult(&self) -> f32 {
        self.support
            .iter()
            .map(|s| s.str_damage_taken_mult())
            .product()
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
    pub fn can_assault(&self) -> bool {
        self.is_combat_effective()
            && !self.acted
            && !self.is_emplaced
            && !self.is_rocket()
            && !self.requires_emplacement()
    }

    /// May this unit provide fire support right now (§6.3): one action per
    /// turn; towed guns must be emplaced first; rocket launchers must have
    /// finished reloading (the salvo cooldown).
    pub fn can_fire_support(&self) -> bool {
        self.is_combat_effective()
            && !self.acted
            && self.fire_cooldown <= 0
            && (!self.requires_emplacement() || self.is_emplaced)
    }

    /// Effective defense including Hold stance and entrenchment (§6.8).
    pub fn effective_defense(&self, entrench_defense_per_layer: f32, hold_bonus: f32) -> f32 {
        let mut d = self.defense;
        if self.is_holding {
            d *= 1.0 + hold_bonus;
        }
        d *= 1.0 + self.entrenchment as f32 * entrench_defense_per_layer;
        d
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
        // 100 × 1.25 (hold) × 1.20 (2 layers × 10%)
        let d = u.effective_defense(0.10, 0.25);
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
}
