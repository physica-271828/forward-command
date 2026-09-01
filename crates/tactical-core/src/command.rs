//! Division headquarters & the chain of command — DESIGN.md §6.13.
//!
//! One HQ unit per division, synthesized at roster build time (save mapper,
//! battle script, preset alike). The HQ is fragile and fights with basic
//! self-defense fire only, but it keeps its division effective:
//!
//! - same-division battalions inside the aura radius get a +10% attack and
//!   defense bonus (applied as a LINEAR post-step modifier in the §6.3
//!   model, so the effect really is ±10%) and regenerate
//!   `hq_org_regen_frac` of their max org per full turn;
//! - when the HQ is annihilated (strength 0 — retreat and surrender do NOT
//!   count), every surviving same-division battalion takes
//!   `hq_death_org_frac` of its max org and the aura is gone for good.
//!
//! A signal company ([`SupportKind::Signal`]) rides ON THE HQ — routed
//! there by [`synthesize_hqs`] (replacing the earlier battalion-relay
//! design) — and extends the aura radius by
//! `hq_signal_radius_bonus` (3 → 6 km).

use std::collections::HashMap;

use crate::hex::HexCoord;
use crate::params::CombatParams;
use crate::unit::{Attrs, BattalionUnit, Chassis, Side, SupportKind, UnitType};

// §6.13 HQ baseline stats (first-pass tuning): support-company-class org and
// durability, token self-defense firepower. Armor/hardness stay at the
// `BattalionUnit::new` defaults (0) — the HQ is meant to be fragile.
pub const HQ_SOFT_ATTACK: f32 = 3.0;
pub const HQ_HARD_ATTACK: f32 = 0.0;
pub const HQ_DEFENSE: f32 = 6.0;
pub const HQ_BREAKTHROUGH: f32 = 2.0;

/// How a battalion receives its division's command (§6.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandLink {
    /// Inside the HQ aura (signal-extended when the HQ hosts a signal
    /// company).
    Direct,
    /// The division has a commanding HQ but this battalion is out of reach
    /// (drawn as a dashed line in the UI).
    OutOfRange,
    /// No commanding HQ: the division never had one, or it is destroyed /
    /// retreating / surrendered / withdrawn — or the unit IS an HQ (an HQ
    /// does not benefit from its own aura).
    NoHq,
}

/// True when the link confers the command aura (attack/defense bonus +
/// per-turn org regen).
pub fn in_command(link: CommandLink) -> bool {
    matches!(link, CommandLink::Direct)
}

/// The aura radius a commanding HQ projects (§6.13): `hq_aura_radius`, plus
/// `hq_signal_radius_bonus` when the HQ hosts a signal company.
pub fn aura_radius_of(hq: &BattalionUnit, params: &CombatParams) -> i32 {
    params.hq_aura_radius
        + if hq.has_support(SupportKind::Signal) {
            params.hq_signal_radius_bonus
        } else {
            0
        }
}

/// Compute every unit's command link in one pass (aligned by unit index),
/// so the fire phase does not re-run the scan per strike. Divisions are
/// keyed per side — attacker and defender may share a division name.
pub fn compute_command_links(units: &[BattalionUnit], params: &CombatParams) -> Vec<CommandLink> {
    // Commanding HQs by (side, division) (a retreating/surrendered/destroyed
    // HQ commands nothing).
    let mut hqs: HashMap<(Side, &str), &BattalionUnit> = HashMap::new();
    for u in units {
        if u.is_hq() && u.is_combat_effective() && !u.division.is_empty() {
            hqs.insert((u.side, u.division.as_str()), u);
        }
    }
    units
        .iter()
        .map(|u| {
            if u.is_hq() || u.division.is_empty() {
                return CommandLink::NoHq;
            }
            let Some(hq) = hqs.get(&(u.side, u.division.as_str())) else {
                return CommandLink::NoHq;
            };
            if u.position.distance(hq.position) <= aura_radius_of(hq, params) {
                CommandLink::Direct
            } else {
                CommandLink::OutOfRange
            }
        })
        .collect()
}

/// Classify the HQ chassis for a division from its composition (§6.13):
/// any tank battalion → armored car (the armored division HQ rides an
/// armored car, never a tank); motorized/mechanized majority →
/// trucks; anything else → foot. The bool marks the armored-car variant
/// (model + `Attrs::HQ_ARMORED`); chassis is `Wheeled` for both motor
/// variants (12 km/h).
pub fn hq_chassis_for_division(members: &[&BattalionUnit]) -> (Chassis, bool) {
    let mut motor = 0usize;
    for u in members {
        if u.unit_type.is_armor() {
            return (Chassis::Wheeled, true);
        }
        if matches!(u.unit_type, UnitType::Motorized | UnitType::Mechanized) {
            motor += 1;
        }
    }
    if motor * 2 > members.len() {
        (Chassis::Wheeled, false)
    } else {
        (Chassis::None, false)
    }
}

/// Append one synthesized HQ per division to `units`, grouped by
/// the non-empty `division` label of `side`'s battalions. Divisions are
/// processed in first-seen order; `position_of(n)` hands out positions for
/// the appended HQs (script/preset paths place units onto zone hexes
/// directly; the save path leaves positions to the deployment phase).
///
/// The HQ gets the fixed §6.13 stat line — no doctrine, equipment or
/// experience scaling (its division-level ratios, when the caller has them,
/// are applied by the caller afterwards, see tactical-save).
pub fn synthesize_hqs(
    units: &mut Vec<BattalionUnit>,
    next_id: &mut usize,
    side: Side,
    position_of: impl Fn(usize) -> HexCoord,
) {
    // Divisions in first-seen order, skipping any division that ALREADY has
    // an HQ — strictly idempotent: ANY same-division HQ blocks synthesis,
    // even a combat-ineffective remnant (org 0) still on the roster,
    // otherwise a repeated call after the HQ was mauled would grow a second
    // HQ (compute_command_links would silently pick one and the roster
    // would carry two).
    let mut divisions: Vec<String> = Vec::new();
    for u in units.iter() {
        if u.side == side
            && !u.is_hq()
            && !u.division.is_empty()
            && !divisions.contains(&u.division)
        {
            divisions.push(u.division.clone());
        }
    }
    let has_hq = |division: &str| {
        units
            .iter()
            .any(|u| u.side == side && u.is_hq() && u.division == division)
    };
    divisions.retain(|d| !has_hq(d));
    let n_div = divisions.len();
    for (n, division) in divisions.into_iter().enumerate() {
        let members: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.side == side && !u.is_hq() && u.division == division)
            .collect();
        let (chassis, armored) = hq_chassis_for_division(&members);
        let mut hq =
            BattalionUnit::new(*next_id, "HQ", UnitType::Headquarters, side, position_of(n));
        hq.division = division;
        hq.set_chassis(chassis);
        if armored {
            // OR-ed AFTER set_chassis (which rebuilds attrs from type ⊕
            // chassis) — same pattern as the AMPHIBIOUS/FLAME token flags.
            hq.attrs |= Attrs::HQ_ARMORED;
        }
        hq.soft_attack = HQ_SOFT_ATTACK;
        hq.hard_attack = HQ_HARD_ATTACK;
        hq.defense = HQ_DEFENSE;
        hq.breakthrough = HQ_BREAKTHROUGH;
        *next_id += 1;
        units.push(hq);
    }
    // Signal companies ride on the HQ (§6.13): any Signal
    // attachment the data source placed on a battalion moves to the division
    // HQ, where it extends the aura radius (see [`aura_radius_of`]).
    for hi in (units.len() - n_div)..units.len() {
        let (side, division) = (units[hi].side, units[hi].division.clone());
        let mut moved = Vec::new();
        for u in units.iter_mut() {
            if u.side == side && !u.is_hq() && u.division == division {
                let mut i = 0;
                while i < u.support.len() {
                    if u.support[i].kind == SupportKind::Signal {
                        if let Some(att) = u.detach(i) {
                            moved.push(att);
                        }
                    } else {
                        i += 1;
                    }
                }
            }
        }
        for att in moved {
            units[hi].attach(att);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inf(id: usize, division: &str, pos: HexCoord) -> BattalionUnit {
        let mut u = BattalionUnit::new(id, "1.Inf", UnitType::Infantry, Side::Attacker, pos);
        u.division = division.to_string();
        u
    }

    fn signal_inf(id: usize, division: &str, pos: HexCoord) -> BattalionUnit {
        let mut u = inf(id, division, pos);
        u.attach(crate::unit::SupportAttachment {
            kind: SupportKind::Signal,
            name: "Sig".to_string(),
        });
        u
    }

    fn deploy_at(side_units: &mut Vec<BattalionUnit>, side: Side, start_id: usize) {
        let mut next_id = start_id;
        synthesize_hqs(side_units, &mut next_id, side, |_| HexCoord::ZERO);
    }

    fn deploy(side_units: &mut Vec<BattalionUnit>, side: Side) {
        deploy_at(side_units, side, 100);
    }

    #[test]
    fn hq_synthesized_per_division_with_classification() {
        let mut units = vec![
            inf(0, "1. Infanterie", HexCoord::ZERO),
            inf(1, "1. Infanterie", HexCoord::ZERO),
            inf(2, "2. Panzer", HexCoord::ZERO),
            {
                let mut t = BattalionUnit::new(
                    3,
                    "1.Pz",
                    UnitType::MediumArmor,
                    Side::Attacker,
                    HexCoord::ZERO,
                );
                t.division = "2. Panzer".to_string();
                t
            },
        ];
        deploy(&mut units, Side::Attacker);
        let hqs: Vec<&BattalionUnit> = units.iter().filter(|u| u.is_hq()).collect();
        assert_eq!(hqs.len(), 2);
        let inf_hq = hqs.iter().find(|u| u.division == "1. Infanterie").unwrap();
        let pz_hq = hqs.iter().find(|u| u.division == "2. Panzer").unwrap();
        assert_eq!(inf_hq.chassis, Chassis::None);
        assert!(!inf_hq.attrs.has(Attrs::HQ_ARMORED));
        assert_eq!(pz_hq.chassis, Chassis::Wheeled);
        assert!(pz_hq.attrs.has(Attrs::HQ_ARMORED));
        // Fixed §6.13 stat line, support-company-class org/strength.
        assert_eq!(pz_hq.max_org, 20.0);
        assert_eq!(pz_hq.soft_attack, HQ_SOFT_ATTACK);
    }

    #[test]
    fn links_direct_out_of_range_and_hq_loss() {
        let p = CombatParams::default();
        let mut units = vec![
            inf(0, "Div", HexCoord::new(0, 0)),
            inf(1, "Div", HexCoord::new(3, 0)), // edge of radius 3
            inf(2, "Div", HexCoord::new(6, 0)), // far out
        ];
        deploy(&mut units, Side::Attacker);
        // HQ sits at ZERO (position_of closure) — next to unit 0.
        let links = compute_command_links(&units, &p);
        assert_eq!(links[0], CommandLink::Direct);
        assert_eq!(links[1], CommandLink::Direct);
        assert_eq!(links[2], CommandLink::OutOfRange);
        // The HQ itself never benefits.
        let hq_idx = units.iter().position(|u| u.is_hq()).unwrap();
        assert_eq!(links[hq_idx], CommandLink::NoHq);

        // HQ destroyed → everyone NoHq (aura permanently gone).
        let hq = units.iter_mut().find(|u| u.is_hq()).unwrap();
        hq.strength = 0.0;
        hq.state = crate::unit::UnitState::Eliminated;
        let links = compute_command_links(&units, &p);
        assert!(links.iter().all(|&l| l == CommandLink::NoHq));
    }

    #[test]
    fn signal_on_hq_extends_aura() {
        let p = CombatParams::default();
        // A signal company attached to a battalion is routed to the division
        // HQ at synthesis time; the HQ's aura grows 3 → 6 km.
        let mut units = vec![
            signal_inf(0, "Div", HexCoord::new(4, 0)),
            inf(1, "Div", HexCoord::new(6, 0)),
            inf(2, "Div", HexCoord::new(7, 0)),
        ];
        deploy(&mut units, Side::Attacker);
        let hq = units.iter().find(|u| u.is_hq()).unwrap();
        assert!(hq.has_support(SupportKind::Signal));
        assert!(!units[0].has_support(SupportKind::Signal));
        assert_eq!(
            aura_radius_of(hq, &p),
            p.hq_aura_radius + p.hq_signal_radius_bonus
        );
        let links = compute_command_links(&units, &p);
        assert_eq!(links[0], CommandLink::Direct); // 4 <= 6
        assert_eq!(links[1], CommandLink::Direct); // 6 <= 6
        assert_eq!(links[2], CommandLink::OutOfRange); // 7 > 6
    }

    #[test]
    fn command_scoped_per_side_and_division() {
        let p = CombatParams::default();
        // Same division NAME on both sides (keyed per side): the attacker's
        // HQ hosts a signal company (radius 6), the defender's does not (3).
        let mut def = BattalionUnit::new(
            10,
            "Gren",
            UnitType::Infantry,
            Side::Defender,
            HexCoord::new(5, 0),
        );
        def.division = "X".to_string();
        let mut units = vec![
            signal_inf(0, "X", HexCoord::ZERO),
            inf(1, "X", HexCoord::new(5, 0)),
            def,
        ];
        deploy(&mut units, Side::Attacker);
        deploy_at(&mut units, Side::Defender, 200);
        let links = compute_command_links(&units, &p);
        assert_eq!(links[1], CommandLink::Direct); // attacker: 5 <= 6
        let def_idx = units
            .iter()
            .position(|u| u.side == Side::Defender && !u.is_hq())
            .unwrap();
        assert_eq!(links[def_idx], CommandLink::OutOfRange); // defender: 5 > 3
    }

    #[test]
    fn synthesize_hqs_is_strictly_idempotent() {
        // A repeated call must not append a duplicate HQ.
        let mut units = vec![inf(0, "Div", HexCoord::ZERO), inf(1, "Div", HexCoord::ZERO)];
        deploy(&mut units, Side::Attacker);
        deploy(&mut units, Side::Attacker);
        let hqs = units.iter().filter(|u| u.is_hq()).count();
        assert_eq!(hqs, 1, "repeated synthesis must not grow a second HQ");
    }

    #[test]
    fn synthesize_hqs_blocked_by_broken_hq_remnant() {
        // The guard keys on HQ EXISTENCE, not combat effectiveness: a HQ
        // beaten to org 0 but not yet removed from the roster still blocks
        // re-synthesis.
        let mut units = vec![inf(0, "Div", HexCoord::ZERO), inf(1, "Div", HexCoord::ZERO)];
        deploy(&mut units, Side::Attacker);
        let hq = units.iter_mut().find(|u| u.is_hq()).unwrap();
        hq.org = 0.0;
        assert!(!hq.is_combat_effective());
        deploy(&mut units, Side::Attacker);
        let hqs = units.iter().filter(|u| u.is_hq()).count();
        assert_eq!(hqs, 1, "a broken HQ remnant still blocks re-synthesis");
    }
}
