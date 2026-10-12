//! Fog of war — per-side visibility with stale intel decay (§4/§6.1 sight),
//! plus the sound-ranging marks a HEARD shot leaves on a hex (§6.1).

use crate::grid::HexGrid;
use crate::hex::HexCoord;
use crate::los::visible_hexes;
use crate::unit::{BattalionUnit, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityState {
    /// In sight of a friendly unit right now.
    Visible,
    /// Seen before, but no current observer — stale intel.
    Revealed,
    /// Never seen (or intel fully decayed).
    Hidden,
}

#[derive(Debug, Clone)]
pub struct FogOfWar {
    pub width: usize,
    pub height: usize,
    /// Turn number when each hex was last observed; -1 = never.
    last_seen: Vec<i32>,
    /// Currently visible this update.
    visible: Vec<bool>,
    /// Turns an observation stays "revealed" before decaying to hidden.
    pub decay_turns: u32,
    /// Turn number when a firing was last HEARD on each hex; -1 = never.
    fired: Vec<i32>,
    /// Turns a firing mark stays live before it decays.
    pub sound_mark_turns: u32,
}

impl FogOfWar {
    pub fn new(width: usize, height: usize, decay_turns: u32, sound_mark_turns: u32) -> Self {
        FogOfWar {
            width,
            height,
            last_seen: vec![-1; width * height],
            visible: vec![false; width * height],
            decay_turns,
            fired: vec![-1; width * height],
            sound_mark_turns,
        }
    }

    fn idx(&self, h: HexCoord) -> Option<usize> {
        if h.q >= 0 && h.r >= 0 && (h.q as usize) < self.width && (h.r as usize) < self.height {
            Some(h.r as usize * self.width + h.q as usize)
        } else {
            None
        }
    }

    /// Recompute current visibility from all combat-effective units of `side`.
    /// Effective sight = unit sight + terrain modifier (§6.1 × §6.6).
    /// Concealing terrain (Forest/Jungle, `Terrain::conceals`)
    /// costs observers 1 sight seeing IN — such hexes are marked only within
    /// `sight − 1`, floored at adjacency so sight-1 units still spot the
    /// treeline next to them. (The occupant's own sight is unaffected.)
    pub fn update(&mut self, grid: &HexGrid, units: &[BattalionUnit], side: Side, turn: u32) {
        self.visible.iter_mut().for_each(|v| *v = false);
        for u in units
            .iter()
            .filter(|u| u.side == side && u.is_combat_effective())
        {
            let terrain_sight = grid
                .cell(u.position)
                .map(|c| c.terrain.sight_range())
                .unwrap_or(2);
            let sight = crate::los::effective_sight(u.sight_range, terrain_sight);
            // Beyond this distance, concealing hexes stay unseen.
            let conceal_limit = (sight - 1).max(1);
            for h in visible_hexes(grid, u.position, sight) {
                if u.position.distance(h) > conceal_limit
                    && grid.cell(h).map(|c| c.terrain.conceals()).unwrap_or(false)
                {
                    continue;
                }
                if let Some(i) = self.idx(h) {
                    self.visible[i] = true;
                    self.last_seen[i] = turn as i32;
                }
            }
        }
    }

    pub fn state(&self, h: HexCoord, turn: u32) -> VisibilityState {
        let Some(i) = self.idx(h) else {
            return VisibilityState::Hidden;
        };
        if self.visible[i] {
            VisibilityState::Visible
        } else if self.last_seen[i] >= 0
            && (turn as i32 - self.last_seen[i]) <= self.decay_turns as i32
        {
            VisibilityState::Revealed
        } else {
            VisibilityState::Hidden
        }
    }

    pub fn is_visible(&self, h: HexCoord) -> bool {
        self.idx(h).map(|i| self.visible[i]).unwrap_or(false)
    }

    /// Record a firing HEARD from `h` on `turn` — sound-ranging intel marks
    /// the ground, never the unit standing on it (DESIGN.md §6.1). Overwriting
    /// keeps the last report only, so a battery firing again REFRESHES a live
    /// mark instead of stacking a second one. Out-of-bounds hexes are ignored.
    pub fn mark_firing(&mut self, h: HexCoord, turn: u32) {
        if let Some(i) = self.idx(h) {
            self.fired[i] = turn as i32;
        }
    }

    /// The firing mark on `h` as its AGE in turns (0 = heard this turn), or
    /// `None` when the hex carries no live mark. A mark stays live for
    /// `sound_mark_turns` turns and is judged lazily against `turn`, exactly
    /// like [`Self::state`]'s stale intel. Out-of-bounds hexes never mark.
    pub fn firing_marker(&self, h: HexCoord, turn: u32) -> Option<u32> {
        self.age_at(self.idx(h)?, turn)
    }

    /// Every live mark as (hex, age in turns), in a stable row-major order —
    /// the whole-map form of [`Self::firing_marker`] for the displays and
    /// planners that walk the side's whole intel picture.
    pub fn firing_marks(&self, turn: u32) -> impl Iterator<Item = (HexCoord, u32)> + '_ {
        let width = self.width.max(1);
        self.fired.iter().enumerate().filter_map(move |(i, _)| {
            self.age_at(i, turn)
                .map(|age| (HexCoord::new((i % width) as i32, (i / width) as i32), age))
        })
    }

    /// Lazily judge the raw stamp at flat hex index `i` against `turn`.
    fn age_at(&self, i: usize, turn: u32) -> Option<u32> {
        let fired = self.fired[i];
        if fired < 0 {
            return None;
        }
        let age = turn as i32 - fired;
        (age >= 0 && age <= self.sound_mark_turns as i32).then_some(age as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Terrain;
    use crate::unit::UnitType;

    #[test]
    fn sees_then_stale_then_hidden() {
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mut fow = FogOfWar::new(10, 10, 3, 3);
        let u = BattalionUnit::new(0, "R", UnitType::Recon, Side::Attacker, HexCoord::new(5, 5));
        let units = vec![u];

        fow.update(&g, &units, Side::Attacker, 1);
        let far = HexCoord::new(5, 2); // distance 3, within recon sight 4
        assert_eq!(fow.state(far, 1), VisibilityState::Visible);

        // Unit gone: intel stays revealed for 3 turns then decays.
        fow.update(&g, &[], Side::Attacker, 2);
        assert_eq!(fow.state(far, 2), VisibilityState::Revealed);
        fow.update(&g, &[], Side::Attacker, 6);
        assert_eq!(fow.state(far, 6), VisibilityState::Hidden);
    }

    /// Forest/jungle no longer blind the OCCUPANT (plains sight),
    /// they CONCEAL it instead — observers pay 1 sight seeing in, floored
    /// at adjacency. Marsh is open ground on both counts.
    #[test]
    fn forest_conceals_but_does_not_blind() {
        let mut g = HexGrid::new(12, 12, Terrain::Plains);
        g.set_terrain(HexCoord::new(5, 5), Terrain::Forest);
        g.set_terrain(HexCoord::new(7, 7), Terrain::Jungle);
        g.set_terrain(HexCoord::new(3, 3), Terrain::Marsh);

        // 1. The occupant's own sight is plains-level (distance-2 seen).
        let inf = BattalionUnit::new(
            0,
            "I",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(5, 5),
        );
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[inf], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 3), 0), VisibilityState::Visible);

        // 2. Infantry observer (sight 2): the forest hex is hidden at
        //    distance 2, revealed adjacent (the sight−1 = 1 limit).
        let obs2 = BattalionUnit::new(
            1,
            "O",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(5, 3),
        );
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[obs2], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Hidden);
        let obs1 = BattalionUnit::new(
            2,
            "O",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(5, 4),
        );
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[obs1], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Visible);

        // 3. Recon (sight 4): sees into forest at distance 3, not 4.
        let rec3 = BattalionUnit::new(3, "R", UnitType::Recon, Side::Attacker, HexCoord::new(5, 2));
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[rec3], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Visible);
        let rec4 = BattalionUnit::new(4, "R", UnitType::Recon, Side::Attacker, HexCoord::new(5, 1));
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[rec4], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Hidden);

        // 4. Adjacency floor: a sight-1 unit still spots the treeline.
        let gun = BattalionUnit::new(
            5,
            "G",
            UnitType::ArtilleryBrigade,
            Side::Attacker,
            HexCoord::new(5, 4),
        );
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[gun], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Visible);

        // 5. Jungle conceals the same way; marsh does not (sight-2 open).
        let obs_j = BattalionUnit::new(
            6,
            "O",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(7, 5),
        );
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[obs_j], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(7, 7), 0), VisibilityState::Hidden);
        let obs_m = BattalionUnit::new(
            7,
            "O",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(3, 1),
        );
        let mut fow = FogOfWar::new(12, 12, 3, 3);
        fow.update(&g, &[obs_m], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(3, 3), 0), VisibilityState::Visible);
    }

    /// §6.1 sound ranging: a mark is heard this turn (age 0), stays live
    /// through `sound_mark_turns`, and is forgotten the turn after — a
    /// fresh shot from the same hex REFRESHES it instead of stacking.
    #[test]
    fn firing_marks_age_out_and_refresh() {
        let mut fow = FogOfWar::new(8, 8, 3, 3);
        let h = HexCoord::new(2, 2);
        assert_eq!(
            fow.firing_marker(h, 0),
            None,
            "a silent hex carries no mark"
        );

        fow.mark_firing(h, 10);
        assert_eq!(fow.firing_marker(h, 10), Some(0));
        assert_eq!(fow.firing_marker(h, 13), Some(3), "last live turn");
        assert_eq!(fow.firing_marker(h, 14), None, "forgotten the turn after");

        // Re-fire on a live mark: the clock restarts, ages never stack.
        fow.mark_firing(h, 12);
        assert_eq!(fow.firing_marker(h, 12), Some(0));
        assert_eq!(fow.firing_marker(h, 15), Some(3), "refreshed, not expired");
        assert_eq!(fow.firing_marker(h, 16), None);
    }

    /// A mark is HEX intel, not an observation: it never touches the
    /// visibility ladder, off-map hexes are inert, and a turn before the
    /// shot reports nothing.
    #[test]
    fn firing_marks_are_hex_intel_only() {
        let mut fow = FogOfWar::new(6, 6, 3, 3);
        fow.mark_firing(HexCoord::new(6, 2), 4);
        assert_eq!(fow.firing_marker(HexCoord::new(6, 2), 4), None, "past q");
        fow.mark_firing(HexCoord::new(-1, 0), 4);
        assert_eq!(
            fow.firing_marker(HexCoord::new(-1, 0), 4),
            None,
            "negative q"
        );

        let h = HexCoord::new(2, 2);
        fow.mark_firing(h, 4);
        assert_eq!(fow.state(h, 4), VisibilityState::Hidden, "hearing ≠ seeing");
        assert_eq!(fow.firing_marker(h, 3), None, "before the shot");
    }

    /// The whole-map iterator walks the same store the per-hex query reads:
    /// live marks only, in a stable row-major order.
    #[test]
    fn firing_marks_iterates_the_live_set() {
        let mut fow = FogOfWar::new(4, 3, 3, 2);
        fow.mark_firing(HexCoord::new(3, 0), 5);
        fow.mark_firing(HexCoord::new(1, 2), 6);
        fow.mark_firing(HexCoord::new(0, 1), 2); // stale by turn 6
        let live: Vec<(HexCoord, u32)> = fow.firing_marks(6).collect();
        assert_eq!(
            live,
            vec![(HexCoord::new(3, 0), 1), (HexCoord::new(1, 2), 0)],
            "row-major order, live marks only"
        );
        for (h, age) in live {
            assert_eq!(fow.firing_marker(h, 6), Some(age));
        }
        assert!(
            fow.firing_marks(9).next().is_none(),
            "the whole window closed"
        );
    }

    /// The sight sweep must leave the marks alone — a shot is heard whether
    /// or not anyone was watching the hex, and seeing it later neither
    /// clears nor refreshes the mark.
    #[test]
    fn sight_updates_leave_firing_marks_alone() {
        let g = HexGrid::new(8, 8, Terrain::Plains);
        let mut fow = FogOfWar::new(8, 8, 3, 3);
        let h = HexCoord::new(2, 2);
        fow.mark_firing(h, 1);
        fow.update(&g, &[], Side::Attacker, 3); // nobody watching
        assert_eq!(fow.firing_marker(h, 3), Some(2));
        let u = BattalionUnit::new(0, "R", UnitType::Recon, Side::Attacker, HexCoord::new(2, 3));
        fow.update(&g, &[u], Side::Attacker, 5);
        assert_eq!(fow.state(h, 5), VisibilityState::Visible);
        assert_eq!(fow.firing_marker(h, 5), None, "expires on its own clock");
    }
}
