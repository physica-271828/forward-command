//! Hex grid container (DESIGN §4). Row-major cell storage; the province
//! shape and the out-of-bounds ring are marked per-cell (`is_passable =
//! false` on water, `out_of_bounds = true` outside the province, §6.14).
//! Width was once capped at 32 columns for a long-gone u32 bitmap layout —
//! the cap is now 512 per axis (true-scale full-province maps; Sedan ≈
//! 55×118 at 1 hex = 1 km).

use crate::hex::{HexCoord, HexDirection};
use crate::terrain::{RiverBand, Terrain};

#[derive(Debug, Clone, Copy)]
pub struct GridCell {
    pub terrain: Terrain,
    /// Relative elevation level (0 = lowland); used by LoS (higher sees over lower).
    pub elevation: i32,
    /// River runs along one or more edges of this hex (§4.2 adjacency rivers).
    pub river_edges: u8,
    /// Width band of the river water under this hex — the widest stroke
    /// that painted it (Small is the default; meaningless when the cell
    /// is not a river hex). Pure geometry: every river hex shares the
    /// uniform §6.6 ford row, the band only decides how many hexes wide
    /// the water strip is.
    pub river_band: RiverBand,
    /// Paved road runs through this hex (§4.2 road network): movement pays
    /// the base terrain cost with the motor excess waived and no off-road
    /// surcharge. Villages, towns and factory districts are network nodes.
    pub road: bool,
    /// Railway runs through this hex — same movement rule as `road` (the
    /// graded bed helps any march; strategic rail connectivity is not
    /// modelled at the tactical scale, so a damaged line still counts).
    pub rail: bool,
    /// Bridge deck on a full-hex River: crossing pays a flat 1 km for every
    /// mobility class instead of the §6.6 ford row. Movement-only — no
    /// combat modifiers.
    pub bridge: bool,
    /// Bridge over an ADJACENCY river edge (bit i = HexDirection::ALL[i],
    /// same convention as `river_edges`): crossing that edge pays no
    /// `river_edge_surcharge_km`.
    pub bridge_edges: u8,
    /// Road actually CROSSES these hex edges (bit i = HexDirection::ALL[i],
    /// set on both cells of the shared edge along each painted road path).
    /// This is the renderer's source of truth for which links to draw —
    /// hex-adjacency of the `road` flag alone would invent links between
    /// touching-but-unrelated road hexes (cluster villages, pass-bys).
    /// Movement reads the hex-level `road` flag, not these bits.
    pub road_edges: u8,
    /// Railway crosses these hex edges — same convention as `road_edges`.
    pub rail_edges: u8,
    /// Factory district overlay (§4.2): industrial buildings stamped near
    /// the city. Village-grade cover (0.30) regardless of the base terrain,
    /// a road-network node, and light enough to not block LoS.
    pub factory: bool,
    /// False for water (sea/lake). Out-of-province LAND stays passable at
    /// its own terrain cost but is flagged `out_of_bounds` (§6.14).
    pub is_passable: bool,
    /// §6.14: outside the battle province (the shoreline margin ring,
    /// enclaves, foreign islands). A unit ending a full turn
    /// here accrues dwell; at `CombatParams::oob_leaving_turns` it leaves
    /// the battle (UnitState::LeftBattle). Pathfinding soft-avoids these
    /// hexes; routs prefer them as their exit.
    pub out_of_bounds: bool,
}

impl GridCell {
    pub fn new(terrain: Terrain) -> Self {
        GridCell {
            terrain,
            elevation: 0,
            river_edges: 0,
            river_band: RiverBand::Small,
            road: false,
            rail: false,
            bridge: false,
            bridge_edges: 0,
            road_edges: 0,
            rail_edges: 0,
            factory: false,
            // Derived, not hardcoded: `HexGrid::new(w, h, Water)` must
            // produce impassable open sea without every caller remembering
            // to flip the flag.
            is_passable: terrain.is_passable(),
            out_of_bounds: false,
        }
    }

    pub fn has_river(&self) -> bool {
        self.river_edges != 0
    }

    /// True when a surfaced route (paved road or railway) serves this hex —
    /// the §6.6 movement rule treats both identically.
    pub fn has_road(&self) -> bool {
        self.road || self.rail
    }

    /// Cover the occupant of this cell actually receives (§6.6): a factory
    /// district grants village-grade masonry (0.30) whatever the base
    /// terrain says; everything else defers to the terrain table.
    pub fn cover_percent(&self) -> f32 {
        if self.factory {
            0.30
        } else {
            self.terrain.cover_percent()
        }
    }
}

#[derive(Debug, Clone)]
pub struct HexGrid {
    pub width: usize,
    pub height: usize,
    cells: Vec<GridCell>,
    /// Which map edges the attacker crosses (§4.2 step 9, dynamic frontlines).
    pub attack_dirs: Vec<HexDirection>,
}

impl HexGrid {
    pub fn new(width: usize, height: usize, fill: Terrain) -> Self {
        assert!(
            width <= 512,
            "grid width capped at 512 columns"
        );
        assert!(
            height <= 512,
            "grid height capped at 512 rows"
        );
        HexGrid {
            width,
            height,
            cells: vec![GridCell::new(fill); width * height],
            attack_dirs: Vec::new(),
        }
    }

    pub fn in_bounds(&self, h: HexCoord) -> bool {
        h.q >= 0 && h.r >= 0 && (h.q as usize) < self.width && (h.r as usize) < self.height
    }

    pub fn cell(&self, h: HexCoord) -> Option<&GridCell> {
        if self.in_bounds(h) {
            Some(&self.cells[h.r as usize * self.width + h.q as usize])
        } else {
            None
        }
    }

    pub fn cell_mut(&mut self, h: HexCoord) -> Option<&mut GridCell> {
        if self.in_bounds(h) {
            Some(&mut self.cells[h.r as usize * self.width + h.q as usize])
        } else {
            None
        }
    }

    /// Stamp `t` on one hex and mirror the terrain's own side effects:
    /// the cell's elevation level is RESET to the terrain's default
    /// (Hills 1, Mountain 2, Marsh/River/Water −1, everything else 0), and
    /// passability follows the terrain table. Callers that stamped a
    /// bespoke elevation first (noise relief, painted ridgelines) must
    /// re-stamp it after the terrain swap.
    pub fn set_terrain(&mut self, h: HexCoord, t: Terrain) {
        if let Some(c) = self.cell_mut(h) {
            c.terrain = t;
            c.elevation = match t {
                Terrain::Hills => 1,
                Terrain::Mountain => 2,
                Terrain::Marsh | Terrain::River | Terrain::Water => -1,
                _ => 0,
            };
            // Mirror the terrain's own passability (terrain.rs doc: "the
            // grid cell flag mirrors") — painting Water must close the hex,
            // no caller-side flip required.
            c.is_passable = t.is_passable();
        }
    }

    /// In-bounds, passable neighbors.
    pub fn passable_neighbors(&self, h: HexCoord) -> Vec<HexCoord> {
        h.neighbors()
            .into_iter()
            .filter(|n| self.cell(*n).map(|c| c.is_passable).unwrap_or(false))
            .collect()
    }

    pub fn iter_coords(&self) -> impl Iterator<Item = HexCoord> + '_ {
        (0..self.height)
            .flat_map(move |r| (0..self.width).map(move |q| HexCoord::new(q as i32, r as i32)))
    }
}

/// Minimum hex distance enforced between the attacker and defender
/// deployment zones. 3 = two hexes of no-man's land between the
/// zone borders, so the battle never starts with the armies in contact.
pub const MIN_ZONE_DISTANCE: i32 = 3;

/// Retains only the `hexes` lying at least `min_dist` away from every
/// `anchors` hex. Used to trim the defender deployment zone back from the
/// attacker zone (§4.2 step 9). If the filter would empty the list (tiny
/// maps), the single hex farthest from the anchors is kept instead.
pub fn filter_min_distance(
    hexes: Vec<HexCoord>,
    anchors: &[HexCoord],
    min_dist: i32,
) -> Vec<HexCoord> {
    let min_to_anchors = |h: HexCoord| {
        anchors
            .iter()
            .map(|a| h.distance(*a))
            .min()
            .unwrap_or(i32::MAX)
    };
    let kept: Vec<HexCoord> = hexes
        .iter()
        .copied()
        .filter(|&h| min_to_anchors(h) >= min_dist)
        .collect();
    if !kept.is_empty() || hexes.is_empty() {
        return kept;
    }
    // Fallback for tiny maps: keep the farthest hex.
    let farthest = *hexes.iter().max_by_key(|&&h| min_to_anchors(h)).unwrap();
    vec![farthest]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_and_cells() {
        let mut g = HexGrid::new(10, 8, Terrain::Plains);
        assert!(g.in_bounds(HexCoord::new(9, 7)));
        assert!(!g.in_bounds(HexCoord::new(10, 7)));
        assert!(!g.in_bounds(HexCoord::new(-1, 0)));
        g.set_terrain(HexCoord::new(3, 3), Terrain::Mountain);
        assert_eq!(
            g.cell(HexCoord::new(3, 3)).unwrap().terrain,
            Terrain::Mountain
        );
        assert_eq!(g.cell(HexCoord::new(3, 3)).unwrap().elevation, 2);
    }

    #[test]
    fn corner_neighbors_clipped() {
        let g = HexGrid::new(5, 5, Terrain::Plains);
        let n = g.passable_neighbors(HexCoord::new(0, 0));
        assert!(n.len() >= 2 && n.len() < 6);
        assert!(n.iter().all(|h| g.in_bounds(*h)));
    }

    #[test]
    fn water_is_impassable_by_construction_and_by_painting() {
        // is_passable is DERIVED from the terrain: a water-filled grid is
        // open sea with no passable hex, and painting Water onto land
        // closes the hex (no caller-side flag flip required).
        let mut g = HexGrid::new(4, 4, Terrain::Water);
        assert!(g.iter_coords().all(|h| !g.cell(h).unwrap().is_passable));
        assert!(g.passable_neighbors(HexCoord::new(1, 1)).is_empty());
        g.set_terrain(HexCoord::new(2, 2), Terrain::Plains);
        assert!(g.cell(HexCoord::new(2, 2)).unwrap().is_passable);
        g.set_terrain(HexCoord::new(2, 2), Terrain::Water);
        assert!(!g.cell(HexCoord::new(2, 2)).unwrap().is_passable);
    }

    #[test]
    fn filter_min_distance_trims_and_falls_back() {
        let anchors = vec![HexCoord::new(0, 0)];
        let hexes = vec![
            HexCoord::new(1, 0), // dist 1 — trimmed
            HexCoord::new(2, 0), // dist 2 — trimmed at min_dist 3
            HexCoord::new(3, 0), // dist 3 — kept
        ];
        let kept = filter_min_distance(hexes, &anchors, 3);
        assert_eq!(kept, vec![HexCoord::new(3, 0)]);
        // Tiny-map fallback: everything too close → keep the farthest hex.
        let close = vec![HexCoord::new(1, 0), HexCoord::new(0, 2)];
        let kept = filter_min_distance(close, &anchors, 3);
        assert_eq!(kept, vec![HexCoord::new(0, 2)]);
    }

    #[test]
    fn factory_overlay_grants_village_grade_cover() {
        // §4.2 factory district: village-grade masonry (0.30) whatever the
        // base terrain says — over Urban that is a deliberate downgrade
        // (light industrial halls, not dense masonry).
        let mut g = HexGrid::new(4, 4, Terrain::Plains);
        g.set_terrain(HexCoord::new(1, 1), Terrain::Urban);
        assert_eq!(g.cell(HexCoord::new(1, 1)).unwrap().cover_percent(), 0.50);
        g.cell_mut(HexCoord::new(1, 1)).unwrap().factory = true;
        assert_eq!(g.cell(HexCoord::new(1, 1)).unwrap().cover_percent(), 0.30);
        // Road/rail flags do not touch cover.
        g.cell_mut(HexCoord::new(2, 2)).unwrap().road = true;
        assert_eq!(g.cell(HexCoord::new(2, 2)).unwrap().cover_percent(), 0.0);
        assert!(g.cell(HexCoord::new(2, 2)).unwrap().has_road());
    }
}
