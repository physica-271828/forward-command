//! tactical-ai — tactic-driven enemy AI for the tactical wargame (DESIGN §7).
//!
//! Implements the three-layer decision architecture of §7.1:
//! strategic objective selection per the §7.2 tactic table, role assignment
//! by battalion type, and per-unit action execution on top of
//! `tactical-core` pathfinding / line-of-sight, under the §7.3 constraint
//! rules (damaged units withdraw, 3:1 local odds refuse assault, artillery
//! keeps 1–2 hexes behind the frontline, support companies stay attached).
//!
//! The AI only *proposes* [`AiAction`]s — it never mutates unit state. The
//! game loop executes the proposals in order via `tactical-combat` (§6.3,
//! §6.12). Decisions are deterministic for a fixed RNG seed (`XorShift64`).
//!
//! Fog-of-war note: the game loop pre-filters enemy state by the
//! AI side's fog before planning, so objectives and valuations only ever
//! use what the AI can actually see.

pub mod action;
mod deploy;
mod planner;
pub mod tactic;

pub use action::AiAction;
pub use deploy::ai_deploy;
pub use deploy::ai_deploy_impl;
pub use planner::{DivOrderTarget, StrategicObjective, TacticalAi, UnitRole};
pub use tactic::CombatTactic;

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_core::{BattalionUnit, HexCoord, HexGrid, Side, Terrain, UnitState, UnitType};

    fn grid(w: usize, h: usize) -> HexGrid {
        HexGrid::new(w, h, Terrain::Plains)
    }

    /// Synthetic battalion with combat-ready stats.
    fn unit(id: usize, ty: UnitType, side: Side, q: i32, r: i32) -> BattalionUnit {
        let mut u = BattalionUnit::new(id, format!("U{id}"), ty, side, HexCoord::new(q, r));
        u.soft_attack = 25.0;
        u.hard_attack = 12.0;
        u.defense = 40.0;
        u.max_org = 100.0;
        u.org = 100.0;
        u.max_strength = 100.0;
        u.strength = 100.0;
        u
    }

    fn set_org_pct(u: &mut BattalionUnit, pct: f32) {
        u.org = u.max_org * pct;
    }

    fn dist_to_nearest(h: HexCoord, units: &[BattalionUnit]) -> i32 {
        units.iter().map(|u| h.distance(u.position)).min().unwrap()
    }

    fn action_for<'a>(actions: &'a [AiAction], unit_id: usize) -> &'a AiAction {
        actions
            .iter()
            .find(|a| match a {
                AiAction::MoveUnit { unit_id: id, .. }
                | AiAction::Assault {
                    attacker_id: id, ..
                }
                | AiAction::FireSupport {
                    attacker_id: id, ..
                }
                | AiAction::Hold { unit_id: id }
                | AiAction::Emplace { unit_id: id }
                | AiAction::Limber { unit_id: id }
                | AiAction::Retreat { unit_id: id } => *id == unit_id,
                AiAction::EndTurn => false,
            })
            .unwrap_or_else(|| panic!("no action for unit {unit_id}"))
    }

    fn move_dest(action: &AiAction) -> HexCoord {
        match action {
            AiAction::MoveUnit { path, .. } => *path.last().unwrap(),
            other => panic!("expected MoveUnit, got {other:?}"),
        }
    }

    // 1 ─ Tactic parsing (§7.2 tokens) + metadata for the Tactic Card (§9.1).
    #[test]
    fn tactic_parsing_and_metadata() {
        assert_eq!(CombatTactic::from_str("blitz"), CombatTactic::Blitz);
        assert_eq!(CombatTactic::from_str("Blitz"), CombatTactic::Blitz);
        assert_eq!(
            CombatTactic::from_str("elastic_defense"),
            CombatTactic::ElasticDefense
        );
        assert_eq!(
            CombatTactic::from_str("overwhelming_fire"),
            CombatTactic::OverwhelmingFire
        );
        assert_eq!(
            CombatTactic::from_str("infiltration_assault"),
            CombatTactic::InfiltrationAssault
        );
        assert_eq!(
            CombatTactic::from_str("mass_charge"),
            CombatTactic::MassCharge
        );
        assert_eq!(
            CombatTactic::from_str("guerrilla_tactics"),
            CombatTactic::GuerrillaTactics
        );
        assert_eq!(
            CombatTactic::from_str("tactical_withdrawal"),
            CombatTactic::TacticalWithdrawal
        );
        assert_eq!(
            CombatTactic::from_str("encirclement"),
            CombatTactic::Encirclement
        );
        assert_eq!(
            CombatTactic::from_str("basic_attack"),
            CombatTactic::Default
        );
        assert_eq!(CombatTactic::from_str("nonsense"), CombatTactic::Default);

        for t in [
            CombatTactic::Blitz,
            CombatTactic::ElasticDefense,
            CombatTactic::OverwhelmingFire,
            CombatTactic::InfiltrationAssault,
            CombatTactic::MassCharge,
            CombatTactic::GuerrillaTactics,
            CombatTactic::TacticalWithdrawal,
            CombatTactic::Encirclement,
            CombatTactic::Counterattack,
            CombatTactic::Ambush,
            CombatTactic::RiverDefense,
            CombatTactic::UrbanDefense,
            CombatTactic::Delay,
            CombatTactic::Assault,
            CombatTactic::RiverAssault,
            CombatTactic::Default,
        ] {
            assert!(!t.name().is_empty());
            assert!(!t.description().is_empty());
            assert!(!t.counter_hint().is_empty());
            assert_eq!(CombatTactic::from_str(t.token()), t);
        }
    }

    // EVERY vanilla HOI4 tactic token lands on exactly one of the
    // 16 cards — nothing falls to Default except basic_defend and
    // unknown tokens. The two card names without a vanilla token
    // (infiltration_assault / mass_charge) ride along as accepted inputs.
    #[test]
    fn all_vanilla_tokens_single_map_to_the_16_cards() {
        let vanilla: [(&str, CombatTactic); 57] = [
            ("basic_attack", CombatTactic::Default),
            ("basic_defend", CombatTactic::Default),
            ("counterattack", CombatTactic::Counterattack),
            ("backhand_blow", CombatTactic::Counterattack),
            ("assault", CombatTactic::Assault),
            ("planned_attack", CombatTactic::Assault),
            ("relentless_assault", CombatTactic::Assault),
            ("unexpected_thrust", CombatTactic::Assault),
            ("barrage", CombatTactic::Assault),
            ("cc_attack", CombatTactic::Assault),
            ("cc_defend", CombatTactic::Assault),
            ("tw_attack", CombatTactic::Assault),
            ("tw_chase", CombatTactic::Assault),
            ("tw_intercept", CombatTactic::Assault),
            ("cc_storm", CombatTactic::MassCharge),
            ("shock", CombatTactic::MassCharge),
            ("human_wave_tactics", CombatTactic::MassCharge),
            ("banzai_charge", CombatTactic::MassCharge),
            ("grand_banzai_charge", CombatTactic::MassCharge),
            ("infantry_charge", CombatTactic::MassCharge),
            ("encirclement", CombatTactic::Encirclement),
            ("ambush", CombatTactic::Ambush),
            ("cc_local_strong_point", CombatTactic::Ambush),
            ("delay", CombatTactic::Delay),
            ("masterful_delay", CombatTactic::Delay),
            ("tactical_withdrawal", CombatTactic::TacticalWithdrawal),
            ("tw_defend", CombatTactic::TacticalWithdrawal),
            ("tw_evade", CombatTactic::TacticalWithdrawal),
            ("cc_withdraw", CombatTactic::Assault),
            ("blitz", CombatTactic::Blitz),
            ("masterful_blitz", CombatTactic::Blitz),
            ("breakthrough", CombatTactic::Blitz),
            ("elastic_defense", CombatTactic::ElasticDefense),
            ("overwhelming_fire", CombatTactic::OverwhelmingFire),
            ("guerrilla_tactics", CombatTactic::GuerrillaTactics),
            ("infiltration_assault", CombatTactic::InfiltrationAssault),
            ("mass_charge", CombatTactic::MassCharge),
            ("seize_bridge", CombatTactic::RiverAssault),
            ("attacker_sb_hold", CombatTactic::RiverAssault),
            ("attacker_sb_skillful_defence", CombatTactic::RiverAssault),
            ("attacker_hb_attack", CombatTactic::RiverAssault),
            ("attacker_hb_rush", CombatTactic::RiverAssault),
            ("attacker_hb_storm", CombatTactic::RiverAssault),
            ("hold_bridge", CombatTactic::RiverDefense),
            ("defender_sb_assault", CombatTactic::RiverDefense),
            ("defender_sb_reckless_assault", CombatTactic::RiverDefense),
            ("defender_sb_retake_bridge", CombatTactic::RiverDefense),
            ("defender_hb_hold", CombatTactic::RiverDefense),
            ("defender_hb_skillful_defence", CombatTactic::RiverDefense),
            ("urban_defense", CombatTactic::UrbanDefense),
            ("sf_defense", CombatTactic::UrbanDefense),
            ("sf_fortify", CombatTactic::UrbanDefense),
            ("sf_ambush", CombatTactic::UrbanDefense),
            ("sf_storm", CombatTactic::Assault),
            ("sf_barrage", CombatTactic::Assault),
            ("sf_armor_supported_assault", CombatTactic::Assault),
            ("sf_mouse_holing", CombatTactic::Assault),
        ];
        for (token, card) in vanilla {
            assert_eq!(
                CombatTactic::from_str(token),
                card,
                "token '{token}' must map onto {card:?}"
            );
        }
    }

    // A Default attacker is lifted onto the plain-advance posture: the
    // generic vanilla attack roll (basic_attack) must march on the intel
    // objective instead of parking invisibly at the deployment, while a
    // Default defender keeps the hold-and-engage posture.
    #[test]
    fn default_attacker_advances_on_objective() {
        let g = grid(14, 14);
        let mut atk = TacticalAi::new(Side::Attacker, CombatTactic::Default, 9);
        assert_eq!(
            atk.tactic,
            CombatTactic::Assault,
            "Default attacker folds onto the plain-advance card"
        );
        let def = TacticalAi::new(Side::Defender, CombatTactic::Default, 9);
        assert_eq!(
            def.tactic,
            CombatTactic::Default,
            "Default defender keeps the hold posture"
        );

        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 2, 2)];
        let enemy: Vec<BattalionUnit> = vec![];
        let goal_hex = unit(99, UnitType::Infantry, Side::Defender, 11, 11);
        let start_dist = dist_to_nearest(own[0].position, std::slice::from_ref(&goal_hex));
        let actions = atk.plan_turn_toward(&g, &own, &enemy, Some(goal_hex.position));
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert!(
                    dist_to_nearest(dest, std::slice::from_ref(&goal_hex)) < start_dist,
                    "Default attacker must close on the objective, ended at {dest:?}"
                );
            }
            other => panic!("Default attacker must advance on the objective, got {other:?}"),
        }
    }

    // 2 ─ §7.2 blitz: armor rushes toward the enemy (deep penetration).
    #[test]
    fn blitz_advances_armor_toward_enemy() {
        let g = grid(10, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 42);

        let tank = unit(1, UnitType::MediumArmor, Side::Attacker, 3, 1);
        let own = vec![
            tank.clone(),
            unit(2, UnitType::Infantry, Side::Attacker, 1, 1),
        ];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 3, 5)];

        let actions = ai.plan_turn(&g, &own, &enemy);
        let act = action_for(&actions, 1);
        let dest = move_dest(act);
        assert!(
            dest.distance(enemy[0].position) < tank.position.distance(enemy[0].position),
            "blitz armor must close on the enemy: dest {dest:?}"
        );
    }

    // 3 ─ §7.2 elastic_defense: fall back 1 hex when attacked by a stronger
    //     (non-isolated) enemy instead of counter-attacking.
    #[test]
    fn elastic_defense_falls_back_when_adjacent() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 999);

        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        // Two mutually supporting enemies adjacent to our unit.
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 5, 4),
            unit(11, UnitType::Infantry, Side::Attacker, 4, 4),
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        let act = action_for(&actions, 1);
        let dest = move_dest(act);
        assert!(
            dist_to_nearest(dest, &enemy) >= 2,
            "elastic defense must disengage to distance ≥2, ended at {dest:?}"
        );
        assert_eq!(
            dest.distance(HexCoord::new(5, 5)),
            1,
            "falls back exactly 1 hex"
        );
    }

    // 4 ─ §7.3: artillery stays 1–2 hexes behind the frontline.
    #[test]
    fn artillery_stays_off_the_frontline() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 7);

        // Artillery starting in contact with the enemy.
        let arty = unit(1, UnitType::ArtilleryBrigade, Side::Defender, 5, 5);
        let own = vec![arty, unit(2, UnitType::Infantry, Side::Defender, 5, 4)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 5, 6)];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert!(
                    dist_to_nearest(dest, &enemy) >= 2,
                    "artillery must keep standoff ≥2, ended at {dest:?}"
                );
            }
            other => panic!("artillery in contact must reposition, got {other:?}"),
        }
    }

    // 5 ─ §7.3: a unit under 30% org is withdrawn from the frontline.
    #[test]
    fn damaged_unit_withdraws() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 5);

        let mut battered = unit(1, UnitType::Infantry, Side::Defender, 5, 5);
        set_org_pct(&mut battered, 0.25); // 25% org < 30% threshold
        let own = vec![battered, unit(2, UnitType::Infantry, Side::Defender, 6, 5)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 5, 3)];

        let start_dist = dist_to_nearest(HexCoord::new(5, 5), &enemy);
        let actions = ai.plan_turn(&g, &own, &enemy);
        let act = action_for(&actions, 1);
        match act {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert!(
                    dist_to_nearest(dest, &enemy) > start_dist,
                    "damaged unit must move away from the enemy, ended at {dest:?}"
                );
            }
            AiAction::Retreat { .. } => {} // also acceptable per §6.8
            other => panic!("damaged unit must withdraw, got {other:?}"),
        }
    }

    // 6 ─ §7.3: outnumbered 3:1 locally → refuse assault, go defensive.
    #[test]
    fn outnumbered_three_to_one_refuses_assault() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 3);

        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        // Mutually supporting triangle: every hex the attacker is adjacent to
        // has the defender plus two adjacent friends → foes 3, friends 1.
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Defender, 5, 4),
            unit(11, UnitType::Infantry, Side::Defender, 4, 4),
            unit(12, UnitType::Infantry, Side::Defender, 4, 5),
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "3:1 local odds must refuse assault: {actions:?}"
        );
    }

    // An org-0 remnant (Withdrawn on the zone rim, or Active at 0) blocks
    // the corridor — the AI must CLEAR it with an assault, not treat it as
    // invisible to attack while pathfinding routes around its hex.
    #[test]
    fn ai_assaults_org_zero_remnant() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);

        let mut wreck = unit(10, UnitType::Infantry, Side::Defender, 5, 4);
        wreck.org = 0.0;
        wreck.state = UnitState::Withdrawn;
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        let enemy = vec![wreck];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::Assault { target_id, .. } => {
                assert_eq!(*target_id, 10, "must clear the org-0 remnant")
            }
            other => panic!("must assault the org-0 remnant, got {other:?}"),
        }
    }

    #[test]
    fn ai_assaults_broken_active_unit_too() {
        // Same rule for an Active unit whose org was just ground to 0 —
        // it is a corpse in the way until it breaks (or is cleared).
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);

        let mut zombie = unit(10, UnitType::Infantry, Side::Defender, 5, 4);
        zombie.org = 0.0;
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        let enemy = vec![zombie];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::Assault { target_id, .. } => {
                assert_eq!(*target_id, 10, "must clear the broken Active unit")
            }
            other => panic!("must assault the org-0 Active unit, got {other:?}"),
        }
    }

    // 7 ─ §6.2: units that already acted this turn hold.
    #[test]
    fn acted_units_hold() {
        let g = grid(10, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 1);

        let mut exhausted = unit(1, UnitType::MediumArmor, Side::Attacker, 3, 1);
        exhausted.acted = true;
        let own = vec![exhausted];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 3, 5)];

        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(matches!(
            action_for(&actions, 1),
            AiAction::Hold { unit_id: 1 }
        ));
    }

    // 8 ─ §7.2 mass_charge: all infantry advances simultaneously, max 1 hex.
    #[test]
    fn mass_charge_advances_line_one_hex() {
        let g = grid(10, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::MassCharge, 11);

        let own = vec![
            unit(1, UnitType::Infantry, Side::Attacker, 2, 1),
            unit(2, UnitType::Infantry, Side::Attacker, 3, 1),
            unit(3, UnitType::Infantry, Side::Attacker, 4, 1),
        ];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Defender, 2, 4),
            unit(11, UnitType::Infantry, Side::Defender, 3, 4),
            unit(12, UnitType::Infantry, Side::Defender, 4, 4),
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        for u in &own {
            let act = action_for(&actions, u.id);
            match act {
                AiAction::MoveUnit { path, .. } => {
                    assert_eq!(path.len(), 1, "mass charge advances max 1 hex: {path:?}");
                    let dest = path[0];
                    assert!(
                        dist_to_nearest(dest, &enemy) < dist_to_nearest(u.position, &enemy),
                        "must advance toward the enemy"
                    );
                }
                other => panic!("mass charge infantry must advance, got {other:?}"),
            }
        }
    }

    // 9 ─ §7.2 guerrilla_tactics: never end the turn adjacent to the enemy.
    #[test]
    fn guerrilla_never_ends_adjacent() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::GuerrillaTactics, 77);

        // Starting 3 hexes away: AP allows reaching adjacency, but doctrine forbids it.
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 6)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 5, 3)];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert!(
                    dist_to_nearest(dest, &enemy) >= 2,
                    "guerrilla must not end adjacent, ended at {dest:?}"
                );
                assert!(
                    dist_to_nearest(dest, &enemy) < 3,
                    "guerrilla should still close to striking distance"
                );
            }
            other => panic!("guerrilla should maneuver, got {other:?}"),
        }
    }

    // 10 ─ §7.2 overwhelming_fire: all artillery concentrates on the weakest
    //      hex — but towed guns must emplace first (§6.3).
    #[test]
    fn overwhelming_fire_concentrates_on_weakest() {
        let g = grid(12, 10);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::OverwhelmingFire, 13);

        let own = vec![
            unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 2, 4),
            unit(2, UnitType::ArtilleryBrigade, Side::Attacker, 6, 4),
            unit(3, UnitType::Infantry, Side::Attacker, 4, 1),
        ];
        let mut weak = unit(10, UnitType::Infantry, Side::Defender, 4, 3);
        set_org_pct(&mut weak, 0.20);
        let strong = unit(11, UnitType::Infantry, Side::Defender, 4, 5);
        let enemy = vec![weak, strong];

        // Turn 1: both towed batteries are limbered — they emplace.
        let actions = ai.plan_turn(&g, &own, &enemy);
        for arty_id in [1usize, 2] {
            assert!(
                matches!(action_for(&actions, arty_id), AiAction::Emplace { .. }),
                "battery {arty_id} must emplace before firing: {actions:?}"
            );
        }

        // Turn 2 (emplaced): both concentrate on the weakest hex.
        let own_emplaced: Vec<BattalionUnit> = own
            .into_iter()
            .map(|mut u| {
                if u.requires_emplacement() {
                    u.is_emplaced = true;
                }
                u
            })
            .collect();
        let actions = ai.plan_turn(&g, &own_emplaced, &enemy);
        for arty_id in [1usize, 2] {
            match action_for(&actions, arty_id) {
                AiAction::FireSupport { target_hex, .. } => {
                    assert_eq!(
                        *target_hex,
                        HexCoord::new(4, 3),
                        "battery {arty_id} must concentrate on the weakest hex"
                    );
                }
                other => panic!("battery {arty_id} should fire, got {other:?}"),
            }
        }
        // Infantry holds the line (§7.2).
        assert!(matches!(
            action_for(&actions, 3),
            AiAction::Hold { unit_id: 3 }
        ));
    }

    // 11 ─ Determinism: same seed + same inputs → identical action list.
    #[test]
    fn plan_turn_is_deterministic_per_seed() {
        let g = grid(12, 10);
        let own = vec![
            unit(1, UnitType::MediumArmor, Side::Attacker, 3, 2),
            unit(2, UnitType::Infantry, Side::Attacker, 2, 2),
            unit(3, UnitType::Recon, Side::Attacker, 1, 3),
            unit(4, UnitType::ArtilleryBrigade, Side::Attacker, 4, 1),
        ];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Defender, 3, 6),
            unit(11, UnitType::Infantry, Side::Defender, 5, 5),
            unit(12, UnitType::Infantry, Side::Defender, 6, 6),
        ];

        let mut a = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 12345);
        let mut b = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 12345);
        let ra = a.plan_turn(&g, &own, &enemy);
        let rb = b.plan_turn(&g, &own, &enemy);
        assert_eq!(ra, rb);
    }

    // 12 ─ Turn protocol: exactly one EndTurn, as the final action (§6.12).
    #[test]
    fn end_turn_is_final_action() {
        let g = grid(10, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 2);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 2, 2)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 2, 6)];

        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(matches!(actions.last(), Some(AiAction::EndTurn)));
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, AiAction::EndTurn))
                .count(),
            1
        );
    }

    // 13 ─ River discipline: a battery prefers the EXPOSED dry-crest target
    //      over one caught in the valley. The river row still punishes
    //      assaults (melee gain × river ×2 — the attacker standing on the
    //      bank fires down into the ford), but REMOTE fire on a ford target
    //      reads the near bank as defilade: the target's step (elev −1
    //      river bed) is LOWER than its neighbour, so the bank throttles
    //      the shell — ×0.5, cancelling the ×2 ford column. The guns hit
    //      what they can see.
    #[test]
    fn artillery_prefers_exposed_crest_over_defiladed_ford() {
        let mut g = grid(12, 12);
        g.set_terrain(HexCoord::new(4, 7), Terrain::River); // elev −1 river bed
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 5);

        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Defender, 4, 4);
        arty.is_emplaced = true;
        arty.attack_range = 8;
        let own = vec![arty];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 4, 7), // mid-ford, defiladed
            unit(11, UnitType::Infantry, Side::Attacker, 7, 4), // dry crest step, same distance
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(
                    *target_hex,
                    HexCoord::new(7, 4),
                    "the defiladed ford target must NOT outrank the exposed dry one at equal range"
                );
            }
            other => panic!("emplaced battery with in-range targets must fire, got {other:?}"),
        }
    }

    // 13.5 ─ Spotting is no longer a sight check — a mission is PRECISE
    //      iff the aim hex holds a fog-VISIBLE enemy (the player-facing
    //      rule: right-click a visible enemy = full damage; F-barrage /
    //      intel fire = area ÷7). Every enemy the planner sees (ctx.enemy
    //      is fog-filtered) is therefore a full-damage target, and ranking
    //      falls back to raw expected damage — the "borrowed eyes"
    //      discrimination is gone.
    #[test]
    fn visible_enemies_all_precise_damage_decides() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 5);

        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Defender, 4, 4);
        arty.is_emplaced = true;
        arty.attack_range = 8;
        let mut tough = unit(10, UnitType::Infantry, Side::Attacker, 8, 4);
        tough.defense = 100.0;
        let mut weak = unit(11, UnitType::Infantry, Side::Attacker, 4, 8);
        weak.defense = 10.0;
        let own = vec![arty];
        let enemy = vec![tough, weak];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(
                    *target_hex,
                    HexCoord::new(4, 8),
                    "both visible → both precise (full damage) → the weaker hex wins on expected damage"
                );
            }
            other => panic!("emplaced battery with in-range targets must fire, got {other:?}"),
        }
    }

    // 13.6 ─ The single in-range visible enemy is a precise (full-damage)
    //      mission — no observer is needed any more.
    #[test]
    fn lone_visible_enemy_is_engaged_precise() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 5);
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Defender, 4, 4);
        arty.is_emplaced = true;
        arty.attack_range = 8;
        let own = vec![arty];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 4, 8)];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(*target_hex, HexCoord::new(4, 8));
            }
            other => panic!("emplaced battery must fire the visible enemy, got {other:?}"),
        }
    }

    // 13.7 ─ Rocket exemption: a rocket salvo never dilutes (every unit
    //      in the zone takes full-strength hits), so its ranking ignores
    //      the area-fire penalty — the raw weakest target wins even when a
    //      tougher one is precise-able.
    #[test]
    fn rocket_ranking_ignores_area_dilution() {
        let mut g = grid(12, 12);
        g.cell_mut(HexCoord::new(6, 4)).unwrap().elevation = 4;
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 5);

        let mut rkt = unit(1, UnitType::RocketArtillery, Side::Defender, 4, 4);
        rkt.is_emplaced = true;
        rkt.attack_range = 8;
        let observer = unit(2, UnitType::Infantry, Side::Defender, 6, 4);
        let mut tough = unit(10, UnitType::Infantry, Side::Attacker, 8, 4); // visible, tough
        tough.defense = 100.0;
        let mut weak = unit(11, UnitType::Infantry, Side::Attacker, 4, 8); // raw-weak
        weak.defense = 10.0;
        let own = vec![rkt, observer];
        let enemy = vec![tough, weak];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(
                    *target_hex,
                    HexCoord::new(4, 8),
                    "rockets rank by raw strength, never the area-fire penalty"
                );
            }
            other => panic!("rocket launcher with in-range targets must fire, got {other:?}"),
        }
    }

    // 14 ─ On a sparse long front the defender's towed guns never fired —
    //      they crept toward visible enemies 20+ hexes away and their
    //      re-planned routes kept resetting march hours. The defender is
    //      therefore a fire base: out of range, it emplaces in place (the
    //      enemy comes to it); the attacker keeps the §7.3 creep (its guns
    //      must follow the advance).
    #[test]
    fn defender_towed_gun_emplaces_as_fire_base_out_of_range() {
        let g = grid(12, 12);
        let own = |side: Side| vec![unit(1, UnitType::ArtilleryBrigade, side, 2, 2)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 8, 8)];

        // Defender, enemy 12 hexes away (> range 9): emplace as a fire base.
        let mut def_ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 7);
        let actions = def_ai.plan_turn(&g, &own(Side::Defender), &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Emplace { unit_id: 1 }),
            "defender battery out of range must emplace in place: {actions:?}"
        );

        // Attacker, same geometry: creep toward the enemy instead.
        let mut atk_ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = atk_ai.plan_turn(&g, &own(Side::Attacker), &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::MoveUnit { .. }),
            "attacker battery out of range must creep, not squat: {actions:?}"
        );
    }

    // 15 ─ An EMPLACED defender gun does not limber when the enemy leaves
    //      the envelope — a fire base does not chase; only the attacker
    //      limbers to follow the advance.
    #[test]
    fn defender_emplaced_gun_stays_when_enemy_leaves_envelope() {
        let g = grid(12, 12);
        let own = |side: Side| {
            let mut u = unit(1, UnitType::ArtilleryBrigade, side, 2, 2);
            u.is_emplaced = true;
            vec![u]
        };
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 8, 8)];

        let mut def_ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 7);
        let actions = def_ai.plan_turn(&g, &own(Side::Defender), &enemy);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Limber { .. }),
            "defender fire base must not limber to chase: {actions:?}"
        );

        let mut atk_ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = atk_ai.plan_turn(&g, &own(Side::Attacker), &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Limber { unit_id: 1 }),
            "attacker battery must limber to follow the advance: {actions:?}"
        );
    }

    // 16 ─ Blind bombardment: an emplaced tube battery with no visible
    //      target and the intel zone (the besieged city) in range SHELLS
    //      the intel goal instead of limbering away — a siege must bleed
    //      (otherwise it stalls into a 300-turn staring match).
    #[test]
    fn attacker_artillery_blind_fires_intel_goal_when_no_visible_target() {
        let g = grid(20, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        // Emplaced battery; the enemy is hidden by fog (no visible targets)
        // and the intel zone sits 5 hexes ahead — in range 9.
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 4, 4);
        arty.is_emplaced = true;
        let own = vec![arty];
        let enemy: Vec<BattalionUnit> = vec![];
        let intel = vec![HexCoord::new(9, 4)];

        let actions = ai.plan_turn_zone(&g, &own, &enemy, Some(&intel));
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(
                    *target_hex,
                    HexCoord::new(9, 4),
                    "blind fire must land on the intel goal"
                );
            }
            other => panic!("emplaced battery with intel in range must blind-fire, got {other:?}"),
        }
    }

    // 17 ─ Urban storm rule: the besieger may assault an urban garrison
    //      beaten below 40% org or locally outnumbered ≥3:1 despite the
    //      hopeless-trade math — the 26 Sep Warsaw general assault after
    //      days of bombardment. The storm's target gate covers ANY beaten
    //      unit: a visible 7%-org field remnant blocks a siege the same
    //      way.
    #[test]
    fn urban_garrison_can_be_stormed_when_beaten_or_outnumbered() {
        let mut g = grid(12, 12);
        g.set_terrain(HexCoord::new(6, 5), Terrain::Urban);

        // Control: fresh garrison, single attacker — the trade math refuses.
        let mut fresh = unit(10, UnitType::Infantry, Side::Defender, 6, 5);
        fresh.org = 90.0;
        fresh.defense = 100.0; // hard to hurt: hopeless for a lone rifle
        let own_single = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = ai.plan_turn(&g, &own_single, &[fresh]);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "fresh lone garrison in urban must refuse the assault: {actions:?}"
        );

        // Case A: the garrison is beaten below 40% org → storm.
        let mut beaten = unit(11, UnitType::Infantry, Side::Defender, 6, 5);
        beaten.org = 30.0;
        beaten.defense = 100.0;
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = ai.plan_turn(&g, &own_single, &[beaten]);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "beaten urban garrison must be stormable: {actions:?}"
        );

        // Case B: 3:1 local advantage over a fresh garrison → storm.
        let mut fresh2 = unit(12, UnitType::Infantry, Side::Defender, 6, 5);
        fresh2.org = 90.0;
        fresh2.defense = 100.0;
        let own_three = vec![
            unit(1, UnitType::Infantry, Side::Attacker, 5, 5),
            unit(2, UnitType::Infantry, Side::Attacker, 6, 6),
            unit(3, UnitType::Infantry, Side::Attacker, 7, 5),
        ];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = ai.plan_turn(&g, &own_three, &[fresh2]);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "3:1 urban siege line must storm: {actions:?}"
        );
    }

    // 18 ─ Coordinated fires: under the assault cards the fire preparation
    //      lands on the hex the assault pool will storm (the would-be
    //      pool's volley decides), NOT the globally weakest hex; no
    //      poolable target falls back to the weakest hex; Attrition
    //      (OverwhelmingFire) keeps the weakest hex regardless.
    #[test]
    fn assault_card_prep_targets_the_pool_hex() {
        let g = grid(12, 12);
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 4, 4);
        arty.is_emplaced = true;
        arty.attack_range = 8;
        // The pool: three rifles adjacent to the TOUGH defender at (8,4).
        let p1 = unit(2, UnitType::Infantry, Side::Attacker, 7, 4);
        let p2 = unit(3, UnitType::Infantry, Side::Attacker, 8, 3);
        let p3 = unit(4, UnitType::Infantry, Side::Attacker, 8, 5);
        let own = vec![arty, p1, p2, p3];
        // Tough but pooled: the breach point. Weak (20% org) and isolated:
        // the old globally-weakest pick.
        let mut tough = unit(10, UnitType::Infantry, Side::Defender, 8, 4);
        tough.defense = 100.0;
        let mut weak = unit(11, UnitType::Infantry, Side::Defender, 4, 8);
        set_org_pct(&mut weak, 0.2);
        weak.defense = 10.0;
        let enemy = vec![tough, weak];

        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 5);
        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(
                    *target_hex,
                    HexCoord::new(8, 4),
                    "assault-card preparation must land on the pool's breach hex"
                );
            }
            other => panic!("emplaced battery with in-range targets must fire, got {other:?}"),
        }
    }

    #[test]
    fn assault_card_prep_falls_back_to_weakest_without_a_pool() {
        let g = grid(12, 12);
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 4, 4);
        arty.is_emplaced = true;
        arty.attack_range = 8;
        let own = vec![arty]; // no line — no pool can form anywhere
        let mut tough = unit(10, UnitType::Infantry, Side::Defender, 8, 4);
        tough.defense = 100.0;
        let mut weak = unit(11, UnitType::Infantry, Side::Defender, 4, 8);
        set_org_pct(&mut weak, 0.2);
        weak.defense = 10.0;
        let enemy = vec![tough, weak];

        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 5);
        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(
                    *target_hex,
                    HexCoord::new(4, 8),
                    "no poolable target → the weakest-hex preparation stands"
                );
            }
            other => panic!("emplaced battery with in-range targets must fire, got {other:?}"),
        }
    }

    #[test]
    fn attrition_prep_keeps_the_weakest_hex() {
        // Same layout as the pool test — a poolable tough target exists —
        // but the Attrition doctrine (OverwhelmingFire) bleeds the weakest.
        let g = grid(12, 12);
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 4, 4);
        arty.is_emplaced = true;
        arty.attack_range = 8;
        let p1 = unit(2, UnitType::Infantry, Side::Attacker, 7, 4);
        let p2 = unit(3, UnitType::Infantry, Side::Attacker, 8, 3);
        let p3 = unit(4, UnitType::Infantry, Side::Attacker, 8, 5);
        let own = vec![arty, p1, p2, p3];
        let mut tough = unit(10, UnitType::Infantry, Side::Defender, 8, 4);
        tough.defense = 100.0;
        let mut weak = unit(11, UnitType::Infantry, Side::Defender, 4, 8);
        set_org_pct(&mut weak, 0.2);
        weak.defense = 10.0;
        let enemy = vec![tough, weak];

        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::OverwhelmingFire, 5);
        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(
                    *target_hex,
                    HexCoord::new(4, 8),
                    "Attrition bleeds the weakest hex even when a pool target exists"
                );
            }
            other => panic!("emplaced battery with in-range targets must fire, got {other:?}"),
        }
    }

    // 17.5 ─ Fog-wall probe: the last enemy stand can sit in DARK fog
    //      (mountain LOS + expired reveal) — the fog view sees nothing, yet
    //      its hex blocks the route and the march halts adjacent every
    //      turn. A beaten (<40% org) invisible wall must be stormed blind;
    //      a fresh one stays unmoved.
    #[test]
    fn beaten_invisible_wall_is_stormed_blind() {
        let g = grid(12, 12);
        let own_single = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        // The unit marches on the pre-battle intel (the frozen battle's
        // blind goal); the fog view (enemy_units) carries NO enemy.
        let intel = vec![HexCoord::new(6, 6)];

        // Case A: the wall is beaten below 40% org → blind storm.
        let mut beaten = unit(10, UnitType::Infantry, Side::Defender, 6, 5);
        set_org_pct(&mut beaten, 0.3);
        beaten.defense = 100.0; // hopeless trade on paper — storm must open
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = ai.plan_turn_full(
            &g,
            &own_single,
            &[],
            Some(&intel),
            None,
            None,
            Some(std::slice::from_ref(&beaten)),
        );
        assert!(
            matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "beaten invisible wall must be probed blind: {actions:?}"
        );

        // Control: a FRESH invisible wall is not worth the blind trade —
        // the probe needs the storm conditions.
        let mut fresh = unit(11, UnitType::Infantry, Side::Defender, 6, 5);
        fresh.org = 90.0;
        fresh.defense = 100.0;
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = ai.plan_turn_full(
            &g,
            &own_single,
            &[],
            Some(&intel),
            None,
            None,
            Some(std::slice::from_ref(&fresh)),
        );
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "fresh invisible wall stays unmoved: {actions:?}"
        );

        // Widening the storm to ANY beaten target was tried and REVERTED —
        // at 60% and at 40% all-target the visible-field assaults on rough
        // ground lost more than they gained: the gate stays
        // urban/invisible — a VISIBLE beaten field remnant is finished by
        // the PLAYER, not by an AI free-for-all.
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 7);
        let actions = ai.plan_turn_full(
            &g,
            &own_single,
            std::slice::from_ref(&beaten),
            Some(&intel),
            None,
            None,
            Some(std::slice::from_ref(&beaten)),
        );
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "visible beaten enemy on plains stays doctrine-refused: {actions:?}"
        );
    }

    // 18 ─ Line anchoring: falling back never steps INTO a ford, even when
    //      the ford is the best-scoring retreat hex.
    #[test]
    fn fall_back_never_steps_into_river() {
        let mut g = grid(12, 12);
        g.set_terrain(HexCoord::new(4, 5), Terrain::River);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 999);

        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        // Two mutually supporting enemies adjacent (east side): the only two
        // disengaging hexes are the ford (4,5) and dry (4,6) — the ford would
        // win the coordinate tie-break if it were not rejected outright.
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 6, 5),
            unit(11, UnitType::Infantry, Side::Attacker, 6, 4),
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(
            dest,
            HexCoord::new(4, 6),
            "ford must be rejected, got {dest:?}"
        );
    }

    // 15 ─ Line anchoring: among equal-distance retreat hexes the best
    //      defensive ground wins (forest beats bare plains).
    #[test]
    fn fall_back_prefers_defensive_terrain() {
        let mut g = grid(12, 12);
        g.set_terrain(HexCoord::new(5, 6), Terrain::Forest);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 999);

        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 5, 4),
            unit(11, UnitType::Infantry, Side::Attacker, 6, 4),
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(
            dest,
            HexCoord::new(5, 6),
            "forest beats plains at equal distance, got {dest:?}"
        );
    }

    // 16 ─ River discipline: the elastic second line forms on the OWN side
    //      of the river — reinforcing never crosses to the threat's bank
    //      (the failure this prevents: 4.Inf ordered across the Meuse at
    //      turn 3).
    #[test]
    fn elastic_reinforcement_stays_behind_river() {
        let mut g = grid(16, 12);
        for r in 0..12 {
            g.set_terrain(HexCoord::new(8, r), Terrain::River);
        }
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 999);

        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 10, 7)];
        // Two mutually supporting attackers on the WEST bank, distance 3.
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 7, 8),
            unit(11, UnitType::Infantry, Side::Attacker, 7, 9),
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        let dest = move_dest(action_for(&actions, 1));
        assert!(
            dest.q >= 9,
            "second line must form east of the river, got {dest:?}"
        );
        assert_eq!(
            g.cell(dest).map(|c| c.terrain),
            Some(Terrain::Plains),
            "never screen from inside a ford"
        );
        assert_eq!(
            dest.distance(HexCoord::new(7, 8)),
            2,
            "standoff ring at distance 2"
        );
    }

    // 17 ─ Manned-ring rule: when every river-shielded ring hex is already
    //      held by the front line, a backfield unit behind the river HOLDS
    //      instead of crossing to the threat's bank (the failure this
    //      prevents: 4.Inf ordered to (8,6) because (10,6-8) were manned).
    #[test]
    fn reinforcement_holds_when_shield_ring_manned() {
        let mut g = grid(16, 12);
        for r in 0..12 {
            g.set_terrain(HexCoord::new(8, r), Terrain::River);
        }
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 999);

        // Front line already manning the shielded ring of the threat (7,8):
        // (9,7)/(9,8)/(9,6) are its distance-2 hexes east of the river.
        let own = vec![
            unit(1, UnitType::Infantry, Side::Defender, 9, 8),
            unit(2, UnitType::Infantry, Side::Defender, 9, 7),
            unit(3, UnitType::Infantry, Side::Defender, 9, 6),
            unit(4, UnitType::Infantry, Side::Defender, 10, 6), // backfield
        ];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 7, 8),
            unit(11, UnitType::Infantry, Side::Attacker, 7, 9),
        ];

        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, AiAction::MoveUnit { .. })),
            "no one crosses when the shield ring is manned: {actions:?}"
        );
    }

    // 18 ─ Hold-role infantry on an ATTACKING side marches on the
    //      pre-battle intel when no enemy is visible — otherwise the parked
    //      shoulder line seals the vanguard in with friendly-packed hexes
    //      and pathing deadlocks (tanks stuck at the frontier).
    #[test]
    fn blind_attacker_hold_role_marches_on_intel() {
        let g = grid(16, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 9);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 2, 3)];
        let intel = vec![HexCoord::new(12, 3)];
        let actions = ai.plan_turn_zone(&g, &own, &[], Some(&intel));
        assert!(
            matches!(action_for(&actions, 1), AiAction::MoveUnit { .. }),
            "blind attacking infantry must march on the intel: {actions:?}"
        );
    }

    // 19 ─ ...but with the enemy in sight the §7.2 doctrine stands: blitz
    //      shoulder infantry holds (assault-role units carry the fight).
    #[test]
    fn hold_role_infantry_holds_when_enemy_visible() {
        let g = grid(16, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 9);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 2, 3)];
        // Enemy 2 hexes away (visible, not assaultable).
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 2, 5)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { .. }),
            "hold-role infantry holds while the enemy is in sight: {actions:?}"
        );
    }

    // 20 ─ The blind march aims at the 3-8 hex ring inside the intel
    //      zone - the band where the defender's line sits. Edge goals spun
    //      units in place and centroid goals stalled them past the line;
    //      the ring fans the line into the zone.
    #[test]
    fn blind_march_targets_the_intel_ring() {
        let g = grid(16, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 3);
        // Unit WEST of the intel zone (edge-adjacent).
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 6, 3)];
        let intel: Vec<HexCoord> = (8..=13)
            .flat_map(|q| (2..=4).map(move |r| HexCoord::new(q, r)))
            .collect();
        let actions = ai.plan_turn_zone(&g, &own, &[], Some(&intel));
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert!(
                    dest.distance(HexCoord::new(6, 3)) >= 3,
                    "blind goal must reach the 3+ ring, got {dest:?}"
                );
            }
            other => panic!("edge-adjacent unit must advance, got {other:?}"),
        }
    }

    // 21 ─ A unit that strayed INTO the intel zone marches on the zone
    //      CENTROID - a fixed goal, so it never drifts toward the zone's
    //      far corner along a tie-break (the q tie-break marched a whole
    //      vanguard through the empty defender zone into its own
    //      encirclement). Units at the centroid hold in place.
    #[test]
    fn unit_inside_intel_marches_on_the_centroid_not_the_far_corner() {
        let g = grid(16, 8);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 3);
        // Unit at the EAST edge of the intel zone; the centroid is (10,3).
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 13, 3)];
        let intel: Vec<HexCoord> = (8..=13)
            .flat_map(|q| (2..=4).map(move |r| HexCoord::new(q, r)))
            .collect();
        let actions = ai.plan_turn_zone(&g, &own, &[], Some(&intel));
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert!(
                    dest.distance(HexCoord::new(13, 3)) <= 3
                        && dest.distance(HexCoord::new(10, 3)) < 3,
                    "inside the zone: march toward the centroid, got {dest:?}"
                );
            }
            other => panic!("inside the zone must march on the centroid, got {other:?}"),
        }
    }

    // 22 ─ Delay: in contact, fall back EXACTLY one hex and keep the 2-hex
    //      contact band — never break contact the way tactical_withdrawal
    //      does.
    #[test]
    fn delay_falls_back_one_hex_in_contact() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Delay, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 5, 4)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(
            dist_to_nearest(dest, &enemy),
            2,
            "delay keeps the 2-hex contact band, ended at {dest:?}"
        );
        assert_eq!(
            dest.distance(HexCoord::new(5, 5)),
            1,
            "exactly one hex back"
        );
    }

    // 23 ─ Delay: beyond the band the line holds (no pursuit), and an
    //      adjacent enemy never gets assaulted (delay screens — striking is
    //      the counterattack card's job).
    #[test]
    fn delay_holds_beyond_the_band_and_never_strikes() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Delay, 2);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 5, 1)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { .. }),
            "enemy 4 hexes out: the delay screen holds: {actions:?}"
        );
    }

    // 24 ─ Counterattack: the counter-punch opens on a beaten target and
    //      keeps the ground; a fresh target — isolated or supported — is no
    //      window.
    #[test]
    fn counterattack_strikes_only_a_window_target() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Counterattack, 3);
        let mut beaten = unit(10, UnitType::Infantry, Side::Attacker, 5, 4);
        set_org_pct(&mut beaten, 0.05); // low enough to be broken by one strike
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let actions = ai.plan_turn(&g, &own, &[beaten]);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "isolated beaten target is the counter window: {actions:?}"
        );

        // Fresh isolated target: the trade filter refuses it.
        let mut ai2 = TacticalAi::new(Side::Defender, CombatTactic::Counterattack, 3);
        let fresh = unit(11, UnitType::Infantry, Side::Attacker, 5, 4);
        let own2 = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let actions = ai2.plan_turn(&g, &own2, &[fresh]);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "fresh isolated target is no counter window: {actions:?}"
        );

        // Supported fresh target: refused by assault_permitted.
        let mut ai3 = TacticalAi::new(Side::Defender, CombatTactic::Counterattack, 3);
        let own3 = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy3 = vec![
            unit(12, UnitType::Infantry, Side::Attacker, 5, 4),
            unit(13, UnitType::Infantry, Side::Attacker, 4, 4),
        ];
        let actions = ai3.plan_turn(&g, &own3, &enemy3);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "supported fresh line is no counter window: {actions:?}"
        );
    }

    // 25 ─ Ambush: absolutely still at range, strike at point blank.
    #[test]
    fn ambush_never_moves_but_strikes_at_point_blank() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Ambush, 4);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 5, 3)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { .. }),
            "ambusher at range does not shuffle (no cover-shift): {actions:?}"
        );

        let mut ai2 = TacticalAi::new(Side::Defender, CombatTactic::Ambush, 4);
        let own2 = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy2 = vec![unit(10, UnitType::Infantry, Side::Attacker, 5, 4)];
        let actions = ai2.plan_turn(&g, &own2, &enemy2);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "ambusher strikes the enemy that steps adjacent: {actions:?}"
        );
    }

    // 26 ─ River defense: holds the bank against adjacent pressure (elastic
    //      would fall back), and strikes only a half-forded enemy.
    #[test]
    fn river_defense_holds_the_bank_and_hits_half_fords() {
        let mut g = grid(12, 12);
        g.set_terrain(HexCoord::new(5, 4), Terrain::River);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::RiverDefense, 5);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 4, 4),
            unit(11, UnitType::Infantry, Side::Attacker, 6, 4),
        ];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { .. }),
            "river defense holds against adjacent pressure, never falls back: {actions:?}"
        );

        let mut ai2 = TacticalAi::new(Side::Defender, CombatTactic::RiverDefense, 5);
        let own2 = vec![unit(1, UnitType::Infantry, Side::Defender, 5, 5)];
        let enemy2 = vec![unit(12, UnitType::Infantry, Side::Attacker, 5, 4)];
        let actions = ai2.plan_turn(&g, &own2, &enemy2);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "half-forded enemy is struck: {actions:?}"
        );
    }

    // 27 ─ Urban defense: fights enemies that enter the city, and never
    //      leaves it (a unit outside the urban hexes holds in place).
    #[test]
    fn urban_defense_fights_in_the_city_only() {
        let mut g = grid(12, 12);
        for r in 0..12 {
            g.set_terrain(HexCoord::new(6, r), Terrain::Urban);
        }
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::UrbanDefense, 6);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 6, 6)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 6, 5)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "city garrison strikes an enemy entering the city: {actions:?}"
        );

        let mut ai2 = TacticalAi::new(Side::Defender, CombatTactic::UrbanDefense, 6);
        let own2 = vec![unit(2, UnitType::Infantry, Side::Defender, 4, 6)];
        let enemy2 = vec![unit(11, UnitType::Infantry, Side::Attacker, 4, 5)];
        let actions = ai2.plan_turn(&g, &own2, &enemy2);
        assert!(
            matches!(action_for(&actions, 2), AiAction::Hold { .. }),
            "units outside the city hold instead of marching out: {actions:?}"
        );
    }

    // 28 ─ Assault: fire preparation concentrates on the weakest hex while
    //      the infantry line advances in step.
    #[test]
    fn assault_card_prepares_with_artillery_then_advances_infantry() {
        let g = grid(12, 10);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 7);
        let mut weak = unit(10, UnitType::Infantry, Side::Defender, 4, 5);
        set_org_pct(&mut weak, 0.20);
        let strong = unit(11, UnitType::Infantry, Side::Defender, 7, 5);
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 4, 2);
        arty.is_emplaced = true;
        arty.attack_range = 8;
        let inf = unit(2, UnitType::Infantry, Side::Attacker, 4, 3);
        let own = vec![arty, inf];
        let enemy = vec![weak, strong];

        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(*target_hex, HexCoord::new(4, 5), "prep on the weakest hex");
            }
            other => panic!("battery must fire preparation, got {other:?}"),
        }
        assert!(
            matches!(action_for(&actions, 2), AiAction::MoveUnit { .. }),
            "assault infantry advances (blitz shoulders would hold): {actions:?}"
        );
    }

    // 29 ─ River assault: the crossing infantry advances toward the far
    //      bank (blitz infantry of the same type would hold).
    #[test]
    fn river_assault_forces_the_crossing_with_infantry() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::RiverAssault, 8);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 3, 5)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 8, 5)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::MoveUnit { .. }),
            "river-assault infantry advances to the crossing: {actions:?}"
        );
    }

    // 30 ─ §6.13: the HQ never takes a combat action; it shadows
    //      its division and breaks contact when the enemy reaches it.
    fn hq(id: usize, side: Side, q: i32, r: i32, division: &str) -> BattalionUnit {
        let mut u = BattalionUnit::new(id, "HQ", UnitType::Headquarters, side, HexCoord::new(q, r));
        u.division = division.to_string();
        u
    }

    #[test]
    fn hq_shadows_division_and_never_attacks() {
        let g = grid(14, 14);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 1);
        let mut inf = unit(1, UnitType::Infantry, Side::Defender, 5, 5);
        inf.division = "D".to_string();
        let own = vec![inf, hq(2, Side::Defender, 1, 1, "D")];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 12, 12)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        // Off the leash (8 hexes from the division anchor) → moves toward it.
        match action_for(&actions, 2) {
            AiAction::MoveUnit { path, .. } => {
                let dest = path.last().unwrap();
                assert!(
                    dest.distance(HexCoord::new(5, 5)) < 8,
                    "HQ closes on the division: {dest:?}"
                );
            }
            other => panic!("HQ must never attack, got {other:?}"),
        }
    }

    #[test]
    fn hq_on_leash_holds_position() {
        let g = grid(14, 14);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 1);
        let mut inf = unit(1, UnitType::Infantry, Side::Defender, 5, 5);
        inf.division = "D".to_string();
        // HQ two hexes off the anchor — inside the 3-hex aura.
        let own = vec![inf, hq(2, Side::Defender, 5, 7, "D")];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 12, 12)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 2), AiAction::Hold { .. }),
            "HQ on the leash holds: {actions:?}"
        );
    }

    #[test]
    fn hq_breaks_contact_before_holding_the_leash() {
        let g = grid(14, 14);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 1);
        let mut inf = unit(1, UnitType::Infantry, Side::Defender, 5, 5);
        inf.division = "D".to_string();
        // HQ in contact with the enemy — survival outranks coverage (§6.13).
        let own = vec![inf, hq(2, Side::Defender, 2, 1, "D")];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 2, 2)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 2) {
            AiAction::MoveUnit { path, .. } => {
                let dest = path.last().unwrap();
                assert!(
                    dest.distance(HexCoord::new(2, 2)) > 1,
                    "HQ disengages from contact: {dest:?}"
                );
            }
            other => panic!("HQ in contact must disengage, got {other:?}"),
        }
    }

    #[test]
    fn hq_follow_targets_a_free_hex_not_the_occupied_anchor() {
        // Regression: the follow target used to be the anchor hex itself —
        // always occupied by the anchor member, so a static division left
        // the HQ blocked-waiting on a standing move order all battle.
        let g = grid(14, 14);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 1);
        let mut inf = unit(1, UnitType::Infantry, Side::Defender, 5, 5);
        inf.division = "D".to_string();
        let own = vec![inf, hq(2, Side::Defender, 1, 1, "D")];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 12, 12)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 2) {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert_ne!(dest, HexCoord::new(5, 5), "never the occupied anchor");
                assert!(
                    dest.distance(HexCoord::new(5, 5)) < 3,
                    "still inside the aura: {dest:?}"
                );
            }
            other => panic!("off-leash HQ must follow, got {other:?}"),
        }
    }

    #[test]
    fn hq_reaffirms_a_still_valid_standing_destination() {
        // Hysteresis: while the standing destination stays free and on the
        // leash, the same hex is proposed again — the executive's
        // same-destination rule then keeps the invested movement hours, so
        // centroid creep cannot pin a foot-speed HQ.
        let g = grid(14, 14);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Default, 1);
        let mut inf = unit(1, UnitType::Infantry, Side::Defender, 5, 5);
        inf.division = "D".to_string();
        let mut hq = hq(2, Side::Defender, 2, 2, "D");
        hq.move_order = Some(tactical_core::MoveOrder {
            path: vec![HexCoord::new(3, 3), HexCoord::new(4, 4)],
            hours: 0.1,
        });
        let own = vec![inf, hq];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 12, 12)];
        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 2) {
            AiAction::MoveUnit { path, .. } => {
                assert_eq!(
                    path.last(),
                    Some(&HexCoord::new(4, 4)),
                    "standing destination is re-affirmed: {path:?}"
                );
            }
            other => panic!("HQ with a valid standing order keeps following: {other:?}"),
        }
    }

    // 31 ─ §6.8: a RETREATING enemy is free damage — it never counters
    //      (return fire is structurally zero), so the leaver is assaulted
    //      past the trade/odds gates and ranked like an org-0 remnant, and
    //      stays visible to fire missions until it exits the map.
    #[test]
    fn retreating_enemy_is_assaulted_past_the_odds_gate() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        // The leaver, flanked by two FRESH friends — a standing target with
        // this escort would trip the 3:1 local-odds refusal; the leaver
        // never counters, so the gate does not apply to it.
        let mut leaver = unit(10, UnitType::Infantry, Side::Defender, 5, 6);
        leaver.state = UnitState::Retreating;
        let enemy = vec![
            leaver,
            unit(11, UnitType::Infantry, Side::Defender, 4, 6),
            unit(12, UnitType::Infantry, Side::Defender, 6, 6),
        ];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(
                action_for(&actions, 1),
                AiAction::Assault { target_id: 10, .. }
            ),
            "the retreating enemy is assaulted despite the escort: {actions:?}"
        );
    }

    #[test]
    fn retreating_enemy_outranks_a_same_pool_standing_target() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        // Same joinable pool (both adjacent): a beaten standing unit vs a
        // full-org leaver — the free-damage leaver wins the key.
        let mut leaver = unit(10, UnitType::Infantry, Side::Defender, 5, 6);
        leaver.state = UnitState::Retreating;
        let mut beaten = unit(11, UnitType::Infantry, Side::Defender, 4, 5);
        beaten.org = 10.0;
        let enemy = vec![leaver, beaten];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(
                action_for(&actions, 1),
                AiAction::Assault { target_id: 10, .. }
            ),
            "the leaver ranks as free damage: {actions:?}"
        );
    }

    #[test]
    fn retreating_enemy_stays_visible_to_fire_missions() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 1);
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 2, 2);
        arty.is_emplaced = true;
        let own = vec![arty];
        // The ONLY enemy in range is retreating — the battery still fires.
        let mut leaver = unit(10, UnitType::Infantry, Side::Defender, 5, 2);
        leaver.state = UnitState::Retreating;
        let enemy = vec![leaver];
        let actions = ai.plan_turn(&g, &own, &enemy);
        match action_for(&actions, 1) {
            AiAction::FireSupport { target_hex, .. } => {
                assert_eq!(*target_hex, HexCoord::new(5, 2));
            }
            other => panic!("battery must shell the leaver, got {other:?}"),
        }
    }

    // 32 ─ §6.5 contact-ring discipline: a unit adjacent to a visible enemy
    //      strikes when the gates allow and HOLDS when they refuse — moving
    //      instead just burns the order at the first step, turn after turn.
    #[test]
    fn probe_strikes_a_weak_contact_instead_of_ramming() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 1);
        // Recon = Probe role (pre-fix: never tried the assault at all).
        let own = vec![unit(1, UnitType::Recon, Side::Attacker, 5, 5)];
        // A soft target — the trade math approves.
        let mut soft = unit(10, UnitType::Recon, Side::Defender, 5, 6);
        soft.defense = 1.0;
        let enemy = vec![soft];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(
                action_for(&actions, 1),
                AiAction::Assault { target_id: 10, .. }
            ),
            "a winning contact is struck, not rammed: {actions:?}"
        );
    }

    #[test]
    fn probe_holds_a_refused_contact_instead_of_ramming() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 1);
        let own = vec![unit(1, UnitType::Recon, Side::Attacker, 5, 5)];
        // An unbreakable wall — the trade math refuses; the prober must
        // hunker in contact, not shuffle against the ring.
        let mut wall = unit(10, UnitType::MediumArmor, Side::Defender, 5, 6);
        wall.defense = 1000.0;
        let enemy = vec![wall];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { unit_id: 1 }),
            "refused contact = hold, not another rammed approach: {actions:?}"
        );
    }

    #[test]
    fn assault_role_holds_a_refused_contact() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        // Line infantry under the Assault card, face-hugging an
        // unbreakable target: the trade math refuses, and the
        // default-advance arm must not re-ram the ring either.
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 5, 5)];
        let mut wall = unit(10, UnitType::MediumArmor, Side::Defender, 5, 6);
        wall.defense = 1000.0;
        let enemy = vec![wall];
        let actions = ai.plan_turn(&g, &own, &enemy);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { unit_id: 1 }),
            "refused contact = hold the line: {actions:?}"
        );
    }

    // ── §7.3 / §6.11 flag anchoring & defense tiers ────────────────────────

    fn flag_state(progress: i32, anchor: HexCoord, zone: &[HexCoord]) -> tactical_core::FlagState {
        let mut f = tactical_core::FlagZone::new(anchor, zone.to_vec());
        f.progress = progress;
        tactical_core::FlagState {
            kind: tactical_core::FlagKind::Field,
            flags: vec![f],
            collapsed: false,
        }
    }

    #[test]
    fn attacker_blind_march_aims_at_the_flags() {
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 1);
        // The city (flag zone) sits EAST; the pre-battle intel zone lies
        // WEST — the flags must win the blind march.
        let zone: Vec<HexCoord> = (18..22).map(|r| HexCoord::new(20, r)).collect();
        let flags = flag_state(0, HexCoord::new(20, 20), &zone);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 4, 4)];
        let enemy: Vec<BattalionUnit> = Vec::new();
        let intel: Vec<HexCoord> = (0..2).map(|r| HexCoord::new(0, r)).collect();
        let actions = ai.plan_turn_flags(&g, &own, &enemy, Some(&intel), Some(&flags), None);
        let dest = move_dest(action_for(&actions, 1));
        assert!(
            zone.iter().any(|z| z.distance(dest) <= 1),
            "blind march must aim at the flag zone, got {dest:?}"
        );
    }

    #[test]
    fn hold_role_infantry_occupies_a_cleared_flag_zone_despite_visible_pocket() {
        // §6.11 occupation bypass: the garrison is gone from the flag zone
        // but a remnant pocket is still visible on the far side of the map —
        // the global "no effective enemy visible" gate must not park a
        // NEARBY hold-role battalion outside an empty city (Alamein trace:
        // a dozen battalions ringed an empty flag zone for 100+ turns).
        let g = grid(30, 30);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 1);
        let zone: Vec<HexCoord> = vec![HexCoord::new(12, 10), HexCoord::new(13, 10)];
        let flags = flag_state(0, HexCoord::new(13, 10), &zone);
        // Blitz line infantry = HoldPosition role (holds the shoulders).
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 10, 10)];
        let enemy = vec![unit(9, UnitType::Infantry, Side::Defender, 25, 25)];
        let intel: Vec<HexCoord> = vec![HexCoord::new(25, 25)];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, Some(&intel), Some(&flags), None);
        let dest = move_dest(action_for(&actions, 1));
        assert!(
            zone.iter().any(|z| z.distance(dest) <= 1),
            "cleared flag zone nearby = occupy it, got {dest:?}"
        );
    }

    #[test]
    fn hold_role_infantry_holds_while_the_flag_zone_is_contested() {
        // Same geometry, but a combat-effective defender stands INSIDE the
        // zone — the occupation bypass must stay shut (and the enemy is
        // visible, so the global gate is shut too): the line holds.
        let g = grid(30, 30);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 1);
        let zone: Vec<HexCoord> = vec![HexCoord::new(12, 10), HexCoord::new(13, 10)];
        let flags = flag_state(0, HexCoord::new(13, 10), &zone);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 10, 10)];
        // In-zone but NOT adjacent (distance 3) — no assault opportunity.
        let enemy = vec![unit(9, UnitType::Infantry, Side::Defender, 13, 10)];
        let intel: Vec<HexCoord> = vec![HexCoord::new(13, 10)];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, Some(&intel), Some(&flags), None);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { unit_id: 1 }),
            "contested zone + visible enemy = hold: {actions:?}"
        );
    }

    #[test]
    fn hold_role_infantry_holds_when_the_cleared_flag_is_beyond_reach() {
        // A cleared flag zone on the FAR side of the map must not pull the
        // shoulder line off the front — only units within FLAG_OCCUPY_REACH
        // of the zone walk in (the Warsaw line-surge failure mode).
        let g = grid(30, 30);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Blitz, 1);
        let zone: Vec<HexCoord> = vec![HexCoord::new(25, 25)];
        let flags = flag_state(0, HexCoord::new(25, 25), &zone);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 10, 10)];
        // A visible enemy at distance 2 (not adjacent — no assault) keeps
        // the global gate shut; the flag is 15+ hexes away, beyond reach.
        let enemy = vec![unit(9, UnitType::Infantry, Side::Defender, 12, 10)];
        let intel: Vec<HexCoord> = vec![HexCoord::new(25, 25)];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, Some(&intel), Some(&flags), None);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { unit_id: 1 }),
            "cleared but distant flag = keep holding: {actions:?}"
        );
    }

    // ── The physical-enemy standoff cut ────────────────────────────────────

    #[test]
    fn blind_battery_stops_at_physical_envelope_edge() {
        // A towed battery blind-marching on the city sees NOTHING (the
        // garrison is behind fog+urban LOS), so a standoff cut keyed on
        // the fog-filtered `enemy` list never fires — the guns walk onto
        // the garrison's own hex, trip the d < 2 escape rule, and
        // oscillate between the city edge and the deep rear forever. The
        // cut must read the PHYSICAL foe list and stop at the melee
        // standoff (2), never stepping on the enemy; the blind-goal
        // emplace branch then parks the guns once the city enters the
        // envelope (the march may cross the <9 ring — that is the point;
        // an envelope-edge cut freezes the advance behind every physical
        // foe at once).
        let g = grid(30, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Default, 5);
        let mut arty = unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 4, 4);
        arty.attack_range = 9;
        let own = vec![arty];
        let city = HexCoord::new(20, 4);
        let garrison = unit(10, UnitType::Infantry, Side::Defender, city.q, city.r);
        let flags = flag_state(0, city, &[city]);
        let actions = ai.plan_turn_full(
            &g,
            &own,
            &[], // fog shows nothing
            Some(&[city]),
            Some(&flags),
            None,
            Some(&[garrison]), // physical foe list
        );
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                assert!(
                    path.iter().all(|h| h.distance(city) >= 2),
                    "the march must never step onto the enemy (standoff 2), got path {path:?}"
                );
                assert!(
                    path.last().map(|h| h.distance(city) <= 9).unwrap_or(false),
                    "and must close to within the envelope for the emplace branch: {path:?}"
                );
            }
            other => panic!("battery should march, got {other:?}"),
        }
    }

    #[test]
    fn defender_holds_inside_threatened_flag_zone() {
        let g = grid(24, 24);
        // Elastic defense, in contact INSIDE a zone at 1/3+ progress: the
        // fall-back that cedes the zone is refused (the enemy has friends,
        // so no assault window opens either).
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 1);
        let zone = vec![HexCoord::new(10, 10), HexCoord::new(11, 10)];
        let flags = flag_state(4, HexCoord::new(10, 10), &zone);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 10, 10)];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Attacker, 11, 10),
            unit(11, UnitType::Infantry, Side::Attacker, 12, 10),
        ];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, Some(&flags), None);
        assert!(
            matches!(action_for(&actions, 1), AiAction::Hold { .. }),
            "in-zone defender at 1/3+ progress must hold, got {:?}",
            action_for(&actions, 1)
        );
        // Below 1/3 the doctrine stands — the same spot falls back.
        let mut ai2 = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 1);
        let flags2 = flag_state(1, HexCoord::new(10, 10), &zone);
        let actions2 = ai2.plan_turn_flags(&g, &own, &enemy, None, Some(&flags2), None);
        match action_for(&actions2, 1) {
            AiAction::MoveUnit { path, .. } => {
                assert!(
                    path.last().unwrap().distance(HexCoord::new(11, 10)) > 1,
                    "below 1/3 the elastic line still falls back"
                );
            }
            other => panic!("below 1/3 the doctrine stands, got {other:?}"),
        }
    }

    #[test]
    fn defender_tier_counterattack_marches_into_the_zone() {
        let g = grid(24, 24);
        // > 2/3 progress: the nearest out-of-contact line battalion must
        // march INTO the flag zone (press the control ratio).
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::Counterattack, 1);
        let zone: Vec<HexCoord> = (9..11)
            .flat_map(|q| (9..11).map(move |r| HexCoord::new(q, r)))
            .collect();
        let flags = flag_state(9, HexCoord::new(10, 10), &zone);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 14, 14)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 20, 20)];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, Some(&flags), None);
        let dest = move_dest(action_for(&actions, 1));
        assert!(
            zone.iter().any(|z| *z == dest) || dest.distance(HexCoord::new(10, 10)) <= 2,
            "counterattack must head INTO the flag zone, got {dest:?}"
        );
    }

    #[test]
    fn defender_tier_screen_forms_around_the_anchor() {
        let g = grid(24, 24);
        // 1/3–2/3 progress: reroute to the screen ring (standoff 2 from the
        // anchor) — the reinforcement tier.
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 1);
        let zone: Vec<HexCoord> = (9..11)
            .flat_map(|q| (9..11).map(move |r| HexCoord::new(q, r)))
            .collect();
        let flags = flag_state(6, HexCoord::new(10, 10), &zone);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 14, 14)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 20, 20)];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, Some(&flags), None);
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(
            dest.distance(HexCoord::new(10, 10)),
            2,
            "reinforcement screens the anchor at standoff 2, got {dest:?}"
        );
    }

    #[test]
    fn defender_units_inside_a_zone_are_never_rerouted_away() {
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 1);
        let zone = vec![HexCoord::new(10, 10), HexCoord::new(11, 10)];
        let flags = flag_state(12, HexCoord::new(10, 10), &zone);
        // Unit INSIDE the zone (progress full) + a unit far behind it: the
        // in-zone unit holds; the rear unit counterattacks into the zone.
        let own = vec![
            unit(1, UnitType::Infantry, Side::Defender, 10, 10),
            unit(2, UnitType::Infantry, Side::Defender, 15, 15),
        ];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 20, 20)];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, Some(&flags), None);
        assert!(matches!(action_for(&actions, 1), AiAction::Hold { .. }));
        let dest = move_dest(action_for(&actions, 2));
        assert!(
            dest.distance(HexCoord::new(10, 10)) <= 2,
            "rear unit counterattacks the zone, got {dest:?}"
        );
    }

    #[test]
    fn flags_are_ignored_below_one_third_progress() {
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Defender, CombatTactic::ElasticDefense, 1);
        let zone = vec![HexCoord::new(10, 10), HexCoord::new(11, 10)];
        let flags = flag_state(1, HexCoord::new(10, 10), &zone);
        let own = vec![unit(1, UnitType::Infantry, Side::Defender, 14, 14)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Attacker, 20, 20)];
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, Some(&flags), None);
        // No visible threat within 3 hexes and < 1/3 progress: no reroute —
        // elastic simply holds.
        assert!(matches!(action_for(&actions, 1), AiAction::Hold { .. }));
    }

    // ── Division orders ─────────────────────────────────────────────────────

    #[test]
    fn seize_order_marches_on_the_hex_not_the_intel() {
        // A commanded division's blind march aims at the seize hex, not the
        // pre-battle intel zone — the order outranks the doctrine.
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 4, 4)];
        let enemy: Vec<BattalionUnit> = Vec::new();
        let intel = vec![HexCoord::new(20, 20)];
        let seize = HexCoord::new(12, 8);

        let with_order = ai.plan_turn_div_order(
            &g,
            &own,
            &enemy,
            Some(&intel),
            None,
            Some(DivOrderTarget::Seize { hex: seize }),
        );
        assert_eq!(move_dest(action_for(&with_order, 1)), seize);

        // Sanity: without the order the same setup marches on the intel.
        let without = ai.plan_turn_div_order(&g, &own, &enemy, Some(&intel), None, None);
        assert_eq!(move_dest(action_for(&without, 1)), HexCoord::new(20, 20));
    }

    /// A unit with a standing move order into the objective area keeps its
    /// destination across turns — reservation churn from other units'
    /// proposals must not reassign it (flapping destinations reset
    /// invested march hours in the re-affirm path, pinning slow units).
    #[test]
    fn seize_goal_keeps_standing_destination_across_turns() {
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let seize = HexCoord::new(12, 8);
        let kept = HexCoord::new(12, 9); // a legal ring-1 hex of the objective
        let mut u1 = unit(1, UnitType::Infantry, Side::Attacker, 4, 4);
        u1.move_order = Some(tactical_core::unit::MoveOrder {
            path: vec![HexCoord::new(5, 4), kept],
            hours: 0.15,
        });
        let own = vec![u1];
        let enemy: Vec<BattalionUnit> = Vec::new();
        let intel = vec![HexCoord::new(20, 20)];
        let actions = ai.plan_turn_div_order(
            &g,
            &own,
            &enemy,
            Some(&intel),
            None,
            Some(DivOrderTarget::Seize { hex: seize }),
        );
        // A fresh spread would take the centre itself (3-hex preference);
        // hysteresis keeps the standing ring-1 destination instead.
        assert_eq!(move_dest(action_for(&actions, 1)), kept);
    }

    /// The keep rule lapses when the standing destination becomes illegal
    /// (a friend holds it now) — the unit re-spreads to a free hex of the
    /// objective area.
    #[test]
    fn seize_goal_respreads_when_standing_destination_occupied() {
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let seize = HexCoord::new(12, 8);
        let mut u1 = unit(1, UnitType::Infantry, Side::Attacker, 4, 4);
        u1.move_order = Some(tactical_core::unit::MoveOrder {
            path: vec![HexCoord::new(5, 4), HexCoord::new(12, 9)],
            hours: 0.15,
        });
        // A friend already holds the standing destination.
        let own = vec![u1, unit(2, UnitType::Infantry, Side::Attacker, 12, 9)];
        let enemy: Vec<BattalionUnit> = Vec::new();
        let actions = ai.plan_turn_div_order(
            &g,
            &own,
            &enemy,
            None,
            None,
            Some(DivOrderTarget::Seize { hex: seize }),
        );
        let dest = move_dest(action_for(&actions, 1));
        assert_ne!(dest, HexCoord::new(12, 9), "occupied keep-lapsed: {dest:?}");
        assert!(
            dest.distance(seize) <= 2,
            "re-spread stays in the objective area: {dest:?}"
        );
    }

    /// A lone attacker refuses an even trade (their defense vs our
    /// breakthrough), but the SAME matchup opens once a friend stands
    /// adjacent — the pooled volley squares the numbers (P = Σ(q·g)×Σg),
    /// and both friends converge on the shared victim.
    #[test]
    fn even_match_assault_opens_when_pooled() {
        let g = grid(24, 24);
        let enemy = vec![unit(9, UnitType::Infantry, Side::Defender, 10, 10)];
        // Lone attacker: the solo trade estimate fails.
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let solo_own = vec![unit(1, UnitType::Infantry, Side::Attacker, 10, 11)];
        let actions = ai.plan_turn_div_order(&g, &solo_own, &enemy, None, None, None);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "lone attacker refuses the even trade"
        );
        // A friend adjacent to the same target: the pooled estimate passes
        // and both pile onto the shared victim.
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![
            unit(1, UnitType::Infantry, Side::Attacker, 10, 11),
            unit(2, UnitType::Infantry, Side::Attacker, 11, 10),
        ];
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, None);
        for id in [1, 2] {
            match action_for(&actions, id) {
                AiAction::Assault { target_id, .. } => assert_eq!(*target_id, 9),
                other => panic!("unit {id} should join the pooled assault, got {other:?}"),
            }
        }
    }

    /// Between equally juicy victims the JOINABLE POOL breaks the tie — the
    /// attacker picks the target its friend can also reach, so the volley
    /// forms instead of fragmenting on the RNG tie-break.
    #[test]
    fn assault_prefers_the_joinable_victim() {
        let g = grid(24, 24);
        // Both defenders beaten to break-outright range, so either alone
        // passes the trade gate; only defA is reachable by both attackers.
        let mut d_a = unit(9, UnitType::Infantry, Side::Defender, 10, 10);
        d_a.org = 5.0;
        let mut d_b = unit(10, UnitType::Infantry, Side::Defender, 12, 10);
        d_b.org = 5.0;
        let enemy = vec![d_a, d_b];
        let own = vec![
            unit(1, UnitType::Infantry, Side::Attacker, 11, 10),
            unit(2, UnitType::Infantry, Side::Attacker, 10, 11),
        ];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, None);
        match action_for(&actions, 1) {
            AiAction::Assault { target_id, .. } => {
                assert_eq!(*target_id, 9, "the joinable victim wins the tie")
            }
            other => panic!("unit 1 should assault, got {other:?}"),
        }
    }

    #[test]
    fn engage_order_pursues_the_target_ignoring_nearer_enemies() {
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 4, 4)];
        // Target at (13,4); a nearer enemy at (7,4) would win without order.
        let target = unit(10, UnitType::Infantry, Side::Defender, 13, 4);
        let enemy = vec![
            unit(11, UnitType::Infantry, Side::Defender, 7, 4),
            target.clone(),
        ];
        let order = DivOrderTarget::Engage {
            unit: target.id,
            last_pos: target.position,
        };
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, Some(order));
        // The pursuit closes on the target's AREA — the target hex itself
        // is occupied by the enemy, so the unit aims at the nearest free
        // ring hex (12,4), stopping adjacent for the assault.
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(
            dest,
            HexCoord::new(12, 4),
            "pursuit closes on the target: {dest:?}"
        );
        assert_ne!(
            dest,
            HexCoord::new(7, 4),
            "never traded for the nearer decoy"
        );

        let without = ai.plan_turn_div_order(&g, &own, &enemy, None, None, None);
        assert_eq!(move_dest(action_for(&without, 1)), HexCoord::new(7, 4));
    }

    #[test]
    fn engage_order_assaults_the_target_itself() {
        // Two adjacent enemies; the engage target is the pick regardless of
        // the weakness ranking / RNG tie-breaks.
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let mut mk_own = |id: usize, q: i32, r: i32| {
            let mut u = unit(id, UnitType::Infantry, Side::Attacker, q, r);
            u.breakthrough = 40.0; // even trade math, no hopeless-trade refusal
            u
        };
        let own = vec![mk_own(1, 4, 4), mk_own(2, 5, 4), mk_own(3, 4, 5)];
        let target = unit(10, UnitType::Infantry, Side::Defender, 3, 4);
        let enemy = vec![
            unit(11, UnitType::Infantry, Side::Defender, 3, 5),
            target.clone(),
        ];
        let order = DivOrderTarget::Engage {
            unit: target.id,
            last_pos: target.position,
        };
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, Some(order));
        match action_for(&actions, 1) {
            AiAction::Assault { target_id, .. } => assert_eq!(*target_id, target.id),
            other => panic!("expected the target assault, got {other:?}"),
        }
    }

    #[test]
    fn seize_order_prefers_the_points_own_defenders() {
        // Two adjacent enemies, one standing ON the seize hex — the assault
        // picks the point's defender, not the weaker neighbour.
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let mut mk_own = |id: usize, q: i32, r: i32| {
            let mut u = unit(id, UnitType::Infantry, Side::Attacker, q, r);
            u.breakthrough = 40.0; // even trade math, no hopeless-trade refusal
            u
        };
        let own = vec![mk_own(1, 4, 4), mk_own(2, 5, 4), mk_own(3, 4, 5)];
        let on_hex = unit(10, UnitType::Infantry, Side::Defender, 3, 4);
        let enemy = vec![
            on_hex.clone(),
            unit(11, UnitType::Infantry, Side::Defender, 3, 5),
        ];
        let actions = ai.plan_turn_div_order(
            &g,
            &own,
            &enemy,
            None,
            None,
            Some(DivOrderTarget::Seize {
                hex: on_hex.position,
            }),
        );
        match action_for(&actions, 1) {
            AiAction::Assault { target_id, .. } => assert_eq!(*target_id, on_hex.id),
            other => panic!("expected the point's defender, got {other:?}"),
        }
    }

    #[test]
    fn division_plan_skips_manually_overridden_units_and_has_no_end_turn() {
        let g = grid(12, 12);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let mut micromanaged = unit(1, UnitType::Infantry, Side::Attacker, 4, 4);
        micromanaged.manual_override = true; // player personally commands it
        let own = vec![
            micromanaged,
            unit(2, UnitType::Infantry, Side::Attacker, 5, 4),
        ];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 8, 8)];
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, None);
        let planned_for_1 = |a: &AiAction| -> bool {
            match a {
                AiAction::MoveUnit { unit_id, .. } => *unit_id == 1,
                AiAction::Assault { attacker_id, .. } => *attacker_id == 1,
                AiAction::FireSupport { attacker_id, .. } => *attacker_id == 1,
                AiAction::Hold { unit_id } => *unit_id == 1,
                AiAction::Emplace { unit_id } => *unit_id == 1,
                AiAction::Limber { unit_id } => *unit_id == 1,
                AiAction::Retreat { unit_id } => *unit_id == 1,
                AiAction::EndTurn => false,
            }
        };
        assert!(
            !actions.iter().any(planned_for_1),
            "manual override must shield the unit: {actions:?}"
        );
        assert!(
            !matches!(actions.last(), Some(AiAction::EndTurn)),
            "division plans must not carry the EndTurn marker: {actions:?}"
        );
        // The untouched unit still gets planned.
        let _ = action_for(&actions, 2);
    }

    // ── §7.4 division sensor radius (R=10) ─────────────────────────────────

    #[test]
    fn advance_ignores_enemies_outside_sensor_radius() {
        // A far enemy (12 hexes, outside the R=10 sensor) must not pull the
        // division off its own advance — it marches on the intel instead.
        let g = grid(30, 30);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 4, 4)];
        // Enemy 12 hexes away: visible (fog-wise) but out of the division's
        // sensor radius.
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 4, 16)];
        let intel = vec![HexCoord::new(20, 20)];
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, Some(&intel), None, None);
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(
            dest,
            HexCoord::new(20, 20),
            "advance marches on the intel, not the far enemy: {dest:?}"
        );
    }

    #[test]
    fn advance_engages_enemies_inside_sensor_radius() {
        // An enemy within the sensor radius IS the division's own front —
        // the advance closes on it.
        let g = grid(30, 30);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 4, 4)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 8, 8)]; // distance 8 ≤ 10
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, None);
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(
            dest,
            HexCoord::new(8, 8),
            "in-radius enemy is the goal: {dest:?}"
        );
    }

    #[test]
    fn sensor_filter_keeps_far_enemies_as_path_obstacles() {
        // A far enemy is not a target, but its hex still blocks routing —
        // the division walks AROUND it (occupancy, not targeting).
        let g = grid(30, 30);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 4, 4)];
        // 11 hexes away, sitting on the straight line to the intel.
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 15, 4)];
        let intel = vec![HexCoord::new(20, 4)];
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, Some(&intel), None, None);
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                assert_eq!(
                    *path.last().unwrap(),
                    HexCoord::new(20, 4),
                    "destination still reachable: {path:?}"
                );
                assert!(
                    !path.contains(&HexCoord::new(15, 4)),
                    "the far enemy's hex still blocks the route: {path:?}"
                );
            }
            other => panic!("expected a march around the far enemy, got {other:?}"),
        }
    }

    #[test]
    fn seize_order_ignores_far_enemies_outside_sensor_radius() {
        // The sensor filter applies to ALL orders — a Seize still rushes
        // the point even with a far enemy in view.
        let g = grid(30, 30);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 4, 4)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 4, 16)];
        let seize = HexCoord::new(12, 8);
        let actions = ai.plan_turn_div_order(
            &g,
            &own,
            &enemy,
            None,
            None,
            Some(DivOrderTarget::Seize { hex: seize }),
        );
        let dest = move_dest(action_for(&actions, 1));
        assert_eq!(dest, seize, "seize rushes the point: {dest:?}");
    }

    #[test]
    fn seize_order_spreads_the_division_around_the_point() {
        // A whole division converging on ONE hex queues into a single
        // corridor and piles up on one corner of the objective. The order
        // spreads the movement goals across the objective area (the point +
        // its rings): distinct destinations, all within ring 2 — and at
        // least one unit takes the point itself (somebody must occupy the
        // hex to declare it seized).
        let g = grid(24, 24);
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let own = vec![
            unit(1, UnitType::Infantry, Side::Attacker, 4, 4),
            unit(2, UnitType::Infantry, Side::Attacker, 4, 8),
            unit(3, UnitType::Infantry, Side::Attacker, 8, 4),
        ];
        let seize = HexCoord::new(12, 12);
        let actions = ai.plan_turn_div_order(
            &g,
            &own,
            &[],
            None,
            None,
            Some(DivOrderTarget::Seize { hex: seize }),
        );
        let dests: Vec<HexCoord> = [1, 2, 3]
            .iter()
            .map(|id| move_dest(action_for(&actions, *id)))
            .collect();
        let distinct: std::collections::HashSet<HexCoord> = dests.iter().copied().collect();
        assert!(
            distinct.len() >= 2,
            "goals must spread, not funnel: {dests:?}"
        );
        for d in &dests {
            assert!(
                d.distance(seize) <= 2,
                "all goals stay in the objective area: {dests:?}"
            );
        }
        assert!(
            dests.contains(&seize),
            "someone must march onto the point itself: {dests:?}"
        );
    }

    #[test]
    fn seized_hold_back_emplaces_the_guns_instead_of_creeping() {
        // The SEIZED hold-back phase (elastic-defense card) turns the
        // division into a fire base — a towed gun out of the envelope
        // emplaces in place instead of marching after the point. The
        // maneuver phase (assault card) still creeps.
        let g = grid(24, 24);
        let own = vec![unit(1, UnitType::ArtilleryBrigade, Side::Attacker, 4, 4)];
        // 10 hexes away: inside the R=10 sensor (a target) but outside the
        // 9-hex envelope — the creep/emplace decision is exercised.
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 4, 14)];
        let seize = HexCoord::new(12, 12);
        let order = DivOrderTarget::Seize { hex: seize };
        let mut holdback = TacticalAi::new(Side::Attacker, CombatTactic::ElasticDefense, 1);
        let actions = holdback.plan_turn_div_order(&g, &own, &enemy, None, None, Some(order));
        match action_for(&actions, 1) {
            AiAction::Emplace { .. } => {}
            other => panic!("hold-back guns emplace as a fire base, got {other:?}"),
        }
        let mut maneuver = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = maneuver.plan_turn_div_order(&g, &own, &enemy, None, None, Some(order));
        assert!(
            matches!(action_for(&actions, 1), AiAction::MoveUnit { .. }),
            "maneuver guns still creep: {:?}",
            action_for(&actions, 1)
        );
    }

    #[test]
    fn division_order_fights_through_an_adjacent_blocker() {
        // A commanded battalion 贴脸 to an enemy must ASSAULT it even when
        // the trade math refuses — a refused assault leaves it pinned in
        // contact: every march step re-triggers the §6.5 contact stop and
        // the enemy's hex blocks the route, so it neither fights nor
        // advances. Without the order the doctrine refusal stands.
        let g = grid(16, 16);
        let mut attacker = unit(1, UnitType::Infantry, Side::Attacker, 4, 4);
        attacker.soft_attack = 5.0; // weak — a hopeless trade by the estimate
        let mut blocker = unit(10, UnitType::MediumArmor, Side::Defender, 5, 4);
        blocker.defense = 200.0; // not breakable in one strike
        let seize = HexCoord::new(10, 8);
        let with_order = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1)
            .plan_turn_div_order(
                &g,
                &[attacker.clone()],
                &[blocker.clone()],
                None,
                None,
                Some(DivOrderTarget::Seize { hex: seize }),
            );
        match action_for(&with_order, 1) {
            AiAction::Assault { target_id, .. } => assert_eq!(*target_id, blocker.id),
            other => panic!("commanded 贴脸 must assault the blocker, got {other:?}"),
        }
        let without_order = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1)
            .plan_turn_div_order(
                &g,
                &[attacker.clone()],
                &[blocker.clone()],
                None,
                None,
                None,
            );
        assert!(
            !matches!(action_for(&without_order, 1), AiAction::Assault { .. }),
            "doctrine refusal must stand without an order: {:?}",
            action_for(&without_order, 1)
        );
    }

    #[test]
    fn engage_order_strikes_the_quarry_despite_hopeless_odds() {
        // 歼敌: the quarry itself is assaulted when adjacent even when the
        // trade math refuses — a pursuit that cannot strike is no pursuit.
        let g = grid(16, 16);
        let mut attacker = unit(1, UnitType::Infantry, Side::Attacker, 4, 4);
        attacker.soft_attack = 5.0;
        let mut quarry = unit(10, UnitType::MediumArmor, Side::Defender, 5, 4);
        quarry.defense = 200.0;
        let order = DivOrderTarget::Engage {
            unit: quarry.id,
            last_pos: quarry.position,
        };
        let actions = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1)
            .plan_turn_div_order(&g, &[attacker], &[quarry.clone()], None, None, Some(order));
        match action_for(&actions, 1) {
            AiAction::Assault { target_id, .. } => assert_eq!(*target_id, quarry.id),
            other => panic!("engage strikes its quarry when adjacent, got {other:?}"),
        }
    }

    // ── Passive friendlies (§7.5) ───────────────────────────────────────────
    // An allied planner commands a SLICE of its side; the rest of the side
    // (player-commanded units, other allied nations) are passive friendlies:
    // they occupy hexes and count in the odds statistics, but never receive
    // actions.

    #[test]
    fn passive_friendly_blocks_the_corridor_forcing_a_detour() {
        // A water wall spanning the board with a single gap; a passive
        // friendly stands IN the gap. The blind march must route AROUND the
        // wall instead of booking the occupied corridor hex.
        let mut g = grid(20, 20);
        for r in 2..18 {
            if r == 10 {
                continue; // the gap
            }
            let h = HexCoord::new(6, r);
            g.set_terrain(h, Terrain::Water);
            g.cell_mut(h).unwrap().is_passable = false;
        }
        let gap = HexCoord::new(6, 10);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 2, 10)];
        let passive = vec![unit(2, UnitType::Infantry, Side::Attacker, 6, 10)];
        let enemy: Vec<BattalionUnit> = Vec::new();
        let intel: Vec<HexCoord> = (9..12).map(|r| HexCoord::new(12, r)).collect();

        // Control: without the passive friendly the march goes THROUGH the gap.
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_flags(&g, &own, &enemy, Some(&intel), None, None);
        let path = match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => path.clone(),
            other => panic!("blind march must move, got {other:?}"),
        };
        assert!(
            path.contains(&gap),
            "control: the open corridor is used, got {path:?}"
        );

        // With the passive friendly parked in the gap the path detours.
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_flags(&g, &own, &enemy, Some(&intel), None, Some(&passive));
        let path = match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => path.clone(),
            other => panic!("blind march must still move (detour), got {other:?}"),
        };
        assert!(
            !path.contains(&gap),
            "the passive friendly's hex blocks pathing — the march must detour, got {path:?}"
        );
    }

    #[test]
    fn passive_friendly_counts_toward_the_local_odds_gate() {
        // §7.3: three enemies packed around the target hex refuse the assault
        // at 1-vs-3 local odds; a passive friendly adjacent to the target
        // tips the count to 2-vs-3 and the strike is permitted. All units
        // get equal breakthrough so the trade math is neutral and the local
        // odds gate is the ONLY variable.
        let g = grid(20, 20);
        let mut line_inf = |id: usize, side: Side, q: i32, r: i32| {
            let mut u = unit(id, UnitType::Infantry, side, q, r);
            u.breakthrough = 40.0;
            u
        };
        let own = vec![line_inf(1, Side::Attacker, 10, 9)];
        let passive = vec![line_inf(2, Side::Attacker, 9, 10)];
        let enemy = vec![
            line_inf(10, Side::Defender, 10, 10),
            line_inf(11, Side::Defender, 10, 11),
            line_inf(12, Side::Defender, 11, 10),
        ];

        // Own-only: 1 attacker vs 3 defenders at the target hex — 3 !< 3*1,
        // the local-odds gate refuses the assault.
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, None, None);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::Assault { .. }),
            "1v3 must be refused by the local odds gate: {:?}",
            action_for(&actions, 1)
        );

        // With the passive friend adjacent: 2 vs 3 — 3 < 3*2, permitted.
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, None, Some(&passive));
        match action_for(&actions, 1) {
            AiAction::Assault { target_id, .. } => assert_eq!(*target_id, 10),
            other => panic!("passive friend must tip the local odds, got {other:?}"),
        }
    }

    #[test]
    fn passive_friendlies_never_receive_actions() {
        // Fresh, combat-effective, well-positioned allied battalions — the
        // kind of units the planner WOULD command if they were in its slice —
        // must never appear as the actor of any proposed action.
        let g = grid(20, 20);
        let own = vec![
            unit(1, UnitType::Infantry, Side::Attacker, 4, 4),
            unit(2, UnitType::ArtilleryBrigade, Side::Attacker, 4, 6),
        ];
        let passive = vec![
            unit(7, UnitType::Infantry, Side::Attacker, 5, 4),
            unit(8, UnitType::MediumArmor, Side::Attacker, 5, 6),
            unit(9, UnitType::Recon, Side::Attacker, 3, 5),
        ];
        let enemy = vec![
            unit(10, UnitType::Infantry, Side::Defender, 6, 4),
            unit(11, UnitType::Infantry, Side::Defender, 6, 6),
        ];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_flags(&g, &own, &enemy, None, None, Some(&passive));
        assert!(matches!(actions.last(), Some(AiAction::EndTurn)));
        let actor_id = |a: &AiAction| -> Option<usize> {
            match a {
                AiAction::MoveUnit { unit_id, .. }
                | AiAction::Hold { unit_id }
                | AiAction::Emplace { unit_id }
                | AiAction::Limber { unit_id }
                | AiAction::Retreat { unit_id } => Some(*unit_id),
                AiAction::Assault { attacker_id, .. }
                | AiAction::FireSupport { attacker_id, .. } => Some(*attacker_id),
                AiAction::EndTurn => None,
            }
        };
        for a in &actions {
            if let Some(id) = actor_id(a) {
                assert!(
                    !passive.iter().any(|p| p.id == id),
                    "passive unit {id} must never receive an action: {a:?}"
                );
            }
        }
        // Sanity: both own units DID get orders.
        for id in [1, 2] {
            assert!(
                actions.iter().filter_map(actor_id).any(|a| a == id),
                "own unit {id} got no action: {actions:?}"
            );
        }
    }

    #[test]
    fn passive_friendlies_count_toward_the_global_force_ratio() {
        // §7.1: an aggressive tactic outnumbered 3:1 GLOBALLY downgrades to
        // Hold. The ratio must count the whole side — a weak allied slice
        // propped up by strong passive friendlies keeps its aggressive
        // objective. (The downgrade is behaviorally invisible outside the
        // Attrition fire plan, so the merged own+passive slice that
        // plan_turn_flags builds is verified at select_objective directly.)
        let enemy: Vec<BattalionUnit> = (0..6)
            .map(|i| unit(10 + i, UnitType::Infantry, Side::Defender, 10, i as i32))
            .collect();
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 2, 2)];
        let passive: Vec<BattalionUnit> = (0..4)
            .map(|i| unit(2 + i, UnitType::Infantry, Side::Attacker, 3, i as i32))
            .collect();
        let ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        // Own-only: 100*3 = 300 < 600 → the 3:1 downgrade fires.
        assert_eq!(ai.select_objective(&own, &enemy), StrategicObjective::Hold);
        // The merged slice plan_turn_flags feeds when passive friendlies are
        // present: 500*3 = 1500 ≥ 600 → PushCenter stands.
        let mut merged = own.clone();
        merged.extend(passive.iter().cloned());
        assert_eq!(
            ai.select_objective(&merged, &enemy),
            StrategicObjective::PushCenter
        );
    }

    #[test]
    fn dense_advance_blob_no_stalled_battalions_repro() {
        // Live repro probe: a division deployed in a TIGHT cluster and
        // ordered to ADVANCE (no div target — tactic-card march) must not
        // leave battalions sitting with a move order but zero displacement.
        let g = grid(40, 40);
        let params = tactical_core::CombatParams::default();
        // Tight blob: center + the full ring-1 neighborhood.
        let blob = [
            (8, 8),
            (9, 8),
            (9, 9),
            (8, 9),
            (7, 9),
            (7, 8),
            (8, 7),
        ];
        let mut own: Vec<BattalionUnit> = blob
            .iter()
            .enumerate()
            .map(|(i, (q, r))| unit(1 + i, UnitType::Infantry, Side::Attacker, *q, *r))
            .collect();
        // Enemy far enough that 30 turns of marching (0.5 hex/turn for
        // foot) never makes contact — the probe isolates the march.
        let enemy: Vec<BattalionUnit> = vec![
            unit(50, UnitType::Infantry, Side::Defender, 8, 26),
            unit(51, UnitType::Infantry, Side::Defender, 9, 27),
            unit(52, UnitType::Infantry, Side::Defender, 7, 27),
        ];
        let intel: Vec<HexCoord> = enemy.iter().map(|e| e.position).collect();
        let start: Vec<HexCoord> = own.iter().map(|u| u.position).collect();
        let mut last_action: Vec<String> = own.iter().map(|_| String::new()).collect();
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 7);
        for _turn in 0..30 {
            let actions = ai.plan_turn_div_order(&g, &own, &enemy, Some(&intel), None, None);
            for a in &actions {
                match a {
                    AiAction::MoveUnit { unit_id, path } => {
                        last_action[*unit_id - 1] = format!("move->{:?}", path.last().unwrap());
                        if let Some(u) = own.iter_mut().find(|u| u.id == *unit_id) {
                            match &u.move_order {
                                // Game-layer re-affirm: same destination keeps
                                // the standing order and its invested hours.
                                Some(o) if o.path.last() == path.last() => {}
                                _ => {
                                    u.move_order =
                                        Some(tactical_core::unit::MoveOrder {
                                            path: path.clone(),
                                            hours: 0.0,
                                        })
                                }
                            }
                        }
                    }
                    // Hold (and any combat action) leaves the standing order
                    // untouched — same as the game layer's apply.
                    AiAction::Hold { unit_id } => last_action[*unit_id - 1] = "hold".into(),
                    AiAction::Assault { attacker_id, .. }
                    | AiAction::FireSupport { attacker_id, .. } => {
                        last_action[*attacker_id - 1] = "combat".into()
                    }
                    _ => {}
                }
            }
            tactical_core::movement::advance_move_orders(
                &g,
                &mut own,
                Side::Attacker,
                &params,
            );
        }
        for u in &own {
            let d = start[u.id - 1].distance(u.position);
            println!(
                "unit {} {:?} moved {} hexes | last={} | order_path={:?}",
                u.id,
                u.position,
                d,
                last_action[u.id - 1],
                u.move_order.as_ref().map(|o| o.path.len())
            );
            assert!(
                d >= 2,
                "unit {} stalled at {:?}: {} hexes in 30 turns (last action {})",
                u.id,
                u.position,
                d,
                last_action[u.id - 1]
            );
        }
    }

    /// Tracking probe for the "advance order, bars not growing" report:
    /// mirrors the game loop per turn (plan → apply → refresh → march) and
    /// logs every battalion's invested hours + blocked events turn by turn.
    #[test]
    fn dense_advance_progress_tracking_probe() {
        let g = grid(40, 40);
        let params = tactical_core::CombatParams::default();
        let blob = [
            (8, 8),
            (9, 8),
            (9, 9),
            (8, 9),
            (7, 9),
            (7, 8),
            (8, 7),
        ];
        let mut own: Vec<BattalionUnit> = blob
            .iter()
            .enumerate()
            .map(|(i, (q, r))| unit(1 + i, UnitType::Infantry, Side::Attacker, *q, *r))
            .collect();
        let enemy: Vec<BattalionUnit> = vec![
            unit(50, UnitType::Infantry, Side::Defender, 8, 26),
            unit(51, UnitType::Infantry, Side::Defender, 9, 27),
            unit(52, UnitType::Infantry, Side::Defender, 7, 27),
        ];
        let intel: Vec<HexCoord> = enemy.iter().map(|e| e.position).collect();
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 7);
        let n = own.len();
        let mut blocked_count = vec![0usize; n + 1];
        let mut blocker_of: Vec<String> = vec![String::new(); n + 1];
        let mut replaced_count = vec![0usize; n + 1];
        let mut hours_log: Vec<Vec<f32>> = vec![vec![]; n + 1];
        let mut dest_log: Vec<Vec<(i32, i32)>> = vec![vec![]; n + 1];
        for _turn in 0..40 {
            // 1) planner — the division's Advance (order = None).
            let actions = ai.plan_turn_div_order(&g, &own, &enemy, Some(&intel), None, None);
            for a in &actions {
                if let AiAction::MoveUnit { unit_id, path } = a {
                    if let Some(u) = own.iter_mut().find(|u| u.id == *unit_id) {
                        match &u.move_order {
                            Some(o) if o.path.last() == path.last() => {}
                            _ => {
                                replaced_count[*unit_id] += 1;
                                u.move_order = Some(tactical_core::unit::MoveOrder {
                                    path: path.clone(),
                                    hours: 0.0,
                                })
                            }
                        }
                    }
                }
            }
            // 2) refresh standing routes against the current situation
            //    (the game layer runs this before every march phase).
            let view = own.clone();
            for u in own.iter_mut().filter(|u| {
                u.side == Side::Attacker && u.is_combat_effective() && u.move_order.is_some()
            }) {
                tactical_core::movement::refresh_move_order(&g, u, &view, &params);
            }
            // 3) march + collect events.
            let events = tactical_core::movement::advance_move_orders(
                &g,
                &mut own,
                Side::Attacker,
                &params,
            );
            for ev in &events {
                if let tactical_core::movement::MovementEvent::Blocked { unit_id, blocker_id } =
                    ev
                {
                    blocked_count[*unit_id] += 1;
                    blocker_of[*unit_id] = format!("{blocker_id}");
                }
            }
            for u in &own {
                hours_log[u.id].push(
                    u.move_order
                        .as_ref()
                        .map(|o| (o.hours * 1000.0).round() / 1000.0)
                        .unwrap_or(-1.0),
                );
                dest_log[u.id].push(
                    u.move_order
                        .as_ref()
                        .and_then(|o| o.path.last().copied())
                        .map(|h| (h.q, h.r))
                        .unwrap_or((-99, -99)),
                );
            }
        }
        for u in &own {
            let id = u.id;
            let frozen = longest_frozen_run(&hours_log[id]);
            assert!(
                frozen <= 3,
                "unit {id} progress frozen for {frozen} turns (regression of the \
                 destination-flap wipe): {:?}",
                hours_log[id]
            );
            println!(
                "unit {id} end={:?} blocked={} by[{}] replaced={} frozen_run={}\n  hours={:?}\n  dest ={:?}",
                u.position,
                blocked_count[id],
                blocker_of[id],
                replaced_count[id],
                frozen,
                hours_log[id],
                dest_log[id],
            );
        }
    }

    /// Longest run of consecutive turns with an unchanged invested-hours
    /// value (>= 0 — an existing order); -1 (no order) breaks a run.
    fn longest_frozen_run(hours: &[f32]) -> usize {
        let mut best = 0usize;
        let mut cur = 0usize;
        let mut prev: Option<f32> = None;
        for &h in hours {
            if h >= 0.0 && prev == Some(h) {
                cur += 1;
                best = best.max(cur);
            } else if h >= 0.0 {
                cur = 1;
            } else {
                cur = 0;
            }
            prev = Some(h);
        }
        best
    }

    /// The frozen-bar fix: with a position-dependent goal (the intel
    /// ring), the planner must RE-AFFIRM the standing destination instead
    /// of re-basing to the fresh goal — a flip wipes the invested hours at
    /// the apply layer and the progress bar sits frozen.
    #[test]
    fn advance_keeps_standing_destination_when_goal_shifts_within_hysteresis() {
        let g = grid(24, 24);
        let mut own = vec![unit(1, UnitType::Infantry, Side::Attacker, 8, 8)];
        own[0].move_order = Some(tactical_core::unit::MoveOrder {
            path: vec![HexCoord::new(8, 9), HexCoord::new(8, 10)],
            hours: 0.1,
        });
        let intel = vec![HexCoord::new(8, 12), HexCoord::new(9, 12), HexCoord::new(7, 13)];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_div_order(&g, &own, &[], Some(&intel), None, None);
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => assert_eq!(
                *path.last().unwrap(),
                HexCoord::new(8, 10),
                "standing destination kept while the goal stays in the neighbourhood"
            ),
            other => panic!("expected the march to continue, got {other:?}"),
        }
    }

    /// Direct-fire guns (AT/AA) fire precision missions at units only:
    /// an emplaced AT with a visible target in range precise-fires; with
    /// none it never blind-bombards its intel goal (the old area path
    /// splashed the gun's own hex).
    #[test]
    fn at_gun_precision_fires_but_never_blind_bombards() {
        // Target in range → precision fire mission.
        let g = grid(24, 24);
        let mut own = vec![unit(1, UnitType::AntiTankBrigade, Side::Attacker, 8, 8)];
        own[0].is_emplaced = true;
        let enemy = vec![unit(50, UnitType::Infantry, Side::Defender, 8, 10)];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, None);
        assert!(
            matches!(action_for(&actions, 1), AiAction::FireSupport { .. }),
            "AT gun precise-fires at a target in range: {:?}",
            action_for(&actions, 1)
        );

        // No target in range → never a blind zone mission at the intel goal.
        let enemy_far = vec![unit(50, UnitType::Infantry, Side::Defender, 8, 22)];
        let intel: Vec<HexCoord> = enemy_far.iter().map(|e| e.position).collect();
        let actions = ai.plan_turn_div_order(&g, &own, &enemy_far, Some(&intel), None, None);
        assert!(
            !matches!(action_for(&actions, 1), AiAction::FireSupport { .. }),
            "AT guns never blind-bombard: {:?}",
            action_for(&actions, 1)
        );
    }

    #[test]
    fn friendly_held_goal_queues_at_the_edge() {
        // A goal hex HELD by a parked sister battalion
        // must not freeze the march — path up to the goal's edge, stop on
        // a free hex, strictly closer than the start (a far-off occupied
        // objective gates nothing; only execution-time adjacent blocking
        // may hold the last step).
        let g = grid(24, 24);
        let own = vec![
            unit(1, UnitType::Infantry, Side::Attacker, 8, 8),
            unit(2, UnitType::Infantry, Side::Attacker, 8, 14), // parked ON the goal
        ];
        let goal = HexCoord::new(8, 14);
        let intel = vec![goal];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_div_order(&g, &own, &[], Some(&intel), None, None);
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => {
                let dest = *path.last().unwrap();
                assert_ne!(dest, goal, "never book a path into a friendly-held goal");
                assert!(
                    dest.distance(goal) <= 1,
                    "queue at the goal's edge: {dest:?}"
                );
                assert!(
                    dest.distance(goal) < own[0].position.distance(goal),
                    "the march must make progress toward the goal: {dest:?}"
                );
            }
            other => panic!("friendly-held goal must not freeze the march: {other:?}"),
        }
    }

    #[test]
    fn enemy_held_goal_keeps_marching_into_contact() {
        // Guard rail: the queue-at-the-edge rule is FRIENDLY-only. An
        // enemy-held goal (the nearest-foe advance) must still be marched
        // into — interception/contact is how the line engages.
        let g = grid(24, 24);
        let own = vec![unit(1, UnitType::Infantry, Side::Attacker, 8, 8)];
        let enemy = vec![unit(10, UnitType::Infantry, Side::Defender, 8, 12)];
        let mut ai = TacticalAi::new(Side::Attacker, CombatTactic::Assault, 1);
        let actions = ai.plan_turn_div_order(&g, &own, &enemy, None, None, None);
        match action_for(&actions, 1) {
            AiAction::MoveUnit { path, .. } => assert_eq!(
                *path.last().unwrap(),
                enemy[0].position,
                "march into the enemy-held hex (execution decides the stop)"
            ),
            other => panic!("expected the march into contact, got {other:?}"),
        }
    }
}
