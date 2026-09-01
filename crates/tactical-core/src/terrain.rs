//! Terrain table — DESIGN.md §6.6 (move cost / sight / cover) plus rendering
//! hints (height & base color) used by the 3D renderer. Table v3.3: the
//! uniform attack/defense modifier columns are retired — cover is the
//! single uniform combat layer, and per-battalion vanilla terrain
//! adjusters (`TerrainAdjusters`, unit.rs) carry the unit-class identity.
//! `River` exists both as an edge feature (§4.2) and, when generated as a
//! full hex, behaves per the §6.6 row.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Terrain {
    Plains,
    Forest,
    Hills,
    Mountain,
    Urban,
    Jungle,
    Marsh,
    Desert,
    River,
    Clearing,
    /// Open water (sea/lake) — impassable, no combat rules; map border and
    /// coastal flavour from the province bitmap.
    Water,
    /// Rural settlement — seeded scatter on real provinces: lighter than
    /// Urban (no LoS block, solid cover: 0.30).
    Village,
}

impl Terrain {
    pub const ALL: [Terrain; 12] = [
        Terrain::Plains,
        Terrain::Forest,
        Terrain::Hills,
        Terrain::Mountain,
        Terrain::Urban,
        Terrain::Jungle,
        Terrain::Marsh,
        Terrain::Desert,
        Terrain::River,
        Terrain::Clearing,
        Terrain::Water,
        Terrain::Village,
    ];

    /// Effective kilometres to move one hex for **leg** units (§6.2/§6.6):
    /// 1 hex = 1 km baseline, rougher terrain counts extra. River hexes are
    /// fordable at 3× (revised ford rule: crossable and may be held, but
    /// slow enough that pathfinding strongly prefers dry routes).
    pub fn movement_cost(self) -> f32 {
        match self {
            Terrain::Plains => 1.0,
            Terrain::Forest => 1.5,
            Terrain::Hills => 1.5,
            Terrain::Mountain => 3.0,
            Terrain::Urban => 1.2,
            Terrain::Jungle => 2.0,
            Terrain::Marsh => 2.5,
            Terrain::Desert => 1.2,
            Terrain::River => 3.0,
            Terrain::Clearing => 1.0,
            // Impassable anyway (cell flag); a huge cost keeps A* honest if
            // it ever evaluates a water step.
            Terrain::Water => 99.0,
            Terrain::Village => 1.0,
        }
    }

    /// Effective kilometres for a mobility class (§6.6): motor units
    /// (vehicles, towed wagons) suffer a larger off-road debuff than
    /// legs — the excess over the 1 km baseline is multiplied.
    pub fn movement_cost_for(self, class: crate::unit::MobilityClass) -> f32 {
        let base = self.movement_cost();
        match class {
            crate::unit::MobilityClass::Leg => base,
            crate::unit::MobilityClass::Motor => {
                1.0 + (base - 1.0) * crate::pathfinding::MOTOR_TERRAIN_MULT
            }
        }
    }

    /// Index of this terrain in the fixed `Terrain::ALL` order — the slot
    /// used by per-battalion `TerrainAdjusters` arrays (unit.rs).
    pub const fn idx(self) -> usize {
        match self {
            Terrain::Plains => 0,
            Terrain::Forest => 1,
            Terrain::Hills => 2,
            Terrain::Mountain => 3,
            Terrain::Urban => 4,
            Terrain::Jungle => 5,
            Terrain::Marsh => 6,
            Terrain::Desert => 7,
            Terrain::River => 8,
            Terrain::Clearing => 9,
            Terrain::Water => 10,
            Terrain::Village => 11,
        }
    }

    /// The HOI4 battalion-file terrain key this variant answers to
    /// (§6.6 v3.3). `amphibious`/`fort` and unknown keys map to None —
    /// those vanilla keys govern landing craft and fort assaults,
    /// which have no counterpart hex here (rivers exist as full hexes, and
    /// the crossing-feature rule is not implemented).
    pub fn from_hoi4_key(key: &str) -> Option<Terrain> {
        Some(match key {
            "plains" => Terrain::Plains,
            "forest" => Terrain::Forest,
            "hills" => Terrain::Hills,
            "mountain" => Terrain::Mountain,
            "urban" => Terrain::Urban,
            "jungle" => Terrain::Jungle,
            "marsh" => Terrain::Marsh,
            "desert" => Terrain::Desert,
            "river" => Terrain::River,
            _ => return None,
        })
    }

    /// Base sight range for a unit standing here (§6.6). Forest/Jungle do
    /// not blind their OCCUPANT (plains-level 2) — they conceal them from
    /// observers instead (`conceals`); Marsh opens to 2 (open wetland).
    /// Urban stays 1 (buildings) on top of its hard LoS block.
    pub fn sight_range(self) -> i32 {
        match self {
            Terrain::Plains => 2,
            Terrain::Forest => 2,
            Terrain::Hills => 3,
            Terrain::Mountain => 4,
            Terrain::Urban => 1,
            Terrain::Jungle => 2,
            Terrain::Marsh => 2,
            Terrain::Desert => 3,
            Terrain::River => 2,
            Terrain::Clearing => 2,
            Terrain::Water => 3,
            Terrain::Village => 2,
        }
    }

    /// Whether this terrain CONCEALS its occupant: an observer's
    /// sight counts −1 against a target hex on concealing terrain (fog.rs),
    /// floored at adjacency — the treeline next to you is always spotted.
    pub fn conceals(self) -> bool {
        matches!(self, Terrain::Forest | Terrain::Jungle)
    }

    /// Damage reduction fraction from cover (§6.6, table v3.3): cover is
    /// the ONLY uniform terrain combat layer — the global attack/defense
    /// modifier tables are retired (their values were linear duplicates of
    /// this channel; vanilla terrain has no defense bonus at all). Unit-
    /// class terrain identity comes from the per-battalion vanilla
    /// adjusters (`TerrainAdjusters`), not from this table. Negative cover
    /// = exposed ground (the occupant takes EXTRA damage). Per-terrain
    /// reasons:
    /// - Forest 0.15: trees conceal but splinter under shellfire.
    /// - Hills 0.20: slopes and folds; the melee elevation rule (§6.6) adds
    ///   the dynamic slope part on top.
    /// - Mountain 0.40: rock and relief — assault belongs to specialists
    ///   (the mountaineers' +0.35 attack adjuster offsets it).
    /// - Urban 0.50: dense masonry, the hardest terrain on the map; vehicles
    ///   are canalized (armor's −0.40 attack adjuster).
    /// - Jungle 0.20: dense vegetation, no hard cover.
    /// - Marsh 0.10: open wetland — mud hinders, little to hide behind.
    /// - Desert −0.10: exposed on open ground; fire superiority tells.
    /// - River −0.50: caught mid-ford in open water. Replaces the old ×2
    ///   ford special case (retired — one number per terrain).
    /// - Village 0.30: masonry farmsteads, the classic strongpoint in open
    ///   country (Eastern-Front Ortschaft) — above forest/hills, below dense
    ///   urban; no LoS block and no concealment, so observed artillery still
    ///   answers it. Capped here (not 0.35+): villages are seeded scatter on
    ///   real provinces — harder villages would bog every map.
    pub fn cover_percent(self) -> f32 {
        match self {
            Terrain::Plains => 0.0,
            Terrain::Forest => 0.15,
            Terrain::Hills => 0.20,
            Terrain::Mountain => 0.40,
            Terrain::Urban => 0.50,
            Terrain::Jungle => 0.20,
            Terrain::Marsh => 0.10,
            Terrain::Desert => -0.10,
            Terrain::River => -0.50,
            Terrain::Clearing => 0.0,
            Terrain::Water => 0.0,
            Terrain::Village => 0.30,
        }
    }

    /// Whether this terrain blocks line of sight through it. MOUNTAIN was
    /// removed — relief is carried by per-hex elevation noise instead
    /// (§4.2 step 8, los.rs). Urban stays: buildings block sight
    /// regardless of ground height (street fighting).
    pub fn blocks_los(self) -> bool {
        matches!(self, Terrain::Urban)
    }

    /// Vehicular movement is impossible (mountain ridges; §6.5 mentions
    /// impassable terrain). Kept conservative: nothing is fully impassable
    /// by default — river cells are passable shallows (costly, dangerous),
    /// river edges only add a crossing surcharge. Open Water is the
    /// exception (impassable; the grid cell flag mirrors).
    pub fn is_passable(self) -> bool {
        !matches!(self, Terrain::Water)
    }

    /// Whether units may be *deployed* onto this terrain: full-hex
    /// water (`River`) can be crossed and held during the battle (at 3× cost
    /// and exposed — negative cover, +50% damage taken) but never
    /// deployed on. Combine with the grid cell's `is_passable` flag.
    pub fn is_deployable(self) -> bool {
        !matches!(self, Terrain::River | Terrain::Water)
    }

    /// Prism height for the 3D renderer (world units).
    pub fn render_height(self) -> f32 {
        match self {
            Terrain::Plains => 0.30,
            Terrain::Forest => 0.35,
            Terrain::Hills => 0.70,
            Terrain::Mountain => 1.40,
            Terrain::Urban => 0.45,
            Terrain::Jungle => 0.40,
            Terrain::Marsh => 0.18,
            Terrain::Desert => 0.30,
            Terrain::River => 0.10,
            Terrain::Clearing => 0.28,
            Terrain::Water => 0.05,
            Terrain::Village => 0.34,
        }
    }

    /// World prism height for a per-hex ELEVATION level. The
    /// board renders every hex at its own elevation so the picture matches
    /// the LOS rule (a ridge the player sees IS a ridge that blocks sight).
    /// 0.30 + 0.35/level keeps the old terrain look: plains 0.30, hills
    /// 0.65 (was 0.70), mountain 1.00–1.70 across levels 2–4; lows (river,
    /// marsh, water = level −1) clamp to the 0.05 water bed.
    pub fn elevation_render_height(elev: i32) -> f32 {
        (0.30 + 0.35 * elev as f32).max(0.05)
    }

    /// Top-face base color (linear-ish RGB 0..1) for the 3D renderer.
    /// Pixel/blocky style: saturated, unambiguous per terrain.
    pub fn render_color(self) -> [f32; 3] {
        match self {
            Terrain::Plains => [0.42, 0.62, 0.30],
            Terrain::Forest => [0.16, 0.38, 0.16],
            // Yellow-green hills — distinct from both
            // Plains (greener) and Clearing (lighter/bluer).
            Terrain::Hills => [0.58, 0.60, 0.24],
            Terrain::Mountain => [0.45, 0.45, 0.48],
            Terrain::Urban => [0.60, 0.58, 0.55],
            Terrain::Jungle => [0.10, 0.45, 0.22],
            Terrain::Marsh => [0.28, 0.38, 0.25],
            Terrain::Desert => [0.85, 0.74, 0.45],
            Terrain::River => [0.20, 0.42, 0.72],
            Terrain::Clearing => [0.58, 0.62, 0.35],
            // Deep open-water blue — visibly darker than a fordable River.
            Terrain::Water => [0.09, 0.22, 0.52],
            // Warm tan — settlement roofs, lighter than Urban grey.
            Terrain::Village => [0.66, 0.55, 0.38],
        }
    }

    /// Elevation-banded top-face color: Mountain reads
    /// its per-hex elevation as a DISCRETE contour-map band (5 hard steps,
    /// matching the blocky per-hex prisms of §4.2 step 8b) — dark valley
    /// floors up to near-white peaks, so the color IS the height. Every
    /// other terrain keeps its flat `render_color` (Hills stay the flat
    /// yellow-green — only mountains band).
    pub fn banded_color(self, elevation: i32) -> [f32; 3] {
        if self != Terrain::Mountain {
            return self.render_color();
        }
        const MOUNTAIN_BANDS: [[f32; 3]; 5] = [
            [0.30, 0.32, 0.22], // L0 valley floor — dark shaded green-brown
            [0.42, 0.40, 0.30], // L1 lower slopes
            [0.45, 0.45, 0.48], // L2 base grey (the old flat mountain color)
            [0.58, 0.58, 0.62], // L3 high ridge
            [0.78, 0.78, 0.82], // L4 peak — near-white
        ];
        MOUNTAIN_BANDS[(elevation.clamp(0, 4)) as usize]
    }

    pub fn name(self) -> &'static str {
        match self {
            Terrain::Plains => "Plains",
            Terrain::Forest => "Forest",
            Terrain::Hills => "Hills",
            Terrain::Mountain => "Mountain",
            Terrain::Urban => "Urban",
            Terrain::Jungle => "Jungle",
            Terrain::Marsh => "Marsh",
            Terrain::Desert => "Desert",
            Terrain::River => "River",
            Terrain::Clearing => "Clearing",
            Terrain::Water => "Water",
            Terrain::Village => "Village",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_table_values() {
        // Spot-check the §6.6 table verbatim. v3.3:
        // the attack/defense modifier columns are retired — cover is the
        // only uniform combat layer (per-terrain values).
        assert_eq!(Terrain::Mountain.movement_cost(), 3.0);
        assert_eq!(Terrain::Plains.cover_percent(), 0.0);
        assert_eq!(Terrain::Forest.cover_percent(), 0.15);
        assert_eq!(Terrain::Hills.cover_percent(), 0.20);
        assert_eq!(Terrain::Mountain.cover_percent(), 0.40);
        assert_eq!(Terrain::Urban.cover_percent(), 0.50);
        assert_eq!(Terrain::Jungle.cover_percent(), 0.20);
        assert_eq!(Terrain::Marsh.cover_percent(), 0.10);
        assert_eq!(Terrain::Desert.cover_percent(), -0.10);
        assert_eq!(Terrain::River.cover_percent(), -0.50);
        assert_eq!(Terrain::Village.cover_percent(), 0.30);
        assert_eq!(Terrain::Clearing.cover_percent(), 0.0);
        assert_eq!(Terrain::Marsh.movement_cost(), 2.5);
        assert_eq!(Terrain::Hills.sight_range(), 3);
        // Forest/jungle no longer blind
        // the occupant (plains-level 2) — they conceal instead; marsh opens
        // to 2 (open wetland); urban keeps sight 1 (buildings).
        assert_eq!(Terrain::Forest.sight_range(), 2);
        assert_eq!(Terrain::Jungle.sight_range(), 2);
        assert_eq!(Terrain::Marsh.sight_range(), 2);
        assert_eq!(Terrain::Urban.sight_range(), 1);
        assert!(Terrain::Forest.conceals());
        assert!(Terrain::Jungle.conceals());
        assert!(!Terrain::Marsh.conceals());
        assert!(!Terrain::Plains.conceals());
        // River row: 3× ford, exposed (negative cover carries the old
        // ×2 ford vulnerability).
        assert_eq!(Terrain::River.movement_cost(), 3.0);
    }

    #[test]
    fn terrain_idx_matches_all_order() {
        for (i, t) in Terrain::ALL.into_iter().enumerate() {
            assert_eq!(t.idx(), i, "{t:?}");
        }
    }

    #[test]
    fn hoi4_key_mapping() {
        assert_eq!(Terrain::from_hoi4_key("forest"), Some(Terrain::Forest));
        assert_eq!(Terrain::from_hoi4_key("hills"), Some(Terrain::Hills));
        assert_eq!(Terrain::from_hoi4_key("mountain"), Some(Terrain::Mountain));
        assert_eq!(Terrain::from_hoi4_key("urban"), Some(Terrain::Urban));
        assert_eq!(Terrain::from_hoi4_key("river"), Some(Terrain::River));
        assert_eq!(Terrain::from_hoi4_key("desert"), Some(Terrain::Desert));
        // Landing-craft and fort-assault keys have no counterpart hex.
        assert_eq!(Terrain::from_hoi4_key("amphibious"), None);
        assert_eq!(Terrain::from_hoi4_key("fort"), None);
        assert_eq!(Terrain::from_hoi4_key("nonsense"), None);
    }

    #[test]
    fn los_blockers() {
        // Mountain relief is carried by per-hex elevation noise;
        // urban keeps its hard block (buildings).
        assert!(!Terrain::Mountain.blocks_los());
        assert!(Terrain::Urban.blocks_los());
        assert!(!Terrain::Forest.blocks_los());
    }

    #[test]
    fn elevation_height_mapping() {
        assert_eq!(Terrain::elevation_render_height(0), 0.30);
        assert_eq!(Terrain::elevation_render_height(2), 1.00);
        assert_eq!(Terrain::elevation_render_height(4), 1.70);
        // Lowlands clamp to the water bed instead of going underground.
        assert_eq!(Terrain::elevation_render_height(-1), 0.05);
        assert_eq!(Terrain::elevation_render_height(-5), 0.05);
        // Monotone in elevation.
        assert!(Terrain::elevation_render_height(1) > Terrain::elevation_render_height(0));
    }

    #[test]
    fn mountain_banded_colors() {
        // Five discrete bands, pairwise distinct, monotone
        // lightness from valley floor to peak.
        let band = |e| Terrain::Mountain.banded_color(e);
        let lum = |c: [f32; 3]| c[0] + c[1] + c[2];
        for e in 0..4 {
            assert!(lum(band(e + 1)) > lum(band(e)), "band {e} → {}", e + 1);
            for e2 in (e + 1)..=4 {
                assert_ne!(band(e), band(e2));
            }
        }
        // Out-of-range elevations clamp to the table ends.
        assert_eq!(band(-1), band(0));
        assert_eq!(band(9), band(4));
        // L2 keeps the legacy flat mountain grey.
        assert_eq!(band(2), Terrain::Mountain.render_color());
        // Non-mountain terrain ignores elevation entirely.
        for t in [
            Terrain::Plains,
            Terrain::Hills,
            Terrain::Forest,
            Terrain::Water,
        ] {
            assert_eq!(t.banded_color(0), t.render_color());
            assert_eq!(t.banded_color(4), t.render_color());
        }
        // Hills ride the yellow-green.
        assert_eq!(Terrain::Hills.render_color(), [0.58, 0.60, 0.24]);
    }
}
