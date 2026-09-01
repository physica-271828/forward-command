//! Fog of war — per-side visibility with stale intel decay (§4/§6.1 sight).

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
}

impl FogOfWar {
    pub fn new(width: usize, height: usize, decay_turns: u32) -> Self {
        FogOfWar {
            width,
            height,
            last_seen: vec![-1; width * height],
            visible: vec![false; width * height],
            decay_turns,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Terrain;
    use crate::unit::UnitType;

    #[test]
    fn sees_then_stale_then_hidden() {
        let g = HexGrid::new(10, 10, Terrain::Plains);
        let mut fow = FogOfWar::new(10, 10, 3);
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
        let mut fow = FogOfWar::new(12, 12, 3);
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
        let mut fow = FogOfWar::new(12, 12, 3);
        fow.update(&g, &[obs2], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Hidden);
        let obs1 = BattalionUnit::new(
            2,
            "O",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(5, 4),
        );
        let mut fow = FogOfWar::new(12, 12, 3);
        fow.update(&g, &[obs1], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Visible);

        // 3. Recon (sight 4): sees into forest at distance 3, not 4.
        let rec3 = BattalionUnit::new(3, "R", UnitType::Recon, Side::Attacker, HexCoord::new(5, 2));
        let mut fow = FogOfWar::new(12, 12, 3);
        fow.update(&g, &[rec3], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(5, 5), 0), VisibilityState::Visible);
        let rec4 = BattalionUnit::new(4, "R", UnitType::Recon, Side::Attacker, HexCoord::new(5, 1));
        let mut fow = FogOfWar::new(12, 12, 3);
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
        let mut fow = FogOfWar::new(12, 12, 3);
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
        let mut fow = FogOfWar::new(12, 12, 3);
        fow.update(&g, &[obs_j], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(7, 7), 0), VisibilityState::Hidden);
        let obs_m = BattalionUnit::new(
            7,
            "O",
            UnitType::Infantry,
            Side::Attacker,
            HexCoord::new(3, 1),
        );
        let mut fow = FogOfWar::new(12, 12, 3);
        fow.update(&g, &[obs_m], Side::Attacker, 0);
        assert_eq!(fow.state(HexCoord::new(3, 3), 0), VisibilityState::Visible);
    }
}
