//! "What can the player fight right now" — the single battle judgment
//! (DESIGN.md §10.7).
//!
//! The scan publisher (which states get activated in the decision layer) and
//! the post-pick resolution ([`crate::snapshot::pick_province`]) both derive
//! their battle list from [`enterable_battles`], so a state can never be
//! offered without a battle behind it, and a battle never exists without
//! being offered.

use std::collections::HashMap;

use tactical_save::{BorderWarCombatSideData, LandCombatSideData, SaveGame};

/// Which vanilla record a battle was derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BattleKind {
    /// A running `land_combat` — normal fronts, contested landings (the
    /// attacker's divisions sit at sea) and airdrops.
    Land,
    /// A running `border_war_combat` — each side stays in its own state.
    BorderWar,
}

/// One battle the player can take tactical command of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnterableBattle {
    pub kind: BattleKind,
    /// The state the player picks (the state-target decision's target).
    pub state: u32,
    /// The province handed to the battle launcher.
    pub province: u32,
    /// The player is on the ATTACKING side of the vanilla record.
    pub player_attacker: bool,
    pub attacker_tags: Vec<String>,
    pub defender_tags: Vec<String>,
    pub attacker_units: usize,
    pub defender_units: usize,
}

/// Every running battle `tag` takes part in, keyed by the state the player
/// would pick. `p2s` maps province → state.
pub fn enterable_battles(
    save: &SaveGame,
    p2s: &HashMap<u32, u32>,
    tag: &str,
) -> Vec<EnterableBattle> {
    let owners = unit_owners(save);
    let mut out = Vec::new();

    for combat in &save.land_combats {
        let player_attacker = land_side_has_player(&combat.attacker, &owners, tag);
        let player_defender = land_side_has_player(&combat.defender, &owners, tag);
        if !player_attacker && !player_defender {
            continue;
        }
        let Some(state) = p2s.get(&combat.location).copied() else {
            continue;
        };
        out.push(EnterableBattle {
            kind: BattleKind::Land,
            state,
            province: combat.location,
            player_attacker,
            attacker_tags: combat.attacker.tags.clone(),
            defender_tags: combat.defender.tags.clone(),
            attacker_units: combat.attacker.unit_ids.len(),
            defender_units: combat.defender.unit_ids.len(),
        });
    }

    for combat in &save.border_war_combats {
        let player_attacker = border_side_has_player(&combat.attacker, &owners, tag);
        let player_defender = border_side_has_player(&combat.defender, &owners, tag);
        if !player_attacker && !player_defender {
            continue;
        }
        // Anchor on the record's `location` — the DEFENDER's province — the
        // same convention as a land combat (the attacker stages in its own
        // province and fights INTO the defender's). Border-war sides never
        // co-locate, so when the player IS the attacking side, the picked
        // state is the enemy's border state.
        let province = combat.location;
        let Some(state) = p2s.get(&province).copied().or(combat.defender.state) else {
            continue;
        };
        out.push(EnterableBattle {
            kind: BattleKind::BorderWar,
            state,
            province,
            player_attacker,
            attacker_tags: combat.attacker.tags.clone(),
            defender_tags: combat.defender.tags.clone(),
            attacker_units: combat.attacker.unit_ids.len(),
            defender_units: combat.defender.unit_ids.len(),
        });
    }

    out
}

/// division id → owning country tag. Division ids are unique across a save
/// and the containing country block IS the owner — the same rule the battle
/// assembly uses.
pub(crate) fn unit_owners(save: &SaveGame) -> HashMap<u64, &str> {
    let mut owners = HashMap::new();
    for (tag, country) in &save.countries {
        for div in &country.divisions {
            owners.insert(div.id, tag.as_str());
        }
    }
    owners
}

/// The player's tag in the side's tag list, or on one of its divisions —
/// volunteer and expeditionary units may fight under another country's
/// banner while still being owned by the player.
pub(crate) fn land_side_has_player(
    side: &LandCombatSideData,
    owners: &HashMap<u64, &str>,
    tag: &str,
) -> bool {
    side.tags.iter().any(|t| t == tag)
        || side
            .unit_ids
            .iter()
            .any(|id| owners.get(id).copied() == Some(tag))
}

pub(crate) fn border_side_has_player(
    side: &BorderWarCombatSideData,
    owners: &HashMap<u64, &str>,
    tag: &str,
) -> bool {
    side.tag.as_deref() == Some(tag)
        || side.tags.iter().any(|t| t == tag)
        || side
            .unit_ids
            .iter()
            .any(|id| owners.get(id).copied() == Some(tag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_save::{
        BorderWarCombatData, CountryData, DivisionData, LandCombatData, LandCombatSideData,
    };

    fn div(id: u64) -> DivisionData {
        DivisionData {
            id,
            name: format!("Div-{id}"),
            template_name: "T".to_string(),
            names_group: None,
            name_order: None,
            location: None,
            organization: 1.0,
            strength: 1.0,
            experience: 0.0,
            entrenchment: 0.0,
            supply_current: None,
            supply_max: None,
            fuel: None,
            equipment: HashMap::new(),
            equipment_ids: Vec::new(),
            battalions: Vec::new(),
            support_companies: Vec::new(),
            is_army_hq: false,
        }
    }

    fn country(tag: &str, unit_ids: &[u64]) -> CountryData {
        CountryData {
            tag: tag.to_string(),
            divisions: unit_ids.iter().map(|id| div(*id)).collect(),
            technologies: Vec::new(),
            active_ideas: Vec::new(),
            dynamic_modifiers: Vec::new(),
            leader_traits: Vec::new(),
            active_advisors: Vec::new(),
        }
    }

    fn side(units: &[u64], tags: &[&str]) -> LandCombatSideData {
        LandCombatSideData {
            unit_ids: units.to_vec(),
            tactic: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn land(
        location: u32,
        attacker: LandCombatSideData,
        defender: LandCombatSideData,
    ) -> LandCombatData {
        LandCombatData {
            location,
            attacker,
            defender,
        }
    }

    fn save_with(combats: Vec<LandCombatData>, border_wars: Vec<BorderWarCombatData>) -> SaveGame {
        SaveGame {
            player: Some("ITA".to_string()),
            land_combats: combats,
            border_war_combats: border_wars,
            ..Default::default()
        }
    }

    #[test]
    fn land_combat_with_player_tag_is_enterable() {
        let save = save_with(
            vec![land(1001, side(&[1], &["ITA"]), side(&[2], &["ETH"]))],
            vec![],
        );
        let p2s: HashMap<u32, u32> = [(1001, 835)].into_iter().collect();
        let battles = enterable_battles(&save, &p2s, "ITA");
        assert_eq!(battles.len(), 1);
        assert_eq!(battles[0].kind, BattleKind::Land);
        assert_eq!(battles[0].state, 835);
        assert_eq!(battles[0].province, 1001);
        assert!(battles[0].player_attacker);
        assert_eq!(battles[0].attacker_tags, vec!["ITA".to_string()]);
    }

    #[test]
    fn land_combat_counts_player_owned_units_without_a_tag() {
        // Volunteer shape: the side's tag list may omit the player while the
        // units themselves are owned by the player's country block.
        let save = SaveGame {
            countries: [("GER".to_string(), country("GER", &[77]))]
                .into_iter()
                .collect(),
            ..save_with(
                vec![land(1001, side(&[77], &["SPR"]), side(&[2], &["SPD"]))],
                vec![],
            )
        };
        let p2s: HashMap<u32, u32> = [(1001, 835)].into_iter().collect();
        let battles = enterable_battles(&save, &p2s, "GER");
        assert_eq!(battles.len(), 1);
        assert!(battles[0].player_attacker);
    }

    #[test]
    fn unrelated_combats_are_skipped() {
        let save = save_with(
            vec![land(1001, side(&[1], &["ETH"]), side(&[2], &["SOV"]))],
            vec![],
        );
        let p2s: HashMap<u32, u32> = [(1001, 835)].into_iter().collect();
        assert!(enterable_battles(&save, &p2s, "ITA").is_empty());
    }

    #[test]
    fn combat_without_a_state_mapping_is_skipped() {
        let save = save_with(
            vec![land(1001, side(&[1], &["ITA"]), side(&[2], &["ETH"]))],
            vec![],
        );
        let p2s: HashMap<u32, u32> = HashMap::new();
        assert!(enterable_battles(&save, &p2s, "ITA").is_empty());
    }

    #[test]
    fn border_war_anchors_on_the_defenders_province() {
        let save = save_with(
            vec![],
            vec![BorderWarCombatData {
                location: 11879,
                attacker: BorderWarCombatSideData {
                    unit_ids: vec![1890],
                    tags: vec!["JAP".to_string()],
                    tag: Some("JAP".to_string()),
                    state: Some(1028),
                    province: Some(6944),
                    ..Default::default()
                },
                defender: BorderWarCombatSideData {
                    unit_ids: vec![495, 496],
                    tags: vec!["SOV".to_string()],
                    tag: Some("SOV".to_string()),
                    state: Some(408),
                    province: Some(11879),
                    ..Default::default()
                },
            }],
        );
        let p2s: HashMap<u32, u32> = [(6944, 1028), (11879, 408)].into_iter().collect();
        // The defending player picks its own state (the record's `location`).
        let battles = enterable_battles(&save, &p2s, "SOV");
        assert_eq!(battles.len(), 1);
        assert_eq!(battles[0].kind, BattleKind::BorderWar);
        assert_eq!(battles[0].state, 408);
        assert_eq!(battles[0].province, 11879);
        assert!(!battles[0].player_attacker);
        assert_eq!(battles[0].defender_units, 2);
    }

    #[test]
    fn border_war_attacker_picks_the_defenders_state() {
        // The attacking player picks the ENEMY's border state — the same
        // convention as attacking into an enemy state in a land combat.
        let save = save_with(
            vec![],
            vec![BorderWarCombatData {
                location: 11879,
                attacker: BorderWarCombatSideData {
                    unit_ids: vec![1],
                    tags: vec!["ITA".to_string()],
                    tag: Some("ITA".to_string()),
                    state: Some(1028),
                    province: Some(6944),
                    ..Default::default()
                },
                defender: BorderWarCombatSideData {
                    unit_ids: vec![2],
                    tags: vec!["SOV".to_string()],
                    tag: Some("SOV".to_string()),
                    state: Some(408),
                    province: Some(11879),
                    ..Default::default()
                },
            }],
        );
        let p2s: HashMap<u32, u32> = [(6944, 1028), (11879, 408)].into_iter().collect();
        let battles = enterable_battles(&save, &p2s, "ITA");
        assert_eq!(battles.len(), 1);
        assert_eq!(battles[0].state, 408);
        assert_eq!(battles[0].province, 11879);
        assert!(battles[0].player_attacker);
    }

    #[test]
    fn border_war_state_falls_back_to_the_defender_field() {
        let save = save_with(
            vec![],
            vec![BorderWarCombatData {
                location: 6944,
                attacker: BorderWarCombatSideData {
                    unit_ids: vec![1],
                    tags: vec!["ITA".to_string()],
                    tag: Some("ITA".to_string()),
                    state: None,
                    province: Some(6944),
                    ..Default::default()
                },
                defender: BorderWarCombatSideData {
                    unit_ids: vec![2],
                    state: Some(408),
                    ..Default::default()
                },
            }],
        );
        // No province→state mapping (odd install) — the record's own field
        // carries the state.
        let p2s: HashMap<u32, u32> = HashMap::new();
        let battles = enterable_battles(&save, &p2s, "ITA");
        assert_eq!(battles.len(), 1);
        assert_eq!(battles[0].state, 408);
        assert!(battles[0].player_attacker);
    }
}
