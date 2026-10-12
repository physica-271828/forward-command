//! tactical-map — HOI4 province → tactical hex grid generation (DESIGN.md §4).
//!
//! Pipeline (§4.1 inputs, §4.2 algorithm):
//! 1. [`ProvinceMap::load_bmp`] parses `map/provinces.bmp` (24-bit BMP,
//!    pixel color → province id).
//! 2. [`load_definition_csv`] parses `map/definition.csv` (province id →
//!    terrain / RGB / coastal / continent).
//! 3. [`load_adjacencies_csv`] parses `map/adjacencies.csv` (river crossings).
//! 4. [`MapGenerator::generate`] builds the 1 km hex grid for one province:
//!    bounding box (+ [`SHORE_MARGIN_PX`] shoreline ring) → grid size (§4.1
//!    uniform bitmap scale, §4.3 — NO latitude correction; the HOI4 bitmap
//!    is the standard) → occupancy down-sample → seeded terrain variation →
//!    coastal/border water → rivers.bmp overlay → VP urban → river edges →
//!    factory districts → road network → railways → deployment zones
//!    (§4.2 step 9). Full province, up to 512×512.

mod bmp;
mod csv;
mod generator;
mod localisation;
mod rails;
mod states;
mod unitstacks;

pub use bmp::{IndexMap, ProvinceMap};
pub use csv::{
    load_adjacencies_csv, load_definition_csv, Adjacency, AdjacencyKind, ProvinceInfo, ProvinceKind,
};
pub use generator::{DeploymentZones, MapGenerator, TacticalMap};
pub use localisation::{load_vp_names, vp_names_path};
pub use rails::{load_railways, RailwayLine};
pub use states::{
    load_province_to_state, load_state_categories, load_state_industry, load_state_infrastructure,
    load_victory_points, states_dir_of,
};
pub use unitstacks::load_unit_stacks;

use std::fmt;
use std::path::PathBuf;

/// Pixel→hex-column density on the map X axis, derived from
/// `MAP_SCALE_PIXEL_TO_KM` (DESIGN.md §4.1, defines.lua).
/// Sanity check: 5632 px × 7.114 ≈ 40,066 km ≈ Earth equator circumference.
/// §4.3: the bitmap is taken as-is — this factor is applied verbatim, with
/// NO cos(latitude) correction.
pub const KM_PER_PIXEL_X: f32 = 7.114;

/// Pixel→hex-row density on the map Y axis. NOT a geographic scale: the
/// pointy-top hex layout (`HexCoord::to_world`) spaces rows 1.5·size apart
/// but columns √3·size apart, so a pixel row needs 2/√3 ≈ 1.1547 more hex
/// rows than a pixel column needs hex columns for the rendered board to be
/// isotropic with the bitmap. = 7.114 × 2/√3 ≈ 8.2145.
/// (Replaces the pole-to-pole Mercator-era 9.768 km/px — that value
/// overstated N-S extent by ~19% even before the erroneous latitude
/// correction.)
pub const KM_PER_PIXEL_Y: f32 = 8.2145;

/// Tactical hex scale in km (§12 `hex_scale_km` default).
pub const HEX_SCALE_KM: f32 = 1.0;

/// Grid caps for full-province maps — 1 hex = 1 km. 512×512 covers 98% of
/// the 10,028 battleable (non-impassable) land provinces at bitmap scale;
/// larger mega-provinces (arctic/desert) are still squeezed into the cap
/// (documented fallback). Sedan ≈ 114×132 for reference.
pub const MAX_GRID_WIDTH: usize = 512;
pub const MAX_GRID_HEIGHT: usize = 512;

/// §4.2: a province producing fewer in-province hexes than this is rejected
/// as too small for a tactical battle.
pub const MIN_PROVINCE_HEXES: usize = 20;

/// §4.2 step 9: attacker deployment strips run this many hexes deep.
pub const DEPLOY_STRIP_DEPTH: usize = 3;

/// Multi-province stitching: depth in PIXELS of the attacker origin
/// province's staging strip folded into the map (~14–20 km at true scale —
/// plenty of room for 30–40 battalions).
pub const ORIGIN_STRIP_PX: i32 = 2;

/// Shoreline margin: the province bbox (plus any staging strip) is expanded
/// by this many PIXELS on every side before grid sizing, so the sea/lake a
/// coastal province borders — and a ring of neighbouring land — is sampled
/// into the map instead of the grid hard-cutting at the province edge.
/// 1 px ≈ 7 hex columns on x / ≈8 rows on y. Margin cells stay
/// out-of-province (§6.14 out_of_bounds): impassable water
/// (`Terrain::Water`) or PASSABLE neighbour-terrain backdrop.
pub const SHORE_MARGIN_PX: u32 = 1;

/// Errors from map loading and grid generation. Every loader returns a
/// descriptive error instead of panicking on malformed input.
#[derive(Debug)]
pub enum MapError {
    /// A required file could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `provinces.bmp` is not a valid 24-bit uncompressed BMP.
    InvalidBmp(String),
    /// A CSV file contained no usable records.
    InvalidCsv { path: PathBuf, reason: String },
    /// Province id is unknown (absent from definition.csv) or has no pixels
    /// in `provinces.bmp`.
    ProvinceNotFound(u32),
    /// Province produced fewer than [`MIN_PROVINCE_HEXES`] in-province hexes.
    ProvinceTooSmall { id: u32, hexes: usize },
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::Io { path, source } => {
                write!(f, "failed to read '{}': {}", path.display(), source)
            }
            MapError::InvalidBmp(msg) => write!(f, "invalid provinces.bmp: {msg}"),
            MapError::InvalidCsv { path, reason } => {
                write!(f, "invalid CSV '{}': {}", path.display(), reason)
            }
            MapError::ProvinceNotFound(id) => write!(
                f,
                "province {id} not found (absent from definition.csv or no pixels in provinces.bmp)"
            ),
            MapError::ProvinceTooSmall { id, hexes } => write!(
                f,
                "province {id} too small for a tactical map: {hexes} in-province hexes (minimum {MIN_PROVINCE_HEXES})"
            ),
        }
    }
}

impl std::error::Error for MapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MapError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, MapError>;

/// Best-effort HOI4 install detection, for callers that were not given an
/// explicit path. Checks common Steam locations and verifies that
/// `map\provinces.bmp` actually exists inside. All loaders in this crate
/// take explicit paths and never call this implicitly.
pub fn detect_hoi4_dir() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &[
        r"D:\Steam\steamapps\common\Hearts of Iron IV",
        r"C:\Program Files (x86)\Steam\steamapps\common\Hearts of Iron IV",
        r"C:\Program Files\Steam\steamapps\common\Hearts of Iron IV",
        r"E:\Steam\steamapps\common\Hearts of Iron IV",
        r"C:\Steam\steamapps\common\Hearts of Iron IV",
    ];
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|dir| dir.join("map").join("provinces.bmp").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_hoi4_dir_is_consistent() {
        // Whatever the machine, a detected dir must really contain the map file.
        if let Some(dir) = detect_hoi4_dir() {
            assert!(dir.join("map").join("provinces.bmp").is_file());
        }
    }

    /// Smoke test against a real HOI4 installation; ignored by default since
    /// CI/test machines may not have the game installed.
    #[test]
    #[ignore = "requires a local Hearts of Iron IV installation"]
    fn real_hoi4_map_smoke() {
        let dir = detect_hoi4_dir().expect("HOI4 install not found");
        let map = ProvinceMap::load_bmp(&dir.join("map").join("provinces.bmp")).unwrap();
        let defs = load_definition_csv(&dir.join("map").join("definition.csv")).unwrap();
        let adjs = load_adjacencies_csv(&dir.join("map").join("adjacencies.csv")).unwrap();
        assert!(map.width > 1000 && map.height > 500);
        assert!(defs.len() > 10_000, "expected 13k+ provinces");
        let mut gen = MapGenerator::new(map, defs, adjs);
        // Full overlay wiring (rivers.bmp + history/states VPs).
        gen.set_rivers(IndexMap::load_indexed_bmp(&dir.join("map").join("rivers.bmp")).unwrap());
        gen.set_victory_points(load_victory_points(&states_dir_of(&dir)).unwrap());
        gen.set_province_to_state(load_province_to_state(&states_dir_of(&dir)).unwrap());
        gen.set_state_categories(load_state_categories(&states_dir_of(&dir)).unwrap());
        gen.set_state_industry(load_state_industry(&states_dir_of(&dir)).unwrap());
        gen.set_state_infrastructure(load_state_infrastructure(&states_dir_of(&dir)).unwrap());
        gen.set_railways(load_railways(&dir.join("map").join("railways.txt")).unwrap());
        // unitstacks index-0 positions anchor VP cities.
        gen.set_unit_stacks(load_unit_stacks(&dir.join("map").join("unitstacks.txt")));
        // VP display names for the floating city label.
        gen.set_vp_names(load_vp_names(
            &dir.join("localisation")
                .join("english")
                .join("victory_points_l_english.yml"),
        ));
        // 3560 = Sedan (state 18 Champagne, forest). NB: pre-1.14 docs said
        // 6334 — province ids shifted between HOI4 versions; 6334 is now in
        // West Prussia (Baltic coast). Verify ids against victory_points
        // localisation when updating.
        let tm = gen
            .generate(
                3560,
                &[
                    tactical_core::HexDirection::NW,
                    tactical_core::HexDirection::W,
                ],
            )
            .unwrap();
        eprintln!("[smoke] 3560 grid: {}x{}", tm.grid.width, tm.grid.height);
        assert!(tm.grid.width <= MAX_GRID_WIDTH && tm.grid.height <= MAX_GRID_HEIGHT);
        assert!(!tm.zones.defender.is_empty());
        // Full-map assertions: the Meuse must show as River hexes (SW border),
        // and Sedan's VP (level 1) places a 3-hex small city + villages.
        let mut river = 0usize;
        let mut urban = 0usize;
        let mut village = 0usize;
        let mut road = 0usize;
        let mut rail = 0usize;
        let mut factory = 0usize;
        for c in tm.grid.iter_coords() {
            let cell = tm.grid.cell(c).unwrap();
            match cell.terrain {
                tactical_core::Terrain::River => river += 1,
                tactical_core::Terrain::Urban => urban += 1,
                tactical_core::Terrain::Village => village += 1,
                _ => {}
            }
            if cell.road {
                road += 1;
            }
            if cell.rail {
                rail += 1;
            }
            if cell.factory {
                factory += 1;
            }
        }
        eprintln!("[smoke] 3560: river={river} urban={urban} village={village} road={road} rail={rail} factory={factory}");
        assert!(
            river >= 10,
            "expected the Meuse as river hexes, got {river}"
        );
        assert_eq!(urban, 3, "Sedan VP level 1 → 3-hex small city, got {urban}");
        assert!(village >= 10, "expected a village scatter, got {village}");
        // The road net links the villages/city and reaches the borders.
        assert!(road >= village, "expected a road network, got {road}");
        // Stitched maps: attack from W,NW → origin province(s)
        // folded in; attacker deploys on origin soil, defender in Sedan.
        let w = tm.grid.width;
        let count_pid = |pid: u32, cells: &[tactical_core::HexCoord]| {
            cells
                .iter()
                .filter(|c| tm.cell_province[c.r as usize * w + c.q as usize] == pid)
                .count()
        };
        eprintln!(
            "[smoke] origins={:?} attacker={} defender={}",
            tm.origin_provinces,
            tm.zones.attacker.len(),
            tm.zones.defender.len()
        );
        assert!(
            !tm.origin_provinces.is_empty(),
            "no origin province inferred"
        );
        assert!(!tm.zones.attacker.is_empty());
        for o in &tm.origin_provinces {
            let n = count_pid(*o, &tm.zones.attacker);
            eprintln!("[smoke] attacker hexes in origin {o}: {n}");
        }
        let foreign_attacker = tm
            .zones
            .attacker
            .iter()
            .filter(|c| {
                !tm.origin_provinces
                    .contains(&tm.cell_province[c.r as usize * w + c.q as usize])
            })
            .count();
        assert_eq!(
            foreign_attacker, 0,
            "attacker hexes outside origin provinces"
        );
        let defender_in_sedan = count_pid(3560, &tm.zones.defender);
        assert_eq!(
            defender_in_sedan,
            tm.zones.defender.len(),
            "defender outside Sedan"
        );

        // Urban province: 306 = Smolensk (definition.csv terrain "urban",
        // VP level 15). The countryside must follow the plains template —
        // the ONLY Urban hexes are the VP-stamped city (linear formula:
        // int((15 + 2) * 1.2) = 20).
        let tm2 = gen
            .generate(306, &[tactical_core::HexDirection::E])
            .unwrap();
        let urban2 = tm2
            .grid
            .iter_coords()
            .filter(|c| tm2.grid.cell(*c).map(|c| c.terrain) == Some(tactical_core::Terrain::Urban))
            .count();
        eprintln!("[smoke] 306: urban={urban2}");
        assert_eq!(
            urban2, 20,
            "Smolensk VP 15 urban → 20-hex city, got {urban2}"
        );
        // The city must sit at the unitstacks VP position, not the province
        // centroid. Smolensk's stack (3306, 1616 bottom-up) lies SW of the
        // bbox centre, so the city's mean hex must be SW of the in-province
        // centroid.
        let w2 = tm2.grid.width;
        let centroid = |f: &dyn Fn(u32) -> bool| {
            let (mut sx, mut sy, mut n) = (0f32, 0f32, 0u32);
            for c in tm2.grid.iter_coords() {
                let idx = c.r as usize * w2 + c.q as usize;
                if f(tm2.cell_province[idx]) {
                    sx += c.q as f32;
                    sy += c.r as f32;
                    n += 1;
                }
            }
            (sx / n as f32, sy / n as f32)
        };
        let (pcx, pcy) = centroid(&|pid| pid == 306);
        let (mut ux, mut uy, mut un) = (0f32, 0f32, 0u32);
        for c in tm2.grid.iter_coords() {
            if tm2.grid.cell(c).map(|c| c.terrain) == Some(tactical_core::Terrain::Urban) {
                ux += c.q as f32;
                uy += c.r as f32;
                un += 1;
            }
        }
        let (ucx, ucy) = (ux / un as f32, uy / un as f32);
        eprintln!("[smoke] 306: province centroid=({pcx:.1},{pcy:.1}) city=({ucx:.1},{ucy:.1})");
        assert!(
            (ucx - pcx).abs() + (ucy - pcy).abs() > 1.0,
            "city sits at the centroid — unitstacks anchor not applied?"
        );
        // The VP label carries the localised name + city anchor.
        let (name, _anchor) = tm2.vp_label.as_ref().expect("306 must have a VP label");
        assert_eq!(name, "Smolensk");
        assert_eq!(
            tm.vp_label.as_ref().map(|(n, _)| n.as_str()),
            Some("Sedan"),
            "Sedan VP label"
        );

        // Warsaw 1939: 3544 = Warsaw (state 10-Poland, urban,
        // VP level 25 → int((25+2)*1.2) = 32-hex city). Attack from W,SW —
        // the 8th Army's Łódź axis (W) and 4th Panzer Division's first
        // assault from the Rawa Mazowiecka direction (SW); battle_scan:
        // W=11492/SW=9400,467 plains, NW=6567 forest (Kampinos direction).
        let tm3 = gen
            .generate(
                3544,
                &[
                    tactical_core::HexDirection::W,
                    tactical_core::HexDirection::SW,
                ],
            )
            .unwrap();
        assert!(tm3.grid.width <= MAX_GRID_WIDTH && tm3.grid.height <= MAX_GRID_HEIGHT);
        assert!(!tm3.zones.defender.is_empty());
        let urban3 = tm3
            .grid
            .iter_coords()
            .filter(|c| tm3.grid.cell(*c).map(|c| c.terrain) == Some(tactical_core::Terrain::Urban))
            .count();
        eprintln!(
            "[smoke] 3544: urban={urban3} origins={:?} attacker={} defender={}",
            tm3.origin_provinces,
            tm3.zones.attacker.len(),
            tm3.zones.defender.len()
        );
        assert_eq!(urban3, 32, "Warsaw VP 25 urban → 32-hex city, got {urban3}");
        assert!(
            !tm3.origin_provinces.is_empty(),
            "no origin province inferred for Warsaw W,SW"
        );
        assert!(!tm3.zones.attacker.is_empty());
        let w3 = tm3.grid.width;
        let foreign3 = tm3
            .zones
            .attacker
            .iter()
            .filter(|c| {
                !tm3.origin_provinces
                    .contains(&tm3.cell_province[c.r as usize * w3 + c.q as usize])
            })
            .count();
        assert_eq!(foreign3, 0, "attacker hexes outside origin provinces");
        let warsaw_def = tm3
            .zones
            .defender
            .iter()
            .filter(|c| tm3.cell_province[c.r as usize * w3 + c.q as usize] == 3544)
            .count();
        assert_eq!(
            warsaw_def,
            tm3.zones.defender.len(),
            "defender outside Warsaw 3544"
        );
        // The defender zone is the WHOLE battle province, so the 32-hex
        // city must be fully inside it (Warsaw is a tiny 108px province;
        // the old central-third window slid off the city).
        let city_in_def = tm3
            .zones
            .defender
            .iter()
            .filter(|c| {
                tm3.grid.cell(**c).map(|c| c.terrain) == Some(tactical_core::Terrain::Urban)
            })
            .count();
        eprintln!("[smoke] 3544: urban-in-defender-zone={city_in_def}/{urban3}");
        assert_eq!(
            city_in_def, urban3,
            "defender zone must contain the whole city"
        );
        let (name3, _anchor) = tm3.vp_label.as_ref().expect("3544 must have a VP label");
        assert_eq!(name3, "Warsaw");
    }



    /// TEMP topology analyzer: dump road-edge graph cycles/junctions.
    #[test]
    #[ignore = "requires a local Hearts of Iron IV installation"]
    fn tmp_topology_report() {
        use tactical_core::{HexCoord, HexDirection, Terrain};
        let dir = detect_hoi4_dir().expect("HOI4 install not found");
        let map = ProvinceMap::load_bmp(&dir.join("map").join("provinces.bmp")).unwrap();
        let defs = load_definition_csv(&dir.join("map").join("definition.csv")).unwrap();
        let adjs = load_adjacencies_csv(&dir.join("map").join("adjacencies.csv")).unwrap();
        let mut gen = MapGenerator::new(map, defs, adjs);
        gen.set_rivers(IndexMap::load_indexed_bmp(&dir.join("map").join("rivers.bmp")).unwrap());
        gen.set_victory_points(load_victory_points(&states_dir_of(&dir)).unwrap());
        gen.set_province_to_state(load_province_to_state(&states_dir_of(&dir)).unwrap());
        gen.set_state_categories(load_state_categories(&states_dir_of(&dir)).unwrap());
        gen.set_state_industry(load_state_industry(&states_dir_of(&dir)).unwrap());
        gen.set_state_infrastructure(load_state_infrastructure(&states_dir_of(&dir)).unwrap());
        gen.set_railways(load_railways(&dir.join("map").join("railways.txt")).unwrap());
        gen.set_unit_stacks(load_unit_stacks(&dir.join("map").join("unitstacks.txt")));
        for (pid, dirs, name) in [
            (3544u32, vec![HexDirection::W, HexDirection::SW], "warsaw"),
            (3560, vec![HexDirection::E], "sedan"),
            (1198, vec![HexDirection::E], "desert"),
            (3914, vec![HexDirection::NW], "ioannina"),
        ] {
            let tm = gen.generate(pid, &dirs).unwrap();
            let g = &tm.grid;
            // Road edge graph over playable hexes.
            let linked = |a: HexCoord, b: HexCoord| -> bool {
                let Some(d) = a.direction_to(b) else { return false };
                let ab = g.cell(a).map(|c| c.road_edges & d.bit() != 0).unwrap_or(false);
                let ba = g.cell(b).map(|c| c.road_edges & d.opposite().bit() != 0).unwrap_or(false);
                ab || ba
            };
            let nodes: Vec<HexCoord> = g
                .iter_coords()
                .filter(|&h| !g.cell(h).unwrap().out_of_bounds && g.cell(h).unwrap().road_edges != 0)
                .collect();
            let deg = |h: HexCoord| {
                h.neighbors().into_iter().filter(|&n| linked(h, n)).count()
            };
            let mut tris = Vec::new();
            let mut quads = Vec::new();
            for (i, &a) in nodes.iter().enumerate() {
                let na: Vec<HexCoord> = a.neighbors().into_iter().filter(|&n| linked(a, n)).collect();
                let k = |h: HexCoord| (h.q, h.r);
                for &b in &na {
                    if k(b) <= k(a) { continue; }
                    let nb: Vec<HexCoord> = b.neighbors().into_iter().filter(|&n| linked(b, n)).collect();
                    // triangle: a-b, b-c, c-a
                    for &c in &nb {
                        if k(c) <= k(b) || c == a { continue; }
                        if linked(c, a) {
                            tris.push((a, b, c));
                        }
                    }
                    // quad: a-b-c-d-a with a smallest index
                    for &c in &nb {
                        if c == a { continue; }
                        for &d in &c.neighbors() {
                            if d == c || d == b || d == a { continue; }
                            if linked(c, d) && linked(d, a) && k(a) < k(b) && k(a) < k(c) && k(a) < k(d) {
                                let _ = i;
                                quads.push((a, b, c, d));
                            }
                        }
                    }
                }
            }
            // Short forks: junction (deg>=3) hex whose two branches rejoin within 4 steps.
            let junctions: Vec<HexCoord> = nodes.iter().copied().filter(|&h| deg(h) >= 3).collect();
            // Bridges on edge-exit runs: bridge hex whose road path does NOT continue on both sides.
            let mut edge_bridges = Vec::new();
            for &h in &nodes {
                let c = g.cell(h).unwrap();
                if !c.bridge { continue; }
                let in_links = h.neighbors().into_iter().filter(|&n| linked(h, n) && !g.cell(n).unwrap().out_of_bounds).count();
                let _ = in_links;
                // A bridge whose band exits the playable mask: any edge bit toward an out-of-bounds/off-grid hex.
                let exits = HexDirection::ALL.iter().any(|d| {
                    c.road_edges & d.bit() != 0 && {
                        let n = h.neighbor(*d);
                        g.cell(n).map(|n2| n2.out_of_bounds).unwrap_or(true)
                    }
                });
                if exits { edge_bridges.push(h); }
            }
            eprintln!("[topo] {name} pid={pid}: nodes={} junctions={} tris={} quads={} edge_bridges={:?}",
                nodes.len(), junctions.len(), tris.len(), quads.len(), edge_bridges);
            for t in &tris { eprintln!("[topo] {name} TRI ({},{})-({},{})-({},{})", (t.0).q,(t.0).r,(t.1).q,(t.1).r,(t.2).q,(t.2).r); }
            for qd in quads.iter().take(12) { eprintln!("[topo] {name} QUAD ({},{})-({},{})-({},{})-({},{})", (qd.0).q,(qd.0).r,(qd.1).q,(qd.1).r,(qd.2).q,(qd.2).r,(qd.3).q,(qd.3).r); }
            // RAIL triangles (same analysis on rail_edges).
            let rlinked = |a: HexCoord, b: HexCoord| -> bool {
                let Some(d) = a.direction_to(b) else { return false };
                let ab = g.cell(a).map(|c| c.rail_edges & d.bit() != 0).unwrap_or(false);
                let ba = g.cell(b).map(|c| c.rail_edges & d.opposite().bit() != 0).unwrap_or(false);
                ab || ba
            };
            let rnodes: Vec<HexCoord> = g
                .iter_coords()
                .filter(|&h| !g.cell(h).unwrap().out_of_bounds && g.cell(h).unwrap().rail_edges != 0)
                .collect();
            let mut rtris = 0usize;
            for &a in &rnodes {
                for &b in &a.neighbors() {
                    if !rlinked(a, b) || (b.q, b.r) <= (a.q, a.r) { continue; }
                    for &c in &b.neighbors() {
                        if (c.q, c.r) <= (b.q, b.r) || c == a { continue; }
                        if rlinked(b, c) && rlinked(c, a) { rtris += 1; }
                    }
                }
            }
            // Adjacent-but-UNLINKED road-flag pairs: bands pass within one
            // hex without an edge — visual merge suspects.
            let mut adjacent_unlinked = 0usize;
            for h in g.iter_coords() {
                let c = g.cell(h).unwrap();
                if c.out_of_bounds || !c.road { continue; }
                for n in h.neighbors() {
                    let Some(nc) = g.cell(n) else { continue };
                    if nc.out_of_bounds || !nc.road { continue; }
                    if (n.q, n.r) <= (h.q, h.r) { continue; }
                    if !linked(h, n) { adjacent_unlinked += 1; }
                }
            }
            eprintln!("[topo] {name}: rail_nodes={} rail_tris={} adjacent_unlinked_road_pairs={}",
                rnodes.len(), rtris, adjacent_unlinked);
            // Visual triangles: adjacent-unlinked pair (a,b) sharing a
            // common linked neighbour c (a-c and b-c both real links).
            let mut vtri = 0usize;
            for h in g.iter_coords() {
                let c0 = g.cell(h).unwrap();
                if c0.out_of_bounds || !c0.road { continue; }
                for n in h.neighbors() {
                    let Some(nc) = g.cell(n) else { continue; };
                    if nc.out_of_bounds || !nc.road { continue; }
                    if (n.q, n.r) <= (h.q, h.r) { continue; }
                    if linked(h, n) { continue; }
                    let common = h.neighbors().into_iter().any(|x| {
                        x != n && linked(h, x) && linked(n, x)
                    });
                    if common {
                        vtri += 1;
                        if vtri <= 8 {
                            eprintln!("[topo] {name} VTRI ({},{})~({},{})", h.q, h.r, n.q, n.r);
                        }
                    }
                }
            }
            eprintln!("[topo] {name}: visual_triangles={vtri}");
            // Coastal diagnostics: playable water / shore hexes / spurs.
            let mut water_in = 0usize;
            let mut shore = 0usize;
            for h in g.iter_coords() {
                let c = g.cell(h).unwrap();
                if c.out_of_bounds { continue; }
                if c.terrain == Terrain::Water { water_in += 1; }
                if c.terrain != Terrain::Water && c.is_passable
                    && h.neighbors().into_iter().any(|n| {
                        g.cell(n).map(|x| x.terrain == Terrain::Water).unwrap_or(false)
                    })
                {
                    shore += 1;
                }
            }
            // branch endpoints = road hexes with exactly 1 edge bit, adjacent to water dist 1
            let mut coastal_ends = 0usize;
            for h in g.iter_coords() {
                let c = g.cell(h).unwrap();
                if c.out_of_bounds || !c.road { continue; }
                if c.road_edges.count_ones() == 1
                    && h.neighbors().into_iter().any(|n| {
                        g.cell(n).map(|x| x.terrain == Terrain::Water).unwrap_or(false)
                    })
                {
                    coastal_ends += 1;
                    eprintln!("[topo] {name} COASTAL-END ({},{})", h.q, h.r);
                }
            }
            eprintln!("[topo] {name}: playable_water={water_in} shore_hexes={shore} coastal_ends={coastal_ends}");

            // village not adjacent to road check
            let mut orphan_villages = 0usize;
            for h in g.iter_coords() {
                let c = g.cell(h).unwrap();
                if c.out_of_bounds || c.terrain != Terrain::Village { continue; }
                let near_road = std::iter::once(h).chain(h.neighbors()).any(|x| g.cell(x).map(|x2| x2.road).unwrap_or(false));
                let near_water = h.neighbors().into_iter().any(|x| matches!(g.cell(x).map(|x2| x2.terrain), Some(Terrain::Water) | Some(Terrain::River)));
                if !near_road && !near_water { orphan_villages += 1; }
            }
            eprintln!("[topo] {name}: orphan_villages={orphan_villages}");

            // Road net vs the elevation field (roads are priced by
            // relief instead of terrain type — how much relief does the
            // painted net actually sit on?).
            let mut by_elev = std::collections::BTreeMap::new();
            let mut by_terrain = std::collections::BTreeMap::new();
            let mut climbs = std::collections::BTreeMap::new();
            for h in g.iter_coords() {
                let c = g.cell(h).unwrap();
                if c.out_of_bounds || !(c.road || c.rail) {
                    continue;
                }
                *by_elev.entry(c.elevation).or_insert(0usize) += 1;
                *by_terrain
                    .entry(format!("{:?}", c.terrain))
                    .or_insert(0usize) += 1;
                for n in h.neighbors() {
                    if (n.q, n.r) <= (h.q, h.r) || !linked(h, n) {
                        continue;
                    }
                    let ne = g.cell(n).map(|x| x.elevation).unwrap_or(0);
                    *climbs.entry(ne - c.elevation).or_insert(0usize) += 1;
                }
            }
            eprintln!("[topo] {name}: road/rail hexes by elevation {by_elev:?}");
            eprintln!("[topo] {name}: road/rail hexes by terrain {by_terrain:?}");
            eprintln!("[topo] {name}: road steps by Δelev {climbs:?}");
        }
    }
    /// Fuzz the MENU BACKDROP path (menu.rs `backdrop_state`): every land
    /// province in the backdrop's pixel-area window × every single attack
    /// direction goes through `generate()` under `catch_unwind`. A panicking
    /// province kills the menu before the first window appears — the
    /// suspected cause of a reported "release first-launch crash". Run
    /// (release — debug is ~10× slower):
    /// `cargo test --release -p tactical-map backdrop_fuzz -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a local Hearts of Iron IV installation"]
    fn backdrop_fuzz_all_provinces() {
        // Same wiring as tactical3d-bin scenario::build_generator.
        let dir = detect_hoi4_dir().expect("HOI4 install not found");
        let map = ProvinceMap::load_bmp(&dir.join("map").join("provinces.bmp")).unwrap();
        let defs = load_definition_csv(&dir.join("map").join("definition.csv")).unwrap();
        let adjs = load_adjacencies_csv(&dir.join("map").join("adjacencies.csv")).unwrap();
        let mut gen = MapGenerator::new(map, defs, adjs);
        gen.set_rivers(IndexMap::load_indexed_bmp(&dir.join("map").join("rivers.bmp")).unwrap());
        gen.set_victory_points(load_victory_points(&states_dir_of(&dir)).unwrap());
        gen.set_province_to_state(load_province_to_state(&states_dir_of(&dir)).unwrap());
        gen.set_state_categories(load_state_categories(&states_dir_of(&dir)).unwrap());
        gen.set_state_industry(load_state_industry(&states_dir_of(&dir)).unwrap());
        gen.set_state_infrastructure(load_state_infrastructure(&states_dir_of(&dir)).unwrap());
        gen.set_railways(load_railways(&dir.join("map").join("railways.txt")).unwrap());
        gen.set_unit_stacks(load_unit_stacks(&dir.join("map").join("unitstacks.txt")));
        gen.set_vp_names(load_vp_names(
            &dir.join("localisation")
                .join("english")
                .join("victory_points_l_english.yml"),
        ));
        // Same candidate window as menu.rs BACKDROP_MIN_PX/MAX_PX.
        let candidates: Vec<u32> = gen
            .land_province_areas()
            .into_iter()
            .filter(|(_, area)| (60..=4000).contains(area))
            .map(|(id, _)| id)
            .collect();
        let dirs = tactical_core::HexDirection::ALL;
        let total = candidates.len() * dirs.len();
        eprintln!(
            "[fuzz] {} provinces × {} dirs = {total} generate() calls",
            candidates.len(),
            dirs.len()
        );
        let (mut ok, mut errs, mut panics) = (0usize, 0usize, 0usize);
        for (i, &id) in candidates.iter().enumerate() {
            for dir in dirs {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    gen.generate(id, &[dir])
                }));
                match r {
                    Ok(Ok(_)) => ok += 1,
                    Ok(Err(_)) => errs += 1, // graceful errors are fine
                    Err(payload) => {
                        panics += 1;
                        let msg = payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| payload.downcast_ref::<&str>().copied())
                            .unwrap_or("<non-string panic>");
                        eprintln!("[fuzz] PANIC province={id} dir={dir:?}: {msg}");
                    }
                }
            }
            if (i + 1) % 500 == 0 {
                eprintln!(
                    "[fuzz] progress {}/{} provinces (panics so far: {panics})",
                    i + 1,
                    candidates.len()
                );
            }
        }
        eprintln!("[fuzz] done: {ok} ok, {errs} Err, {panics} panics over {total} calls");
        assert_eq!(panics, 0, "generate() must never panic");
    }

    /// Part of the attacker deployment zone can be a PENINSULA — strip
    /// hexes in a disconnected land component (e.g. the Viipuri Bay cuts
    /// the SE strip; the bbox edge truncates it) with no passable path to
    /// the defender zone. Units deployed there can never march to the front
    /// (they hold forever). This test asserts the invariant: every attacker
    /// hex must be passable-path-connected to the defender zone.
    #[test]
    #[ignore = "requires a local Hearts of Iron IV installation"]
    fn attacker_zone_connected_to_defender_zone() {
        let dir = detect_hoi4_dir().expect("HOI4 install not found");
        let map = ProvinceMap::load_bmp(&dir.join("map").join("provinces.bmp")).unwrap();
        let defs = load_definition_csv(&dir.join("map").join("definition.csv")).unwrap();
        let adjs = load_adjacencies_csv(&dir.join("map").join("adjacencies.csv")).unwrap();
        let mut gen = MapGenerator::new(map, defs, adjs);
        gen.set_rivers(IndexMap::load_indexed_bmp(&dir.join("map").join("rivers.bmp")).unwrap());
        gen.set_victory_points(load_victory_points(&states_dir_of(&dir)).unwrap());
        gen.set_province_to_state(load_province_to_state(&states_dir_of(&dir)).unwrap());
        gen.set_state_categories(load_state_categories(&states_dir_of(&dir)).unwrap());
        gen.set_state_industry(load_state_industry(&states_dir_of(&dir)).unwrap());
        gen.set_state_infrastructure(load_state_infrastructure(&states_dir_of(&dir)).unwrap());
        gen.set_railways(load_railways(&dir.join("map").join("railways.txt")).unwrap());
        gen.set_unit_stacks(load_unit_stacks(&dir.join("map").join("unitstacks.txt")));
        gen.set_vp_names(load_vp_names(
            &dir.join("localisation")
                .join("english")
                .join("victory_points_l_english.yml"),
        ));
        for (id, dirs) in [
            (9206u32, vec![tactical_core::HexDirection::SE]),
            (
                3560,
                vec![
                    tactical_core::HexDirection::NW,
                    tactical_core::HexDirection::W,
                ],
            ),
            (
                3544,
                vec![
                    tactical_core::HexDirection::W,
                    tactical_core::HexDirection::SW,
                ],
            ),
        ] {
            let tm = gen.generate(id, &dirs).unwrap();
            let (w, h) = (tm.grid.width, tm.grid.height);
            let passable = |c: tactical_core::HexCoord| {
                tm.grid.cell(c).map(|c| c.is_passable).unwrap_or(false)
            };
            let def_set: std::collections::HashSet<tactical_core::HexCoord> =
                tm.zones.defender.iter().copied().collect();
            // BFS from each attacker hex over passable hexes; count how many
            // attacker hexes reach ANY defender hex (the battlefield).
            let mut reachable_def: Vec<Vec<bool>> = vec![vec![false; w]; h];
            for &a in &tm.zones.attacker {
                let mut seen: std::collections::HashSet<tactical_core::HexCoord> =
                    std::collections::HashSet::new();
                let mut stack = vec![a];
                seen.insert(a);
                let mut hit_def = false;
                while let Some(c) = stack.pop() {
                    if def_set.contains(&c) {
                        hit_def = true;
                        break;
                    }
                    for n in c.neighbors() {
                        if n.q < 0
                            || n.r < 0
                            || n.q as usize >= w
                            || n.r as usize >= h
                            || !passable(n)
                            || !seen.insert(n)
                        {
                            continue;
                        }
                        stack.push(n);
                    }
                }
                reachable_def[a.r as usize][a.q as usize] = hit_def;
            }
            let total = tm.zones.attacker.len();
            let stuck = tm
                .zones
                .attacker
                .iter()
                .filter(|c| !reachable_def[c.r as usize][c.q as usize])
                .count();
            eprintln!(
                "[connect] {id} dirs={dirs:?}: attacker={total} stuck={stuck} ({}%)",
                if total > 0 { stuck * 100 / total } else { 0 }
            );
            assert_eq!(
                stuck, 0,
                "{id}: {stuck}/{total} attacker hexes cannot reach the defender zone — \
                 the deployment zone is a disconnected peninsula"
            );
        }
    }
}
