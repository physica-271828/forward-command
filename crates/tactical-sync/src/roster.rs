//! The mid-battle HOI4 division roster (DESIGN.md §8.2).
//!
//! A live tactical battle mirrors the HOI4 `land_combat` division list, but
//! that list is not static: vanilla forces a 0-org division out of combat
//! the moment the injected `damage_units` lines grind it down,
//! and fresh divisions reinforce mid-battle (an attacker opening a new
//! attack direction, a defender marching into the contested province). The
//! tactical board must track both flows or it drifts from the HOI4 truth
//! the damage writeback books against:
//!
//! * a division whose HOI4 mirror left the combat hours ago would keep
//!   fighting (and counting for victory) on the tactical board — the
//!   "routed at hour 3, arrived at hour 6" hole;
//! * a division that reinforced the HOI4 battle would fight HOI4-side with
//!   no tactical mirror — the "reinforcement holds" hole.
//!
//! So at every post-sync save snapshot the `land_combat` unit-id lists are
//! diffed against this roster (the HOI4 list is ground truth — it can only
//! change at a sync boundary, because vanilla damage is frozen and injected
//! damage lands during the clock advance). Departures have their battalions
//! marched off (`UnitState::LeftBattle` — the same terminal state as the
//! §6.14 out-of-bounds leavers, zero board/AI surgery); joiners are
//! assembled from the save and placed at the map edge in their approach
//! direction (no mid-battle deployment phase).
//!
//! The approach direction comes from `last_seen`: every division's last
//! observed province, refreshed at every sync for BOTH countries' whole
//! armies (watching only participants would know nothing about a division
//! that never was in the combat). A joiner currently standing in the
//! contested province (a defender that just marched in) takes the bearing
//! of the province it was seen at last sync; an attacker joiner stands in
//! its source province, which IS the bearing. `approach_dirs` maps
//! neighbour provinces of the contested one to compass directions
//! (assembly-time border-bearing table, tactical-map).

use std::collections::{HashMap, HashSet};

use tactical_core::{HexDirection, Side};

/// One participating HOI4 division tracked across syncs.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    /// HOI4 division id (`division.id` in the save, `unit = { id = N }` in
    /// the `land_combat` lists).
    pub division_id: u64,
    pub side: Side,
    /// Owning country tag (roster membership is per country): display +
    /// the save-side owner lookups.
    pub tag: String,
    /// HOI4 division name (player-facing join/leave log lines).
    pub name: String,
    /// Tactical battalion ids mirroring this division (HQ included).
    pub battalion_ids: Vec<usize>,
    /// Σ battalion `max_org` of this division (HQ excluded) — the
    /// damage-ratio base currency (the numerator is counted in the same
    /// battalion-scale points).
    pub org_pool: f32,
    /// HOI4 division max strength (subunit sum, support companies
    /// included — `tactical_save::division_maxima`).
    pub max_str: f32,
    /// Damage-routing province: attacker entries carry the division's
    /// SOURCE province, defender entries the contested one (mirrors the
    /// battalions' `hoi4_province`).
    pub province: Option<u32>,
}

/// The outcome of one roster diff against a fresh `land_combat` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RosterDiff {
    /// Division ids newly listed in the combat, with their side.
    pub joined: Vec<(u64, Side)>,
    /// Roster entries whose division is no longer listed on either side.
    pub left: Vec<RosterEntry>,
}

impl RosterDiff {
    pub fn is_empty(&self) -> bool {
        self.joined.is_empty() && self.left.is_empty()
    }
}

/// The mid-battle roster: which HOI4 divisions the tactical battle mirrors.
/// Carried by [`crate::BattleSession`] (Clone — checkpoint snapshots ride
/// along; the bin re-takes the sync checkpoint right after applying a
/// roster change so a rollback never resurrects a departed division).
/// Empty outside live battles — the maintenance entry point
/// (`scenario::apply_roster_sync`) gates on the session's battle context,
/// so an empty roster there is a valid "everyone left" state, not a
/// disable flag.
#[derive(Debug, Clone, Default)]
pub struct BattleRoster {
    entries: Vec<RosterEntry>,
    /// Every division's last observed province (both belligerent countries'
    /// whole armies — a mid-battle joiner is by definition NOT a previous
    /// participant). Refreshed at every roster sync.
    last_seen: HashMap<u64, u32>,
    /// Neighbour province of the contested one → compass direction from it
    /// (assembly-time border-bearing table, tactical-map).
    approach_dirs: HashMap<u32, HexDirection>,
}

impl BattleRoster {
    pub fn new(
        entries: Vec<RosterEntry>,
        last_seen: HashMap<u64, u32>,
        approach_dirs: HashMap<u32, HexDirection>,
    ) -> Self {
        BattleRoster {
            entries,
            last_seen,
            approach_dirs,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[RosterEntry] {
        &self.entries
    }

    pub fn get(&self, division_id: u64) -> Option<&RosterEntry> {
        self.entries.iter().find(|e| e.division_id == division_id)
    }

    pub fn insert(&mut self, entry: RosterEntry) {
        self.entries.push(entry);
    }

    /// Remove and return the entry (None when the division was not tracked).
    pub fn remove(&mut self, division_id: u64) -> Option<RosterEntry> {
        let i = self
            .entries
            .iter()
            .position(|e| e.division_id == division_id)?;
        Some(self.entries.remove(i))
    }

    /// Diff the roster against the fresh `land_combat` unit-id lists (the
    /// HOI4 ground truth). Ids are compared as sets; the combat's attacker
    /// / defender lists are the side assignment for joiners. Deterministic
    /// order: joiners ascend by id, leavers keep roster order.
    pub fn diff(&self, attacker_ids: &HashSet<u64>, defender_ids: &HashSet<u64>) -> RosterDiff {
        let known: HashSet<u64> = self.entries.iter().map(|e| e.division_id).collect();
        let mut joined: Vec<(u64, Side)> = attacker_ids
            .iter()
            .filter(|id| !known.contains(*id))
            .map(|id| (*id, Side::Attacker))
            .chain(
                defender_ids
                    .iter()
                    .filter(|id| !known.contains(*id))
                    .map(|id| (*id, Side::Defender)),
            )
            .collect();
        joined.sort_by_key(|(id, _)| *id);
        let left: Vec<RosterEntry> = self
            .entries
            .iter()
            .filter(|e| {
                !attacker_ids.contains(&e.division_id) && !defender_ids.contains(&e.division_id)
            })
            .cloned()
            .collect();
        RosterDiff { joined, left }
    }

    /// Refresh the last-seen province table from a fresh division
    /// enumeration (`(division_id, location)` pairs; divisions with no
    /// location are skipped).
    pub fn refresh_last_seen(&mut self, locations: impl Iterator<Item = (u64, Option<u32>)>) {
        for (id, loc) in locations {
            if let Some(p) = loc {
                self.last_seen.insert(id, p);
            }
        }
    }

    /// The province a division was last seen at (any watched division,
    /// participant or not).
    pub fn last_seen(&self, division_id: u64) -> Option<u32> {
        self.last_seen.get(&division_id).copied()
    }

    /// The approach direction of a division joining from `from_province`:
    /// the contested province's border-bearing table entry for it.
    pub fn approach_dir(&self, from_province: u32) -> Option<HexDirection> {
        self.approach_dirs.get(&from_province).copied()
    }

    /// Resolve a joiner's approach direction (see the module docs): an
    /// attacker stands in its source province (its bearing IS the
    /// direction); a defender already inside the contested province falls
    /// back to the province it was seen at last sync. `current`/`contested`
    /// are provinces; `None` when nothing resolves (caller places at the
    /// side's deployment zone instead).
    pub fn join_direction(
        &self,
        division_id: u64,
        side: Side,
        current: Option<u32>,
        contested: u32,
    ) -> Option<HexDirection> {
        let from = match (side, current) {
            (Side::Attacker, Some(p)) => Some(p),
            // A defender already IN the contested province came from its
            // last-seen province; anything else uses where it stands.
            (_, Some(p)) if p == contested => self.last_seen(division_id),
            (_, Some(p)) => Some(p),
            (_, None) => self.last_seen(division_id),
        }?;
        if from == contested {
            return None;
        }
        self.approach_dir(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, side: Side) -> RosterEntry {
        RosterEntry {
            division_id: id,
            side,
            tag: "ITA".to_string(),
            name: format!("Div-{id}"),
            battalion_ids: vec![(id * 10) as usize],
            org_pool: 100.0,
            max_str: 50.0,
            province: Some(13251),
        }
    }

    fn roster() -> BattleRoster {
        BattleRoster::new(
            vec![entry(1, Side::Attacker), entry(10, Side::Defender)],
            [(99, 13250), (20, 2072)].into_iter().collect(),
            [(13250, HexDirection::W), (2072, HexDirection::NE)]
                .into_iter()
                .collect(),
        )
    }

    #[test]
    fn diff_reports_joiners_leavers_and_stayers() {
        let r = roster();
        // Division 1 routed out; 99 joined the attack, 20 the defense;
        // 10 stays.
        let atk: HashSet<u64> = [99].into_iter().collect();
        let def: HashSet<u64> = [10, 20].into_iter().collect();
        let d = r.diff(&atk, &def);
        // Deterministic: joiners ascend by id.
        assert_eq!(d.joined, vec![(20, Side::Defender), (99, Side::Attacker)]);
        assert_eq!(d.left.len(), 1);
        assert_eq!(d.left[0].division_id, 1);
        assert!(!d.is_empty());
    }

    #[test]
    fn diff_of_an_unchanged_combat_is_empty() {
        let r = roster();
        let atk: HashSet<u64> = [1].into_iter().collect();
        let def: HashSet<u64> = [10].into_iter().collect();
        assert!(r.diff(&atk, &def).is_empty());
    }

    #[test]
    fn join_direction_prefers_the_attackers_current_source_province() {
        let r = roster();
        // Attacker stands in its source province → its bearing, even if the
        // watch table saw it elsewhere earlier.
        assert_eq!(
            r.join_direction(99, Side::Attacker, Some(13250), 13238),
            Some(HexDirection::W)
        );
    }

    #[test]
    fn join_direction_of_an_arrived_defender_uses_last_seen() {
        let r = roster();
        // Defender already IN the contested province → the province it
        // marched in from.
        assert_eq!(
            r.join_direction(20, Side::Defender, Some(13238), 13238),
            Some(HexDirection::NE)
        );
        // Never seen anywhere else → no direction (zone fallback).
        assert_eq!(
            r.join_direction(21, Side::Defender, Some(13238), 13238),
            None
        );
    }

    #[test]
    fn refresh_last_seen_updates_and_skips_locationless() {
        let mut r = roster();
        r.refresh_last_seen([(99, Some(9999)), (20, None)].into_iter());
        assert_eq!(r.last_seen(99), Some(9999));
        assert_eq!(r.last_seen(20), Some(2072)); // untouched
    }

    #[test]
    fn remove_returns_the_entry_once() {
        let mut r = roster();
        let e = r.remove(1).expect("tracked");
        assert_eq!(e.division_id, 1);
        assert!(r.remove(1).is_none());
        assert_eq!(r.entries().len(), 1);
    }
}
