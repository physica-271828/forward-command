//! §4.2 grid generation: one HOI4 province → a capped 1 km hex grid with
//! occupancy mask, seeded terrain variation, river edges, the
//! factory/road/rail overlays and deployment zones.

use std::collections::{HashMap, HashSet, VecDeque};

use tactical_core::rng::XorShift64;
use tactical_core::{HexCoord, HexDirection, HexGrid, RiverBand, Terrain};

use crate::bmp::ProvinceMap;
use crate::csv::{Adjacency, AdjacencyKind, ProvinceInfo};
use crate::rails::RailwayLine;
use crate::{
    MapError, Result, DEPLOY_STRIP_DEPTH, HEX_SCALE_KM, KM_PER_PIXEL_X, KM_PER_PIXEL_Y,
    MAX_GRID_HEIGHT, MAX_GRID_WIDTH, MIN_PROVINCE_HEXES, ORIGIN_STRIP_PX, SHORE_MARGIN_PX,
};

/// Generated tactical map for one province (§4.2 output).
#[derive(Debug, Clone)]
pub struct TacticalMap {
    pub grid: HexGrid,
    pub zones: DeploymentZones,
    pub province_id: u32,
    /// Province terrain from `definition.csv`, before per-hex variation
    /// (§4.2 step 8).
    pub base_terrain: Terrain,
    /// Stitched maps: attacker origin province(s) folded into the grid
    /// (empty for single-province maps).
    pub origin_provinces: Vec<u32>,
    /// Pixel province id per grid cell (0 = foreign/water) — which
    /// province each hex's centre pixel belongs to.
    pub cell_province: Vec<u32>,
    /// VP display name + city anchor hex for the floating label (only
    /// when the battle province has a real victory point).
    pub vp_label: Option<(String, HexCoord)>,
    /// Interior battles (§4.2 step 9 — paradrop / civil war / entrapment):
    /// the rout escape rim OVERRIDES the defender zone's east rim with the
    /// province boundary (the zone blobs' rims would sit mid-province).
    /// `None` on ordinary battles, which keep the zone-rim semantics.
    pub retreat_rim: Option<Vec<HexCoord>>,
    /// Live battles: each source province's own staging hexes inside the
    /// attacker zone (land strip slice / sea beach segment / interior
    /// pockets) — AI / auto-deploy ties each attacking division to its own
    /// source's staging area. Empty on fallback / script forms.
    pub staging: Vec<(u32, Vec<HexCoord>)>,
}

/// §4.2 step 9 deployment zones. Attacker: ~3-hex-deep strips along the
/// attacked edges of the in-province extent (the grid carries a shoreline
/// margin ring outside the province); defender: the central third of that
/// extent. The two sets are disjoint by construction and kept at least
/// `MIN_ZONE_DISTANCE` hexes apart; both contain only in-bounds, passable
/// hexes.
#[derive(Debug, Clone, Default)]
pub struct DeploymentZones {
    pub attacker: Vec<HexCoord>,
    pub defender: Vec<HexCoord>,
}

/// Bit convention for [`tactical_core::GridCell::river_edges`]: bit `i`
/// corresponds to `HexDirection::ALL[i]` — canonical definition lives in
/// tactical-core ([`HexDirection::bit`]); this helper keeps the call sites
/// in this module readable.
fn river_bit(dir: HexDirection) -> u8 {
    dir.bit()
}

/// One river-bearing pixel kind (§4.2 rivers.bmp). The palette holds
/// blue WIDTH strokes (3–11) plus source/junction MARKERS: green (0) at
/// a river source, red (1) at a merge point, yellow (2) at a split
/// point. Markers ARE the riverbed — vanilla draws them where the
/// channel runs but no width stroke exists, so they must carry the
/// centreline across junctions (never read as land) — yet they own no
/// width themselves: their band is inherited from the strokes they
/// connect (widest connected course; an isolated marker falls back to
/// Small). 5/8 are unused palette slots and 254/255 the sea/land
/// background: not river at all. `None` = not a river pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RiverPx {
    Stroke(RiverBand),
    Marker,
}

fn classify_river_index(i: u8) -> Option<RiverPx> {
    match i {
        3 | 4 => Some(RiverPx::Stroke(RiverBand::Small)),
        6 | 7 => Some(RiverPx::Stroke(RiverBand::Medium)),
        9 | 10 | 11 => Some(RiverPx::Stroke(RiverBand::Large)),
        0 | 1 | 2 => Some(RiverPx::Marker),
        _ => None,
    }
}

// §4.3 (revised): NO latitude/Mercator correction is applied anywhere.
// The HOI4 bitmap is the geographic standard — the game renders
// `provinces.bmp` 1:1 (unitstacks world coordinates ARE bitmap pixels),
// so the tactical grid samples it at a uniform density on both axes
// (KM_PER_PIXEL_X / KM_PER_PIXEL_Y, the latter carrying only the
// pointy-top 2/√3 row-packing factor). The rendered board's shape then
// matches the in-game province silhouette at any latitude. (This comment
// replaces the removed `mercator_correction` fn.)

/// Deterministic per-hex hash → [0, 1). SplitMix64-style finalizer over
/// (province_id, q, r); no external rand crate (§4.2 step 8 requires a
/// seeded pseudo-random variation).
fn hash01(province_id: u32, q: i32, r: i32) -> f32 {
    let mut h = (province_id as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((q as u32 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
        .wrapping_add((r as u32 as u64).wrapping_mul(0x1656_67B1_9E37_79F9));
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    ((h >> 40) as f32) / ((1u64 << 24) as f32)
}

/// Road rasterization: the A* search window is the endpoints' bounding
/// box plus this margin (hexes) — it bounds the ~100+ pathfinds a full
/// road net needs on a 512×512 grid. Edges that cannot route inside the
/// window fall back to the straight hex line.
const ROAD_ASTAR_MARGIN: i32 = 8;

/// Road rasterization: entering an Urban hex costs this instead of the
/// terrain's 1.2 — roads are drawn TOWARD the city, not around it. The
/// urban stretch itself is never drawn (the renderer excludes Urban
/// hexes), so a path diving in reads as the road terminating at the city
/// boundary and re-emerging on the far side — connecting to the city's
/// corners rather than skimming past the cluster.
const ROAD_URBAN_COST: f32 = 0.5;

/// Road rasterization: entering an already-painted road hex multiplies
/// the step cost by this factor (floored at [`ROAD_MERGE_MIN`]) — later
/// paths merge onto existing roads, forming real junctions instead of
/// parallel lines.
const ROAD_MERGE_MULT: f32 = 0.35;

/// Floor of the merge multiplier: even the cheapest terrain never dips
/// below this step cost on an existing road.
const ROAD_MERGE_MIN: f32 = 0.3;

/// Road rasterization: a step onto a hex that is NOT itself road but
/// touches an already-painted road hex (other than the step's own
/// predecessor) adds this — paths either truly merge onto the painted
/// net or keep ≥2 hexes of clearance, never skim alongside it (a skim
/// renders as a spurious junction).
const ROAD_ADJACENCY_PENALTY: f32 = 1.5;

/// Road rasterization: turning inside a River hex is surcharged by the
/// turn angle — straight is free, a 120° (two-step) turn adds this, so
/// bridges cross straight or in a gentle arc.
const ROAD_RIVER_TURN_WIDE: f32 = 1.0;

/// A 60° (one-step) turn inside a River hex adds this — effectively
/// banning sharp bends (U-turns pay the same rate) without a hard rule.
const ROAD_RIVER_TURN_SHARP: f32 = 4.0;

/// Branch roads: one long branch per this many in-province hexes …
const ROAD_BRANCH_PER_AREA: usize = 150;
/// … clamped to this range; short spurs get the same count.
const ROAD_BRANCH_MIN: usize = 2;
const ROAD_BRANCH_MAX: usize = 5;

/// §4.2 step 8 terrain variation. The forest row is verbatim from the spec
/// (70% forest / 20% clearing / 10% rough hills); the other rows follow the
/// same pattern — dominant base terrain with minor variation.
fn varied_terrain(base: Terrain, roll: f32) -> Terrain {
    match base {
        Terrain::Forest => {
            if roll < 0.70 {
                Terrain::Forest
            } else if roll < 0.90 {
                Terrain::Clearing
            } else {
                Terrain::Hills
            }
        }
        Terrain::Mountain => {
            if roll < 0.50 {
                Terrain::Mountain
            } else if roll < 0.80 {
                Terrain::Hills
            } else {
                Terrain::Plains
            }
        }
        // An urban *province* is countryside around one city: the base
        // variation follows the plains template and the city itself is
        // stamped later by place_vp_urban (size scales with VP level).
        Terrain::Urban => {
            if roll < 0.80 {
                Terrain::Plains
            } else if roll < 0.95 {
                Terrain::Forest
            } else {
                Terrain::Hills
            }
        }
        Terrain::Plains => {
            if roll < 0.80 {
                Terrain::Plains
            } else if roll < 0.95 {
                Terrain::Forest
            } else {
                Terrain::Hills
            }
        }
        Terrain::Desert => {
            if roll < 0.85 {
                Terrain::Desert
            } else if roll < 0.95 {
                Terrain::Hills
            } else {
                Terrain::Plains
            }
        }
        Terrain::Jungle => {
            if roll < 0.75 {
                Terrain::Jungle
            } else if roll < 0.90 {
                Terrain::Forest
            } else {
                Terrain::Plains
            }
        }
        Terrain::Marsh => {
            if roll < 0.70 {
                Terrain::Marsh
            } else if roll < 0.90 {
                Terrain::Plains
            } else {
                Terrain::Forest
            }
        }
        Terrain::Hills => {
            if roll < 0.60 {
                Terrain::Hills
            } else if roll < 0.80 {
                Terrain::Forest
            } else {
                Terrain::Plains
            }
        }
        other => other,
    }
}

/// Direction from a province center toward a point, given as an image-space
/// angle (`atan2(dy, dx)`, x east, y south). 60° sectors centered on each of
/// the six pointy-top bearings.
fn direction_from_angle(angle: f32) -> HexDirection {
    match sector_from_angle(angle) {
        0 => HexDirection::E,
        1 => HexDirection::SE,
        2 => HexDirection::SW,
        3 => HexDirection::W,
        4 => HexDirection::NW,
        _ => HexDirection::NE,
    }
}

/// The 0..6 sector index of an image-space angle (see
/// [`direction_from_angle`]): 0=E, 1=SE, 2=SW, 3=W, 4=NW, 5=NE.
fn sector_from_angle(angle: f32) -> usize {
    use std::f32::consts::{PI, TAU};
    let a = angle.rem_euclid(TAU);
    (a / (PI / 3.0)).round() as usize % 6
}

/// The sector index of a compass direction (inverse of [`sector_from_angle`]).
fn sector_of_dir(d: HexDirection) -> usize {
    match d {
        HexDirection::E => 0,
        HexDirection::SE => 1,
        HexDirection::SW => 2,
        HexDirection::W => 3,
        HexDirection::NW => 4,
        HexDirection::NE => 5,
    }
}

/// The compass direction at a sector index (inverse of [`sector_of_dir`]).
fn dir_from_sector(s: usize) -> HexDirection {
    match s % 6 {
        0 => HexDirection::E,
        1 => HexDirection::SE,
        2 => HexDirection::SW,
        3 => HexDirection::W,
        4 => HexDirection::NW,
        _ => HexDirection::NE,
    }
}

/// Holds the resolved province-id grid plus all lookup tables needed to
/// generate tactical maps for any province (§4.1 inputs, loaded once).
pub struct MapGenerator {
    width: u32,
    height: u32,
    /// Province id per pixel (0 = color not in definition.csv), row-major
    /// from the top (north) row.
    ids: Vec<u32>,
    provinces: HashMap<u32, ProvinceInfo>,
    adjacencies: Vec<Adjacency>,
    /// rivers.bmp palette indices (same orientation as `ids`). Optional —
    /// without it only adjacency river *edges* are generated.
    rivers: Option<crate::bmp::IndexMap>,
    /// Victory-point level per province id (history/states/*.txt).
    victory_points: HashMap<u32, u32>,
    /// Index-0 unitstacks position per province (raw unitstacks coords:
    /// x right, z bottom-up) — the in-province VP/city location.
    unit_stacks: HashMap<u32, (f32, f32)>,
    /// VP display names (victory_points_l_english.yml) for the floating
    /// city label.
    vp_names: HashMap<u32, String>,
    /// Province id → state id (history/states/*.txt) — the battle
    /// province's state drives the village density and the factory
    /// district overlay.
    province_to_state: HashMap<u32, u32>,
    /// State id → `state_category` name (history/states/*.txt).
    state_categories: HashMap<u32, String>,
    /// State id → industry level (`industrial_complex` + `arms_factory`
    /// sum of the state's buildings block).
    state_industry: HashMap<u32, u32>,
    /// State id → infrastructure level (top-level `infrastructure` key of
    /// the state's buildings block) — the main-road bridge probability is
    /// level / 5.
    state_infrastructure: HashMap<u32, u32>,
    /// Static rail network (map/railways.txt) as province-id paths.
    railways: Vec<RailwayLine>,
    /// Live per-province rail levels from the save (pid → level); `None`
    /// = no live information, every static line paints.
    rail_levels: Option<HashMap<u32, u8>>,
}

impl MapGenerator {
    /// Combine the three §4.1 inputs. The bitmap's packed colors are resolved
    /// to province ids once, up front, via the definition.csv palette.
    pub fn new(
        map: ProvinceMap,
        definitions: HashMap<u32, ProvinceInfo>,
        adjacencies: Vec<Adjacency>,
    ) -> Self {
        // The palette is expected to keep its RGBs unique; should two rows
        // ever share one, the LOWEST province id owns it — a HashMap walk
        // would otherwise decide by iteration order and map the same save
        // differently between runs (same seed → same map, §4.2).
        let mut palette: Vec<&ProvinceInfo> = definitions.values().collect();
        palette.sort_unstable_by_key(|p| p.id);
        let mut color_to_id: HashMap<u32, u32> = HashMap::with_capacity(palette.len());
        for p in palette {
            color_to_id.entry(pack_rgb(p.rgb)).or_insert(p.id);
        }
        let ids = map
            .colors()
            .iter()
            .map(|c| color_to_id.get(c).copied().unwrap_or(0))
            .collect();
        MapGenerator {
            width: map.width,
            height: map.height,
            ids,
            provinces: definitions,
            adjacencies,
            rivers: None,
            victory_points: HashMap::new(),
            unit_stacks: HashMap::new(),
            vp_names: HashMap::new(),
            province_to_state: HashMap::new(),
            state_categories: HashMap::new(),
            state_industry: HashMap::new(),
            state_infrastructure: HashMap::new(),
            railways: Vec::new(),
            rail_levels: None,
        }
    }

    /// Attach the rivers.bmp index map (river hex overlay).
    pub fn set_rivers(&mut self, rivers: crate::bmp::IndexMap) {
        self.rivers = Some(rivers);
    }

    /// Attach victory-point levels (province id → level, parsed from
    /// history/states/*.txt). A VP on the battle province places Urban
    /// hex(es) at the VP position (unitstacks) or the province centroid.
    pub fn set_victory_points(&mut self, vps: HashMap<u32, u32>) {
        self.victory_points = vps;
    }

    /// Attach unitstacks index-0 positions (province id → raw unitstacks
    /// x/z, z bottom-up). Used to anchor the VP city at the province's
    /// real city location instead of the province centroid.
    pub fn set_unit_stacks(&mut self, stacks: HashMap<u32, (f32, f32)>) {
        self.unit_stacks = stacks;
    }

    /// Attach VP display names (province id → name, parsed from
    /// victory_points_l_english.yml) for the floating city label.
    pub fn set_vp_names(&mut self, names: HashMap<u32, String>) {
        self.vp_names = names;
    }

    /// Attach the province → state map (parsed from history/states/*.txt).
    /// The battle province's state selects the village-density category
    /// and the factory-district industry level.
    pub fn set_province_to_state(&mut self, map: HashMap<u32, u32>) {
        self.province_to_state = map;
    }

    /// Attach state categories (state id → `state_category` name, parsed
    /// from history/states/*.txt) for the village-density factor.
    pub fn set_state_categories(&mut self, cats: HashMap<u32, String>) {
        self.state_categories = cats;
    }

    /// Attach per-state industry levels (`industrial_complex` +
    /// `arms_factory` sums, parsed from history/states/*.txt) for the
    /// factory-district overlay.
    pub fn set_state_industry(&mut self, ind: HashMap<u32, u32>) {
        self.state_industry = ind;
    }

    /// Attach per-state infrastructure levels (top-level `infrastructure`
    /// key, parsed from history/states/*.txt) for the main-road bridge
    /// probability.
    pub fn set_state_infrastructure(&mut self, infra: HashMap<u32, u32>) {
        self.state_infrastructure = infra;
    }

    /// Attach the static rail network (map/railways.txt). Lines crossing
    /// the battle province paint `rail` hexes between the path provinces'
    /// unitstacks anchors.
    pub fn set_railways(&mut self, lines: Vec<RailwayLine>) {
        self.railways = lines;
    }

    /// Attach live per-province rail levels from the save (province id →
    /// level). `None` (the default) means "no live information": every
    /// static line paints. With a map attached, a segment paints only when
    /// BOTH endpoint provinces have level > 0 (a damaged-but-graded bed
    /// still counts — the level is the only criterion), and player-built
    /// lines absent from railways.txt are synthesized from the battle
    /// province outward.
    pub fn set_rail_levels(&mut self, levels: Option<HashMap<u32, u8>>) {
        self.rail_levels = levels;
    }

    /// Village-density factor from the battle province's state category:
    /// urbanised and developed states get a sparser countryside roll,
    /// wastelands and islands sparser still; an unmapped province or an
    /// unknown category keeps the full base rates.
    fn village_density(&self, province_id: u32) -> f32 {
        let Some(cat) = self
            .province_to_state
            .get(&province_id)
            .and_then(|sid| self.state_categories.get(sid))
        else {
            return 1.0;
        };
        match cat.as_str() {
            "rural" => 1.0,
            "pastoral" => 0.6,
            "town" => 0.7,
            "large_town" => 0.5,
            "city" | "large_city" | "metropolis" | "megalopolis" => 0.6,
            "wasteland" => 0.15,
            "enclave" | "tiny_island" | "small_island" | "large_island" => 0.3,
            _ => 1.0,
        }
    }

    /// Industry level of the battle province's state (0 when unmapped) —
    /// the factory-district overlay's input.
    fn battle_industry(&self, province_id: u32) -> u32 {
        self.province_to_state
            .get(&province_id)
            .and_then(|sid| self.state_industry.get(sid))
            .copied()
            .unwrap_or(0)
    }

    /// Infrastructure level of the battle province's state (0 when
    /// unmapped) — the main-road bridge probability is level / 5.
    fn battle_infrastructure(&self, province_id: u32) -> u32 {
        self.province_to_state
            .get(&province_id)
            .and_then(|sid| self.state_infrastructure.get(sid))
            .copied()
            .unwrap_or(0)
    }
}

fn pack_rgb(rgb: (u8, u8, u8)) -> u32 {
    ((rgb.0 as u32) << 16) | ((rgb.1 as u32) << 8) | rgb.2 as u32
}

impl MapGenerator {
    /// Every land province present in the bitmap with its pixel area
    /// (single scan over the id grid). The menu backdrop picks a random
    /// scenic province from this list; sea/lake provinces and zero-pixel
    /// ids are excluded.
    pub fn land_province_areas(&self) -> Vec<(u32, u32)> {
        let mut counts: HashMap<u32, u32> = HashMap::new();
        for &id in &self.ids {
            if id != 0 {
                *counts.entry(id).or_default() += 1;
            }
        }
        let mut out: Vec<(u32, u32)> = counts
            .into_iter()
            .filter(|(id, _)| {
                self.provinces.get(id).map(|i| i.kind) == Some(crate::csv::ProvinceKind::Land)
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// Generate the tactical hex map for one province (§4.2 steps 1–10;
    /// rendering, step 10, is left to the UI crate). Elevation noise
    /// (step 8b) keys off the PROVINCE id — stable across battles (menu
    /// backdrops, debug battle). Battle paths call
    /// [`generate_with_elevation_seed`] with the battle seed so each seed
    /// fights on its own relief (same determinism contract as the RNG).
    pub fn generate(&self, province_id: u32, attack_dirs: &[HexDirection]) -> Result<TacticalMap> {
        self.generate_with_elevation_seed(province_id, attack_dirs, province_id as u64)
    }

    /// Derive attack directions from the REAL source provinces — the
    /// attacker divisions' `location` in the save (1.19 serializes ongoing
    /// battles in `combat.land_combat`, and the attacker's divisions stand
    /// in their SOURCE provinces, not the contested one). Each source
    /// province's shared-border pixel centroid votes for the nearest
    /// compass direction; sources sharing no border (not adjacent /
    /// slivers) are skipped. An empty result means the caller should fall
    /// back to the mod's placeholder dirs.
    pub fn dirs_from_source_provinces(
        &self,
        province_id: u32,
        source_provinces: &[u32],
    ) -> Vec<HexDirection> {
        let mut out: Vec<HexDirection> = Vec::new();
        if source_provinces.is_empty() {
            return out;
        }
        let (w, h) = (self.width, self.height);
        // Contested-province centroid + per-neighbour shared-border pixel
        // centroid (single scan, sums only).
        let (mut sum_x, mut sum_y, mut n) = (0u64, 0u64, 0u64);
        let mut borders: HashMap<u32, (u64, u64, u64)> = HashMap::new();
        for y in 0..h {
            for x in 0..w {
                if self.ids[(y * w + x) as usize] != province_id {
                    continue;
                }
                sum_x += x as u64;
                sum_y += y as u64;
                n += 1;
                for (nx, ny) in [
                    (x.wrapping_sub(1), y),
                    (x + 1, y),
                    (x, y.wrapping_sub(1)),
                    (x, y + 1),
                ] {
                    if nx < w && ny < h {
                        let np = self.ids[(ny * w + nx) as usize];
                        if np != 0 && np != province_id {
                            let e = borders.entry(np).or_default();
                            e.0 += x as u64;
                            e.1 += y as u64;
                            e.2 += 1;
                        }
                    }
                }
            }
        }
        if n == 0 {
            return out;
        }
        let (cx, cy) = (sum_x as f32 / n as f32, sum_y as f32 / n as f32);
        for src in source_provinces {
            let Some(&(sx, sy, cnt)) = borders.get(src) else {
                continue;
            };
            if cnt < 3 {
                continue; // ignore sliver borders (same rule as generate)
            }
            let (bx, by) = (sx as f32 / cnt as f32, sy as f32 / cnt as f32);
            let dir = dir_from_sector(sector_from_angle((by - cy).atan2(bx - cx)));
            if !out.contains(&dir) {
                out.push(dir);
            }
        }
        out
    }

    /// §4.2 step 3b: pick the attacker's origin province(s) and the shared-
    /// border pixels each staging strip is built from.
    ///
    /// Live battles (`strip_sources = Some`): the save names the attacking
    /// divisions' source provinces, so each listed province contributes its
    /// FULL shared border — no 60°-sector clip and no minimum border
    /// length (a 1–2 px sliver is the only way in; the attack must stage
    /// there).
    ///
    /// Script battles (`None`, live fallback when no listed source borders
    /// the province): the sector heuristic — a land neighbour qualifies
    /// when ≥3 of its border pixels fall in an attack direction's sector,
    /// and its strip is clipped to those pixels. The old single-centroid
    /// rule mis-aimed Viipuri 9206, whose SE border's centroid lands in E
    /// (21°); full-border strips of giant neighbours ballooned Ioannina
    /// 3914 to a 4619-hex zone, hence the clip.
    fn resolve_origins(
        &self,
        border_pixels: &HashMap<u32, Vec<(u32, u32)>>,
        center_x: f32,
        center_y: f32,
        attack_dirs: &[HexDirection],
        strip_sources: Option<&[u32]>,
    ) -> (Vec<u32>, HashMap<u32, Vec<(u32, u32)>>) {
        // Deterministic iteration: `border_pixels` is a HashMap — its
        // random order would leak into the outputs, making run-to-run
        // results vary.
        let mut border_entries: Vec<(u32, &Vec<(u32, u32)>)> =
            border_pixels.iter().map(|(np, px)| (*np, px)).collect();
        border_entries.sort_by_key(|(np, _)| *np);
        if let Some(src) = strip_sources {
            let mut origins = Vec::new();
            let mut strip_borders: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
            for (np, pixels) in &border_entries {
                if !src.contains(np) {
                    continue;
                }
                if self.provinces.get(np).map(|i| i.kind)
                    != Some(crate::csv::ProvinceKind::Land)
                {
                    continue;
                }
                origins.push(*np);
                strip_borders.insert(*np, pixels.to_vec());
            }
            if !origins.is_empty() {
                return (origins, strip_borders);
            }
        }
        let attack_sectors: Vec<usize> = attack_dirs.iter().map(|d| sector_of_dir(*d)).collect();
        let mut origins = Vec::new();
        let mut strip_borders = HashMap::new();
        for (np, pixels) in border_entries {
            if pixels.len() < 3 {
                continue; // ignore sliver borders
            }
            if self.provinces.get(&np).map(|i| i.kind) != Some(crate::csv::ProvinceKind::Land) {
                continue;
            }
            let mut per_sector = [0usize; 6];
            let mut in_dirs: Vec<(u32, u32)> = Vec::new();
            for &(x, y) in pixels {
                let s = sector_from_angle((y as f32 - center_y).atan2(x as f32 - center_x));
                per_sector[s] += 1;
                if attack_sectors.contains(&s) {
                    in_dirs.push((x, y));
                }
            }
            let matched = attack_sectors.iter().any(|s| per_sector[*s] >= 3);
            if matched && !origins.contains(&np) {
                origins.push(np);
            }
            if !in_dirs.is_empty() {
                strip_borders.insert(np, in_dirs);
            }
        }
        (origins, strip_borders)
    }

    /// [`generate_with_elevation_seed`] with the attacker's staging ground
    /// taken from real source provinces (see [`resolve_origins`]);
    /// `fallback_dirs` apply when nothing resolves. `attacker_units` sizes
    /// the scattered pockets of an INTERIOR battle (a source province that
    /// IS the battle province — paradrop / civil war / entrapment).
    pub fn generate_from_sources(
        &self,
        province_id: u32,
        source_provinces: &[u32],
        attacker_units: usize,
        fallback_dirs: &[HexDirection],
        elevation_seed: u64,
    ) -> Result<TacticalMap> {
        let derived = self.dirs_from_source_provinces(province_id, source_provinces);
        let dirs = if derived.is_empty() {
            fallback_dirs
        } else {
            &derived
        };
        self.generate_impl(
            province_id,
            dirs,
            elevation_seed,
            Some(source_provinces),
            attacker_units,
        )
    }

    /// (§8.2) The contested province's full neighbour → direction table,
    /// for mid-battle reinforcement placement. Same border-bearing
    /// rule as [`dirs_from_source_provinces`] (each neighbour's shared-border
    /// pixel centroid votes for the nearest compass direction, sliver borders
    /// < 3 px skipped), but kept PER NEIGHBOUR instead of collapsed into a
    /// direction set. The roster looks up a reinforcing division's source /
    /// last-seen province here to pick its map edge.
    pub fn neighbour_dirs(&self, province_id: u32) -> HashMap<u32, HexDirection> {
        let mut out = HashMap::new();
        let (w, h) = (self.width, self.height);
        // Contested-province centroid + per-neighbour shared-border pixel
        // centroid (single scan, sums only) — identical to
        // [`dirs_from_source_provinces`].
        let (mut sum_x, mut sum_y, mut n) = (0u64, 0u64, 0u64);
        let mut borders: HashMap<u32, (u64, u64, u64)> = HashMap::new();
        for y in 0..h {
            for x in 0..w {
                if self.ids[(y * w + x) as usize] != province_id {
                    continue;
                }
                sum_x += x as u64;
                sum_y += y as u64;
                n += 1;
                for (nx, ny) in [
                    (x.wrapping_sub(1), y),
                    (x + 1, y),
                    (x, y.wrapping_sub(1)),
                    (x, y + 1),
                ] {
                    if nx < w && ny < h {
                        let np = self.ids[(ny * w + nx) as usize];
                        if np != 0 && np != province_id {
                            let e = borders.entry(np).or_default();
                            e.0 += x as u64;
                            e.1 += y as u64;
                            e.2 += 1;
                        }
                    }
                }
            }
        }
        if n == 0 {
            return out;
        }
        let (cx, cy) = (sum_x as f32 / n as f32, sum_y as f32 / n as f32);
        for (np, (sx, sy, cnt)) in borders {
            if cnt < 3 {
                continue; // ignore sliver borders (same rule as generate)
            }
            let (bx, by) = (sx as f32 / cnt as f32, sy as f32 / cnt as f32);
            out.insert(
                np,
                dir_from_sector(sector_from_angle((by - cy).atan2(bx - cx))),
            );
        }
        out
    }

    /// [`generate`] with an explicit elevation-noise seed.
    pub fn generate_with_elevation_seed(
        &self,
        province_id: u32,
        attack_dirs: &[HexDirection],
        elevation_seed: u64,
    ) -> Result<TacticalMap> {
        self.generate_impl(province_id, attack_dirs, elevation_seed, None, 0)
    }

    /// Shared body of [`generate_with_elevation_seed`] and
    /// [`generate_from_sources`]. `strip_sources` (live battles, where the
    /// save names the attacking divisions' source provinces) stages the
    /// attacker from those provinces' full shared borders; `None` (script
    /// battles) uses the direction-sector heuristic alone. `attacker_units`
    /// sizes the interior-battle pockets (irrelevant with `None`).
    fn generate_impl(
        &self,
        province_id: u32,
        attack_dirs: &[HexDirection],
        elevation_seed: u64,
        strip_sources: Option<&[u32]>,
        attacker_units: usize,
    ) -> Result<TacticalMap> {
        let info = self
            .provinces
            .get(&province_id)
            .filter(|i| i.kind == crate::csv::ProvinceKind::Land)
            .ok_or(MapError::ProvinceNotFound(province_id))?;

        // §4.2 steps 2–3: province pixels, bounding box, and — for river
        // placement in step 8 — the pixels that border each neighbouring
        // province (4-neighbourhood, in image space).
        let (w, h) = (self.width, self.height);
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut found = false;
        let mut border_pixels: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for y in 0..h {
            for x in 0..w {
                if self.ids[(y * w + x) as usize] != province_id {
                    continue;
                }
                found = true;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                for (nx, ny) in [
                    (x.wrapping_sub(1), y),
                    (x + 1, y),
                    (x, y.wrapping_sub(1)),
                    (x, y + 1),
                ] {
                    if nx < w && ny < h {
                        let np = self.ids[(ny * w + nx) as usize];
                        if np != 0 && np != province_id {
                            border_pixels.entry(np).or_default().push((x, y));
                        }
                    }
                }
            }
        }
        if !found {
            return Err(MapError::ProvinceNotFound(province_id));
        }

        // §4.2 step 3b (multi-province stitching): pick the attacker's
        // origin provinces and fold a staging strip of their territory
        // into the map — see [`resolve_origins`]. The inter-province river
        // (if any) becomes a real crossing obstacle instead of an off-map
        // note. INTERIOR battle (paradrop / civil war / military-access
        // entrapment): a source province that IS the battle province means
        // the attacker's divisions physically stand on the contested
        // ground — no origin stitch (its own soil is the whole province).
        // Source composition (live battles): split the attacker's source
        // provinces into the three staging kinds — INTERIOR (a source that
        // IS the battle province: paradrop / civil war / entrapment), SEA
        // (amphibious landing), LAND neighbours (overland attack). Each
        // kind stages ITS part of the attacking force in its own form —
        // no kind swallows the others.
        let (interior_battle, amphibious, land_sources) = match strip_sources {
            Some(s) => {
                let interior = s.contains(&province_id);
                let sea = s.iter().any(|sp| {
                    self.provinces.get(sp).map(|i| i.kind) == Some(crate::csv::ProvinceKind::Sea)
                });
                let land: Vec<u32> = s
                    .iter()
                    .copied()
                    .filter(|sp| {
                        *sp != province_id
                            && self.provinces.get(sp).map(|i| i.kind)
                                == Some(crate::csv::ProvinceKind::Land)
                    })
                    .collect();
                (interior, sea, land)
            }
            None => (false, false, Vec::new()),
        };
        let center_x = (min_x + max_x) as f32 / 2.0;
        let center_y = (min_y + max_y) as f32 / 2.0;
        // Only LAND sources stitch staging strips into the map — interior
        // sources stage in place, sea sources land on the beach.
        let (origins, strip_borders) = if strip_sources.is_some() && land_sources.is_empty() {
            (Vec::new(), HashMap::new())
        } else {
            self.resolve_origins(
                &border_pixels,
                center_x,
                center_y,
                attack_dirs,
                strip_sources.map(|_| land_sources.as_slice()),
            )
        };
        // Staging strip: origin-province pixels within ORIGIN_STRIP_PX
        // (Chebyshev) of the shared border. The strip must stay a STRIP —
        // gifting the attacker every origin pixel inside the bbox ballooned
        // the deploy zone to 4619 hexes when the origin neighbours are
        // giant provinces (Ioannina 3914: the whole top of the map) and the
        // "front" became ill-defined.
        let mut strip_seen: HashSet<(u32, u32)> = HashSet::new();
        for o in &origins {
            let Some(borders) = strip_borders.get(o) else {
                continue;
            };
            for &(bx, by) in borders {
                for dy in -ORIGIN_STRIP_PX..=ORIGIN_STRIP_PX {
                    for dx in -ORIGIN_STRIP_PX..=ORIGIN_STRIP_PX {
                        let nx = bx as i32 + dx;
                        let ny = by as i32 + dy;
                        if nx < 0 || ny < 0 {
                            continue;
                        }
                        let (nx, ny) = (nx as u32, ny as u32);
                        if nx >= w || ny >= h {
                            continue;
                        }
                        if self.ids[(ny * w + nx) as usize] == *o && strip_seen.insert((nx, ny)) {
                            min_x = min_x.min(nx);
                            max_x = max_x.max(nx);
                            min_y = min_y.min(ny);
                            max_y = max_y.max(ny);
                        }
                    }
                }
            }
        }

        // Shoreline margin: expand the bbox by SHORE_MARGIN_PX on every
        // side (clamped to the bitmap) BEFORE sizing the grid, so the
        // sea/lake a coastal province borders — and a ring of neighbouring
        // land — is sampled into the map. Without this the grid hard-cuts at
        // the province's own extent and a straight coastline shows no water
        // at all. Margin cells fall into the out-of-province branch below:
        // impassable Terrain::Water (sea/lake pixel) or PASSABLE
        // neighbour-terrain backdrop (land pixel, §6.14 out_of_bounds).
        min_x = min_x.saturating_sub(SHORE_MARGIN_PX);
        min_y = min_y.saturating_sub(SHORE_MARGIN_PX);
        max_x = (max_x + SHORE_MARGIN_PX).min(w - 1);
        max_y = (max_y + SHORE_MARGIN_PX).min(h - 1);

        // §4.2 steps 4–6: grid size from the pixel bbox at the uniform §4.1
        // bitmap scale — §4.3 revised: NO latitude correction;
        // KM_PER_PIXEL_Y carries the pointy-top 2/√3 row-packing factor so
        // the rendered board is isotropic with the bitmap. Capped at
        // 512×512.
        let pixel_width = (max_x - min_x + 1) as f32;
        let pixel_height = (max_y - min_y + 1) as f32;
        let width_km = pixel_width * KM_PER_PIXEL_X;
        let height_km = pixel_height * KM_PER_PIXEL_Y;
        let cols = ((width_km / HEX_SCALE_KM).ceil().max(1.0) as usize).min(MAX_GRID_WIDTH);
        let rows = ((height_km / HEX_SCALE_KM).ceil().max(1.0) as usize).min(MAX_GRID_HEIGHT);

        // §4.2 step 7: occupancy down-sample — a hex is in-province iff the
        // pixel under its centre belongs to the province. Odd rows are
        // offset by half a cell, matching the pointy-top layout used by
        // HexCoord::to_world (x = √3·size·(q + r/2)).
        let mut grid = HexGrid::new(cols, rows, info.terrain);
        let col_step = pixel_width / cols as f32;
        let row_step = pixel_height / rows as f32;
        let mut in_province = vec![false; cols * rows];
        // Per-hex "inside the origin staging strip" flag: the attacker's
        // deployable ground is the 2px strip, NOT the whole origin-province
        // territory inside the bbox.
        let mut strip_hex = vec![false; cols * rows];
        // Province id of the pixel under each hex centre (water marking
        // needs the pixel's province kind, not just in/out).
        let mut cell_pid = vec![0u32; cols * rows];
        let mut passable_count = 0usize;
        for r in 0..rows {
            for q in 0..cols {
                let mut fx = (q as f32 + 0.5) * col_step;
                if r % 2 == 1 {
                    fx += 0.5 * col_step;
                }
                let fy = (r as f32 + 0.5) * row_step;
                let px = (min_x + fx as u32).min(max_x);
                let py = (min_y + fy as u32).min(max_y);
                let pid = self.ids[(py * w + px) as usize];
                cell_pid[r * cols + q] = pid;
                let in_strip = strip_seen.contains(&(px, py));
                strip_hex[r * cols + q] = in_strip;
                // Stitched maps: the battle province plus the origin
                // staging strip are playable; the size guard still counts
                // battle-province cells.
                let inside = pid == province_id || in_strip;
                in_province[r * cols + q] = inside;
                if pid == province_id {
                    passable_count += 1;
                }
            }
        }
        if passable_count < MIN_PROVINCE_HEXES {
            return Err(MapError::ProvinceTooSmall {
                id: province_id,
                hexes: passable_count,
            });
        }

        // §4.2 step 8: per-hex terrain — deterministic seeded variation of
        // the base terrain; out-of-province hexes are flagged out_of_bounds
        // (§6.14): sea/lake pixels become impassable water (coastlines and
        // lakes become visible), land pixels stay passable backdrop at
        // their own terrain cost.
        let village_density = self.village_density(province_id);
        for r in 0..rows {
            for q in 0..cols {
                let coord = HexCoord::new(q as i32, r as i32);
                if in_province[r * cols + q] {
                    // Per-hex base terrain from the pixel's OWN province
                    // (stitched maps: the origin strip varies by its own
                    // definition.csv terrain, seeded per province).
                    let pid = cell_pid[r * cols + q];
                    let hex_base = self
                        .provinces
                        .get(&pid)
                        .map(|i| i.terrain)
                        .unwrap_or(info.terrain);
                    let roll = hash01(pid, q as i32, r as i32);
                    let base = varied_terrain(hex_base, roll);
                    // Villages are NOT scattered here — they derive from
                    // the road net later (place_road_villages).
                    grid.set_terrain(coord, base);
                } else {
                    // §6.14: out-of-province = OUT OF BOUNDS. Sea/lake
                    // pixels stay impassable water; land backdrop is
                    // PASSABLE at its own terrain cost — but a unit
                    // lingering `oob_leaving_turns` full turns out there
                    // leaves the battle (apply_oob_leaving).
                    let water = self
                        .provinces
                        .get(&cell_pid[r * cols + q])
                        .map(|i| i.kind != crate::csv::ProvinceKind::Land)
                        .unwrap_or(false);
                    if let Some(cell) = grid.cell_mut(coord) {
                        cell.out_of_bounds = true;
                        if water {
                            cell.is_passable = false;
                        }
                    }
                    if water {
                        grid.set_terrain(coord, Terrain::Water);
                    } else {
                        // Out-of-province land: show the neighbouring
                        // province's own varied terrain instead of the
                        // grid default — otherwise an urban battle
                        // province paints the whole backdrop as city.
                        let pid = cell_pid[r * cols + q];
                        let hex_base = self
                            .provinces
                            .get(&pid)
                            .map(|i| i.terrain)
                            .unwrap_or(info.terrain);
                        let roll = hash01(pid, q as i32, r as i32);
                        grid.set_terrain(coord, varied_terrain(hex_base, roll));
                    }
                }
            }
        }

        // §4.2 step 8b: per-hex elevation noise — ONE continuous field
        // across the whole grid (battle province + stitching strip +
        // shoreline margin), keyed by the elevation seed, so ridge lines
        // never break at province borders. Mountain (±1.75) and Hills
        // (±0.75) undulate around their terrain base (2 → levels 0..4 with
        // true peaks/valleys, 1 → 0..2); everything else keeps its base
        // (plains 0, marsh/river/water −1). Later overlays re-stamp their
        // own bases: rivers → −1, VP urban → 0.
        for r in 0..rows {
            for q in 0..cols {
                let coord = HexCoord::new(q as i32, r as i32);
                let Some(cell) = grid.cell_mut(coord) else {
                    continue;
                };
                let amp = match cell.terrain {
                    Terrain::Mountain => 1.75,
                    Terrain::Hills => 0.75,
                    _ => 0.0,
                };
                if amp > 0.0 {
                    let n =
                        tactical_core::noise::elevation_noise(elevation_seed, q as i32, r as i32);
                    let e = cell.elevation + (n * amp).round() as i32;
                    cell.elevation = e.clamp(0, 4);
                }
            }
        }

        // rivers.bmp overlay — river hexes through/along the province
        // (incl. border rivers like the Meuse at Sedan).
        if let Some(rivers) = &self.rivers {
            self.overlay_rivers(
                rivers,
                &mut grid,
                (min_x, min_y, max_x, max_y),
                cols,
                rows,
                &in_province,
            );
        }
        // A victory point on the battle province places Urban at the
        // province centroid (city size scales with VP level). An urban
        // province without any VP still gets a small town so the map keeps
        // its namesake city. The city is anchored at the VP's unitstacks
        // position (HOI4's own city location) when known, and sized by the
        // linear formula N = int((VP + 2) * U), U = 1.2 for urban provinces.
        let is_urban = info.terrain == Terrain::Urban;
        let vp_level = self
            .victory_points
            .get(&province_id)
            .copied()
            .or(if is_urban { Some(1) } else { None });
        let mut vp_label: Option<(String, HexCoord)> = None;
        let mut vp_anchor: Option<HexCoord> = None;
        if let Some(level) = vp_level {
            // Unitstacks z is bottom-up; the bitmap ids are top-down.
            let vp_hex = self.unit_stacks.get(&province_id).map(|&(sx, sz)| {
                pixel_to_hex(
                    (min_x, min_y, max_x, max_y),
                    cols,
                    rows,
                    sx,
                    self.height as f32 - 1.0 - sz,
                )
            });
            let anchor = self.place_vp_urban(&mut grid, level, is_urban, vp_hex, &in_province);
            // The anchor drives the factory-district ring and joins the
            // road network as a node.
            vp_anchor = Some(anchor);
            // Floating VP name above the city — only for a real VP (the
            // no-VP urban fallback town stays anonymous).
            if self.victory_points.contains_key(&province_id) {
                vp_label = self.vp_names.get(&province_id).map(|n| (n.clone(), anchor));
            }
        }

        // §4.2 step 8 (rivers): for each river adjacency whose partner
        // actually shares a pixel border with this province, mark the
        // `river_edges` bitmask along the hexes facing the shared border.
        for adj in &self.adjacencies {
            if adj.kind != AdjacencyKind::River {
                continue;
            }
            let partner = if adj.from == province_id {
                adj.to
            } else if adj.to == province_id {
                adj.from
            } else {
                continue;
            };
            let Some(pixels) = border_pixels.get(&partner) else {
                continue;
            };
            // Direction from the bbox centre toward the shared-border centroid.
            let n = pixels.len() as f32;
            let (cx, cy) = pixels.iter().fold((0f32, 0f32), |(sx, sy), &(x, y)| {
                (sx + x as f32 / n, sy + y as f32 / n)
            });
            let center_x = (min_x + max_x) as f32 / 2.0;
            let center_y = (min_y + max_y) as f32 / 2.0;
            let dir = direction_from_angle((cy - center_y).atan2(cx - center_x));
            let bit = river_bit(dir);
            // Mark every in-province hex whose neighbour across `dir`
            // belongs to a DIFFERENT province (the internal border — on
            // stitched maps the battle↔origin seam) or leaves the grid
            // (bbox edge). The playable-mask boundary is NOT the test:
            // the staging strip folds the seam into the mask, and the
            // strip's outer edge (same origin province on both sides)
            // must stay river-free.
            for r in 0..rows {
                for q in 0..cols {
                    if !in_province[r * cols + q] {
                        continue;
                    }
                    let coord = HexCoord::new(q as i32, r as i32);
                    let n = coord.neighbor(dir);
                    let crosses_province =
                        if n.q >= 0 && n.r >= 0 && (n.q as usize) < cols && (n.r as usize) < rows {
                            cell_pid[n.r as usize * cols + n.q as usize] != cell_pid[r * cols + q]
                        } else {
                            true
                        };
                    if crosses_province {
                        if let Some(cell) = grid.cell_mut(coord) {
                            cell.river_edges |= bit;
                        }
                    }
                }
            }
        }

        // §4.2 overlays: road network FIRST (city radials / rural stub
        // pairs + branches), then railways and bridges; villages DERIVE
        // from the painted net and factory districts top up last. Terrain
        // and passability are final by now (the VP city is placed); the
        // overlays only SET cell flags — except place_road_villages,
        // which swaps countryside terrain to Village (elevation kept) —
        // and the deployment zones below read the finished ground.
        // Road stubs continue the net off-map only toward LAND neighbours
        // — a sea/lake shore has no road continuation.
        let land_neighbours: HashSet<u32> = border_pixels
            .keys()
            .filter(|np| {
                self.provinces.get(np).map(|i| i.kind) == Some(crate::csv::ProvinceKind::Land)
            })
            .copied()
            .collect();
        // The state's industry drives the factory budget (near-city
        // villages consume it first, place_factories tops up); its
        // infrastructure drives the main-road bridge probability.
        let industry = self.battle_industry(province_id);
        let factory_budget = (industry / 4).min(3);
        let bridge_p = self.battle_infrastructure(province_id) as f32 / 5.0;
        let branch_ends = overlay_roads(
            &mut grid,
            &in_province,
            &cell_pid,
            vp_anchor,
            province_id,
            bridge_p,
            &border_pixels,
            &land_neighbours,
            (min_x, min_y, max_x, max_y),
        );
        self.overlay_railways(
            &mut grid,
            &in_province,
            province_id,
            &border_pixels,
            (min_x, min_y, max_x, max_y),
        );
        stamp_bridge_edges(&mut grid, &in_province, province_id, bridge_p);
        // Purely visual cleanup: hex-level road-edge triangles from merge
        // skims (road flags and bridges untouched).
        prune_road_hex_triangles(&mut grid);
        place_road_villages(
            &mut grid,
            &in_province,
            vp_anchor,
            &branch_ends,
            village_density,
            province_id,
            factory_budget,
        );
        place_factories(&mut grid, &in_province, vp_anchor, industry, province_id);

        // §4.2 step 9: attack directions drive the dynamic frontlines.
        // A `tac_start` without dirs (script/synthetic battles) falls back
        // to the live-mode default (W + NW) — an empty dir list would
        // otherwise produce an EMPTY attacker deployment zone and pile
        // every attacker onto (0,0).
        let effective_dirs: &[HexDirection] = if attack_dirs.is_empty() {
            &[HexDirection::W, HexDirection::NW]
        } else {
            attack_dirs
        };
        grid.attack_dirs = effective_dirs.to_vec();
        // The land stitch's zones (its strip carries the dead-peninsula
        // connectivity prune) — alone it is the whole answer; in a
        // composition only its attacker strip joins.
        let stitched = (!origins.is_empty()).then(|| {
            deployment_zones_stitched(&grid, province_id, &cell_pid, &in_province, &strip_hex)
        });
        let zones = match (interior_battle, amphibious, stitched) {
            (false, false, None) => deployment_zones(&grid, effective_dirs, &in_province),
            (false, false, Some(st)) => st,
            // Pure interior battle: anchor blobs for the defender (it MUST
            // hold the keypoints), scattered pockets for the attacker; the
            // province-boundary rim becomes the rout escape (a blob's east
            // rim would sit mid-province). No room → plain edge strips.
            (true, false, None) => {
                deployment_zones_interior(&grid, &in_province, attacker_units, elevation_seed)
                    .unwrap_or_else(|| deployment_zones(&grid, effective_dirs, &in_province))
            }
            // Pure amphibious landing: ALL beachheads along the sea shore
            // for the attacker, everything else for the defender. No shore
            // at all → plain edge strips.
            (false, true, None) => {
                deployment_zones_amphibious(&self.provinces, &grid, &in_province, &cell_pid)
                    .unwrap_or_else(|| deployment_zones(&grid, effective_dirs, &in_province))
            }
            // Composed sources (landing + overland, melee + overland, …):
            // each staging kind contributes its own part of the attacker
            // zone; nothing resolves → plain edge strips.
            (i, a, st) => compose_zones(
                &self.provinces,
                &grid,
                &in_province,
                &cell_pid,
                i,
                a,
                st,
                attacker_units,
                elevation_seed,
            )
            .unwrap_or_else(|| deployment_zones(&grid, effective_dirs, &in_province)),
        };
        let retreat_rim = interior_battle.then(|| province_rim(&grid, &in_province));
        let staging = build_staging(
            &self.provinces,
            &grid,
            &in_province,
            &cell_pid,
            strip_sources,
            &origins,
            interior_battle,
            province_id,
            &zones.attacker,
        );

        Ok(TacticalMap {
            grid,
            zones,
            province_id,
            base_terrain: info.terrain,
            origin_provinces: origins,
            cell_province: cell_pid,
            vp_label,
            retreat_rim,
            staging,
        })
    }

    /// Paint river hexes from rivers.bmp. The 7.1 km/px strokes
    /// are thinned to their NW edge (a pixel is dropped when it has a river
    /// neighbour at W / N / NW / NE), then surviving neighbours (gaps ≤ 2
    /// px after thinning) are connected with hex lines — rivers come out
    /// continuous and one hex wide instead of inflating to the ~7-hex pixel
    /// footprint. In-province river hexes stay passable (fordable §6.6);
    /// river hexes just outside the province become passable too, so a
    /// border river (the Meuse at Sedan) forms a continuous crossable band.
    fn overlay_rivers(
        &self,
        rivers: &crate::bmp::IndexMap,
        grid: &mut HexGrid,
        bbox: (u32, u32, u32, u32),
        cols: usize,
        rows: usize,
        in_province: &[bool],
    ) {
        let (min_x, min_y, max_x, max_y) = bbox;
        let x0 = min_x.saturating_sub(2) as i32;
        let y0 = min_y.saturating_sub(2) as i32;
        let x1 = (max_x + 2).min(rivers.width.saturating_sub(1)) as i32;
        let y1 = (max_y + 2).min(rivers.height.saturating_sub(1)) as i32;
        if x0 > x1 || y0 > y1 || rivers.width == 0 {
            return;
        }
        if std::env::var("TAC_DEBUG_RIVERS").is_ok() {
            let probe: Vec<u8> = [(2880u32, 540u32), (2881, 530), (2885, 540), (100, 100)]
                .iter()
                .map(|&(x, y)| rivers.index_at(x, y).unwrap_or(99))
                .collect();
            eprintln!("[rivers] enter bbox x{x0}..{x1} y{y0}..{y1} probes {probe:?}");
        }
        // HOI4 rivers.bmp: 254/255 = sea/land background. The remaining
        // palette is blue width strokes plus green/red/yellow source &
        // junction MARKERS (a few hundred vanilla pixels). Markers ARE
        // riverbed — they enter the centreline and its smoothing/joining
        // exactly like strokes, only they carry no width of their own
        // (see the band-inheritance step below).
        let kind_at = |x: i32, y: i32| -> Option<RiverPx> {
            if x < 0 || y < 0 {
                return None;
            }
            rivers
                .index_at(x as u32, y as u32)
                .and_then(classify_river_index)
        };
        let mut stroke: Vec<(i32, i32, RiverPx)> = Vec::new();
        for y in y0..=y1 {
            for x in x0..=x1 {
                if let Some(kind) = kind_at(x, y) {
                    stroke.push((x, y, kind));
                }
            }
        }
        if stroke.is_empty() {
            return;
        }
        if std::env::var("TAC_DEBUG_RIVERS").is_ok() {
            eprintln!(
                "[rivers] {} stroke pixels in bbox x{x0}..{x1} y{y0}..{y1}",
                stroke.len()
            );
        }
        // 3×3-centroid smoothing → the sub-pixel centreline. HOI4 strokes
        // are 1-3 px thick FOR LEGIBILITY (the Meuse is not the Yangtze);
        // the centroid of each pixel's 3×3 neighbourhood sits BETWEEN the
        // tracks of a thick band, so both banks collapse onto one line —
        // no thinning, no erosion holes, and thin strokes pass through
        // untouched (their centroid is the pixel itself).
        let smooth = |x: i32, y: i32| -> (f32, f32) {
            let (mut sx, mut sy, mut n) = (0.0f32, 0.0f32, 0.0f32);
            for dy in -1..=1i32 {
                for dx in -1..=1i32 {
                    if kind_at(x + dx, y + dy).is_some() {
                        sx += (x + dx) as f32;
                        sy += (y + dy) as f32;
                        n += 1.0;
                    }
                }
            }
            if n > 0.0 {
                (sx / n, sy / n)
            } else {
                (x as f32, y as f32)
            }
        };
        // Pixel (float) → hex (inverse of the centre-sample mapping).
        // The `+ 0.5` on fx/fy is the pixel-CENTRE convention (a stroke
        // pixel spans [p, p+1) in the same continuous space §4.2 step 7
        // samples), NOT a hex-unit offset. The anchors that feed
        // `pixel_to_hex{,_raw}` (VP city, road stubs, rail endpoints) take
        // the same index-space pixels WITHOUT it, so the two inverse
        // mappings disagree by half a pixel: on an uncapped province — the
        // grid follows the §4.1 bitmap scale there, ~7.1 hex columns per
        // pixel, i.e. one hex ≈ 0.14 px — that is ~3.5 hex columns and ~4
        // hex rows. Where the 512 cap resamples the province coarser than
        // the bitmap (col_step/row_step ≥ 1 px) the same half pixel shrinks
        // to ≤ half a hex and sits on the round() boundary.
        let pixel_width = (max_x - min_x + 1) as f32;
        let pixel_height = (max_y - min_y + 1) as f32;
        let col_step = pixel_width / cols as f32;
        let row_step = pixel_height / rows as f32;
        let to_hex = |fx: f32, fy: f32| -> HexCoord {
            let rf = (fy + 0.5 - min_y as f32) / row_step - 0.5;
            let r = rf.round() as i32;
            let mut qf = (fx + 0.5 - min_x as f32) / col_step - 0.5;
            if r % 2 == 1 {
                qf -= 0.5;
            }
            HexCoord::new(qf.round() as i32, r)
        };
        // Centreline hexes, split by what painted them:
        //  - `banded`: hexes of WIDTH strokes — the widest band that
        //    painted each one (a stroke crossing several palette levels
        //    keeps the max);
        //  - `pending`: hexes whose only source is a MARKER pixel (or a
        //    joining line with a marker endpoint) — riverbed with no
        //    width of its own yet.
        let mut banded: HashMap<(i32, i32), RiverBand> = HashMap::new();
        let mark = |marked: &mut HashMap<(i32, i32), RiverBand>, q: i32, r: i32, b: RiverBand| {
            marked
                .entry((q, r))
                .and_modify(|m| *m = (*m).max(b))
                .or_insert(b);
        };
        let mut pending: HashSet<(i32, i32)> = HashSet::new();
        let stroke_set: HashSet<(i32, i32)> = stroke.iter().map(|&(x, y, _)| (x, y)).collect();
        let kind_of: HashMap<(i32, i32), RiverPx> =
            stroke.iter().map(|&(x, y, k)| ((x, y), k)).collect();
        for &(x, y, kind) in &stroke {
            let (sx, sy) = smooth(x, y);
            let h = to_hex(sx, sy);
            match kind {
                RiverPx::Stroke(band) => mark(&mut banded, h.q, h.r, band),
                RiverPx::Marker => {
                    pending.insert((h.q, h.r));
                }
            }
        }
        // Connect 8-neighbouring river pixels with hex lines between their
        // SMOOTHED centres — cube-lerp keeps the river continuous even
        // where the 1-px source stroke thins out or breaks (earlier
        // attempts: directional thinning deleted thin rivers;
        // neighbour-count erosion punched holes). A connecting line
        // between two width strokes inherits the wider of the two; a line
        // touching a marker only feeds the pending set (a marker pixel is
        // a junction the strokes join THROUGH, not a width source).
        for &(x, y, kind) in &stroke {
            for (dx, dy) in [(1i32, 0i32), (0, 1), (1, 1), (-1, 1)] {
                if !stroke_set.contains(&(x + dx, y + dy)) {
                    continue;
                }
                let (ax, ay) = smooth(x, y);
                let (bx, by) = smooth(x + dx, y + dy);
                let seg_band = match (kind, kind_of.get(&(x + dx, y + dy))) {
                    (RiverPx::Stroke(a), Some(RiverPx::Stroke(b))) => Some(a.max(*b)),
                    _ => None,
                };
                for h in to_hex(ax, ay).line_to(to_hex(bx, by)) {
                    match seg_band {
                        Some(b) => mark(&mut banded, h.q, h.r, b),
                        None => {
                            pending.insert((h.q, h.r));
                        }
                    }
                }
            }
        }
        // Marker hexes inherit the width of the strokes they connect —
        // the WIDEST band whose stroke pixels sit within ONE PIXEL of the
        // marker (markers are drawn on the channel next to their strokes,
        // and at the map's ~7 km/px scale one pixel spans several hexes,
        // so the hex-domain search radius is the pixel-to-hex ratio of the
        // finer axis). A marker within a pixel of a stroke is ON that
        // channel by construction — inheriting its band cannot jump to an
        // unrelated river. An isolated marker (a lone source dot with
        // nothing to connect) falls back to Small. Downstream of a merge
        // the river's own width stroke already carries the band — the
        // marker only closes the gap, so junction water flows into the
        // wider strip naturally.
        let inherit_radius = (1.0 / col_step.min(row_step)).ceil() as i32;
        let banded_coords: Vec<(HexCoord, RiverBand)> = banded
            .iter()
            .map(|(&(q, r), &b)| (HexCoord::new(q, r), b))
            .collect();
        let mut water: HashMap<(i32, i32), RiverBand> = banded.clone();
        for &(q, r) in &pending {
            if water.contains_key(&(q, r)) {
                continue; // a width stroke already owns this hex
            }
            let m = HexCoord::new(q, r);
            let inherit = banded_coords
                .iter()
                .filter(|(h, _)| h.distance(m) <= inherit_radius)
                .map(|(_, b)| *b)
                .max()
                .unwrap_or(RiverBand::Small);
            water.insert((q, r), inherit);
        }
        // Widen the centreline to the band's water strip: the band IS a
        // width — Small 1 hex (the centreline alone, the legacy river),
        // Medium 2, Large 3. The strip grows by flooding a fixed pair of
        // axial offsets (E + SE) for width−1 rings — a river running along
        // either axis widens to exactly that many rows/columns, any other
        // course rounds to a continuous band of the same order (hex grids
        // have no half widths; surplus hexes only round the banks, never
        // flood along the course). Wider bands win where two strips meet,
        // so a tributary merges into the wider water of its main river.
        let widen_offsets = [(1i32, 0i32), (0, 1)];
        let mut queue: VecDeque<((i32, i32), RiverBand, i32)> = VecDeque::new();
        for (&(q, r), &band) in &water {
            queue.push_back(((q, r), band, band.width_hexes() - 1));
        }
        while let Some(((q, r), band, rings)) = queue.pop_front() {
            if rings <= 0 {
                continue;
            }
            for (dq, dr) in widen_offsets {
                let n = (q + dq, r + dr);
                if water.get(&n).is_some_and(|&b| b >= band) {
                    continue; // equal-or-wider water already flowed here
                }
                water.insert(n, band);
                queue.push_back((n, band, rings - 1));
            }
        }
        for ((q, r), band) in water {
            if q < 0 || r < 0 || q as usize >= cols || r as usize >= rows {
                continue;
            }
            let coord = HexCoord::new(q, r);
            let inside = in_province[r as usize * cols + q as usize];
            let is_water = grid
                .cell(coord)
                .map(|c| c.terrain == Terrain::Water)
                .unwrap_or(false);
            // Special hexes keep their identity: this overlay runs BEFORE
            // the VP city overlay, so any later Urban re-stamp wins; here
            // the only exclusion is open water outside the province (a
            // river mouth must not flood the sea backdrop).
            if inside || !is_water {
                grid.set_terrain(coord, Terrain::River);
                if let Some(cell) = grid.cell_mut(coord) {
                    cell.river_band = cell.river_band.max(band);
                    if !inside {
                        // Border river band: crossable although it lies just
                        // outside the province (deployment still barred —
                        // is_deployable excludes River).
                        cell.is_passable = true;
                    }
                }
            }
        }
        if std::env::var("TAC_DEBUG_RIVERS").is_ok() {
            let (mut small, mut medium, mut large) = (0usize, 0usize, 0usize);
            for c in grid.iter_coords() {
                let Some(cell) = grid.cell(c) else { continue };
                if cell.terrain != Terrain::River {
                    continue;
                }
                match cell.river_band {
                    RiverBand::Small => small += 1,
                    RiverBand::Medium => medium += 1,
                    RiverBand::Large => large += 1,
                }
            }
            let n = small + medium + large;
            eprintln!(
                "[rivers] painted {n} river hexes (grid {cols}×{rows}); bands small {small} medium {medium} large {large}"
            );
        }
    }

    /// The city sits at the VP's own location (unitstacks index-0
    /// position, converted to a hex by the caller) — falling back to the
    /// province centroid when no unitstacks data is available. Size follows
    /// the linear formula `N = int((VP + 2) * U)` with `U = 1.2` for urban
    /// provinces, else 1.0 (Sedan VP 1 → 3, Smolensk VP 15 urban → 20).
    /// Hexes are taken nearest-first from the centre, skipping River/Water.
    /// Returns the anchor hex actually used (for the floating label).
    fn place_vp_urban(
        &self,
        grid: &mut HexGrid,
        level: u32,
        is_urban: bool,
        vp_hex: Option<HexCoord>,
        in_province: &[bool],
    ) -> HexCoord {
        let (cols, rows) = (grid.width, grid.height);
        let center = if let Some(h) = vp_hex {
            h
        } else {
            let (mut sx, mut sy, mut n) = (0.0f32, 0.0f32, 0u32);
            for r in 0..rows {
                for q in 0..cols {
                    if in_province[r * cols + q] {
                        sx += q as f32;
                        sy += r as f32;
                        n += 1;
                    }
                }
            }
            if n == 0 {
                return HexCoord::new(0, 0);
            }
            HexCoord::new(
                (sx / n as f32).round() as i32,
                (sy / n as f32).round() as i32,
            )
        };
        let u = if is_urban { 1.2 } else { 1.0 };
        let size = ((level + 2) as f32 * u) as usize;
        let mut cands: Vec<HexCoord> = Vec::new();
        for r in 0..rows {
            for q in 0..cols {
                if !in_province[r * cols + q] {
                    continue;
                }
                let h = HexCoord::new(q as i32, r as i32);
                if matches!(
                    grid.cell(h).map(|c| c.terrain),
                    Some(Terrain::River) | Some(Terrain::Water)
                ) {
                    continue;
                }
                cands.push(h);
            }
        }
        cands.sort_by_key(|h| (h.distance(center), h.q, h.r));
        for h in cands.into_iter().take(size) {
            grid.set_terrain(h, Terrain::Urban);
        }
        center
    }

    /// Railways (§4.2): the vanilla railways.txt lines touching the grid
    /// paint `rail` along straight hex lines between consecutive path
    /// provinces' unitstacks anchors — in-province hexes only, river hexes
    /// gain a bridge deck. With live per-province levels
    /// ([`MapGenerator::set_rail_levels`]) a segment paints only when BOTH
    /// endpoint provinces have level > 0; without levels every static
    /// line paints. Player-built lines (live levels carry rail the static
    /// map doesn't know: the battle province has level > 0 yet no static
    /// line painted here) connect the battle province's anchor to every
    /// railed neighbour's anchor.
    fn overlay_railways(
        &self,
        grid: &mut HexGrid,
        in_province: &[bool],
        province_id: u32,
        border_pixels: &HashMap<u32, Vec<(u32, u32)>>,
        bbox: (u32, u32, u32, u32),
    ) {
        let (cols, rows) = (grid.width, grid.height);
        // Province id → its unitstacks anchor hex, UNCLAMPED (the
        // grid-intersection test needs the true landing hex; painting
        // clamps). Z is bottom-up in unitstacks coordinates.
        let anchor_raw = |pid: u32| -> Option<HexCoord> {
            let &(sx, sz) = self.unit_stacks.get(&pid)?;
            Some(pixel_to_hex_raw(
                bbox,
                cols,
                rows,
                sx,
                self.height as f32 - 1.0 - sz,
            ))
        };
        let mut painted_any = false;
        for line in &self.railways {
            let pts: Vec<(u32, HexCoord)> = line
                .path
                .iter()
                .filter_map(|&pid| anchor_raw(pid).map(|h| (pid, h)))
                .collect();
            // Only lines whose anchors touch the grid area can paint here.
            if !pts.iter().any(|&(_, h)| grid.in_bounds(h)) {
                continue;
            }
            for pair in pts.windows(2) {
                let (pa, ha) = pair[0];
                let (pb, hb) = pair[1];
                if let Some(levels) = &self.rail_levels {
                    let alive = |pid: u32| levels.get(&pid).copied().unwrap_or(0) > 0;
                    if !alive(pa) || !alive(pb) {
                        continue;
                    }
                }
                let a = clamp_hex(ha, cols, rows);
                let b = clamp_hex(hb, cols, rows);
                if paint_rail_line(grid, in_province, a, b) {
                    painted_any = true;
                }
            }
        }
        // Player-built lines: the save knows rail the static map doesn't.
        // Nothing painted yet means no static line serves this province.
        let Some(levels) = &self.rail_levels else {
            return;
        };
        if painted_any || levels.get(&province_id).copied().unwrap_or(0) == 0 {
            return;
        }
        let Some(home) = anchor_raw(province_id) else {
            return;
        };
        let home = clamp_hex(home, cols, rows);
        // Deterministic paint order (border_pixels is a HashMap).
        let mut neighbours: Vec<u32> = border_pixels.keys().copied().collect();
        neighbours.sort_unstable();
        for np in neighbours {
            if levels.get(&np).copied().unwrap_or(0) == 0 {
                continue;
            }
            let Some(h) = anchor_raw(np) else {
                continue;
            };
            paint_rail_line(grid, in_province, home, clamp_hex(h, cols, rows));
        }
    }
}

/// Bitmap pixel → grid hex (inverse of the §4.2 step 7 centre-sample
/// mapping; odd rows offset half a cell), clamped into the grid. Anchors
/// cities, border road stubs and rail endpoints at their real map
/// positions.
fn pixel_to_hex(
    bbox: (u32, u32, u32, u32),
    cols: usize,
    rows: usize,
    px: f32,
    py: f32,
) -> HexCoord {
    let (min_x, min_y, max_x, max_y) = bbox;
    let col_step = (max_x - min_x + 1) as f32 / cols as f32;
    let row_step = (max_y - min_y + 1) as f32 / rows as f32;
    let r = ((py - min_y as f32) / row_step - 0.5).round() as i32;
    let r = r.clamp(0, rows as i32 - 1);
    let q =
        ((px - min_x as f32) / col_step - 0.5 - if r % 2 == 1 { 0.5 } else { 0.0 }).round() as i32;
    HexCoord::new(q.clamp(0, cols as i32 - 1), r)
}

/// [`pixel_to_hex`] without the grid clamp — the railway overlay's
/// grid-intersection test needs the true landing hex.
fn pixel_to_hex_raw(
    bbox: (u32, u32, u32, u32),
    cols: usize,
    rows: usize,
    px: f32,
    py: f32,
) -> HexCoord {
    let (min_x, min_y, max_x, max_y) = bbox;
    let col_step = (max_x - min_x + 1) as f32 / cols as f32;
    let row_step = (max_y - min_y + 1) as f32 / rows as f32;
    let r = ((py - min_y as f32) / row_step - 0.5).round() as i32;
    let q =
        ((px - min_x as f32) / col_step - 0.5 - if r % 2 == 1 { 0.5 } else { 0.0 }).round() as i32;
    HexCoord::new(q, r)
}

/// Clamp a hex into the grid (grids are always ≥ 1×1).
fn clamp_hex(h: HexCoord, cols: usize, rows: usize) -> HexCoord {
    HexCoord::new(h.q.clamp(0, cols as i32 - 1), h.r.clamp(0, rows as i32 - 1))
}

/// Factory districts (§4.2): an industrial state tops the factory hexes
/// around its city up to industry / 4 (clamped to 3) — factory-stamped
/// road villages in the ring already count (place_road_villages runs
/// first), only the difference is stamped here. Top-up candidates are
/// in-province open countryside (Plains/Clearing/Forest/Hills) at ring
/// distance 2..=4 from the VP anchor, existing (non-factory) villages
/// preferred (an upgraded village keeps its Village base terrain).
/// Picked hexes stamp a Village base + the factory overlay + a road node.
/// No VP anchor or no industry → none. Returns the newly picked hexes.
fn place_factories(
    grid: &mut HexGrid,
    in_province: &[bool],
    anchor: Option<HexCoord>,
    industry: u32,
    seed_pid: u32,
) -> Vec<HexCoord> {
    let Some(anchor) = anchor else {
        return Vec::new();
    };
    let count = (industry / 4).min(3) as usize;
    if count == 0 {
        return Vec::new();
    }
    let (cols, rows) = (grid.width, grid.height);
    // Road-derived factory villages in the ring count toward the target.
    let mut existing = 0usize;
    for r in 0..rows {
        for q in 0..cols {
            if !in_province[r * cols + q] {
                continue;
            }
            let h = HexCoord::new(q as i32, r as i32);
            if (2..=4).contains(&h.distance(anchor))
                && grid.cell(h).map(|c| c.factory).unwrap_or(false)
            {
                existing += 1;
            }
        }
    }
    let need = count.saturating_sub(existing);
    if need == 0 {
        return Vec::new();
    }
    // Seeded pick order (own hash domain); villages form the preferred
    // tier, both tiers hash-ordered with a (q, r) tie-break. Factory
    // hexes are skipped — they already hold a slot.
    let roll_of = |h: HexCoord| hash01(seed_pid ^ 0xFA47_0B05, h.q, h.r);
    let mut villages: Vec<HexCoord> = Vec::new();
    let mut open: Vec<HexCoord> = Vec::new();
    for r in 0..rows {
        for q in 0..cols {
            if !in_province[r * cols + q] {
                continue;
            }
            let h = HexCoord::new(q as i32, r as i32);
            if !(2..=4).contains(&h.distance(anchor)) {
                continue;
            }
            if grid.cell(h).map(|c| c.factory).unwrap_or(false) {
                continue;
            }
            match grid.cell(h).map(|c| c.terrain) {
                Some(Terrain::Village) => villages.push(h),
                Some(Terrain::Plains)
                | Some(Terrain::Clearing)
                | Some(Terrain::Forest)
                | Some(Terrain::Hills) => open.push(h),
                _ => {}
            }
        }
    }
    let by_roll = |a: &HexCoord, b: &HexCoord| {
        roll_of(*a)
            .total_cmp(&roll_of(*b))
            .then_with(|| (a.q, a.r).cmp(&(b.q, b.r)))
    };
    villages.sort_by(by_roll);
    open.sort_by(by_roll);
    let mut picked = Vec::new();
    for h in villages.into_iter().chain(open).take(need) {
        // Direct terrain swap — set_terrain would reset the elevation
        // (a picked Hills hex keeps its 1–2, the road-village rule).
        if let Some(cell) = grid.cell_mut(h) {
            cell.terrain = Terrain::Village;
            cell.factory = true;
            cell.road = true;
        }
        picked.push(h);
    }
    picked
}

/// Coast stubs (§4.2): a province whose playable edge touches water
/// grows up to 3 roads TO the shore, one per DISTINCT water-bearing
/// direction — a single long coast must not fill every slot on one side.
/// Each shore hex joins the direction bucket holding its most adjacent
/// Water hexes (ties break to the lower direction index); buckets of ≥3
/// shore hexes are valid coasts (no slivers); the 3 largest buckets each
/// seed-pick one stub (own hash domain, within the bucket). The road ends
/// at the waterline and the endpoint grows a seaside village.
fn coast_stub_sites(grid: &HexGrid, in_province: &[bool], province_id: u32) -> Vec<HexCoord> {
    let (cols, rows) = (grid.width, grid.height);
    let mut buckets: [Vec<HexCoord>; 6] = Default::default();
    for r in 0..rows {
        for q in 0..cols {
            if !in_province[r * cols + q] {
                continue;
            }
            let h = HexCoord::new(q as i32, r as i32);
            let Some(cell) = grid.cell(h) else {
                continue;
            };
            if !cell.is_passable
                || matches!(
                    cell.terrain,
                    Terrain::Water | Terrain::River | Terrain::Urban
                )
            {
                continue;
            }
            if !near_water(grid, h, 1, 1) {
                continue;
            }
            let mut counts = [0u32; 6];
            for d in HexDirection::ALL {
                if grid.cell(h.neighbor(d)).map(|c| c.terrain) == Some(Terrain::Water) {
                    counts[d as usize] += 1;
                }
            }
            let mut best = 0usize;
            for d in 1..6 {
                if counts[d] > counts[best] {
                    best = d;
                }
            }
            buckets[best].push(h);
        }
    }
    let mut bucket_order: Vec<usize> = (0..6).filter(|&d| buckets[d].len() >= 3).collect();
    bucket_order.sort_by_key(|&d| (std::cmp::Reverse(buckets[d].len()), d));
    let mut coast_stubs: Vec<HexCoord> = Vec::new();
    for d in bucket_order.into_iter().take(3) {
        let stub = buckets[d]
            .iter()
            .min_by(|x, y| {
                hash01(province_id ^ 0xC0A5_7A01, x.q, x.r)
                    .total_cmp(&hash01(province_id ^ 0xC0A5_7A01, y.q, y.r))
                    .then_with(|| (x.q, x.r).cmp(&(y.q, y.r)))
            })
            .unwrap();
        coast_stubs.push(*stub);
    }

    coast_stubs
}

/// Road network (§4.2) — roads FIRST, villages second
/// ([`place_road_villages`] derives them from the painted net). Main
/// roads: with a city, the VP anchor radiates one A* to every
/// land-neighbour border stub (the in-province border hex nearest the
/// shared border's centroid — the road's off-map continuation point;
/// sea/lake shores get none); without a city, the stubs pair off in
/// seeded order — not necessarily adjacent pairs, an odd one out links
/// to its nearest paired stub — and the result is bridged into one
/// component. Seeded long branches and short spurs then fork off the
/// painted net (plus waterside spurs when the province has water).
/// Rasterization draws later paths toward existing roads and keeps
/// bridges straight (see [`road_path`]). Roads paint in-province hexes
/// only and never change terrain or passability. Returns the branch/spur
/// endpoints — [`place_road_villages`] stamps them as guaranteed
/// villages.
fn overlay_roads(
    grid: &mut HexGrid,
    in_province: &[bool],
    cell_pid: &[u32],
    vp_anchor: Option<HexCoord>,
    province_id: u32,
    bridge_p: f32,
    border_pixels: &HashMap<u32, Vec<(u32, u32)>>,
    land_neighbours: &HashSet<u32>,
    bbox: (u32, u32, u32, u32),
) -> Vec<HexCoord> {
    let (cols, rows) = (grid.width, grid.height);
    // In-province hexes touching the province's edge (a neighbour outside
    // the playable mask or the grid border).
    let mut border_hexes: Vec<HexCoord> = Vec::new();
    for r in 0..rows {
        for q in 0..cols {
            if !in_province[r * cols + q] {
                continue;
            }
            let h = HexCoord::new(q as i32, r as i32);
            let on_border = h.neighbors().into_iter().any(|n| {
                n.q < 0
                    || n.r < 0
                    || n.q as usize >= cols
                    || n.r as usize >= rows
                    || !in_province[n.r as usize * cols + n.q as usize]
            });
            if on_border {
                border_hexes.push(h);
            }
        }
    }
    // Strip provinces (stitched attacker origins) are playable terrain:
    // their outer edge is part of the border-hex pool.
    let strip_pids: HashSet<u32> = (0..cols * rows)
        .filter(|&i| in_province[i] && cell_pid[i] != province_id)
        .map(|i| cell_pid[i])
        .collect();
    // One stub per LAND neighbour, deterministic iteration order — every
    // land neighbour connects, no distance admission. Candidate pool:
    // a strip neighbour's road continues ACROSS the stitched strip and
    // exits at the strip's own outer edge (the visible map edge), while a
    // neighbour outside the playable mask is met at the battle province's
    // own border — either way the radial visibly leaves the map.
    let mut neighbours: Vec<u32> = border_pixels.keys().copied().collect();
    neighbours.sort_unstable();
    let mut stubs: Vec<HexCoord> = Vec::new();
    for np in neighbours {
        if !land_neighbours.contains(&np) {
            continue;
        }
        let pixels = &border_pixels[&np];
        let n = pixels.len() as f32;
        let (cx, cy) = pixels.iter().fold((0f32, 0f32), |(sx, sy), &(x, y)| {
            (sx + x as f32 / n, sy + y as f32 / n)
        });
        let raw = pixel_to_hex(bbox, cols, rows, cx, cy);
        let is_strip = strip_pids.contains(&np);
        // A stitched strip folds the shared border INTO the playable mask,
        // so the border-centroid hex lands at the seam and the nearest rim
        // hex would be the band's NEAR edge — the road would stop at
        // the (invisible) internal boundary. Extrapolate the seam →
        // strip-mass direction out through the band instead: the nearest rim
        // hex to that far point is the strip's FAR outer edge — the road
        // visibly leaves the map. (The battle-centroid direction is NOT
        // reliable here: an L-shaped seam can point it away from the mass.)
        let target = if is_strip {
            let (mut sq, mut sr, mut sn) = (0f32, 0f32, 0u32);
            for i in 0..cols * rows {
                if in_province[i] && cell_pid[i] == np {
                    sq += (i % cols) as f32;
                    sr += (i / cols) as f32;
                    sn += 1;
                }
            }
            let (scq, scr) = (sq / sn.max(1) as f32, sr / sn.max(1) as f32);
            clamp_hex(
                HexCoord::new(
                    (scq + 2.0 * (scq - raw.q as f32)).round() as i32,
                    (scr + 2.0 * (scr - raw.r as f32)).round() as i32,
                ),
                cols,
                rows,
            )
        } else {
            raw
        };
        let want_pid = if is_strip { np } else { province_id };
        let own: Vec<HexCoord> = border_hexes
            .iter()
            .filter(|h| cell_pid[h.r as usize * cols + h.q as usize] == want_pid)
            .copied()
            .collect();
        let pool = if own.is_empty() { &border_hexes } else { &own };
        if let Some(stub) = pool.iter().min_by_key(|h| (h.distance(target), h.q, h.r)) {
            stubs.push(*stub);
        }
    }
    // Coast stubs: up to 3 roads to the shore, one per distinct
    // water-bearing direction (see coast_stub_sites).
    let coast_stubs = coast_stub_sites(grid, in_province, province_id);
    stubs.extend(coast_stubs.iter().copied());
    stubs.sort_by_key(|h| (h.q, h.r));
    stubs.dedup();

    // Main roads.
    if let Some(anchor) = vp_anchor {
        // The city radiates one radial to every land neighbour's stub.
        for &s in &stubs {
            let path = road_path(grid, in_province, anchor, s, true, false);
            paint_road(grid, in_province, &path, province_id, bridge_p);
        }
    } else {
        // Rural: seeded shuffle (own hash domain), consecutive pairs; an
        // odd stub out links to its nearest already-paired stub.
        let mut order = stubs.clone();
        let roll_of = |h: &HexCoord| hash01(province_id ^ 0x5A71_9A01, h.q, h.r);
        order.sort_by(|x, y| {
            roll_of(x)
                .total_cmp(&roll_of(y))
                .then_with(|| (x.q, x.r).cmp(&(y.q, y.r)))
        });
        let idx_of = |h: HexCoord| stubs.iter().position(|&s| s == h).unwrap();
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i + 1 < order.len() {
            pairs.push((idx_of(order[i]), idx_of(order[i + 1])));
            i += 2;
        }
        if order.len() % 2 == 1 {
            let last = *order.last().unwrap();
            if let Some(m) = order[..order.len() - 1]
                .iter()
                .min_by_key(|h| (last.distance(**h), h.q, h.r))
            {
                pairs.push((idx_of(last), idx_of(*m)));
            }
        }
        // Bridging: the nearest cross-component stub pair until the pairs
        // form one component.
        let mut uf: Vec<usize> = (0..stubs.len()).collect();
        fn find(uf: &mut [usize], mut x: usize) -> usize {
            while uf[x] != x {
                uf[x] = uf[uf[x]];
                x = uf[x];
            }
            x
        }
        for &(a, b) in &pairs {
            let (ra, rb) = (find(&mut uf, a), find(&mut uf, b));
            if ra != rb {
                uf[ra] = rb;
            }
        }
        loop {
            let mut best: Option<(i32, usize, usize)> = None;
            for u in 0..stubs.len() {
                let ru = find(&mut uf, u);
                for v in (u + 1)..stubs.len() {
                    if find(&mut uf, v) == ru {
                        continue;
                    }
                    let d = stubs[u].distance(stubs[v]);
                    if best.is_none_or(|(bd, bu, bv)| (d, u, v) < (bd, bu, bv)) {
                        best = Some((d, u, v));
                    }
                }
            }
            let Some((_, u, v)) = best else {
                break;
            };
            pairs.push((u, v));
            let (ru, rv) = (find(&mut uf, u), find(&mut uf, v));
            uf[ru] = rv;
        }
        for (a, b) in pairs {
            let path = road_path(grid, in_province, stubs[a], stubs[b], true, false);
            paint_road(grid, in_province, &path, province_id, bridge_p);
        }
    }

    // Branch roads fork off the painted net; later paths see earlier
    // paint through the merge attraction, so junctions emerge naturally.
    let mut endpoints: Vec<HexCoord> = Vec::new();
    let stub_set: HashSet<HexCoord> = stubs.iter().copied().collect();
    let road_hexes: Vec<HexCoord> = grid
        .iter_coords()
        .filter(|&h| {
            in_province[h.r as usize * cols + h.q as usize]
                && grid.cell(h).is_some_and(|c| {
                    c.road && !c.bridge && c.terrain != Terrain::Urban && !stub_set.contains(&h)
                })
        })
        .collect();
    if road_hexes.is_empty() {
        return endpoints;
    }
    let area = in_province.iter().filter(|&&x| x).count();
    let count = (area / ROAD_BRANCH_PER_AREA).clamp(ROAD_BRANCH_MIN, ROAD_BRANCH_MAX);
    // Long branches reach the 8..=12 ring, short spurs 5..=7.
    for i in 0..count {
        if let Some((_, end)) = paint_branch(
            grid,
            in_province,
            province_id,
            bridge_p,
            &road_hexes,
            &endpoints,
            i,
            8,
            12,
            None,
        ) {
            endpoints.push(end);
        }
    }
    for i in 0..count {
        if let Some((_, end)) = paint_branch(
            grid,
            in_province,
            province_id,
            bridge_p,
            &road_hexes,
            &endpoints,
            count + i,
            5,
            7,
            None,
        ) {
            endpoints.push(end);
        }
    }
    // Waterside spurs (village access within sight of water) plus COASTAL
    // spurs ending right on the shore, when the province has water. The
    // gate is grid-wide: a coastal province's sea sits in the
    // out-of-bounds shore-margin ring, never in-province — the shore hexes
    // (playable land beside Water) are what matter.
    let has_water = grid
        .iter_coords()
        .any(|h| grid.cell(h).map(|c| c.terrain) == Some(Terrain::Water));
    if has_water {
        let sea_count = 1 + usize::from(hash01(province_id ^ 0xB0E5_0000, 0, 0) < 0.5);
        for i in 0..sea_count {
            if let Some((_, end)) = paint_branch(
                grid,
                in_province,
                province_id,
                bridge_p,
                &road_hexes,
                &endpoints,
                2 * count + i,
                4,
                12,
                Some((2, 3)),
            ) {
                endpoints.push(end);
            }
        }
        let coastal_count = 1 + usize::from(hash01(province_id ^ 0xB0E5_0001, 0, 0) < 0.5);
        for i in 0..coastal_count {
            if let Some((_, end)) = paint_branch(
                grid,
                in_province,
                province_id,
                bridge_p,
                &road_hexes,
                &endpoints,
                2 * count + 2 + i,
                4,
                24,
                Some((1, 1)),
            ) {
                endpoints.push(end);
            }
        }
    }
    // Coast stubs are guaranteed-village sites (seaside hamlets).
    for &s in &coast_stubs {
        endpoints.push(s);
    }
    endpoints
}

/// Any Water terrain within hex distance `lo..=hi` of `h` — the waterside
/// checks measure to the grid's water, in- or out-of-province.
fn near_water(grid: &HexGrid, h: HexCoord, lo: i32, hi: i32) -> bool {
    for dq in -hi..=hi {
        for dr in -hi..=hi {
            let d = HexCoord::new(dq, dr).distance(HexCoord::ZERO);
            if d < lo || d > hi {
                continue;
            }
            if grid
                .cell(HexCoord::new(h.q + dq, h.r + dr))
                .map(|c| c.terrain)
                == Some(Terrain::Water)
            {
                return true;
            }
        }
    }
    false
}

/// One seeded branch road: a start picked on the painted net and an end
/// picked in the `[lo, hi]` ring around it — in-province, passable,
/// off-road, not Water/River/Urban, at least 3 hexes from every earlier
/// end, and never adjacent to the existing net (a hugging end reads as a
/// stub-length fork); `water_ring` constrains the end to that distance
/// band from a Water hex — (2, 3) for waterside spurs, (1, 1) for
/// COASTAL spurs (the end hugs the shore). Branches NEVER cross a river
/// (River is impassable to their A*); when no river-free route exists in
/// the window the branch is skipped. Painted with the attracting A*.
/// Returns the (start, end) pair — the end is a guaranteed-village site —
/// when a branch was painted.
#[allow(clippy::too_many_arguments)]
fn paint_branch(
    grid: &mut HexGrid,
    in_province: &[bool],
    province_id: u32,
    bridge_p: f32,
    road_hexes: &[HexCoord],
    endpoints: &[HexCoord],
    index: usize,
    lo: i32,
    hi: i32,
    water_ring: Option<(i32, i32)>,
) -> Option<(HexCoord, HexCoord)> {
    let (cols, rows) = (grid.width, grid.height);
    let road_set: HashSet<HexCoord> = road_hexes.iter().copied().collect();
    // Water-constrained branches (waterside/coastal spurs): the eligible
    // water-side END hexes, scanned once — the spur picks its end first
    // and anchors at the nearest road hex, so a remote shore in a big
    // province simply yields no spur instead of silently missing.
    let reachable_ends: Vec<HexCoord> = if let Some((wlo, whi)) = water_ring {
        let mut v = Vec::new();
        for r in 0..rows as i32 {
            for q in 0..cols as i32 {
                let h = HexCoord::new(q, r);
                if !in_province[h.r as usize * cols + h.q as usize] {
                    continue;
                }
                let Some(cell) = grid.cell(h) else {
                    continue;
                };
                if !cell.is_passable || cell.road || cell.bridge {
                    continue;
                }
                if matches!(cell.terrain, Terrain::Water | Terrain::River | Terrain::Urban) {
                    continue;
                }
                if h.neighbors().into_iter().any(|n| road_set.contains(&n)) {
                    continue;
                }
                if !near_water(grid, h, wlo, whi) {
                    continue;
                }
                v.push(h);
            }
        }
        v
    } else {
        Vec::new()
    };
    let sroll = |h: &HexCoord| hash01(province_id ^ 0xB0A1_0000 ^ index as u32, h.q, h.r);
    let (start, end) = if water_ring.is_some() {
        // Water-side spur: pick the END first (seeded among the eligible
        // shore/waterside hexes), then anchor at the NEAREST road hex — a
        // uniform-random start in a big province almost never lands within
        // ring distance of the shore. `lo` still bars stub-short forks;
        // `hi` is the reach cap (a genuinely remote coast gets no spur).
        let eroll = |h: &HexCoord| hash01(province_id ^ 0xB0E1_0000 ^ index as u32, h.q, h.r);
        let mut cand = reachable_ends.clone();
        cand.sort_by(|x, y| {
            eroll(x)
                .total_cmp(&eroll(y))
                .then_with(|| (x.q, x.r).cmp(&(y.q, y.r)))
        });
        let mut picked = None;
        for e in cand {
            if endpoints.iter().any(|p| e.distance(*p) < 3) {
                continue;
            }
            let Some(&s) = road_hexes
                .iter()
                .min_by_key(|s| (s.distance(e), s.q, s.r))
            else {
                continue;
            };
            let d = s.distance(e);
            if d < lo || d > hi {
                continue;
            }
            picked = Some((s, e));
            break;
        }
        match picked {
            Some(pair) => pair,
            None => return None,
        }
    } else {
        let start = *road_hexes.iter().min_by(|x, y| {
            sroll(x)
                .total_cmp(&sroll(y))
                .then_with(|| (x.q, x.r).cmp(&(y.q, y.r)))
        })?;
        let mut best: Option<(f32, HexCoord)> = None;
        for r in 0..rows as i32 {
            for q in 0..cols as i32 {
                let h = HexCoord::new(q, r);
                if !in_province[h.r as usize * cols + h.q as usize] {
                    continue;
                }
                let d = h.distance(start);
                if d < lo || d > hi {
                    continue;
                }
                let Some(cell) = grid.cell(h) else {
                    continue;
                };
                if !cell.is_passable || cell.road || cell.bridge {
                    continue;
                }
                if matches!(
                    cell.terrain,
                    Terrain::Water | Terrain::River | Terrain::Urban
                ) {
                    continue;
                }
                // An end hugging the existing net reads as a stub-length fork.
                if h.neighbors().into_iter().any(|n| road_set.contains(&n)) {
                    continue;
                }
                if endpoints.iter().any(|e| h.distance(*e) < 3) {
                    continue;
                }
                let roll = hash01(province_id ^ 0xB0E1_0000 ^ index as u32, q, r);
                if best.is_none_or(|(br, bh)| (roll, (q, r)) < (br, (bh.q, bh.r))) {
                    best = Some((roll, h));
                }
            }
        }
        match best {
            Some((_, end)) => (start, end),
            None => return None,
        }
    };
    let path = road_path(grid, in_province, start, end, false, false);
    if path.is_empty() {
        return None; // no river-free route in the window — skip the branch
    }
    paint_road(grid, in_province, &path, province_id, bridge_p);
    Some((start, end))
}

/// Villages derive from the road net (§4.2 — roads first, villages
/// second). Scatter candidates (hexes adjacent to a road, or 2..=3 from
/// water) sit BESIDE the net, never on a road hex; structural classes
/// stamp ON their structure hex (on the road, flag kept): branch/spur
/// endpoints are GUARANTEED (p=1.0); junctions
/// ([`GridCell::road_edges`] with 3+ bits) roll 0.55; roadside scatter
/// rolls 0.12, waterside 0.05. The final p = class × terrain weight ×
/// the state density factor (cap 0.95; classes never stack). Road-free
/// hexes ALSO roll a low wildland scatter (terrain base rate × density:
/// Plains 0.004 … Mountain 0.0005) — never zero, never dense. Near the
/// city (anchor ring ≤4) the roll is ×3 and an accepted village consumes
/// one factory_budget to stamp as a factory district — budget exhausted
/// → a plain village. Candidates process in ascending roll order; an
/// accepted village rejects later scatter-class candidates within 2
/// hexes (structural classes ignore spacing but occupy it). Stamps
/// Terrain::Village directly so the hex keeps its elevation; never on
/// River/Water/Urban/bridge hexes (a structure hex on such terrain grows
/// no village at all).
#[allow(clippy::too_many_arguments)]
fn place_road_villages(
    grid: &mut HexGrid,
    in_province: &[bool],
    vp_anchor: Option<HexCoord>,
    branch_ends: &[HexCoord],
    density: f32,
    province_id: u32,
    mut factory_budget: u32,
) {
    let (cols, rows) = (grid.width, grid.height);
    let ends: HashSet<HexCoord> = branch_ends.iter().copied().collect();
    // A village site: in-province, passable, off-road, no bridge, host
    // terrain (never Urban/River/Water — and never a second stamp on an
    // existing Village).
    let compliant = |grid: &HexGrid, h: HexCoord| -> bool {
        if h.q < 0 || h.r < 0 || h.q as usize >= cols || h.r as usize >= rows {
            return false;
        }
        if !in_province[h.r as usize * cols + h.q as usize] {
            return false;
        }
        let Some(cell) = grid.cell(h) else {
            return false;
        };
        if !cell.is_passable || cell.road || cell.bridge {
            return false;
        }
        !matches!(
            cell.terrain,
            Terrain::Urban | Terrain::River | Terrain::Water | Terrain::Village
        )
    };
    // Host-terrain weight — the old scatter's relative rates, normalized
    // to Plains = 1.0.
    let weight_of = |t: Terrain| -> f32 {
        match t {
            Terrain::Plains => 1.0,
            Terrain::Clearing => 1.25,
            Terrain::Hills | Terrain::Desert => 0.5,
            Terrain::Forest => 0.375,
            Terrain::Marsh | Terrain::Jungle => 0.25,
            Terrain::Mountain => 0.125,
            _ => 0.0,
        }
    };
    // Wildland scatter: a road-independent roll per compliant hex —
    // the terrain base rate already carries the terrain factor (no
    // weight on top).
    let wildland_rate = |t: Terrain| -> f32 {
        match t {
            Terrain::Plains => 0.004,
            Terrain::Clearing => 0.003,
            Terrain::Hills | Terrain::Desert => 0.002,
            Terrain::Forest => 0.0015,
            Terrain::Marsh | Terrain::Jungle => 0.001,
            Terrain::Mountain => 0.0005,
            _ => 0.0,
        }
    };
    let near_city = |h: HexCoord| vp_anchor.is_some_and(|a| h.distance(a) <= 4);
    // The final roll probability: max over classes (roadside/waterside
    // are terrain-weighted; the wildland rate IS its terrain factor) ×
    // density; ×3 near the city; capped at 0.95.
    let final_p = |weighted_class_p: f32, wild: f32, weight: f32, h: HexCoord| -> f32 {
        let mut p = (weighted_class_p * weight).max(wild) * density;
        if near_city(h) {
            p *= 3.0;
        }
        p.min(0.95)
    };
    struct Cand {
        /// The structure hex (junction/endpoint) or the scatter hex
        /// itself; the roll key.
        src: HexCoord,
        /// The final roll probability (all factors applied).
        p: f32,
        /// Endpoint/junction classes: spacing never rejects them (they
        /// still occupy spacing for scatter candidates).
        structural: bool,
        /// Endpoints: no roll at all — always stamped.
        guaranteed: bool,
        roll: f32,
    }
    let mut cands: Vec<Cand> = Vec::new();
    for r in 0..rows {
        for q in 0..cols {
            if !in_province[r * cols + q] {
                continue;
            }
            let h = HexCoord::new(q as i32, r as i32);
            let Some(cell) = grid.cell(h) else {
                continue;
            };
            let roll = hash01(province_id ^ 0x7E57_0001, q as i32, r as i32);
            // Structural classes live ON the road structure hex.
            let endpoint = cell.road && ends.contains(&h);
            let junction = cell.road && cell.road_edges.count_ones() >= 3;
            if endpoint || junction {
                // The terrain gate applies at scan time: River/Water/
                // Urban ground and bridge decks grow no village.
                let weight = weight_of(cell.terrain);
                if weight > 0.0 && !cell.bridge {
                    cands.push(Cand {
                        src: h,
                        p: final_p(if endpoint { 1.0 } else { 0.55 }, 0.0, weight, h),
                        structural: true,
                        guaranteed: endpoint,
                        roll,
                    });
                }
                continue;
            }
            // Scatter classes never sit on the road itself.
            if !compliant(grid, h) {
                continue;
            }
            let roadside = h.neighbors().into_iter().any(|n| {
                n.q >= 0
                    && n.r >= 0
                    && (n.q as usize) < cols
                    && (n.r as usize) < rows
                    && in_province[n.r as usize * cols + n.q as usize]
                    && grid.cell(n).map(|c| c.road).unwrap_or(false)
            });
            let waterside = near_water(grid, h, 2, 3);
            let mut class_p: f32 = 0.0;
            if roadside {
                class_p = class_p.max(0.12);
            }
            if waterside {
                class_p = class_p.max(0.05);
            }
            let wild = wildland_rate(cell.terrain);
            if class_p <= 0.0 && wild <= 0.0 {
                continue;
            }
            cands.push(Cand {
                src: h,
                p: final_p(class_p, wild, weight_of(cell.terrain), h),
                structural: false,
                guaranteed: false,
                roll,
            });
        }
    }
    cands.sort_by(|x, y| {
        x.roll
            .total_cmp(&y.roll)
            .then_with(|| (x.src.q, x.src.r).cmp(&(y.src.q, y.src.r)))
    });
    let mut accepted: Vec<HexCoord> = Vec::new();
    for c in cands {
        // Structural villages stamp ON the structure hex itself (on the
        // road); scatter villages on their own (off-road) hex. Both were
        // gated at scan time.
        let stamp = c.src;
        if !c.guaranteed && c.roll >= c.p {
            continue;
        }
        if !c.structural && accepted.iter().any(|a| stamp.distance(*a) <= 2) {
            continue;
        }
        // Direct terrain swap — set_terrain would reset the elevation.
        let cell = grid.cell_mut(stamp).unwrap();
        cell.terrain = Terrain::Village;
        if near_city(stamp) && factory_budget > 0 {
            factory_budget -= 1;
            cell.factory = true;
        }
        accepted.push(stamp);
    }
}

/// Rasterize one route: terrain-aware A* between the endpoints. Road
/// mode: the entered hex's movement cost is the step price with three
/// adjustments —
/// Urban attracts ([`ROAD_URBAN_COST`]); an already-painted road hex
/// attracts harder ([`ROAD_MERGE_MULT`], floored at [`ROAD_MERGE_MIN`]),
/// so later paths merge onto existing roads instead of drawing parallels;
/// and a step beside the painted net without joining it is surcharged
/// ([`ROAD_ADJACENCY_PENALTY`]), so paths keep ≥2 hexes of clearance
/// instead of skimming. Turning inside a River hex is surcharged by the
/// turn angle
/// ([`ROAD_RIVER_TURN_WIDE`] / [`ROAD_RIVER_TURN_SHARP`]), so bridges
/// cross straight or in a gentle arc. River is crossable at 3.0 for MAIN
/// roads (`allow_river`); branch roads pass `false` and treat River as
/// impassable — when no river-free route exists in the window they get NO
/// path at all (an empty Vec — the branch is skipped), never the straight
/// fallback. RAIL mode is a flat 1.0/hex search with NO pricing
/// adjustments — the railway net keeps its own straight-line feel and the
/// A* exists only to route around impassable sea; no in-window route →
/// empty Vec (the line is dropped), likewise never the straight fallback.
/// Water and out-of-province hexes are always impassable, and
/// the search is clipped to the endpoints' bounding box +
/// [`ROAD_ASTAR_MARGIN`] for speed. The A* state carries the entry
/// direction (q, r, dir) for the river-turn pricing; the start hex
/// carries NO_DIR, so the first river entry is turn-free. When a
/// river-allowed road search yields no path the straight hex line is the
/// fallback (its out-of-province/Water hexes still don't paint — the road
/// visually clips at the province edge). Deterministic: heap ties break
/// by (q, r, dir).
fn road_path(
    grid: &HexGrid,
    in_province: &[bool],
    from: HexCoord,
    to: HexCoord,
    allow_river: bool,
    rail: bool,
) -> Vec<HexCoord> {
    use std::collections::BinaryHeap;

    /// A* state pseudo-direction: the start hex has no entry direction.
    const NO_DIR: u8 = 6;

    #[derive(PartialEq)]
    struct Entry {
        f: f32,
        g: f32,
        q: i32,
        r: i32,
        dir: u8,
    }
    impl Eq for Entry {}
    impl PartialOrd for Entry {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(o))
        }
    }
    impl Ord for Entry {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering {
            // Min-heap on f, then the more progressed g, then (q, r, dir).
            o.f.total_cmp(&self.f)
                .then_with(|| self.g.total_cmp(&o.g))
                .then_with(|| (o.q, o.r, o.dir).cmp(&(self.q, self.r, self.dir)))
        }
    }

    let (cols, rows) = (grid.width as i32, grid.height as i32);
    let q0 = from.q.min(to.q).saturating_sub(ROAD_ASTAR_MARGIN).max(0);
    let q1 = from
        .q
        .max(to.q)
        .saturating_add(ROAD_ASTAR_MARGIN)
        .min(cols - 1);
    let r0 = from.r.min(to.r).saturating_sub(ROAD_ASTAR_MARGIN).max(0);
    let r1 = from
        .r
        .max(to.r)
        .saturating_add(ROAD_ASTAR_MARGIN)
        .min(rows - 1);
    let walkable = |h: HexCoord| -> bool {
        if h.q < q0 || h.q > q1 || h.r < r0 || h.r > r1 {
            return false;
        }
        if rail {
            // Railways route around impassable SEA only: Water is barred,
            // everything else — rivers (always bridged) and the
            // out-of-province backdrop — is walkable; painting still
            // clips to the playable mask.
            return grid
                .cell(h)
                .map(|c| c.terrain != Terrain::Water)
                .unwrap_or(false);
        }
        if !in_province[h.r as usize * grid.width + h.q as usize] {
            return false;
        }
        grid.cell(h)
            .map(|c| c.terrain != Terrain::Water && (allow_river || c.terrain != Terrain::River))
            .unwrap_or(false)
    };
    if !walkable(from) || !walkable(to) {
        // No fallback line for river-barred (branch) or rail searches:
        // no route. Roads may fall back to the straight line.
        return if allow_river && !rail {
            from.line_to(to)
        } else {
            Vec::new()
        };
    }
    let mut g: HashMap<(i32, i32, u8), f32> = HashMap::new();
    let mut came: HashMap<(i32, i32, u8), (i32, i32, u8)> = HashMap::new();
    let mut heap = BinaryHeap::new();
    g.insert((from.q, from.r, NO_DIR), 0.0);
    heap.push(Entry {
        // The heuristic must stay admissible under the merge discount:
        // no step ever costs less than ROAD_MERGE_MIN per hex. Rail mode
        // is uniformly 1.0/hex, so the bare hex distance is exact.
        f: from.distance(to) as f32 * if rail { 1.0 } else { ROAD_MERGE_MIN },
        g: 0.0,
        q: from.q,
        r: from.r,
        dir: NO_DIR,
    });
    let mut found: Option<u8> = None;
    while let Some(e) = heap.pop() {
        let cur = HexCoord::new(e.q, e.r);
        if cur == to {
            found = Some(e.dir);
            break;
        }
        let cg = g[&(e.q, e.r, e.dir)];
        if e.g > cg + 1e-6 {
            continue; // stale heap entry
        }
        for dir in HexDirection::ALL {
            let n = cur.neighbor(dir);
            if !walkable(n) {
                continue;
            }
            let Some(cell) = grid.cell(n) else {
                continue;
            };
            // Rail mode: the railway net keeps its own straight-line feel
            // — flat 1.0 per hex, no merge/adjacency/turn/urban pricing;
            // the A* exists only to route around impassable sea.
            let cost = if rail {
                1.0
            } else {
                let mut cost = match cell.terrain {
                    // City attraction: roads dive to the urban cluster's
                    // corners instead of skirting past it.
                    Terrain::Urban => ROAD_URBAN_COST,
                    t => t.movement_cost(),
                };
                // Merge attraction: run along roads already painted.
                if cell.road {
                    cost = (cost * ROAD_MERGE_MULT).max(ROAD_MERGE_MIN);
                } else {
                    // Anti-skim: a non-road hex touching the painted net
                    // (other than the step's own predecessor — junction and
                    // merge points stay allowed) pays a surcharge, so paths
                    // either merge truly or keep ≥2 hexes of clearance.
                    let skims = n
                        .neighbors()
                        .into_iter()
                        .any(|m| m != cur && grid.cell(m).map(|c| c.road).unwrap_or(false));
                    if skims {
                        cost += ROAD_ADJACENCY_PENALTY;
                    }
                }
                // River-turn surcharge: bend price by turn angle (the start
                // hex has no entry direction, so its river entry is free).
                if cell.terrain == Terrain::River && e.dir != NO_DIR {
                    let step = (dir as i32 - e.dir as i32).rem_euclid(6);
                    cost += match step.min(6 - step) {
                        0 => 0.0,
                        2 => ROAD_RIVER_TURN_WIDE,
                        // 60° and the U-turn are effectively banned.
                        _ => ROAD_RIVER_TURN_SHARP,
                    };
                }
                cost
            };
            let ng = cg + cost;
            let key = (n.q, n.r, dir as u8);
            if ng + 1e-6 < *g.get(&key).unwrap_or(&f32::INFINITY) {
                g.insert(key, ng);
                came.insert(key, (e.q, e.r, e.dir));
                heap.push(Entry {
                    f: ng + n.distance(to) as f32 * if rail { 1.0 } else { ROAD_MERGE_MIN },
                    g: ng,
                    q: n.q,
                    r: n.r,
                    dir: dir as u8,
                });
            }
        }
    }
    let Some(final_dir) = found else {
        return if allow_river && !rail {
            from.line_to(to)
        } else {
            Vec::new()
        };
    };
    let mut path = vec![(to.q, to.r)];
    let mut cur = (to.q, to.r, final_dir);
    while let Some(&prev) = came.get(&cur) {
        path.push((prev.0, prev.1));
        cur = prev;
    }
    path.reverse();
    path.into_iter().map(|(q, r)| HexCoord::new(q, r)).collect()
}

/// In-grid, in-province, non-Water — the hexes a road/rail path may paint
/// (flag or edge bit).
fn path_paintable(grid: &HexGrid, in_province: &[bool], h: HexCoord) -> bool {
    if h.q < 0 || h.r < 0 {
        return false;
    }
    let (q, r) = (h.q as usize, h.r as usize);
    if q >= grid.width || r >= grid.height {
        return false;
    }
    if !in_province[r * grid.width + q] {
        return false;
    }
    grid.cell(h).is_some_and(|c| c.terrain != Terrain::Water)
}

/// Record the crossed hex edge for every consecutive pair of painted
/// hexes in a path — on BOTH cells of the shared edge (bit i =
/// HexDirection::ALL[i], same convention as river_edges). The renderer
/// draws these explicit links rather than guessing from flag adjacency,
/// so clustered node hexes and paths passing BESIDE a flagged hex invent
/// no spurious links; junctions emerge where paths share a hex (the
/// shared hex collects 3+ bits). A single-hex path sets nothing: node
/// hexes get their edges from the paths that connect them. A pair
/// touching an unpaintable or skipped hex sets nothing — no single-side
/// residual bits; the link ends where the flag paint ends.
fn paint_path_edges(
    grid: &mut HexGrid,
    in_province: &[bool],
    path: &[HexCoord],
    rail: bool,
    skip: &HashSet<HexCoord>,
) {
    for pair in path.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if skip.contains(&a) || skip.contains(&b) {
            continue;
        }
        if !path_paintable(grid, in_province, a) || !path_paintable(grid, in_province, b) {
            continue;
        }
        let Some(dir) = a.direction_to(b) else {
            continue;
        };
        let back = dir.opposite();
        if let Some(cell) = grid.cell_mut(a) {
            if rail {
                cell.rail_edges |= dir.bit();
            } else {
                cell.road_edges |= dir.bit();
            }
        }
        if let Some(cell) = grid.cell_mut(b) {
            if rail {
                cell.rail_edges |= back.bit();
            } else {
                cell.road_edges |= back.bit();
            }
        }
    }
}

/// Split a path's contiguous River runs into complete crossings and
/// incomplete border fords (§4.2): complete means the run crosses the
/// river fully INSIDE the playable area — the path hexes before and
/// after the run exist, are paintable and are not River, so the road
/// continues on both banks. An incomplete run dies at the map edge or
/// the water: it gets NO flag paint (the road ends at the near bank) and
/// no deck — a partial bridge at the province border must not appear.
fn river_runs(
    grid: &HexGrid,
    in_province: &[bool],
    path: &[HexCoord],
) -> (Vec<Vec<HexCoord>>, Vec<HexCoord>) {
    let mut runs: Vec<(usize, Vec<HexCoord>)> = Vec::new();
    let mut open = false;
    for (i, &h) in path.iter().enumerate() {
        let on_river = path_paintable(grid, in_province, h)
            && grid.cell(h).map(|c| c.terrain) == Some(Terrain::River);
        if on_river {
            if !open {
                runs.push((i, Vec::new()));
                open = true;
            }
            runs.last_mut().unwrap().1.push(h);
        } else {
            open = false;
        }
    }
    let mut complete = Vec::new();
    let mut incomplete = Vec::new();
    for (start, run) in runs {
        let before_ok = start > 0
            && path_paintable(grid, in_province, path[start - 1])
            && grid.cell(path[start - 1]).map(|c| c.terrain) != Some(Terrain::River);
        let after_i = start + run.len();
        let after_ok = after_i < path.len()
            && path_paintable(grid, in_province, path[after_i])
            && grid.cell(path[after_i]).map(|c| c.terrain) != Some(Terrain::River);
        if before_ok && after_ok {
            complete.push(run);
        } else {
            incomplete.extend(run);
        }
    }
    (complete, incomplete)
}

/// Paint `road` along a path — in-province, non-Water hexes only.
/// Consecutive painted pairs also record `road_edges` on both cells
/// (see [`paint_path_edges`]). Terrain is never changed. Bridges are
/// probabilistic by the state's infrastructure: each COMPLETE contiguous
/// River run on the path (see [`river_runs`]) rolls once (seeded by the
/// run's first hex, own hash domain) against `bridge_p` (= level / 5) —
/// pass and the run gets bridge decks; fail and the crossing stays a
/// FORD (the road flag stays on the River hexes, no deck — wading across
/// at the river's movement price). An INCOMPLETE run (border ford) is
/// skipped entirely: no deck, no road flag — the road ends at the near
/// bank. A `bridge_p` of 0 builds nothing.
fn paint_road(
    grid: &mut HexGrid,
    in_province: &[bool],
    path: &[HexCoord],
    seed_pid: u32,
    bridge_p: f32,
) {
    let (complete, incomplete) = river_runs(grid, in_province, path);
    let skip: HashSet<HexCoord> = incomplete.into_iter().collect();
    for &h in path {
        if skip.contains(&h) || !path_paintable(grid, in_province, h) {
            continue;
        }
        let Some(cell) = grid.cell_mut(h) else {
            continue;
        };
        cell.road = true;
    }
    // Per-segment bridge roll over the complete crossings only.
    for run in complete {
        let first = run[0];
        if hash01(seed_pid ^ 0xB21D_6E00, first.q, first.r) < bridge_p {
            for h in run {
                if let Some(cell) = grid.cell_mut(h) {
                    cell.bridge = true;
                }
            }
        }
    }
    paint_path_edges(grid, in_province, path, false, &skip);
}

/// Paint `rail` between two anchor hexes — the route is the rail-mode A*
/// (flat 1.0/hex, no pricing adjustments): straight where it can, around
/// impassable sea where it must; Water is impassable and a window with no
/// in-province route drops the line (returns false, nothing painted).
/// In-province, non-Water hexes only; COMPLETE River runs (see
/// [`river_runs`]) always gain a bridge deck (railways never ford);
/// incomplete border fords are skipped (no rail flag, no deck);
/// consecutive painted pairs record `rail_edges` on both cells (see
/// [`paint_path_edges`]). Returns true when any in-province hex was
/// painted.
fn paint_rail_line(grid: &mut HexGrid, in_province: &[bool], from: HexCoord, to: HexCoord) -> bool {
    let path = road_path(grid, in_province, from, to, true, true);
    if path.is_empty() {
        return false;
    }
    let (complete, incomplete) = river_runs(grid, in_province, &path);
    let skip: HashSet<HexCoord> = incomplete.into_iter().collect();
    let mut painted = false;
    for &h in &path {
        if skip.contains(&h) || !path_paintable(grid, in_province, h) {
            continue;
        }
        let Some(cell) = grid.cell_mut(h) else {
            continue;
        };
        cell.rail = true;
        painted = true;
    }
    for run in complete {
        for h in run {
            if let Some(cell) = grid.cell_mut(h) {
                cell.bridge = true;
            }
        }
    }
    paint_path_edges(grid, in_province, &path, true, &skip);
    painted
}

/// Adjacency-river bridges: a rail hex sitting on a river-edge border hex
/// always bridges that edge; a ROAD hex rolls against the state's
/// infrastructure probability (same hash domain as the path segments —
/// a crossing and its border edge agree) (§4.2) — runs after both
/// overlays paint.
fn stamp_bridge_edges(grid: &mut HexGrid, in_province: &[bool], seed_pid: u32, bridge_p: f32) {
    let (cols, rows) = (grid.width, grid.height);
    for r in 0..rows {
        for q in 0..cols {
            if !in_province[r * cols + q] {
                continue;
            }
            let h = HexCoord::new(q as i32, r as i32);
            let Some(cell) = grid.cell_mut(h) else {
                continue;
            };
            let bridged = if cell.rail {
                true // railways always bridge
            } else {
                cell.road && hash01(seed_pid ^ 0xB21D_6E00, h.q, h.r) < bridge_p
            };
            if bridged && cell.river_edges != 0 {
                cell.bridge_edges |= cell.river_edges;
            }
        }
    }
}

/// Road-edge triangle cleanup (§4.2): the merge attraction can leave a
/// path skimming an existing road — two hexes linked, both linked to a
/// shared neighbour — a hex-level 3-cycle that renders as a spurious
/// junction. Repeatedly find the first such triangle (hexes scanned in
/// (q, r) order) and clear ONE edge's road_edges bits on both cells —
/// the lexicographically largest UNPROTECTED edge of the trio — until no
/// clearable triangle remains. Edges touching a bridge deck or a River
/// hex are protected (the crossing must read complete); a fully
/// protected triangle stays. Purely visual: the road flags (movement)
/// are never touched. Runs after both overlays and the bridge stamps.
fn prune_road_hex_triangles(grid: &mut HexGrid) {
    let mut coords: Vec<HexCoord> = grid.iter_coords().collect();
    coords.sort_by_key(|h| (h.q, h.r));
    loop {
        let linked = |a: HexCoord, b: HexCoord| -> bool {
            let Some(dir) = a.direction_to(b) else {
                return false;
            };
            let (Some(ca), Some(cb)) = (grid.cell(a), grid.cell(b)) else {
                return false;
            };
            ca.road_edges & dir.bit() != 0 && cb.road_edges & dir.opposite().bit() != 0
        };
        let protected = |a: HexCoord, b: HexCoord| -> bool {
            let (Some(ca), Some(cb)) = (grid.cell(a), grid.cell(b)) else {
                return true;
            };
            ca.bridge || cb.bridge || ca.terrain == Terrain::River || cb.terrain == Terrain::River
        };
        let mut victim: Option<(HexCoord, HexCoord)> = None;
        'scan: for &a in &coords {
            // a's linked neighbours past it in (q, r) order.
            let abits = grid.cell(a).map(|c| c.road_edges).unwrap_or(0);
            if abits == 0 {
                continue;
            }
            let mut ns: Vec<HexCoord> = HexDirection::ALL
                .into_iter()
                .filter(|d| abits & d.bit() != 0)
                .map(|d| a.neighbor(d))
                .filter(|&b| (b.q, b.r) > (a.q, a.r) && linked(a, b))
                .collect();
            ns.sort_by_key(|h| (h.q, h.r));
            for (i, &b) in ns.iter().enumerate() {
                for &c in &ns[i + 1..] {
                    if !linked(b, c) {
                        continue;
                    }
                    // Triangle a-b-c: clear the lexicographically largest
                    // unprotected edge; all-protected → leave it.
                    let mut trio = [(a, b), (a, c), (b, c)];
                    trio.sort_by_key(|&(x, y)| ((x.q, x.r), (y.q, y.r)));
                    for &(x, y) in trio.iter().rev() {
                        if !protected(x, y) {
                            victim = Some((x, y));
                            break;
                        }
                    }
                    if victim.is_some() {
                        break 'scan;
                    }
                }
            }
        }
        let Some((x, y)) = victim else {
            break;
        };
        let Some(dir) = x.direction_to(y) else {
            break;
        };
        let back = dir.opposite();
        if let Some(cell) = grid.cell_mut(x) {
            cell.road_edges &= !dir.bit();
        }
        if let Some(cell) = grid.cell_mut(y) {
            cell.road_edges &= !back.bit();
        }
    }
}

/// §4.2 step 9 deployment zones. Attacker strips are ~[`DEPLOY_STRIP_DEPTH`]
/// hexes deep along each attacked edge; diagonal directions take the matching
/// half of the top/bottom edge (e.g. NW = top edge, western half). The
/// defender holds the central third, minus any attacker hexes, so the zones
/// are disjoint. Only deployable hexes are included (in-province, passable,
/// not full-hex water).
///
/// Edges and thirds are anchored to the IN-PROVINCE hex extent,
/// not the grid edges — the shoreline margin ring (`SHORE_MARGIN_PX`) pads
/// the grid with out-of-province backdrop/water, so grid-edge strips would
/// land in the ring and come out empty (island battles, sea-side attacks).
fn deployment_zones(
    grid: &HexGrid,
    attack_dirs: &[HexDirection],
    in_province: &[bool],
) -> DeploymentZones {
    let (w, h) = (grid.width, grid.height);
    let depth = DEPLOY_STRIP_DEPTH;
    let passable = |q: usize, r: usize| {
        in_province[r * w + q]
            && grid
                .cell(HexCoord::new(q as i32, r as i32))
                .map(|c| c.is_passable && c.terrain.is_deployable())
                .unwrap_or(false)
    };

    // In-province hex extent (inclusive). Falls back to the full grid when
    // the mask is somehow empty — generate() rejects that province first
    // (MIN_PROVINCE_HEXES), so this is only a defensive default.
    let (mut eq0, mut eq1, mut er0, mut er1) = (usize::MAX, 0usize, usize::MAX, 0usize);
    for r in 0..h {
        for q in 0..w {
            if in_province[r * w + q] {
                eq0 = eq0.min(q);
                eq1 = eq1.max(q);
                er0 = er0.min(r);
                er1 = er1.max(r);
            }
        }
    }
    if eq0 > eq1 {
        (eq0, eq1, er0, er1) = (0, w - 1, 0, h - 1);
    }
    // Twice the extent midpoint — the diagonal-strip splitter; equals the
    // old `w`-based comparison when the extent is the whole grid.
    let mid_q2 = eq0 + eq1 + 1;

    let mut attacker: Vec<HexCoord> = Vec::new();
    let mut attacker_set: HashSet<(i32, i32)> = HashSet::new();
    for &dir in attack_dirs {
        for r in 0..h {
            for q in 0..w {
                let in_strip = match dir {
                    HexDirection::W => q < eq0 + depth,
                    HexDirection::E => q + depth > eq1,
                    HexDirection::NW => r < er0 + depth && q * 2 < mid_q2,
                    HexDirection::NE => r < er0 + depth && q * 2 >= mid_q2,
                    HexDirection::SW => r + depth > er1 && q * 2 < mid_q2,
                    HexDirection::SE => r + depth > er1 && q * 2 >= mid_q2,
                };
                if in_strip && passable(q, r) {
                    let c = HexCoord::new(q as i32, r as i32);
                    if attacker_set.insert((c.q, c.r)) {
                        attacker.push(c);
                    }
                }
            }
        }
    }

    // Defender: central third of the province extent, slid toward the
    // threatened edges (the vector average of the attack directions
    // defines the threat axis; shift = quarter span, clamped inside the
    // extent).
    let span_q = eq1 + 1 - eq0;
    let span_r = er1 + 1 - er0;
    let (mut q0, mut q1) = (eq0 + span_q / 3, eq0 + (2 * span_q).div_ceil(3));
    let (mut r0, mut r1) = (er0 + span_r / 3, er0 + (2 * span_r).div_ceil(3));
    if !attack_dirs.is_empty() {
        let (dx, dy) = attack_dirs.iter().fold((0i32, 0i32), |(sx, sy), d| {
            let (dq, dr) = d.offset();
            (sx + dq, sy + dr)
        });
        if dx != 0 || dy != 0 {
            let len = ((dx * dx + dy * dy) as f32).sqrt();
            let (ux, uy) = (dx as f32 / len, dy as f32 / len);
            let (sq, sr) = (
                (ux * span_q as f32 / 4.0).round() as i32,
                (uy * span_r as f32 / 4.0).round() as i32,
            );
            // Clamp the shifted third inside the extent.
            let span_w = (q1 - q0) as i32;
            let span_h = (r1 - r0) as i32;
            let nq0 = (q0 as i32 + sq).clamp(eq0 as i32, (eq1 + 1) as i32 - span_w);
            let nr0 = (r0 as i32 + sr).clamp(er0 as i32, (er1 + 1) as i32 - span_h);
            q0 = nq0 as usize;
            q1 = (nq0 + span_w) as usize;
            r0 = nr0 as usize;
            r1 = (nr0 + span_h) as usize;
        }
    }
    let mut defender = Vec::new();
    for r in r0..r1 {
        for q in q0..q1 {
            if passable(q, r) && !attacker_set.contains(&(q as i32, r as i32)) {
                defender.push(HexCoord::new(q as i32, r as i32));
            }
        }
    }
    // No-man's land — the defender zone keeps at least MIN_ZONE_DISTANCE
    // hexes from every attacker strip.
    let defender = tactical_core::grid::filter_min_distance(
        defender,
        &attacker,
        tactical_core::grid::MIN_ZONE_DISTANCE,
    );

    DeploymentZones { attacker, defender }
}

/// The province's boundary hexes — the interior-battle rout rim
/// ([`deployment_zones_interior`]): in-province, passable cells with at
/// least one neighbour outside the province (or off the grid). A rout
/// walks a real BFS path out of the contested ground here; the defender
/// blobs' own rims would sit mid-province.
fn province_rim(grid: &HexGrid, in_province: &[bool]) -> Vec<HexCoord> {
    let (w, h) = (grid.width, grid.height);
    let mut rim = Vec::new();
    for r in 0..h {
        for q in 0..w {
            if !in_province[r * w + q] {
                continue;
            }
            let c = HexCoord::new(q as i32, r as i32);
            if !grid.cell(c).map(|cell| cell.is_passable).unwrap_or(false) {
                continue;
            }
            let boundary = c.neighbors().iter().any(|n| {
                n.q < 0
                    || n.r < 0
                    || n.q as usize >= w
                    || n.r as usize >= h
                    || !in_province[n.r as usize * w + n.q as usize]
            });
            if boundary {
                rim.push(c);
            }
        }
    }
    rim
}

/// Axial ring walk — every hex within `radius` of `center` (radius 2 is
/// the ~19-hex neighbourhood the flag clusters also use).
fn hexes_within(center: HexCoord, radius: i32) -> Vec<HexCoord> {
    let mut out = Vec::new();
    for dq in -radius..=radius {
        let r1 = (-radius - dq).max(-radius);
        let r2 = (radius - dq).min(radius);
        for dr in r1..=r2 {
            out.push(HexCoord::new(center.q + dq, center.r + dr));
        }
    }
    out
}

/// Multi-source BFS margin over the hex lattice: every cell's distance to
/// the nearest avoided cell (`avoid`) or non-interior cell (seeds, margin
/// 0). Hex distance == graph distance on the lattice, so this is the
/// "depth" of a candidate inside the interior minus the avoid set — the
/// same metric the flag fallback's anchor sampling uses.
fn margin_field(
    grid: &HexGrid,
    interior_set: &HashSet<(i32, i32)>,
    avoid: &HashSet<(i32, i32)>,
) -> Vec<u32> {
    let (w, h) = (grid.width as i32, grid.height as i32);
    let mut margin = vec![u32::MAX; grid.width * grid.height];
    let mut queue: VecDeque<HexCoord> = VecDeque::new();
    for r in 0..h {
        for q in 0..w {
            let c = HexCoord::new(q, r);
            let idx = (r * w + q) as usize;
            if !interior_set.contains(&(q, r)) || avoid.contains(&(q, r)) {
                margin[idx] = 0;
                queue.push_back(c);
            }
        }
    }
    while let Some(c) = queue.pop_front() {
        let d = margin[(c.r * w + c.q) as usize] + 1;
        for n in c.neighbors() {
            if n.q < 0 || n.r < 0 || n.q >= w || n.r >= h {
                continue;
            }
            let idx = (n.r * w + n.q) as usize;
            if margin[idx] == u32::MAX {
                margin[idx] = d;
                queue.push_back(n);
            }
        }
    }
    margin
}

/// Greedy farthest-point sample over `cands`: candidates below
/// `min_margin` (BFS margin from the avoid set + non-interior) are
/// skipped, then the largest-margin candidate at least `min_pairwise`
/// from every chosen one wins, iteratively. `rng` shuffles the pool first,
/// so near-ties scatter per battle while staying deterministic per seed.
#[allow(clippy::too_many_arguments)]
fn farthest_sample(
    grid: &HexGrid,
    cands: &[HexCoord],
    interior_set: &HashSet<(i32, i32)>,
    avoid: &HashSet<(i32, i32)>,
    n: usize,
    min_pairwise: i32,
    min_margin: u32,
    rng: &mut XorShift64,
) -> Vec<HexCoord> {
    let margin = margin_field(grid, interior_set, avoid);
    let w = grid.width as i32;
    let mut pool: Vec<HexCoord> = cands
        .iter()
        .copied()
        .filter(|c| margin[(c.r * w + c.q) as usize] >= min_margin)
        .collect();
    for i in (1..pool.len()).rev() {
        let j = rng.next_below(i as u64 + 1) as usize;
        pool.swap(i, j);
    }
    let mut chosen: Vec<HexCoord> = Vec::new();
    while chosen.len() < n {
        let mut best: Option<(u32, HexCoord)> = None;
        for &c in &pool {
            if chosen.iter().any(|cc| c.distance(*cc) < min_pairwise) {
                continue;
            }
            let nearest = chosen
                .iter()
                .map(|cc| c.distance(*cc))
                .min()
                .unwrap_or(i32::MAX) as u32;
            let score = margin[(c.r * w + c.q) as usize].min(nearest);
            if best.map(|(s, _)| score > s).unwrap_or(true) {
                best = Some((score, c));
            }
        }
        let Some((_, c)) = best else { break };
        chosen.push(c);
    }
    chosen
}

/// §4.2 step 9 interior battles (paradrop / civil war / military-access
/// entrapment — the attacker's divisions physically STAND in the battle
/// province, its source list naming the province itself): no origin
/// stitch and no edge strip describes the front — the front is
/// EVERYWHERE. The defender holds anchor blobs, the attacker holds
/// scattered pockets woven between them — 你中有我，我中有你. Zones are
/// disjoint by construction and deliberately NOT kept `MIN_ZONE_DISTANCE`
/// apart — interleaving is the point. `None` when the province cannot
/// house the defending force at all (the caller falls back).
fn deployment_zones_interior(
    grid: &HexGrid,
    in_province: &[bool],
    attacker_units: usize,
    seed: u64,
) -> Option<DeploymentZones> {
    let (interior, interior_set) = interior_ground(grid, in_province)?;
    let mut rng = XorShift64::new(seed);
    let (defender, defender_set) =
        interior_blobs(grid, in_province, &interior, &interior_set, &mut rng)?;
    let attacker = interior_pockets(
        grid,
        in_province,
        &interior,
        &interior_set,
        &defender_set,
        attacker_units,
        &mut rng,
    )?;
    Some(DeploymentZones { attacker, defender })
}

/// One deployable in-province hex (the deployment zones' shared filter:
/// in-bounds, in-province, passable, not full-hex water).
fn deployable_hex(grid: &HexGrid, in_province: &[bool], c: HexCoord) -> bool {
    let (w, h) = (grid.width, grid.height);
    c.q >= 0
        && c.r >= 0
        && (c.q as usize) < w
        && (c.r as usize) < h
        && in_province[c.r as usize * w + c.q as usize]
        && grid
            .cell(c)
            .map(|cell| cell.is_passable && cell.terrain.is_deployable())
            .unwrap_or(false)
}

/// The in-province deployable hexes of an interior battle's ground (plus
/// the lookup set) — `None` below the interior form's floor, where the
/// edge-strip form describes the cramped ground better.
fn interior_ground(
    grid: &HexGrid,
    in_province: &[bool],
) -> Option<(Vec<HexCoord>, HashSet<(i32, i32)>)> {
    /// Floor for the interior form at all.
    const MIN_INTERIOR_DEPLOYABLE: usize = 30;
    let (w, h) = (grid.width, grid.height);
    let interior: Vec<HexCoord> = (0..h)
        .flat_map(|r| (0..w).map(move |q| HexCoord::new(q as i32, r as i32)))
        .filter(|&c| deployable_hex(grid, in_province, c))
        .collect();
    if interior.len() < MIN_INTERIOR_DEPLOYABLE {
        return None;
    }
    let set = interior.iter().map(|c| (c.q, c.r)).collect();
    Some((interior, set))
}

/// The defender's anchor blobs of an interior battle (§4.2 step 9): the
/// VP-city urban cluster plus its ring (a village-sized VP widens to
/// radius 2 so the garrison actually fits), or on a city-less field 2–3
/// blobs around margin-deep, well-separated centres — the same cores the
/// flag fallback samples its anchors from, so the keypoints the defender
/// MUST hold stay inside its zone by construction.
fn interior_blobs(
    grid: &HexGrid,
    in_province: &[bool],
    interior: &[HexCoord],
    interior_set: &HashSet<(i32, i32)>,
    rng: &mut XorShift64,
) -> Option<(Vec<HexCoord>, HashSet<(i32, i32)>)> {
    let mut defender: Vec<HexCoord> = Vec::new();
    let mut defender_set: HashSet<(i32, i32)> = HashSet::new();
    let urban: Vec<HexCoord> = interior
        .iter()
        .copied()
        .filter(|&c| {
            grid.cell(c)
                .map(|cell| cell.terrain == Terrain::Urban)
                .unwrap_or(false)
        })
        .collect();
    if !urban.is_empty() {
        let radius = if urban.len() < 8 { 2 } else { 1 };
        for &u in &urban {
            for c in hexes_within(u, radius) {
                if deployable_hex(grid, in_province, c) && defender_set.insert((c.q, c.r)) {
                    defender.push(c);
                }
            }
        }
    } else {
        let blob_centres = farthest_sample(
            grid,
            interior,
            interior_set,
            &HashSet::new(),
            if interior.len() >= 90 { 3 } else { 2 },
            5,
            3,
            rng,
        );
        if blob_centres.is_empty() {
            return None;
        }
        for centre in blob_centres {
            for c in hexes_within(centre, 2) {
                if deployable_hex(grid, in_province, c) && defender_set.insert((c.q, c.r)) {
                    defender.push(c);
                }
            }
        }
    }
    if defender.is_empty() {
        return None;
    }
    Some((defender, defender_set))
}

/// The attacker's scattered pockets of an interior battle: the drop lands
/// AROUND the held keypoints, not on top of them — pocket centres keep ≥3
/// hexes from the defender blobs and ≥5 from each other, sized from the
/// force they must house (a radius-2 pocket holds ~19). Short of room,
/// the pockets widen twice; the last tier keeps whatever it could place.
fn interior_pockets(
    grid: &HexGrid,
    in_province: &[bool],
    interior: &[HexCoord],
    interior_set: &HashSet<(i32, i32)>,
    defender_set: &HashSet<(i32, i32)>,
    attacker_units: usize,
    rng: &mut XorShift64,
) -> Option<Vec<HexCoord>> {
    let want = ((attacker_units.max(1) as f32 * 1.3) / 19.0).ceil() as usize;
    let want = want.clamp(1, 6);
    for (i, radius) in [2i32, 3, 4].iter().enumerate() {
        let centres = farthest_sample(grid, interior, interior_set, defender_set, want, 5, 3, rng);
        let mut attacker: Vec<HexCoord> = Vec::new();
        let mut attacker_set: HashSet<(i32, i32)> = HashSet::new();
        for centre in &centres {
            for c in hexes_within(*centre, *radius) {
                if deployable_hex(grid, in_province, c)
                    && !defender_set.contains(&(c.q, c.r))
                    && attacker_set.insert((c.q, c.r))
                {
                    attacker.push(c);
                }
            }
        }
        let fits = attacker.len() as f32 >= attacker_units.max(1) as f32 * 1.2;
        if fits || i == 2 {
            return (!attacker.is_empty()).then_some(attacker);
        }
    }
    unreachable!("the last pocket tier always returns")
}

/// All deployable in-province hexes outside the attacker zone, kept
/// `MIN_ZONE_DISTANCE` off it — the defender rule of the amphibious form
/// and of compositions without an interior part.
fn rest_of_province(grid: &HexGrid, in_province: &[bool], attacker: &[HexCoord]) -> Vec<HexCoord> {
    let attacker_set: HashSet<(i32, i32)> = attacker.iter().map(|c| (c.q, c.r)).collect();
    let (w, h) = (grid.width, grid.height);
    let mut defender = Vec::new();
    for r in 0..h {
        for q in 0..w {
            let c = HexCoord::new(q as i32, r as i32);
            if deployable_hex(grid, in_province, c) && !attacker_set.contains(&(c.q, c.r)) {
                defender.push(c);
            }
        }
    }
    tactical_core::grid::filter_min_distance(
        defender,
        attacker,
        tactical_core::grid::MIN_ZONE_DISTANCE,
    )
}

/// §4.2 step 9 amphibious landings (the attacker's source provinces are SEA
/// provinces — its divisions cross the water): the attacker stages on ALL
/// beachheads along the sea shore, not just one compass edge — every
/// in-province deployable hex within `DEPLOY_STRIP_DEPTH` of a sea-adjacent
/// shoreline hex, a band that can dwarf the plain edge strip on a ragged
/// coast. The defender holds EVERYTHING else in the province (it repositions
/// against the landing anywhere; `MIN_ZONE_DISTANCE` no-man's land and the
/// deployability filter apply as in [`deployment_zones`]). The flag anchors
/// redistribute with it by construction — §6.11 samples them from the new,
/// much larger defender zone. `None` when the province has no sea shore at
/// all (odd bitmap — the caller falls back to the plain edge strips).
fn deployment_zones_amphibious(
    provinces: &HashMap<u32, ProvinceInfo>,
    grid: &HexGrid,
    in_province: &[bool],
    cell_pid: &[u32],
) -> Option<DeploymentZones> {
    let attacker = amphibious_beach(provinces, grid, in_province, cell_pid, None)?;
    let defender = rest_of_province(grid, in_province, &attacker);
    Some(DeploymentZones { attacker, defender })
}

/// The amphibious beach band (§4.2 step 9): every in-province deployable
/// hex within `DEPLOY_STRIP_DEPTH` of a sea-adjacent shoreline hex
/// (multi-source BFS). `None` when the province has no sea shore at all.
/// `sea` restricts the band to ONE sea province's own shoreline segment —
/// the staging areas tie each landing division to the sea it sailed from.
fn amphibious_beach(
    provinces: &HashMap<u32, ProvinceInfo>,
    grid: &HexGrid,
    in_province: &[bool],
    cell_pid: &[u32],
    sea: Option<u32>,
) -> Option<Vec<HexCoord>> {
    let (w, h) = (grid.width, grid.height);
    let sea_cell = |c: HexCoord| {
        c.q >= 0
            && c.r >= 0
            && (c.q as usize) < w
            && (c.r as usize) < h
            && provinces
                .get(&cell_pid[c.r as usize * w + c.q as usize])
                .map(|i| i.kind)
                == Some(crate::csv::ProvinceKind::Sea)
            && sea
                .map(|s| cell_pid[c.r as usize * w + c.q as usize] == s)
                .unwrap_or(true)
    };
    // Beach seeds: in-province deployable hexes touching a sea hex. The
    // band deepens from them (multi-source BFS, distance < STRIP_DEPTH).
    let mut dist = vec![u32::MAX; w * h];
    let mut queue: VecDeque<HexCoord> = VecDeque::new();
    for r in 0..h {
        for q in 0..w {
            let c = HexCoord::new(q as i32, r as i32);
            if !deployable_hex(grid, in_province, c) {
                continue;
            }
            if c.neighbors().iter().any(|n| sea_cell(*n)) {
                dist[r * w + q] = 0;
                queue.push_back(c);
            }
        }
    }
    if queue.is_empty() {
        return None;
    }
    let mut beach: Vec<HexCoord> = Vec::new();
    let mut beach_set: HashSet<(i32, i32)> = HashSet::new();
    while let Some(c) = queue.pop_front() {
        let d = dist[c.r as usize * w + c.q as usize];
        if d as usize >= DEPLOY_STRIP_DEPTH {
            continue;
        }
        if beach_set.insert((c.q, c.r)) {
            beach.push(c);
        }
        for n in c.neighbors() {
            if n.q < 0 || n.r < 0 || n.q as usize >= w || n.r as usize >= h {
                continue;
            }
            let i = n.r as usize * w + n.q as usize;
            if dist[i] == u32::MAX && deployable_hex(grid, in_province, n) {
                dist[i] = d + 1;
                queue.push_back(n);
            }
        }
    }
    Some(beach)
}

/// §4.2 step 9 composed sources (landing + overland, melee + overland, …):
/// each staging kind contributes its own part of the attacker zone —
/// interior pockets, the sea beach, the land stitch's strip. The defender
/// rule comes from the DOMINANT kind: with an interior part the defender
/// keeps the anchor blobs (the keypoints must be held — blobs win any
/// overlap with beach/strip hexes), otherwise it holds the rest of the
/// province off the combined attacker zone. `None` when nothing resolved
/// (the caller falls back to the plain edge strips).
#[allow(clippy::too_many_arguments)]
fn compose_zones(
    provinces: &HashMap<u32, ProvinceInfo>,
    grid: &HexGrid,
    in_province: &[bool],
    cell_pid: &[u32],
    interior_battle: bool,
    amphibious: bool,
    stitched: Option<DeploymentZones>,
    attacker_units: usize,
    seed: u64,
) -> Option<DeploymentZones> {
    let mut rng = XorShift64::new(seed);
    let mut claimed: HashSet<(i32, i32)> = HashSet::new();
    let mut attacker: Vec<HexCoord> = Vec::new();
    // 1. The interior part: blobs (→ the defender) then pockets around
    //    them. A cramped/odd interior part is simply absent, not fatal.
    let mut defender: Option<Vec<HexCoord>> = None;
    if interior_battle {
        if let Some((interior, interior_set)) = interior_ground(grid, in_province) {
            if let Some((blobs, blob_set)) =
                interior_blobs(grid, in_province, &interior, &interior_set, &mut rng)
            {
                claimed = blob_set;
                defender = Some(blobs);
                if let Some(pockets) = interior_pockets(
                    grid,
                    in_province,
                    &interior,
                    &interior_set,
                    &claimed,
                    attacker_units,
                    &mut rng,
                ) {
                    for h in pockets {
                        if claimed.insert((h.q, h.r)) {
                            attacker.push(h);
                        }
                    }
                }
            }
        }
    }
    // 2. The sea beach (blobs keep their hexes — the keypoints stay the
    //    defender's).
    if amphibious {
        if let Some(beach) = amphibious_beach(provinces, grid, in_province, cell_pid, None) {
            for h in beach {
                if claimed.insert((h.q, h.r)) {
                    attacker.push(h);
                }
            }
        }
    }
    // 3. The land stitch's strip (its dead-peninsula prune already ran).
    if let Some(st) = stitched {
        for h in st.attacker {
            if claimed.insert((h.q, h.r)) {
                attacker.push(h);
            }
        }
    }
    if attacker.is_empty() && defender.is_none() {
        return None;
    }
    let defender = defender.unwrap_or_else(|| rest_of_province(grid, in_province, &attacker));
    Some(DeploymentZones { attacker, defender })
}

/// §4.2 step 9 staging areas (live battles): each source province's own
/// staging hexes inside the FINAL attacker zone — land origins' strip
/// slices, sea sources' beach segments, the interior pockets. AI /
/// auto-deploy ties each attacking division to the staging area of ITS
/// source province; manual deployment stays free. Empty on fallback /
/// script forms (no source information there).
#[allow(clippy::too_many_arguments)]
fn build_staging(
    provinces: &HashMap<u32, ProvinceInfo>,
    grid: &HexGrid,
    in_province: &[bool],
    cell_pid: &[u32],
    strip_sources: Option<&[u32]>,
    origins: &[u32],
    interior_battle: bool,
    battle: u32,
    final_attacker: &[HexCoord],
) -> Vec<(u32, Vec<HexCoord>)> {
    let Some(sources) = strip_sources else {
        return Vec::new();
    };
    let (w, h) = (grid.width, grid.height);
    let final_set: HashSet<(i32, i32)> = final_attacker.iter().map(|c| (c.q, c.r)).collect();
    let mut staging: Vec<(u32, Vec<HexCoord>)> = Vec::new();
    let mut claimed: HashSet<(i32, i32)> = HashSet::new();
    // Land origins: the final zone's cells that pixel-map to the origin
    // (the strip slice that survived the composition).
    for &o in origins {
        let cells: Vec<HexCoord> = final_attacker
            .iter()
            .copied()
            .filter(|c| cell_pid[c.r as usize * w + c.q as usize] == o)
            .collect();
        if !cells.is_empty() {
            for c in &cells {
                claimed.insert((c.q, c.r));
            }
            staging.push((o, cells));
        }
    }
    // Sea sources: per-sea beach segments (first owner wins an overlap),
    // clipped to the final zone (anchor blobs keep their hexes).
    let mut seas: Vec<u32> = sources
        .iter()
        .copied()
        .filter(|sp| provinces.get(sp).map(|i| i.kind) == Some(crate::csv::ProvinceKind::Sea))
        .collect();
    seas.sort_unstable();
    seas.dedup();
    for s in seas {
        if let Some(segment) = amphibious_beach(provinces, grid, in_province, cell_pid, Some(s)) {
            let cells: Vec<HexCoord> = segment
                .into_iter()
                .filter(|c| final_set.contains(&(c.q, c.r)) && claimed.insert((c.q, c.r)))
                .collect();
            if !cells.is_empty() {
                staging.push((s, cells));
            }
        }
    }
    // Interior: whatever of the final zone is unclaimed (the pockets).
    if interior_battle {
        let pockets: Vec<HexCoord> = final_attacker
            .iter()
            .copied()
            .filter(|c| !claimed.contains(&(c.q, c.r)))
            .collect();
        if !pockets.is_empty() {
            staging.push((battle, pockets));
        }
    }
    staging
}

/// §4.2 step 9 for stitched maps: the attacker deploys on their OWN
/// province's 2px staging STRIP (hexes whose centre pixel lies within
/// ORIGIN_STRIP_PX of the shared border — gifting the whole origin
/// territory inside the bbox ballooned the zone on giant neighbours, e.g.
/// Ioannina 3914); the defender holds the whole BATTLE province.
/// Disjoint by construction; `MIN_ZONE_DISTANCE` no-man's land and the
/// deployability filter apply as in [`deployment_zones`].
fn deployment_zones_stitched(
    grid: &HexGrid,
    battle: u32,
    cell_pid: &[u32],
    in_province: &[bool],
    strip_hex: &[bool],
) -> DeploymentZones {
    let (w, h) = (grid.width, grid.height);
    let deployable = |q: usize, r: usize| {
        in_province[r * w + q]
            && grid
                .cell(HexCoord::new(q as i32, r as i32))
                .map(|c| c.is_passable && c.terrain.is_deployable())
                .unwrap_or(false)
    };

    let mut attacker: Vec<HexCoord> = Vec::new();
    let mut attacker_set: HashSet<(i32, i32)> = HashSet::new();
    // Battle bbox in hex space (for the defender's central third).
    let (mut bq0, mut bq1, mut br0, mut br1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
    for r in 0..h {
        for q in 0..w {
            let i = r * w + q;
            if !in_province[i] {
                continue;
            }
            if strip_hex[i] {
                if deployable(q, r) {
                    let c = HexCoord::new(q as i32, r as i32);
                    if attacker_set.insert((c.q, c.r)) {
                        attacker.push(c);
                    }
                }
            } else if cell_pid[i] == battle {
                bq0 = bq0.min(q as i32);
                bq1 = bq1.max(q as i32);
                br0 = br0.min(r as i32);
                br1 = br1.max(r as i32);
            }
        }
    }

    let mut defender = Vec::new();
    if bq0 <= bq1 {
        // The defender owns the WHOLE battle province — the central-third
        // window was a single-province (pre-stitching) concept, and on
        // small urban provinces the shifted third could slide off the city
        // (Warsaw 3544 is 108 px; its 32-hex VP city sat outside the
        // defender zone). On stitched maps the attacker deploys on origin
        // soil, so the two zones are disjoint by construction; only
        // MIN_ZONE_DISTANCE's no-man's land remains.
        for r in 0..h {
            for q in 0..w {
                let i = r * w + q;
                let (qq, rr) = (q as i32, r as i32);
                if in_province[i]
                    && cell_pid[i] == battle
                    && deployable(q, r)
                    && !attacker_set.contains(&(qq, rr))
                {
                    defender.push(HexCoord::new(qq, rr));
                }
            }
        }
    }
    let defender = tactical_core::grid::filter_min_distance(
        defender,
        &attacker,
        tactical_core::grid::MIN_ZONE_DISTANCE,
    );
    // The staging strip can be cut into a dead PENINSULA — a land
    // component bounded by water (Viipuri 9206's SE axis: the Viipuri Bay)
    // and the bbox edge, with no passable path to the battle province
    // (30/489 strip hexes at 9206, AI units deployed there held forever
    // and never marched to the front). Keep only strip hexes that are
    // passable-connected to the defender zone; on the impossible case that
    // the filter empties the zone, keep the original strip rather than
    // hand the battle an empty deployment zone.
    if !defender.is_empty() {
        let mut seen = vec![false; w * h];
        let mut stack: Vec<HexCoord> = defender.to_vec();
        for c in &stack {
            seen[c.r as usize * w + c.q as usize] = true;
        }
        while let Some(c) = stack.pop() {
            for n in c.neighbors() {
                if n.q < 0 || n.r < 0 || n.q as usize >= w || n.r as usize >= h {
                    continue;
                }
                let i = n.r as usize * w + n.q as usize;
                if seen[i] {
                    continue;
                }
                if grid.cell(n).map(|c| c.is_passable).unwrap_or(false) {
                    seen[i] = true;
                    stack.push(n);
                }
            }
        }
        let connected: Vec<HexCoord> = attacker
            .iter()
            .copied()
            .filter(|c| seen[c.r as usize * w + c.q as usize])
            .collect();
        if !connected.is_empty() {
            attacker = connected;
        }
    }

    DeploymentZones { attacker, defender }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::Adjacency;

    const RED: u32 = 0xFF0000; // province 1 (forest)
    const GREEN: u32 = 0x00FF00; // province 2 (plains)
    const BLUE: u32 = 0x0000FF; // province 3 (mountain)

    fn color_of(id: u32) -> u32 {
        match id {
            1 => RED,
            2 => GREEN,
            3 => BLUE,
            _ => 0,
        }
    }

    fn defs() -> HashMap<u32, ProvinceInfo> {
        [
            (1u32, Terrain::Forest),
            (2, Terrain::Plains),
            (3, Terrain::Mountain),
        ]
        .into_iter()
        .map(|(id, terrain)| {
            (
                id,
                ProvinceInfo {
                    id,
                    kind: crate::csv::ProvinceKind::Land,
                    terrain,
                    is_coastal: false,
                    continent_id: 1,
                    rgb: match id {
                        1 => (255, 0, 0),
                        2 => (0, 255, 0),
                        _ => (0, 0, 255),
                    },
                },
            )
        })
        .collect()
    }

    /// World where province `id` fills the inclusive pixel rect
    /// (x0, y0, x1, y1) and province 2 fills the background.
    fn world(w: u32, h: u32, rect: (u32, u32, u32, u32), id: u32) -> MapGenerator {
        let (x0, y0, x1, y1) = rect;
        let colors: Vec<u32> = (0..h)
            .flat_map(|y| {
                (0..w).map(move |x| {
                    if x >= x0 && x <= x1 && y >= y0 && y <= y1 {
                        color_of(id)
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        MapGenerator::new(
            ProvinceMap::from_colors(w, h, colors).unwrap(),
            defs(),
            vec![],
        )
    }

    #[test]
    fn a_duplicated_palette_colour_resolves_deterministically() {
        // definition.csv is expected to keep its RGBs unique, but when two
        // rows share one colour the winner must not depend on the HashMap's
        // per-process iteration order (same seed → same map, §4.2).
        const SHARED: u32 = 0x0A0B0C;
        let defs: HashMap<u32, ProvinceInfo> = (1u32..=64)
            .map(|id| {
                (
                    id,
                    ProvinceInfo {
                        id,
                        kind: crate::csv::ProvinceKind::Land,
                        terrain: Terrain::Plains,
                        is_coastal: false,
                        continent_id: 1,
                        rgb: (10, 11, 12),
                    },
                )
            })
            .collect();
        let colors = vec![SHARED; 16 * 16];
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(16, 16, colors).unwrap(),
            defs,
            vec![],
        );
        assert!(
            gen.ids.iter().all(|id| *id == 1),
            "the lowest id owns a duplicated colour, got {:?}",
            gen.ids.iter().max()
        );
    }

    /// §4.2 step 3b: live battles stage from the source
    /// provinces' FULL shared border — no 60°-sector clip, no sliver
    /// minimum; unresolvable source lists fall back to the sector
    /// heuristic (dirs-only script battles).
    #[test]
    fn resolve_origins_identity_uses_full_border() {
        let gen = world(40, 40, (18, 18, 21, 21), 1);
        // One neighbour whose shared border with the battle province is
        // L-shaped as seen from the bbox midpoint: a 21 px west arm plus
        // a 21 px north arm (centre at (10, 10)). The west arm's middle
        // lands in the W sector, its ends in NW/SW; the north arm lands
        // in NW/NE — so a dirs=W sector clip keeps only part of the west
        // arm while the identity path keeps everything.
        let mut border: Vec<(u32, u32)> = (0..=20).map(|y| (0u32, y)).collect();
        border.extend((0..=20).map(|x| (x, 0u32)));
        let mut borders = HashMap::new();
        borders.insert(3u32, border);
        // Identity mode (live): the whole L-shaped border stages.
        let (origins, strips) =
            gen.resolve_origins(&borders, 10.0, 10.0, &[HexDirection::W], Some(&[3]));
        assert_eq!(origins, vec![3]);
        assert_eq!(strips[&3].len(), 42);
        // Sector fallback (script dirs=W): clipped to the W-sector pixels
        // of the west arm only — the old lossy behaviour, kept for
        // dirs-only battles.
        let (origins, strips) = gen.resolve_origins(&borders, 10.0, 10.0, &[HexDirection::W], None);
        assert_eq!(origins, vec![3]);
        assert_eq!(strips[&3].len(), 11);
        // A listed source that does not border the province at all falls
        // back to the sector heuristic too.
        let (origins, strips) =
            gen.resolve_origins(&borders, 10.0, 10.0, &[HexDirection::W], Some(&[999]));
        assert_eq!(origins, vec![3]);
        assert_eq!(strips[&3].len(), 11);
    }

    /// §4.2 step 3b: a 1–2 px total contact still stages in
    /// identity mode — the attack has nowhere else to come through.
    #[test]
    fn resolve_origins_identity_keeps_sliver_border() {
        let gen = world(40, 40, (18, 18, 21, 21), 1);
        let mut borders = HashMap::new();
        borders.insert(3u32, vec![(0u32, 10u32), (1, 10)]);
        let (origins, strips) =
            gen.resolve_origins(&borders, 10.0, 10.0, &[HexDirection::W], Some(&[3]));
        assert_eq!(origins, vec![3]);
        assert_eq!(strips[&3].len(), 2);
        // The sector heuristic keeps its ≥3 px sliver skip (unchanged
        // script behaviour) — both via an unresolvable source list and
        // directly.
        let (origins, strips) =
            gen.resolve_origins(&borders, 10.0, 10.0, &[HexDirection::W], Some(&[999]));
        assert!(origins.is_empty());
        assert!(strips.is_empty());
        let (origins, strips) = gen.resolve_origins(&borders, 10.0, 10.0, &[HexDirection::W], None);
        assert!(origins.is_empty());
        assert!(strips.is_empty());
    }

    /// §4.2 step 3b end-to-end: a live-style battle stitches
    /// the source province in along the FULL shared border and the
    /// attacker's zone samples only source-province strip pixels.
    #[test]
    fn generate_from_sources_stages_full_border_strip() {
        // Province 1 (battle) = 16×16 px rect; province 3 (source) = a
        // 4 px strip along its full west edge; province 2 fills the rest.
        let (w, h) = (48u32, 48u32);
        let colors: Vec<u32> = (0..h)
            .flat_map(|y| {
                (0..w).map(move |x| {
                    if (16..=31).contains(&x) && (16..=31).contains(&y) {
                        RED
                    } else if (12..=15).contains(&x) && (16..=31).contains(&y) {
                        BLUE
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(w, h, colors).unwrap(),
            defs(),
            vec![],
        );
        let tm = gen
            .generate_from_sources(1, &[3], 10, &[HexDirection::W], 7)
            .unwrap();
        assert_eq!(tm.origin_provinces, vec![3]);
        assert!(!tm.zones.attacker.is_empty());
        // Every attacker hex samples a staging-strip pixel of the source
        // province — the strip hugs the real shared border.
        let cols = tm.grid.width;
        assert!(tm
            .zones
            .attacker
            .iter()
            .all(|c| tm.cell_province[c.r as usize * cols + c.q as usize] == 3));
    }

    /// Interior battle: a source list naming the BATTLE province itself
    /// (paradrop / civil war / entrapment) skips the stitching, sets the
    /// province-boundary retreat rim, and every zone hex stays on the
    /// battle province's own soil.
    #[test]
    fn interior_battle_skips_stitching_and_sets_the_rim() {
        let gen = world(48, 48, (16, 16, 31, 31), 1);
        let tm = gen
            .generate_from_sources(1, &[1], 10, &[HexDirection::W], 7)
            .unwrap();
        assert!(tm.origin_provinces.is_empty(), "no origin stitch");
        let rim = tm.retreat_rim.expect("interior battle sets the rim");
        assert!(!rim.is_empty());
        assert!(!tm.zones.attacker.is_empty() && !tm.zones.defender.is_empty());
        let cols = tm.grid.width;
        for c in tm.zones.attacker.iter().chain(tm.zones.defender.iter()) {
            assert_eq!(tm.cell_province[c.r as usize * cols + c.q as usize], 1);
        }
        let attacker: HashSet<(i32, i32)> = tm.zones.attacker.iter().map(|c| (c.q, c.r)).collect();
        assert!(tm
            .zones
            .defender
            .iter()
            .all(|c| !attacker.contains(&(c.q, c.r))));
    }

    /// The defender blob MUST hold the keypoints: with a VP urban cluster
    /// on the map, every urban hex lands in the defender zone.
    #[test]
    fn interior_city_blob_contains_the_vp_cluster() {
        let mut grid = HexGrid::new(30, 30, Terrain::Plains);
        for r in 14..=16 {
            for q in 14..=16 {
                grid.set_terrain(HexCoord::new(q, r), Terrain::Urban);
            }
        }
        let in_province = vec![true; 30 * 30];
        let zones = deployment_zones_interior(&grid, &in_province, 12, 7)
            .expect("roomy province takes the interior form");
        let defender: HashSet<(i32, i32)> = zones.defender.iter().map(|c| (c.q, c.r)).collect();
        for r in 14..=16 {
            for q in 14..=16 {
                assert!(
                    defender.contains(&(q, r)),
                    "VP urban hex ({q},{r}) must deploy the defender"
                );
            }
        }
        assert!(!zones.attacker.is_empty(), "the drop pockets exist too");
    }

    /// Interior zones are disjoint, deployable-only and deterministic per
    /// seed.
    #[test]
    fn interior_zones_are_disjoint_deployable_and_deterministic() {
        let grid = HexGrid::new(30, 30, Terrain::Plains);
        let in_province = vec![true; 30 * 30];
        let a = deployment_zones_interior(&grid, &in_province, 12, 42).unwrap();
        let b = deployment_zones_interior(&grid, &in_province, 12, 42).unwrap();
        assert_eq!(a.attacker, b.attacker);
        assert_eq!(a.defender, b.defender);
        let attacker: HashSet<(i32, i32)> = a.attacker.iter().map(|c| (c.q, c.r)).collect();
        assert_eq!(attacker.len(), a.attacker.len(), "duplicates");
        for c in a.attacker.iter().chain(a.defender.iter()) {
            let cell = grid.cell(*c).unwrap();
            assert!(cell.is_passable && cell.terrain.is_deployable());
        }
        assert!(a.defender.iter().all(|c| !attacker.contains(&(c.q, c.r))));
    }

    /// Field battle (no VP city): the defender blobs feed the flag
    /// sampler — three well-separated anchors, all inside the defender
    /// zone, so the keypoints are defended by construction.
    #[test]
    fn interior_field_blobs_feed_the_flag_sampler() {
        let grid = HexGrid::new(40, 40, Terrain::Plains);
        let in_province = vec![true; 40 * 40];
        let zones = deployment_zones_interior(&grid, &in_province, 12, 7).unwrap();
        let params = tactical_core::params::CombatParams::default();
        let mut rng = XorShift64::new(99);
        let flags = tactical_core::flag::derive_flag_state(
            &grid,
            &zones.attacker,
            &zones.defender,
            &[],
            &params,
            &mut rng,
        )
        .expect("the interior blobs must feed the flag sampler");
        assert_eq!(flags.flags.len(), 3, "field battle keeps three flags");
        let defender: HashSet<(i32, i32)> = zones.defender.iter().map(|c| (c.q, c.r)).collect();
        for f in &flags.flags {
            assert!(
                defender.contains(&(f.anchor.q, f.anchor.r)),
                "flag anchor must sit in the defender zone"
            );
        }
    }

    /// The pockets house the attacking force: capacity scales with the
    /// battalion hint.
    #[test]
    fn interior_pockets_house_the_attacking_force() {
        let grid = HexGrid::new(40, 40, Terrain::Plains);
        let in_province = vec![true; 40 * 40];
        let small = deployment_zones_interior(&grid, &in_province, 12, 7).unwrap();
        assert!(small.attacker.len() >= 12);
        let big = deployment_zones_interior(&grid, &in_province, 40, 7).unwrap();
        assert!(
            big.attacker.len() >= 40,
            "40 battalions need 40 hexes, got {}",
            big.attacker.len()
        );
        assert!(big.attacker.len() > small.attacker.len());
    }

    /// A cramped province cannot take the interior form — the caller
    /// falls back to the plain edge strips.
    #[test]
    fn cramped_province_rejects_the_interior_form() {
        let grid = HexGrid::new(4, 4, Terrain::Plains);
        let in_province = vec![true; 4 * 4];
        assert!(deployment_zones_interior(&grid, &in_province, 12, 7).is_none());
        // Just above the floor but far too tight for margin-3 blob
        // centres: still no interior form.
        let grid = HexGrid::new(6, 5, Terrain::Plains);
        let in_province = vec![true; 6 * 5];
        assert!(deployment_zones_interior(&grid, &in_province, 12, 7).is_none());
    }

    /// Cramped fixture helpers for the amphibious form: a 20×20 plains
    /// grid with a SEA province (id 9) west of x<3 and south of y≥17.
    fn amphibious_fixture() -> (HashMap<u32, ProvinceInfo>, HexGrid, Vec<bool>, Vec<u32>) {
        let sea = ProvinceInfo {
            id: 9,
            kind: crate::csv::ProvinceKind::Sea,
            terrain: Terrain::Plains,
            is_coastal: false,
            continent_id: 1,
            rgb: (0, 0, 128),
        };
        let mut provinces = defs();
        provinces.insert(9, sea);
        let grid = HexGrid::new(20, 20, Terrain::Plains);
        let mut in_province = vec![true; 20 * 20];
        let mut cell_pid = vec![2u32; 20 * 20];
        for r in 0..20usize {
            for q in 0..20usize {
                if q < 3 || r >= 17 {
                    in_province[r * 20 + q] = false;
                    cell_pid[r * 20 + q] = 9;
                }
            }
        }
        (provinces, grid, in_province, cell_pid)
    }

    /// The landing band wraps EVERY sea shore (west AND south here — the
    /// plain fallback could only offer one compass edge), and the defender
    /// holds the rest of the province, MIN_ZONE_DISTANCE off the beaches.
    #[test]
    fn amphibious_beach_wraps_every_sea_shore_and_defender_holds_the_rest() {
        let (provinces, grid, in_province, cell_pid) = amphibious_fixture();
        let zones = deployment_zones_amphibious(&provinces, &grid, &in_province, &cell_pid)
            .expect("a sea shore takes the amphibious form");
        let attacker: HashSet<(i32, i32)> = zones.attacker.iter().map(|c| (c.q, c.r)).collect();
        // West beach (3-deep from the x=3 shoreline) AND south beach —
        // "all beachheads", not one edge.
        for r in 0..17 {
            for q in 3..=5 {
                assert!(attacker.contains(&(q, r)), "west beach ({q},{r}) missing");
            }
        }
        for r in 14..=16 {
            for q in 6..=19 {
                assert!(attacker.contains(&(q, r)), "south beach ({q},{r}) missing");
            }
        }
        // The defender holds the rest: far inland deploys, the beach does
        // not, and the no-man's land is respected.
        let defender: HashSet<(i32, i32)> = zones.defender.iter().map(|c| (c.q, c.r)).collect();
        assert!(defender.contains(&(15, 5)));
        assert!(!defender.contains(&(3, 5)));
        assert!(attacker.iter().all(|&(aq, ar)| {
            defender.iter().all(|&(dq, dr)| {
                HexCoord::new(aq, ar).distance(HexCoord::new(dq, dr))
                    >= tactical_core::grid::MIN_ZONE_DISTANCE
            })
        }));
    }

    /// No VP city on the map: the three flag anchors redistribute into the
    /// new defender zone — none of them lands on a beachhead.
    #[test]
    fn amphibious_field_flags_redistribute_into_the_new_zone() {
        let (provinces, grid, in_province, cell_pid) = amphibious_fixture();
        let zones =
            deployment_zones_amphibious(&provinces, &grid, &in_province, &cell_pid).unwrap();
        let params = tactical_core::params::CombatParams::default();
        let mut rng = XorShift64::new(99);
        let flags = tactical_core::flag::derive_flag_state(
            &grid,
            &zones.attacker,
            &zones.defender,
            &[],
            &params,
            &mut rng,
        )
        .expect("the landing zone feeds the flag sampler");
        assert_eq!(flags.flags.len(), 3);
        let defender: HashSet<(i32, i32)> = zones.defender.iter().map(|c| (c.q, c.r)).collect();
        for f in &flags.flags {
            assert!(defender.contains(&(f.anchor.q, f.anchor.r)));
            assert!(f.anchor.q > 5, "no anchor on a west beachhead");
        }
    }

    /// A landlocked province has no beach — the caller falls back.
    #[test]
    fn landlocked_province_rejects_the_amphibious_form() {
        let provinces = defs();
        let grid = HexGrid::new(20, 20, Terrain::Plains);
        let in_province = vec![true; 20 * 20];
        let cell_pid = vec![2u32; 20 * 20];
        assert!(deployment_zones_amphibious(&provinces, &grid, &in_province, &cell_pid).is_none());
    }

    /// A SEA source province triggers the amphibious form end to end: no
    /// origin stitch, the band hugs the shore, the defender holds the rest.
    #[test]
    fn sea_source_triggers_the_amphibious_form() {
        // Province 1 (battle) = 16×16 px rect; province 9 (sea) = a 4 px
        // strip along its full west edge; province 2 fills the rest.
        let (w, h) = (48u32, 48u32);
        let colors: Vec<u32> = (0..h)
            .flat_map(|y| {
                (0..w).map(move |x| {
                    if (16..=31).contains(&x) && (16..=31).contains(&y) {
                        RED
                    } else if (12..=15).contains(&x) && (16..=31).contains(&y) {
                        0x000080
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let mut provinces = defs();
        provinces.insert(
            9,
            ProvinceInfo {
                id: 9,
                kind: crate::csv::ProvinceKind::Sea,
                terrain: Terrain::Plains,
                is_coastal: false,
                continent_id: 1,
                rgb: (0, 0, 128),
            },
        );
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(w, h, colors).unwrap(),
            provinces,
            vec![],
        );
        let tm = gen
            .generate_from_sources(1, &[9], 12, &[HexDirection::W], 7)
            .unwrap();
        assert!(tm.origin_provinces.is_empty(), "the sea is no origin");
        assert!(tm.retreat_rim.is_none(), "not an interior battle");
        assert!(!tm.zones.attacker.is_empty() && !tm.zones.defender.is_empty());
        let cols = tm.grid.width;
        // The band hugs the battle province's west (sea-facing) edge.
        let min_q = zones_min_q(&tm.zones.attacker);
        let max_q = tm.zones.attacker.iter().map(|c| c.q).max().unwrap();
        assert!(
            max_q - min_q < DEPLOY_STRIP_DEPTH as i32,
            "a strip-depth band"
        );
        assert!(tm
            .zones
            .attacker
            .iter()
            .all(|c| tm.cell_province[c.r as usize * cols + c.q as usize] == 1));
        // …and the defender reaches past it (the rest of the province).
        let defender_max_q = tm.zones.defender.iter().map(|c| c.q).max().unwrap();
        assert!(defender_max_q > max_q + tactical_core::grid::MIN_ZONE_DISTANCE);
    }

    /// Smallest attacker-q in a zone (test helper).
    fn zones_min_q(zone: &[HexCoord]) -> i32 {
        zone.iter().map(|c| c.q).min().unwrap_or(0)
    }

    /// Mixed-source fixture: battle province 1 (16×16 px rect), SEA
    /// province 9 along its west edge, LAND province 3 along its east
    /// edge, province 2 filling the rest.
    fn combination_world() -> MapGenerator {
        let (w, h) = (48u32, 48u32);
        let colors: Vec<u32> = (0..h)
            .flat_map(|y| {
                (0..w).map(move |x| {
                    if (16..=31).contains(&x) && (16..=31).contains(&y) {
                        RED
                    } else if (12..=15).contains(&x) && (16..=31).contains(&y) {
                        0x000080
                    } else if (32..=35).contains(&x) && (16..=31).contains(&y) {
                        BLUE
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let mut provinces = defs();
        provinces.insert(
            9,
            ProvinceInfo {
                id: 9,
                kind: crate::csv::ProvinceKind::Sea,
                terrain: Terrain::Plains,
                is_coastal: false,
                continent_id: 1,
                rgb: (0, 0, 128),
            },
        );
        MapGenerator::new(
            ProvinceMap::from_colors(w, h, colors).unwrap(),
            provinces,
            vec![],
        )
    }

    /// Landing + overland attack at once: the beach AND the land stitch's
    /// strip both stage — neither swallows the other; the defender holds
    /// the rest of the province.
    #[test]
    fn sea_and_land_sources_compose_beach_and_strip() {
        let gen = combination_world();
        let tm = gen
            .generate_from_sources(1, &[9, 3], 24, &[HexDirection::W], 7)
            .unwrap();
        assert_eq!(
            tm.origin_provinces,
            vec![3],
            "only the land source stitches"
        );
        assert!(tm.retreat_rim.is_none(), "not an interior battle");
        let cols = tm.grid.width;
        let cell = |c: HexCoord| tm.cell_province[c.r as usize * cols + c.q as usize];
        let sea_adjacent = |c: HexCoord| {
            c.neighbors().iter().any(|n| {
                n.q >= 0
                    && n.r >= 0
                    && (n.q as usize) < cols
                    && (n.r as usize) < tm.grid.height
                    && tm.cell_province[n.r as usize * cols + n.q as usize] == 9
            })
        };
        assert!(
            tm.zones.attacker.iter().any(|&c| cell(c) == 3),
            "the overland strip stages on province 3 soil"
        );
        assert!(
            tm.zones
                .attacker
                .iter()
                .any(|&c| cell(c) == 1 && sea_adjacent(c)),
            "the landing force stages on the beach"
        );
        // The defender holds the rest: inland hexes deploy, zones disjoint.
        assert!(tm.zones.defender.iter().any(|&c| cell(c) == 1));
        let attacker: HashSet<(i32, i32)> = tm.zones.attacker.iter().map(|c| (c.q, c.r)).collect();
        assert!(tm
            .zones
            .defender
            .iter()
            .all(|c| !attacker.contains(&(c.q, c.r))));
    }

    /// Melee + overland attack at once (paradrop / civil war reinforced
    /// from outside): the defender keeps its anchor blobs (the keypoints
    /// stay its — and feed the flag sampler), the attacker gets pockets
    /// AND the overland strip.
    #[test]
    fn interior_and_land_sources_compose_pockets_and_strip() {
        let gen = combination_world();
        let tm = gen
            .generate_from_sources(1, &[1, 3], 24, &[HexDirection::W], 7)
            .unwrap();
        assert_eq!(tm.origin_provinces, vec![3]);
        assert!(tm.retreat_rim.is_some(), "interior part → boundary rim");
        let cols = tm.grid.width;
        let cell = |c: HexCoord| tm.cell_province[c.r as usize * cols + c.q as usize];
        assert!(
            tm.zones.attacker.iter().any(|&c| cell(c) == 3),
            "the overland strip stages"
        );
        assert!(
            tm.zones.attacker.iter().any(|&c| cell(c) == 1),
            "interior pockets stage inside the province"
        );
        // The defender rule is the interior one: blobs on the battle
        // province only, NOT the whole province (neutral ground remains),
        // and they feed the flag sampler with all anchors inside.
        assert!(tm.zones.defender.iter().all(|&c| cell(c) == 1));
        let attacker: HashSet<(i32, i32)> = tm.zones.attacker.iter().map(|c| (c.q, c.r)).collect();
        assert!(tm
            .zones
            .defender
            .iter()
            .all(|c| !attacker.contains(&(c.q, c.r))));
        let params = tactical_core::params::CombatParams::default();
        let mut rng = XorShift64::new(99);
        let flags = tactical_core::flag::derive_flag_state(
            &tm.grid,
            &tm.zones.attacker,
            &tm.zones.defender,
            &[],
            &params,
            &mut rng,
        )
        .expect("blobs feed the flag sampler");
        let defender: HashSet<(i32, i32)> = tm.zones.defender.iter().map(|c| (c.q, c.r)).collect();
        for f in &flags.flags {
            assert!(defender.contains(&(f.anchor.q, f.anchor.r)));
        }
    }

    /// The staging areas partition the final attacker zone: each source's
    /// own slice, disjoint, union = the whole zone.
    #[test]
    fn staging_union_equals_the_attacker_zone() {
        let gen = combination_world();
        let tm = gen
            .generate_from_sources(1, &[9, 3], 24, &[HexDirection::W], 7)
            .unwrap();
        assert!(!tm.staging.is_empty());
        let attacker: HashSet<(i32, i32)> = tm.zones.attacker.iter().map(|c| (c.q, c.r)).collect();
        let mut union: HashSet<(i32, i32)> = HashSet::new();
        let cols = tm.grid.width;
        for (src, cells) in &tm.staging {
            assert!(!cells.is_empty());
            for c in cells {
                assert!(
                    attacker.contains(&(c.q, c.r)),
                    "staging hex outside the zone"
                );
                assert!(union.insert((c.q, c.r)), "staging entries overlap");
            }
            if *src == 3 {
                assert!(cells
                    .iter()
                    .all(|c| tm.cell_province[c.r as usize * cols + c.q as usize] == 3));
            }
        }
        assert!(tm.staging.iter().any(|(s, _)| *s == 3), "land strip slice");
        assert!(tm.staging.iter().any(|(s, _)| *s == 9), "sea beach segment");
        assert_eq!(
            union, attacker,
            "staging partitions the whole attacker zone"
        );
    }

    /// A two-sea landing: each sea gets its own beach segment, disjoint,
    /// union = the whole band.
    #[test]
    fn staging_partitions_a_two_sea_landing() {
        // Battle province 1 rect; sea 9 west, sea 10 south; province 2 rest.
        let (w, h) = (48u32, 48u32);
        let colors: Vec<u32> = (0..h)
            .flat_map(|y| {
                (0..w).map(move |x| {
                    if (16..=31).contains(&x) && (16..=31).contains(&y) {
                        RED
                    } else if (12..=15).contains(&x) && (16..=31).contains(&y) {
                        0x000080
                    } else if (16..=31).contains(&x) && (32..=35).contains(&y) {
                        0x400000
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let mut provinces = defs();
        for (id, rgb) in [(9u32, (0, 0, 128)), (10, (64, 0, 0))] {
            provinces.insert(
                id,
                ProvinceInfo {
                    id,
                    kind: crate::csv::ProvinceKind::Sea,
                    terrain: Terrain::Plains,
                    is_coastal: false,
                    continent_id: 1,
                    rgb,
                },
            );
        }
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(w, h, colors).unwrap(),
            provinces,
            vec![],
        );
        let tm = gen
            .generate_from_sources(1, &[9, 10], 24, &[HexDirection::W], 7)
            .unwrap();
        assert!(tm.staging.iter().any(|(s, _)| *s == 9));
        assert!(tm.staging.iter().any(|(s, _)| *s == 10));
        let attacker: HashSet<(i32, i32)> = tm.zones.attacker.iter().map(|c| (c.q, c.r)).collect();
        let mut union: HashSet<(i32, i32)> = HashSet::new();
        for (_, cells) in &tm.staging {
            for c in cells {
                assert!(attacker.contains(&(c.q, c.r)));
                assert!(union.insert((c.q, c.r)), "beach segments overlap");
            }
        }
        assert_eq!(union, attacker, "segments partition the whole band");
    }

    /// Script battles carry no source information — no staging areas.
    #[test]
    fn script_battles_carry_no_staging() {
        let gen = world(40, 40, (18, 18, 21, 21), 1);
        let tm = gen
            .generate_with_elevation_seed(1, &[HexDirection::W], 7)
            .unwrap();
        assert!(tm.staging.is_empty());
    }

    #[test]
    fn grid_dimensions_math_and_caps() {
        // 4×4 px province; the shoreline margin expands the bbox to 6×6 px.
        // Uniform bitmap scale: no latitude term.
        let gen = world(40, 40, (18, 18, 21, 21), 1);
        let tm = gen.generate(1, &[]).unwrap();
        // width  = 6 × 7.114  ≈ 42.68 → 43 columns
        // height = 6 × 8.2145 ≈ 49.29 → 50 rows (8.2145 = 7.114 × 2/√3,
        // the pointy-top row-packing factor, §4.3 revised)
        assert_eq!(tm.grid.width, 43);
        assert_eq!(tm.grid.height, 50);

        // 10×12 px province → margin bbox 12×14 px → 86×116
        // (fits under the 512 cap).
        let gen = world(40, 40, (10, 14, 19, 25), 1);
        let tm = gen.generate(1, &[]).unwrap();
        assert_eq!(tm.grid.width, 86);
        assert_eq!(tm.grid.height, 116);

        // Latitude regression: latitude no longer affects grid size — the
        // same 4×4 px province near the "pole" of a 60 px tall bitmap must
        // produce the SAME 43×50 grid (the removed cos(latitude) correction
        // squeezed this to 23×59).
        let gen = world(60, 60, (20, 48, 23, 51), 1);
        let tm = gen.generate(1, &[]).unwrap();
        assert_eq!(tm.grid.width, 43, "width must not shrink at high latitude");
        assert_eq!(tm.grid.height, 50);
    }

    #[test]
    fn land_province_areas_counts_and_filters() {
        // Province 1 = the 4×4 rect (16 px), province 2 = the background.
        let gen = world(40, 40, (18, 18, 21, 21), 1);
        let areas: HashMap<u32, u32> = gen.land_province_areas().into_iter().collect();
        assert_eq!(areas.get(&1), Some(&16));
        assert_eq!(areas.get(&2), Some(&(40 * 40 - 16)));
        // Province 3 has no pixels in the bitmap → not listed.
        assert!(!areas.contains_key(&3));

        // Sea provinces are excluded even when present in the bitmap.
        let mut sea_defs = defs();
        sea_defs.get_mut(&2).unwrap().kind = crate::csv::ProvinceKind::Sea;
        let colors: Vec<u32> = (0..40)
            .flat_map(|y| {
                (0..40).map(move |x| {
                    if (18..=21).contains(&x) && (18..=21).contains(&y) {
                        RED
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors).unwrap(),
            sea_defs,
            vec![],
        );
        let areas: HashMap<u32, u32> = gen.land_province_areas().into_iter().collect();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas.get(&1), Some(&16));
    }

    /// Grid-cap grounding: distribution of bitmap-scale grid sizes over
    /// every land province (uniform 7.114/8.2145 per pixel, no latitude
    /// term), using the same math as `generate()`.
    /// Answers "which provinces exceed MAX_GRID_WIDTH/HEIGHT and what cap
    /// would cover all plausible battle provinces". Run:
    /// `cargo test -p tactical-map land_province_grid_size_stats -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a local Hearts of Iron IV installation"]
    fn land_province_grid_size_stats() {
        let dir = crate::detect_hoi4_dir().expect("HOI4 install not found");
        let map = ProvinceMap::load_bmp(&dir.join("map").join("provinces.bmp")).unwrap();
        let defs = crate::load_definition_csv(&dir.join("map").join("definition.csv")).unwrap();
        let adjs = crate::load_adjacencies_csv(&dir.join("map").join("adjacencies.csv")).unwrap();
        let gen = MapGenerator::new(map, defs, adjs);

        // Impassable scan: current HOI4 marks impassability at the STATE
        // level — `impassable = yes` in history/states/*.txt (Interior
        // Borneo, Papua, central Australia, Amazon pockets…). Every land
        // province belongs to some state since NSB, so "not in any state"
        // is NOT a usable wasteland discriminator.
        let mut impassable: HashSet<u32> = HashSet::new();
        for entry in std::fs::read_dir(crate::states_dir_of(&dir))
            .unwrap()
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let no_comments: String = text
                .lines()
                .map(|l| l.split('#').next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            let tokens: Vec<&str> = no_comments
                .split(|c: char| c.is_whitespace() || c == '{' || c == '}' || c == '=')
                .filter(|t| !t.is_empty())
                .collect();
            if !tokens
                .windows(2)
                .any(|w| w[0] == "impassable" && w[1] == "yes")
            {
                continue;
            }
            let mut i = 0;
            while i < tokens.len() {
                if tokens[i] == "provinces" {
                    let mut j = i + 1;
                    while j < tokens.len() {
                        let Ok(pid) = tokens[j].parse::<u32>() else {
                            break;
                        };
                        impassable.insert(pid);
                        j += 1;
                    }
                    i = j.max(i + 1);
                } else {
                    i += 1;
                }
            }
        }

        // Single pass over the id grid: per-province bounding box.
        let (w, h) = (gen.width, gen.height);
        let mut bbox: HashMap<u32, (u32, u32, u32, u32)> = HashMap::new();
        for y in 0..h {
            for x in 0..w {
                let id = gen.ids[(y * w + x) as usize];
                if id == 0 {
                    continue;
                }
                let e = bbox.entry(id).or_insert((x, y, x, y));
                e.0 = e.0.min(x);
                e.1 = e.1.min(y);
                e.2 = e.2.max(x);
                e.3 = e.3.max(y);
            }
        }

        // Bitmap-scale grid axes per land province (same math as
        // generate(): uniform scale, no latitude term).
        struct Row {
            id: u32,
            cols: usize,
            rows: usize,
            impassable: bool,
        }
        let mut sizes: Vec<Row> = Vec::new();
        for (id, (x0, y0, x1, y1)) in &bbox {
            if gen.provinces.get(id).map(|i| i.kind) != Some(crate::csv::ProvinceKind::Land) {
                continue;
            }
            let cols = ((*x1 - *x0 + 1) as f32 * KM_PER_PIXEL_X / HEX_SCALE_KM).ceil() as usize;
            let rows = ((*y1 - *y0 + 1) as f32 * KM_PER_PIXEL_Y / HEX_SCALE_KM).ceil() as usize;
            sizes.push(Row {
                id: *id,
                cols,
                rows,
                impassable: impassable.contains(id),
            });
        }
        let report = |label: &str, set: &[&Row]| {
            let mut sorted: Vec<&Row> = set.to_vec();
            sorted.sort_by_key(|r| usize::max(r.cols, r.rows));
            let n = sorted.len();
            if n == 0 {
                return;
            }
            let pct = |p: f64| {
                let r = sorted[((n - 1) as f64 * p) as usize];
                format!("{}x{}", r.cols, r.rows)
            };
            eprintln!(
                "[stats] {label}: {n} provinces, max-axis p50={} p90={} p99={}",
                pct(0.50),
                pct(0.90),
                pct(0.99)
            );
            for cap in [128usize, 256, 384, 512, 1024] {
                let over = sorted
                    .iter()
                    .filter(|r| r.cols > cap || r.rows > cap)
                    .count();
                eprintln!(
                    "[stats] {label} exceeding {cap}: {over} ({:.1}%)",
                    over as f64 * 100.0 / n as f64
                );
            }
        };
        let all: Vec<&Row> = sizes.iter().collect();
        let battleable: Vec<&Row> = sizes.iter().filter(|r| !r.impassable).collect();
        let impass: Vec<&Row> = sizes.iter().filter(|r| r.impassable).collect();
        report("all-land  ", &all);
        report("battleable", &battleable);
        report("impassable", &impass);

        let mut top: Vec<&Row> = battleable.to_vec();
        top.sort_by_key(|r| usize::max(r.cols, r.rows));
        eprintln!("[stats] top 15 BATTLEABLE by max axis:");
        for r in top.iter().rev().take(15) {
            eprintln!("  province {}: {}x{}", r.id, r.cols, r.rows);
        }
    }

    #[test]
    fn occupancy_mask_marks_out_of_province_holes() {
        // Province 1 fills two vertical stripes (x = 14, 15; y = 16..=23)
        // plus one outlier pixel at (17, 20), so its bounding box is
        // x 14..=17 × y 16..=23 while the right half of that box is foreign.
        let colors: Vec<u32> = (0..40)
            .flat_map(|y| {
                (0..40).map(move |x| {
                    if (x == 14 || x == 15) && (16..=23).contains(&y) {
                        RED
                    } else if (x, y) == (17, 20) {
                        RED
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors).unwrap(),
            defs(),
            vec![],
        );
        let tm = gen.generate(1, &[]).unwrap();
        let (cols, rows) = (tm.grid.width, tm.grid.height);
        // §6.14: out-of-province cells are flagged out_of_bounds — land
        // backdrop stays PASSABLE (only water is impassable, and this
        // all-land map has none). Roughly three quarters of the bbox is
        // foreign here (two stripes + one outlier pixel).
        let oob = tm
            .grid
            .iter_coords()
            .filter(|c| tm.grid.cell(*c).unwrap().out_of_bounds)
            .count();
        let total = cols * rows;
        assert!(
            oob > total / 4 && oob < total,
            "expected most of the bbox out of bounds, got {oob}/{total}"
        );
        // Shoreline margin: the bbox grew 1px on every side, so the
        // outermost ring is out-of-province backdrop; the stripes sit
        // further in. Hexes below map to pixels (14,16) = stripe, (16,18) =
        // interior hole, (13,15)/(18,24) = margin ring.
        let margin = tm.grid.cell(HexCoord::new(1, 1)).unwrap();
        assert!(margin.out_of_bounds && margin.is_passable);
        let stripe = tm.grid.cell(HexCoord::new(7, 10)).unwrap();
        assert!(!stripe.out_of_bounds && stripe.is_passable);
        let hole = tm.grid.cell(HexCoord::new(22, 29)).unwrap();
        assert!(hole.out_of_bounds && hole.is_passable);
        let right = tm
            .grid
            .cell(HexCoord::new(cols as i32 - 2, rows as i32 - 2))
            .unwrap();
        assert!(right.out_of_bounds && right.is_passable);
    }

    #[test]
    fn shore_margin_fills_sea_water_and_land_backdrop() {
        // The 1px ring around the province bbox is painted from the real
        // bitmap — a SEA neighbour's ring becomes impassable Terrain::Water
        // (coastlines/lakeshores stay visible), a LAND neighbour's becomes
        // PASSABLE out-of-bounds backdrop terrain (§6.14: never Water,
        // flagged out_of_bounds).
        let colors: Vec<u32> = (0..40)
            .flat_map(|y| {
                (0..40).map(move |x| {
                    if (18..=21).contains(&x) && (18..=21).contains(&y) {
                        RED
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let mut sea_defs = defs();
        sea_defs.get_mut(&2).unwrap().kind = crate::csv::ProvinceKind::Sea;
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors.clone()).unwrap(),
            sea_defs,
            vec![],
        );
        let tm = gen.generate(1, &[]).unwrap();
        let (w, h) = (tm.grid.width as i32, tm.grid.height as i32);
        let mut edge = 0;
        for c in tm.grid.iter_coords() {
            if c.q == 0 || c.r == 0 || c.q == w - 1 || c.r == h - 1 {
                let cell = tm.grid.cell(c).unwrap();
                assert_eq!(cell.terrain, Terrain::Water, "sea ring not water at {c:?}");
                assert!(!cell.is_passable && cell.out_of_bounds);
                edge += 1;
            }
        }
        assert!(edge > 0);

        // Land background → backdrop ring: passable, out_of_bounds, never Water.
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors).unwrap(),
            defs(),
            vec![],
        );
        let tm = gen.generate(1, &[]).unwrap();
        let (w, h) = (tm.grid.width as i32, tm.grid.height as i32);
        for c in tm.grid.iter_coords() {
            if c.q == 0 || c.r == 0 || c.q == w - 1 || c.r == h - 1 {
                let cell = tm.grid.cell(c).unwrap();
                assert_ne!(
                    cell.terrain,
                    Terrain::Water,
                    "land ring painted water at {c:?}"
                );
                assert!(cell.is_passable && cell.out_of_bounds);
            }
        }
    }

    #[test]
    fn shore_margin_island_zones_anchor_to_province_extent() {
        // An island province (sea all around → no land origin → the
        // fallback `deployment_zones` path) must still get non-empty zones
        // anchored to the province extent — grid-edge strips would land in
        // the water ring and come out empty.
        let colors: Vec<u32> = (0..40)
            .flat_map(|y| {
                (0..40).map(move |x| {
                    if (18..=21).contains(&x) && (18..=21).contains(&y) {
                        RED
                    } else {
                        GREEN
                    }
                })
            })
            .collect();
        let mut sea_defs = defs();
        sea_defs.get_mut(&2).unwrap().kind = crate::csv::ProvinceKind::Sea;
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors).unwrap(),
            sea_defs,
            vec![],
        );
        let tm = gen.generate(1, &[HexDirection::E]).unwrap();
        assert!(tm.origin_provinces.is_empty());
        assert!(
            !tm.zones.attacker.is_empty(),
            "attacker zone empty — strips anchored to the water ring?"
        );
        assert!(!tm.zones.defender.is_empty());
        let (w, h) = (tm.grid.width, tm.grid.height);
        for c in tm.zones.attacker.iter().chain(tm.zones.defender.iter()) {
            let i = c.r as usize * w + c.q as usize;
            assert_eq!(
                tm.cell_province[i], 1,
                "zone hex {c:?} outside the province"
            );
        }
        let _ = h;
    }

    #[test]
    fn terrain_variation_is_deterministic() {
        // With per-pixel sectors the old island world (central rect in a
        // background ring) now infers an origin for ANY attack direction —
        // the ring matches every sector — which grows the bbox and breaks
        // the two-grid comparison. Use the east-half world with dirs=[E]:
        // the east side of province 1 is the map edge, no origin is
        // inferred, and both grids stay identical.
        let gen = world(40, 40, (20, 0, 39, 39), 1);
        let a = gen.generate(1, &[]).unwrap();
        let b = gen.generate(1, &[HexDirection::E]).unwrap();
        assert_eq!(a.base_terrain, Terrain::Forest);
        assert_eq!(a.grid.width, b.grid.width);
        assert_eq!(a.grid.height, b.grid.height);
        for c in a.grid.iter_coords() {
            let (ca, cb) = (a.grid.cell(c).unwrap(), b.grid.cell(c).unwrap());
            assert_eq!(ca.terrain, cb.terrain, "terrain differs at {c:?}");
            assert_eq!(ca.elevation, cb.elevation, "elevation differs at {c:?}");
            assert_eq!(ca.is_passable, cb.is_passable);
            assert_eq!(ca.river_edges, cb.river_edges);
        }
        // Hundreds of forest hexes at 70/20/10 → all three variants appear.
        let kinds: HashSet<Terrain> = a
            .grid
            .iter_coords()
            .filter(|c| a.grid.cell(*c).unwrap().is_passable)
            .map(|c| a.grid.cell(c).unwrap().terrain)
            .collect();
        assert!(kinds.contains(&Terrain::Forest), "no forest: {kinds:?}");
        assert!(kinds.contains(&Terrain::Clearing), "no clearing: {kinds:?}");
        assert!(kinds.contains(&Terrain::Hills), "no hills: {kinds:?}");
    }

    #[test]
    fn elevation_noise_is_deterministic_and_bounded() {
        // §4.2 step 8b: the same seed reproduces the same relief; the
        // default (province-keyed) generator is stable across calls; every
        // elevation sits in [-1, 4].
        let gen = world(60, 60, (10, 10, 49, 49), 3); // mountain province
        let a = gen.generate(3, &[]).unwrap();
        let b = gen.generate(3, &[]).unwrap();
        for c in a.grid.iter_coords() {
            let (ca, cb) = (a.grid.cell(c).unwrap(), b.grid.cell(c).unwrap());
            assert_eq!(ca.elevation, cb.elevation, "elevation differs at {c:?}");
            assert!(
                (-1..=4).contains(&ca.elevation),
                "elevation {c:?} = {}",
                ca.elevation
            );
        }
        // Different battle seeds → different relief (the seed-keyed variant).
        let c = gen.generate_with_elevation_seed(3, &[], 1).unwrap();
        let d = gen.generate_with_elevation_seed(3, &[], 2).unwrap();
        let (mut same, mut total) = (0usize, 0usize);
        for h in c.grid.iter_coords() {
            if c.grid.cell(h).unwrap().terrain == Terrain::Mountain {
                total += 1;
                if c.grid.cell(h).unwrap().elevation == d.grid.cell(h).unwrap().elevation {
                    same += 1;
                }
            }
        }
        assert!(total > 100, "mountain cells {total}");
        assert!(
            same < total / 2,
            "seeds 1/2 reliefs coincide {same}/{total}"
        );
    }

    #[test]
    fn mountain_province_gains_peaks_and_valleys() {
        // The whole point: a mountain province is NOT a flat plateau — peaks
        // (≥3) and valley floors (≤1) both exist, so the ridge LOS rule has
        // relief to work with (peaks see over saddles, valleys are shut in).
        let gen = world(60, 60, (10, 10, 49, 49), 3);
        let tm = gen.generate(3, &[]).unwrap();
        let mut peaks = 0usize;
        let mut valleys = 0usize;
        for h in tm.grid.iter_coords() {
            let cell = tm.grid.cell(h).unwrap();
            if cell.terrain == Terrain::Mountain {
                if cell.elevation >= 3 {
                    peaks += 1;
                } else if cell.elevation <= 1 {
                    valleys += 1;
                }
            }
        }
        assert!(
            peaks > 100,
            "too few peaks ({peaks}) — the province is flat"
        );
        assert!(
            valleys > 100,
            "too few valleys ({valleys}) — the province is flat"
        );
    }

    #[test]
    fn plains_province_stays_flat() {
        // Plains/forest/lowland provinces are UNTOUCHED by the noise field —
        // Warsaw/Belgorod-style battles keep their flat open ground.
        let gen = world(40, 40, (10, 14, 19, 25), 2); // plains rect on forest bg
        let tm = gen.generate(2, &[]).unwrap();
        for h in tm.grid.iter_coords() {
            let cell = tm.grid.cell(h).unwrap();
            if cell.terrain == Terrain::Plains || cell.terrain == Terrain::Forest {
                assert_eq!(cell.elevation, 0, "plains/forest elevated at {h:?}");
            }
        }
    }

    #[test]
    fn water_and_river_stay_low() {
        // Marsh/River/Water keep their -1 lowland base — the noise never
        // raises a riverbed above its banks.
        let gen = world(60, 60, (10, 10, 49, 49), 3);
        let mut tm = gen.generate(3, &[]).unwrap();
        let h = HexCoord::new(20, 20);
        if let Some(cell) = tm.grid.cell_mut(h) {
            cell.terrain = Terrain::River;
        }
        tm.grid.set_terrain(h, Terrain::River);
        assert_eq!(tm.grid.cell(h).unwrap().elevation, -1);
    }

    #[test]
    fn forest_variation_matches_spec_distribution() {
        // §4.2 step 8: forest → 70% forest / 20% clearing / 10% rough.
        let (mut forest, mut clearing, mut hills) = (0usize, 0usize, 0usize);
        let total = 2000usize;
        for seed in 0..total {
            let roll = hash01(1, (seed % 50) as i32, (seed / 50) as i32);
            match varied_terrain(Terrain::Forest, roll) {
                Terrain::Forest => forest += 1,
                Terrain::Clearing => clearing += 1,
                Terrain::Hills => hills += 1,
                other => panic!("unexpected terrain {other:?}"),
            }
        }
        let frac = |n: usize| n as f32 / total as f32;
        assert!(
            (frac(forest) - 0.70).abs() < 0.06,
            "forest {}",
            frac(forest)
        );
        assert!(
            (frac(clearing) - 0.20).abs() < 0.05,
            "clearing {}",
            frac(clearing)
        );
        assert!((frac(hills) - 0.10).abs() < 0.04, "hills {}", frac(hills));
    }

    #[test]
    fn deployment_zones_disjoint_and_in_bounds() {
        // Province 1 = a central island rect; province 2 = the background
        // ring around it (the "giant neighbour" shape). With per-pixel
        // sectors the ring matches W + SE (≥3 border pixels in each
        // sector), so the stitched path folds the CLIPPED strip bands —
        // the W band on the island's west, the SE band below its SE
        // corner — instead of the old full-border ring.
        let gen = world(40, 40, (10, 14, 19, 25), 1);
        let tm = gen
            .generate(1, &[HexDirection::W, HexDirection::SE])
            .unwrap();
        assert_eq!(
            tm.origin_provinces,
            vec![2],
            "the ring surrounds the island"
        );
        assert!(!tm.zones.attacker.is_empty());
        assert!(!tm.zones.defender.is_empty());
        assert_eq!(tm.grid.attack_dirs, vec![HexDirection::W, HexDirection::SE]);

        let (w, h) = (tm.grid.width as i32, tm.grid.height as i32);
        let attacker: HashSet<HexCoord> = tm.zones.attacker.iter().copied().collect();
        assert_eq!(attacker.len(), tm.zones.attacker.len(), "duplicates");
        for c in tm.zones.attacker.iter().chain(tm.zones.defender.iter()) {
            assert!(tm.grid.in_bounds(*c), "{c:?} out of bounds");
            assert!(tm.grid.cell(*c).unwrap().is_passable, "{c:?} impassable");
        }
        for c in &tm.zones.defender {
            assert!(!attacker.contains(c), "{c:?} in both zones");
            // The "central third" is threat-biased (slides toward the
            // attack side), so only the structural invariants are asserted
            // here — the bias itself has a dedicated check in
            // stitched_origin_province_zones.
            // No-man's land between the zones:
            let nearest = tm
                .zones
                .attacker
                .iter()
                .map(|a| c.distance(*a))
                .min()
                .unwrap();
            assert!(
                nearest >= tactical_core::grid::MIN_ZONE_DISTANCE,
                "{c:?} only {nearest} from attacker zone"
            );
        }
        // The strip is clipped to the attack sectors: a W band hugging the
        // island's west side and a SE band below its SE corner — NOT the
        // whole ring (which would stretch the map, as 9100's full-border
        // strip did at 9206).
        assert!(tm.zones.attacker.iter().any(|c| c.q < 30), "W band missing");
        assert!(
            tm.zones
                .attacker
                .iter()
                .any(|c| c.r > h - 30 && c.q * 2 >= w),
            "SE band missing"
        );
    }

    /// The per-pixel sector rule: a neighbour qualifies for a direction
    /// when ≥3 of its border PIXELS fall in that direction's sector, and
    /// the staging strip is clipped to those pixels. The old centroid rule
    /// misread Viipuri 9206's SE neighbour 9100 (all its border pixels sit
    /// on the southern half of the east side, whose centroid bearing is E)
    /// and dirs=SE fell back to broken zones.
    #[test]
    fn per_pixel_sector_origin_inference() {
        // Province 1 = a tall central rect; province 2 wraps it. The east
        // border pixels split NE (top) / E (middle) / SE (bottom) — like
        // 9100's southern-half border at 9206.
        let gen = world(40, 40, (18, 6, 21, 34), 1);
        let tm = gen.generate(1, &[HexDirection::SE]).unwrap();
        assert_eq!(tm.origin_provinces, vec![2], "SE-sector pixels must match");
        assert!(!tm.zones.attacker.is_empty());
        // The clipped strip hugs the province's SE sector only: the east
        // border below the vertical middle plus the south edge. The old
        // full-border strip wrapped the province's TOP half as well
        // (stretching the map 147 rows at 9206) — here the top rows stay
        // clear: every attacker hex sits at r ≥ 100 of 303.
        let min_r = tm.zones.attacker.iter().map(|c| c.r).min().unwrap();
        assert!(
            min_r >= 100,
            "attacker hexes must stay off the province's top half, min r = {min_r}"
        );
        for c in &tm.zones.attacker {
            assert!(tm.grid.in_bounds(*c), "{c:?} out of bounds");
        }
    }

    /// The non-stitched fallback (`deployment_zones`) still fires when no
    /// land neighbour has ≥3 border pixels in an attack sector — e.g. the
    /// attack side of a coastal province faces open water.
    #[test]
    fn fallback_zones_when_no_origin_matches() {
        // Province 1 = the east half; province 2 = the west half. Attacking
        // E means the east side of province 1 is the map edge (no neighbour
        // there), so no origin is inferred and the classic corner strips
        // take over.
        let gen = world(40, 40, (20, 0, 39, 39), 1);
        let tm = gen.generate(1, &[HexDirection::E]).unwrap();
        assert!(tm.origin_provinces.is_empty(), "no origin on the map edge");
        assert!(!tm.zones.attacker.is_empty());
        assert!(!tm.zones.defender.is_empty());
        let w = tm.grid.width as i32;
        for c in tm.zones.attacker.iter().chain(tm.zones.defender.iter()) {
            assert!(tm.grid.in_bounds(*c), "{c:?} out of bounds");
        }
        // E attack: attacker hexes hug the east edge, DEPLOY_STRIP_DEPTH deep.
        assert!(tm
            .zones
            .attacker
            .iter()
            .any(|c| c.q + DEPLOY_STRIP_DEPTH as i32 >= w));
        assert!(tm
            .zones
            .defender
            .iter()
            .all(|c| c.q < w - DEPLOY_STRIP_DEPTH as i32));
    }

    #[test]
    fn stitched_origin_province_zones() {
        // East half = province 1 (the battle province), west half =
        // province 2 (the attacker's origin). Attack from W → the origin
        // strip is folded in; the attacker deploys in province 2, the
        // defender in province 1's central third.
        let gen = world(40, 40, (20, 0, 39, 39), 1);
        let tm = gen.generate(1, &[HexDirection::W]).unwrap();
        assert_eq!(tm.origin_provinces, vec![2]);
        let w = tm.grid.width;
        // Grid covers the battle province + the origin strip (wider than
        // the battle bbox alone).
        assert!(!tm.zones.attacker.is_empty());
        for c in &tm.zones.attacker {
            let i = c.r as usize * w + c.q as usize;
            assert_eq!(
                tm.cell_province[i], 2,
                "attacker {c:?} not in origin province"
            );
            assert!(tm.grid.cell(*c).unwrap().is_passable, "{c:?} impassable");
        }
        assert!(!tm.zones.defender.is_empty());
        for c in &tm.zones.defender {
            let i = c.r as usize * w + c.q as usize;
            assert_eq!(
                tm.cell_province[i], 1,
                "defender {c:?} not in battle province"
            );
            let nearest = tm
                .zones
                .attacker
                .iter()
                .map(|a| c.distance(*a))
                .min()
                .unwrap();
            assert!(
                nearest >= tactical_core::grid::MIN_ZONE_DISTANCE,
                "{c:?} too close"
            );
        }
        // The shared border must lie INSIDE the grid (not at its edge):
        // some attacker hex sits west of every battle hex.
        let westmost_attacker = tm.zones.attacker.iter().map(|c| c.q).min().unwrap();
        let westmost_battle = (0..tm.grid.height as i32)
            .flat_map(|r| (0..tm.grid.width as i32).map(move |q| (q, r)))
            .filter(|&(q, r)| tm.cell_province[r as usize * w + q as usize] == 1)
            .map(|(q, _)| q)
            .min()
            .unwrap();
        assert!(
            westmost_attacker < westmost_battle,
            "origin strip not west of battle"
        );
        // The defender zone is the WHOLE battle province — the old
        // central-third window (with its threat-axis biasing) could slide
        // off a small urban province's city. On stitched maps the attacker
        // deploys on origin soil, so the zones are disjoint by
        // construction; the defender's mean q now sits at the province
        // centroid, not west of it.
        let (bq0, bq1) = (0..tm.grid.height as i32)
            .flat_map(|r| (0..tm.grid.width as i32).map(move |q| (q, r)))
            .filter(|&(q, r)| tm.cell_province[r as usize * w + q as usize] == 1)
            .map(|(q, _)| q)
            .fold((i32::MAX, i32::MIN), |(lo, hi), q| (lo.min(q), hi.max(q)));
        let bc = (bq0 + bq1) as f32 / 2.0;
        let dn = tm.zones.defender.len() as f32;
        let dq = tm.zones.defender.iter().map(|c| c.q as f32).sum::<f32>() / dn;
        assert!(
            (dq - bc).abs() < 2.0,
            "defender mean q {dq} should match the province centroid {bc}"
        );
    }

    #[test]
    fn tiny_specks_generate_at_bitmap_scale() {
        // Two isolated pixels far apart. Before bitmap-scale sampling, the
        // squeeze collapsed the sampling so almost no hex centres landed
        // inside → ProvinceTooSmall. At bitmap 1:1 scale each speck
        // legitimately covers a patch of hexes (1 px ≈ 7.1 cols × 8.2 rows
        // of centre samples), so the map is now valid — MIN_PROVINCE_HEXES
        // stays as an API guard but is effectively unreachable for real
        // ≥1px provinces.
        let colors: Vec<u32> = (0..40 * 40)
            .map(|i| {
                let (x, y) = (i % 40, i / 40);
                if (x, y) == (5, 5) || (x, y) == (35, 35) {
                    RED
                } else {
                    GREEN
                }
            })
            .collect();
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors).unwrap(),
            defs(),
            vec![],
        );
        let tm = gen
            .generate(1, &[])
            .expect("bitmap-scale specks should generate");
        // In-bounds is the passable-count metric here — the out-of-province
        // sea of GREEN around the specks is now PASSABLE out-of-bounds
        // backdrop (§6.14), so is_passable no longer measures province
        // coverage.
        let in_bounds = tm
            .grid
            .iter_coords()
            .filter(|c| tm.grid.cell(*c).map(|c| !c.out_of_bounds).unwrap_or(false))
            .count();
        // ~2 specks × 7×8 hexes each ≈ 112 (1 px ≈ 7.1×8.2 centre samples;
        // the 128→512 cap raise stopped squeezing this grid).
        assert!(
            in_bounds >= 100 && in_bounds <= 130,
            "in-bounds = {in_bounds}"
        );
    }

    #[test]
    fn unknown_province_is_rejected() {
        let gen = world(40, 40, (10, 10, 19, 19), 1);
        // Not in definition.csv at all.
        assert!(matches!(
            gen.generate(99, &[]),
            Err(MapError::ProvinceNotFound(99))
        ));
        // In definition.csv but with no pixels on the bitmap.
        assert!(matches!(
            gen.generate(3, &[]),
            Err(MapError::ProvinceNotFound(3))
        ));
    }

    #[test]
    fn river_edges_follow_shared_border() {
        // West half = province 1, east half = province 2, river between them.
        let colors: Vec<u32> = (0..40)
            .flat_map(|_y| (0..40).map(move |x| if x < 20 { RED } else { GREEN }))
            .collect();
        let adjs = vec![Adjacency {
            from: 1,
            to: 2,
            kind: AdjacencyKind::River,
            through: -1,
        }];
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors).unwrap(),
            defs(),
            adjs,
        );

        // From province 1 the border lies due EAST → river on E edges only.
        let tm = gen.generate(1, &[]).unwrap();
        let (e_bit, w_bit) = (river_bit(HexDirection::E), river_bit(HexDirection::W));
        let mut e_count = 0usize;
        for c in tm.grid.iter_coords() {
            let cell = tm.grid.cell(c).unwrap();
            if cell.river_edges & e_bit != 0 {
                e_count += 1;
            }
            assert_eq!(cell.river_edges & w_bit, 0, "unexpected W river at {c:?}");
        }
        assert!(e_count > 0, "no E river edges marked");

        // From province 2 the same river appears on W edges.
        let tm2 = gen.generate(2, &[]).unwrap();
        assert!(tm2
            .grid
            .iter_coords()
            .any(|c| tm2.grid.cell(c).unwrap().river_edges & w_bit != 0));
    }

    /// Stitched maps: the river edge belongs to the battle↔origin SEAM
    /// (the province boundary), not to the playable mask's outer edge —
    /// otherwise the front crossing is free and the staging strip's rear
    /// edge grows a phantom river (and bridge deck).
    #[test]
    fn stitched_river_marks_the_seam_not_the_strip_far_edge() {
        // West half = province 2 (the attacker's origin), east half =
        // province 1 (the battle province); river adjacency between them,
        // attack from W → the origin strip folds the seam INTO the mask.
        let colors: Vec<u32> = (0..40)
            .flat_map(|_y| (0..40).map(move |x| if x < 20 { GREEN } else { RED }))
            .collect();
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(40, 40, colors).unwrap(),
            defs(),
            vec![Adjacency {
                from: 1,
                to: 2,
                kind: AdjacencyKind::River,
                through: -1,
            }],
        );
        let tm = gen.generate(1, &[HexDirection::W]).unwrap();
        assert_eq!(tm.origin_provinces, vec![2], "strip folded in");
        let cols = tm.grid.width;
        let w_bit = river_bit(HexDirection::W);
        let pid = |c: HexCoord| tm.cell_province[c.r as usize * cols + c.q as usize];
        // Seam hexes: battle soil whose W neighbour is IN-MASK origin soil
        // (the staging strip — the true shared-border front).
        let seam: Vec<HexCoord> = tm
            .grid
            .iter_coords()
            .filter(|&c| {
                pid(c) == 1 && {
                    let w = c.neighbor(HexDirection::W);
                    tm.grid.in_bounds(w) && pid(w) == 2 && !tm.grid.cell(w).unwrap().out_of_bounds
                }
            })
            .collect();
        assert!(!seam.is_empty(), "the fixture must hold the seam interior");
        assert!(
            seam.iter()
                .all(|&c| tm.grid.cell(c).unwrap().river_edges & w_bit != 0),
            "the river crossing must be marked along the whole seam"
        );
        // No origin-soil hex faces the river westward: the strip's far edge
        // is not the riverbank, and no bridge deck stamps there either.
        for c in tm.grid.iter_coords() {
            if pid(c) == 2 {
                let cell = tm.grid.cell(c).unwrap();
                assert_eq!(
                    cell.river_edges & w_bit,
                    0,
                    "phantom W river on strip hex {c:?}"
                );
                assert_eq!(
                    cell.bridge_edges & w_bit,
                    0,
                    "phantom W bridge on strip hex {c:?}"
                );
            }
        }
    }

    #[test]
    fn direction_from_angle_sectors() {
        use std::f32::consts::PI;
        assert_eq!(direction_from_angle(0.0), HexDirection::E);
        // South in image space (+y down), slightly off the 90°/270° boundaries.
        assert_eq!(direction_from_angle(PI / 2.0 - 0.2), HexDirection::SE);
        assert_eq!(direction_from_angle(PI / 2.0 + 0.2), HexDirection::SW);
        assert_eq!(direction_from_angle(PI), HexDirection::W);
        // North (−90°) is the NW/NE boundary: slightly east of it → NE, west → NW.
        assert_eq!(direction_from_angle(-PI / 2.0 + 0.2), HexDirection::NE);
        assert_eq!(direction_from_angle(-PI / 2.0 - 0.2), HexDirection::NW);
        assert_eq!(direction_from_angle(3.0 * PI / 4.0), HexDirection::SW);
        assert_eq!(direction_from_angle(-3.0 * PI / 4.0), HexDirection::NW);
    }

    #[test]
    fn river_overlay_paints_continuous_line() {
        // Province 1 fills x 2..=17, y 2..=17 of a 20×20 world; a 1-px
        // river stroke runs down column x=9. The stroke must survive
        // thinning (1-px lines are kept whole) and come out as a continuous
        // river line (sub-step sampling outpaces the ~10-hex-per-pixel
        // ratio).
        let mut gen = world(20, 20, (2, 2, 17, 17), 1);
        let mut idx = vec![255u8; 20 * 20];
        for y in 2..=17 {
            idx[y * 20 + 9] = 3;
        }
        gen.set_rivers(crate::bmp::IndexMap::from_indices(20, 20, idx));
        let tm = gen.generate(1, &[]).unwrap();
        let rivers: Vec<HexCoord> = tm
            .grid
            .iter_coords()
            .filter(|c| {
                tm.grid
                    .cell(*c)
                    .map(|c| c.terrain == Terrain::River)
                    .unwrap_or(false)
            })
            .collect();
        assert!(rivers.len() >= 20, "river hexes = {}", rivers.len());
        let (min_r, max_r) = rivers.iter().fold((i32::MAX, i32::MIN), |(lo, hi), c| {
            (lo.min(c.r), hi.max(c.r))
        });
        assert!(
            max_r - min_r > tm.grid.height as i32 / 2,
            "river line should span the province (r {min_r}..{max_r} of {})",
            tm.grid.height
        );
        // Continuity of the single stroke: sorted by row, consecutive hexes
        // stay adjacent (gaps mean the sub-sampling is too coarse).
        let mut rs = rivers.clone();
        rs.sort_by_key(|c| (c.r, c.q));
        for w in rs.windows(2) {
            assert!(
                w[0].distance(w[1]) <= 3,
                "gap between {:?} and {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn river_overlay_bands_paint_1_2_3_hex_wide_strips() {
        // A vertical 1-px stroke widens to its band's physical width: a
        // column river crosses every grid row in exactly 1 / 2 / 3 river
        // hexes for Small (idx 3) / Medium (idx 6) / Large (idx 11).
        // Expectation per row: the water strip of a straight river spans
        // `width_hexes` columns (widen offsets E+SE grow the strip across
        // rows; surplus corner hexes never exceed +2).
        for (idx, expect) in [
            (3u8, RiverBand::Small),
            (6u8, RiverBand::Medium),
            (11u8, RiverBand::Large),
        ] {
            let mut gen = world(24, 24, (2, 2, 21, 21), 1);
            let mut idxs = vec![255u8; 24 * 24];
            for y in 3..=20 {
                idxs[y * 24 + 10] = idx;
            }
            gen.set_rivers(crate::bmp::IndexMap::from_indices(24, 24, idxs));
            let tm = gen.generate(1, &[]).unwrap();
            let mut per_row: HashMap<i32, usize> = HashMap::new();
            let mut band_ok = true;
            for c in tm.grid.iter_coords() {
                let cell = tm.grid.cell(c).unwrap();
                if cell.terrain != Terrain::River {
                    continue;
                }
                if cell.river_band != expect {
                    band_ok = false;
                }
                *per_row.entry(c.r).or_default() += 1;
            }
            assert!(band_ok, "index {idx}: every river hex must carry band {expect:?}");
            // The stroke spans y 3..=20; the strip should cross most rows.
            let widest = per_row.values().copied().max().unwrap_or(0);
            let crossing: Vec<usize> = per_row
                .iter()
                .filter(|(_, &n)| n > 0)
                .map(|(_, &n)| n)
                .collect();
            let typical = crossing.iter().copied().sum::<usize>() as f32 / crossing.len() as f32;
            assert!(
                (typical - expect.width_hexes() as f32).abs() <= 0.6,
                "index {idx}: mean strip width {typical:.2} (want ~{})",
                expect.width_hexes()
            );
            assert!(
                widest <= expect.width_hexes() as usize + 2,
                "index {idx}: widest row {widest} too broad"
            );
        }
    }

    #[test]
    fn river_markers_paint_and_join_the_channel() {
        // Two vertical small-river strokes (idx 3) at x=8 and x=12, with a
        // RED merge-marker row (idx 1) bridging their lower ends — the
        // vanilla encoding for "these two channels merge here". Markers
        // ARE the riverbed: without them the pair stays two unconnected
        // strips; with them the whole channel must be ONE connected water
        // body (the old marker-skip left a land gap right at the merge).
        let river_hexes = |gen: &mut MapGenerator| -> Vec<(i32, i32)> {
            let tm = gen.generate(1, &[]).unwrap();
            tm.grid
                .iter_coords()
                .filter(|c| {
                    tm.grid
                        .cell(*c)
                        .map(|c| c.terrain == Terrain::River)
                        .unwrap_or(false)
                })
                .map(|c| (c.q, c.r))
                .collect()
        };
        let components = |hexes: &[(i32, i32)]| -> usize {
            // 6-neighbour flood count over the river-hex set.
            let mut seen: HashSet<(i32, i32)> = HashSet::new();
            let set: HashSet<(i32, i32)> = hexes.iter().copied().collect();
            let mut comps = 0usize;
            for &h in hexes {
                if seen.contains(&h) {
                    continue;
                }
                comps += 1;
                let mut stack = vec![h];
                seen.insert(h);
                while let Some((q, r)) = stack.pop() {
                    let coord = HexCoord::new(q, r);
                    for d in HexDirection::ALL {
                        let n = coord.neighbor(d);
                        let k = (n.q, n.r);
                        if set.contains(&k) && seen.insert(k) {
                            stack.push(k);
                        }
                    }
                }
            }
            comps
        };
        let mut base = world(24, 24, (2, 2, 21, 21), 1);
        let mut idxs = vec![255u8; 24 * 24];
        for y in 3..=20 {
            idxs[y * 24 + 8] = 3;
            idxs[y * 24 + 12] = 3;
        }
        base.set_rivers(crate::bmp::IndexMap::from_indices(24, 24, idxs));
        let without = river_hexes(&mut base);
        assert_eq!(
            components(&without),
            2,
            "two separate strokes stay two components without a marker"
        );
        // Red merge row at the bottom joins both columns.
        let mut gen = world(24, 24, (2, 2, 21, 21), 1);
        let mut idxs = vec![255u8; 24 * 24];
        for y in 3..=20 {
            idxs[y * 24 + 8] = 3;
            idxs[y * 24 + 12] = 3;
        }
        for x in 8..=12 {
            idxs[20 * 24 + x] = 1; // red merge marker row at y=20
        }
        gen.set_rivers(crate::bmp::IndexMap::from_indices(24, 24, idxs));
        let with = river_hexes(&mut gen);
        assert_eq!(
            components(&with),
            1,
            "the marker row must bridge the two strips into one channel"
        );
        assert!(
            with.len() > without.len(),
            "marker pixels add river hexes ({} vs {})",
            with.len(),
            without.len()
        );
    }

    #[test]
    fn marker_bands_inherit_and_isolated_markers_stay_small() {
        // Markers own no width: a red marker drawn one pixel right of a
        // LARGE stroke's end must inherit Large (the merge flows into the
        // wide river — the water strip extends to the marker, band and
        // all); a lone green source dot with no stroke nearby paints a
        // Small river hex (fallback). Band counts must not inflate from
        // marker-only widths.
        let stats = |gen: &mut MapGenerator| -> (usize, usize, usize) {
            // (small, large, max-q of Large hexes)
            let tm = gen.generate(1, &[]).unwrap();
            let (mut small, mut large, mut max_q) = (0usize, 0usize, i32::MIN);
            for c in tm.grid.iter_coords() {
                let cell = tm.grid.cell(c).unwrap();
                if cell.terrain != Terrain::River {
                    continue;
                }
                match cell.river_band {
                    RiverBand::Small => small += 1,
                    RiverBand::Medium => {}
                    RiverBand::Large => {
                        large += 1;
                        max_q = max_q.max(c.q);
                    }
                }
            }
            (small, large, max_q as usize)
        };
        let mut plain = world(24, 24, (2, 2, 21, 21), 1);
        let mut idxs = vec![255u8; 24 * 24];
        for y in 4..=20 {
            idxs[y * 24 + 10] = 11; // large stroke column
        }
        plain.set_rivers(crate::bmp::IndexMap::from_indices(24, 24, idxs));
        let (_, large_plain, max_q_plain) = stats(&mut plain);
        assert!(large_plain > 0, "large stroke paints");
        // Marker one pixel RIGHT of the stroke's lower end: the wide band
        // must extend to it (its hexes sit ~1 px = several hexes to the
        // right, far beyond the plain strip's own widening).
        let mut gen = world(24, 24, (2, 2, 21, 21), 1);
        let mut idxs = vec![255u8; 24 * 24];
        for y in 4..=20 {
            idxs[y * 24 + 10] = 11;
        }
        idxs[20 * 24 + 11] = 1; // red merge marker beside the stroke end
        idxs[4 * 24 + 3] = 0; // lone green source dot, far from strokes
        gen.set_rivers(crate::bmp::IndexMap::from_indices(24, 24, idxs));
        let (small, large, max_q) = stats(&mut gen);
        assert!(large > large_plain, "marker bridge must inherit Large");
        assert!(
            max_q > max_q_plain,
            "the Large strip must extend to the marker (q {max_q} vs {max_q_plain})"
        );
        assert!(small > 0, "isolated source dot falls back to Small");
        // The isolated dot must not inflate a fake wide strip: with no
        // stroke within a pixel it stays a 1-hex Small dot — the total
        // small count is exactly the dot's own hexes (allow the smooth
        // centroid to touch at most a couple of hexes).
        assert!(
            small <= 3,
            "isolated marker must stay a tiny Small dot, got {small}"
        );
    }

    #[test]
    fn classify_river_index_maps_the_vanilla_palette() {
        // Blue stroke rows split by shade; green/red/yellow are MARKERS
        // (riverbed without width); unused slots (5/8) and the sea/land
        // background (254/255) are not river.
        assert_eq!(
            classify_river_index(3),
            Some(RiverPx::Stroke(RiverBand::Small))
        );
        assert_eq!(
            classify_river_index(4),
            Some(RiverPx::Stroke(RiverBand::Small))
        );
        assert_eq!(
            classify_river_index(6),
            Some(RiverPx::Stroke(RiverBand::Medium))
        );
        assert_eq!(
            classify_river_index(7),
            Some(RiverPx::Stroke(RiverBand::Medium))
        );
        assert_eq!(
            classify_river_index(9),
            Some(RiverPx::Stroke(RiverBand::Large))
        );
        assert_eq!(
            classify_river_index(10),
            Some(RiverPx::Stroke(RiverBand::Large))
        );
        assert_eq!(
            classify_river_index(11),
            Some(RiverPx::Stroke(RiverBand::Large))
        );
        for i in [0u8, 1, 2] {
            assert_eq!(classify_river_index(i), Some(RiverPx::Marker), "index {i}");
        }
        for i in [5u8, 8, 254, 255] {
            assert_eq!(classify_river_index(i), None, "index {i} is not river");
        }
    }

    /// Road painting + rasterization on a synthetic grid: a 24×24 plains
    /// board with a 1-hex River strip at q = 12 and two painted paths —
    /// one along the west bank, one crossing to the east. The painted
    /// roads must connect every endpoint, bridge the river where they
    /// cross, and never touch Water; two identical builds must paint
    /// identical flags.
    #[test]
    fn road_paths_connect_and_bridge_rivers() {
        let build = || {
            let mut grid = HexGrid::new(24, 24, Terrain::Plains);
            for r in 0..24 {
                grid.set_terrain(HexCoord::new(12, r), Terrain::River);
            }
            grid.set_terrain(HexCoord::new(6, 6), Terrain::Water);
            let in_province = vec![true; 24 * 24];
            let nodes = vec![
                HexCoord::new(4, 4),
                HexCoord::new(4, 20),
                HexCoord::new(20, 12),
            ];
            let path = road_path(&grid, &in_province, nodes[0], nodes[1], true, false);
            paint_road(&mut grid, &in_province, &path, 1, 1.0);
            let path = road_path(&grid, &in_province, nodes[1], nodes[2], true, false);
            paint_road(&mut grid, &in_province, &path, 1, 1.0);
            stamp_bridge_edges(&mut grid, &in_province, 1, 1.0);
            (grid, nodes)
        };
        let (grid, nodes) = build();

        // Every endpoint sits on the road net.
        for h in &nodes {
            assert!(grid.cell(*h).unwrap().road, "endpoint {h:?} off the road");
        }
        // … and is reachable from every other node over road hexes.
        let road_set: HashSet<HexCoord> = grid
            .iter_coords()
            .filter(|c| grid.cell(*c).unwrap().road)
            .collect();
        let mut seen: HashSet<HexCoord> = HashSet::new();
        let mut stack = vec![nodes[0]];
        seen.insert(nodes[0]);
        while let Some(c) = stack.pop() {
            for n in c.neighbors() {
                if road_set.contains(&n) && seen.insert(n) {
                    stack.push(n);
                }
            }
        }
        for h in &nodes {
            assert!(seen.contains(h), "node {h:?} not road-connected");
        }
        // The east node is across the river strip: some crossing hex must
        // carry a bridge deck, and crossed River hexes keep their terrain.
        let crossings: Vec<HexCoord> = (0..24)
            .map(|r| HexCoord::new(12, r))
            .filter(|h| grid.cell(*h).unwrap().road)
            .collect();
        assert!(!crossings.is_empty(), "no road crosses the river strip");
        for h in &crossings {
            let cell = grid.cell(*h).unwrap();
            assert_eq!(cell.terrain, Terrain::River, "roads never change terrain");
            assert!(cell.bridge, "river crossing {h:?} not bridged");
        }
        // Never on Water.
        for c in grid.iter_coords() {
            let cell = grid.cell(c).unwrap();
            if cell.terrain == Terrain::Water {
                assert!(!cell.road && !cell.bridge, "road on water at {c:?}");
            }
        }
        // Determinism: an identical build paints identical flags.
        let (grid2, _) = build();
        for c in grid.iter_coords() {
            let (a, b) = (grid.cell(c).unwrap(), grid2.cell(c).unwrap());
            assert_eq!(
                (a.road, a.bridge, a.bridge_edges),
                (b.road, b.bridge, b.bridge_edges),
                "nondeterministic flags at {c:?}"
            );
        }
        // bridge_edges: an adjacency river edge under a road hex bridges.
        assert_eq!(grid.cell(nodes[0]).unwrap().bridge_edges, 0);
        let mut grid = grid;
        grid.cell_mut(nodes[0]).unwrap().river_edges = river_bit(HexDirection::E);
        let in_province = vec![true; 24 * 24];
        stamp_bridge_edges(&mut grid, &in_province, 1, 1.0);
        assert_eq!(
            grid.cell(nodes[0]).unwrap().bridge_edges,
            river_bit(HexDirection::E)
        );
    }

    /// Every road hex reachable from `start` over painted road flags.
    fn road_reachable(grid: &HexGrid, start: HexCoord) -> HashSet<HexCoord> {
        let road_set: HashSet<HexCoord> = grid
            .iter_coords()
            .filter(|c| grid.cell(*c).unwrap().road)
            .collect();
        let mut seen: HashSet<HexCoord> = HashSet::new();
        let mut stack = vec![start];
        seen.insert(start);
        while let Some(c) = stack.pop() {
            for n in c.neighbors() {
                if road_set.contains(&n) && seen.insert(n) {
                    stack.push(n);
                }
            }
        }
        seen
    }

    /// City radials: with a VP anchor, EVERY land neighbour's border stub
    /// gets a main road — no distance admission — and each stub is
    /// road-connected to the city.
    #[test]
    fn city_radiates_to_all_land_stubs() {
        let mut grid = HexGrid::new(40, 40, Terrain::Plains);
        let anchor = HexCoord::new(20, 20);
        let in_province = vec![true; 40 * 40];
        let mut border_pixels: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        border_pixels.insert(2, (0..40).map(|y| (0, y)).collect());
        border_pixels.insert(3, (0..40).map(|y| (39, y)).collect());
        border_pixels.insert(4, (0..40).map(|x| (x, 0)).collect());
        // Neighbour 5 shares border pixels but is SEA — no stub, no road.
        border_pixels.insert(5, (0..40).map(|x| (x, 39)).collect());
        let land: HashSet<u32> = [2, 3, 4].into_iter().collect();
        let cell_pid = vec![1u32; 40 * 40];
        let ends = overlay_roads(
            &mut grid,
            &in_province,
            &cell_pid,
            Some(anchor),
            1,
            1.0,
            &border_pixels,
            &land,
            (0, 0, 39, 39),
        );
        let reach = road_reachable(&grid, anchor);
        // The three land stubs (west, east, north ring) all join.
        for stub in [
            HexCoord::new(0, 19),
            HexCoord::new(39, 18),
            HexCoord::new(19, 0),
        ] {
            assert!(grid.cell(stub).unwrap().road, "stub {stub:?} unpainted");
            assert!(
                reach.contains(&stub),
                "stub {stub:?} not road-connected to the city"
            );
        }
        // The sea side (south ring) stays clean.
        for q in 0..40 {
            assert!(
                !grid.cell(HexCoord::new(q, 39)).unwrap().road,
                "sea-side ring painted at q={q}"
            );
        }
        // Branches forked off the radials and returned their endpoints.
        assert!(!ends.is_empty(), "no branches generated");
    }

    /// Stitched maps: a neighbour folded in as the origin strip gets its
    /// stub on the strip's FAR outer edge (facing the neighbour), not on
    /// the band's near rim at the internal boundary — the radial visibly
    /// leaves the map instead of stopping at the invisible seam.
    #[test]
    fn strip_neighbour_stub_exits_at_far_edge() {
        // Battle province 1 fills the grid; origin strip (pid 2) is a
        // narrow band along the east, rows 10..=30 — its rim hexes at the
        // band's top/bottom sit close to the internal boundary, exactly
        // the real-map failure shape.
        let mut grid = HexGrid::new(40, 40, Terrain::Plains);
        let anchor = HexCoord::new(10, 20);
        let mut in_province = vec![true; 40 * 40];
        let mut cell_pid = vec![1u32; 40 * 40];
        for r in 0..40usize {
            for q in 20..40usize {
                let i = r * 40 + q;
                if (10..=30).contains(&r) {
                    cell_pid[i] = 2;
                } else {
                    in_province[i] = false; // backdrop beyond the band
                }
            }
        }
        let mut border_pixels: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        border_pixels.insert(2, (10..=30).map(|y| (19, y)).collect());
        let land: HashSet<u32> = [2].into_iter().collect();
        overlay_roads(
            &mut grid,
            &in_province,
            &cell_pid,
            Some(anchor),
            1,
            1.0,
            &border_pixels,
            &land,
            (0, 0, 39, 39),
        );
        // The radial crosses the strip and paints the EAST rim (q=39),
        // road-connected to the city — never settling for the band's
        // top/bottom rim by the internal boundary.
        let reach = road_reachable(&grid, anchor);
        let east = (10..=30).find(|&r| {
            let h = HexCoord::new(39, r);
            grid.cell(h).map(|c| c.road).unwrap_or(false) && reach.contains(&h)
        });
        assert!(east.is_some(), "strip far edge unpainted/unconnected");
    }

    /// Rural pairing: without a city the stubs pair off in seeded order —
    /// even-count and odd-count sets both end up one road-connected
    /// component (the odd stub links to its nearest paired stub, and the
    /// pairs are bridged).
    #[test]
    fn rural_stub_pairing_connects_all_stubs() {
        let build = |neighbours: &[u32]| {
            let mut grid = HexGrid::new(40, 40, Terrain::Plains);
            let in_province = vec![true; 40 * 40];
            let mut border_pixels: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
            let sides: [Box<dyn Fn(u32) -> (u32, u32)>; 4] = [
                Box::new(|y| (0, y)),
                Box::new(|y| (39, y)),
                Box::new(|x| (x, 0)),
                Box::new(|x| (x, 39)),
            ];
            for (i, np) in neighbours.iter().enumerate() {
                border_pixels.insert(*np, (0..40).map(&sides[i]).collect());
            }
            let land: HashSet<u32> = neighbours.iter().copied().collect();
            let cell_pid = vec![1u32; 40 * 40];
            overlay_roads(
                &mut grid,
                &in_province,
                &cell_pid,
                None,
                1,
                1.0,
                &border_pixels,
                &land,
                (0, 0, 39, 39),
            );
            grid
        };
        // Even: four stubs (W, E, N, S).
        let grid = build(&[2, 3, 4, 5]);
        let stubs4 = [
            HexCoord::new(0, 19),
            HexCoord::new(39, 18),
            HexCoord::new(19, 0),
            // The f32 border centroid lands a hair under the rounding
            // threshold, so the south stub settles one hex west.
            HexCoord::new(18, 39),
        ];
        let reach = road_reachable(&grid, stubs4[0]);
        for s in stubs4 {
            assert!(grid.cell(s).unwrap().road, "stub {s:?} unpainted");
            assert!(reach.contains(&s), "stub {s:?} not connected");
        }
        // Odd: three stubs (W, E, N).
        let grid = build(&[2, 3, 4]);
        let stubs3 = [
            HexCoord::new(0, 19),
            HexCoord::new(39, 18),
            HexCoord::new(19, 0),
        ];
        let reach = road_reachable(&grid, stubs3[0]);
        for s in stubs3 {
            assert!(grid.cell(s).unwrap().road, "stub {s:?} unpainted");
            assert!(reach.contains(&s), "stub {s:?} not connected");
        }
    }

    /// Road-merge attraction: a later path near an existing road merges
    /// onto it instead of drawing a parallel line one row over.
    #[test]
    fn road_path_merges_onto_existing_road() {
        let mut grid = HexGrid::new(32, 24, Terrain::Plains);
        let in_province = vec![true; 32 * 24];
        let first = HexCoord::new(2, 10).line_to(HexCoord::new(28, 10));
        paint_road(&mut grid, &in_province, &first, 1, 1.0);
        // One row south: straight would be 26 cheap plains steps, but the
        // merge multiplier makes up-road-along-down far cheaper.
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(2, 11),
            HexCoord::new(28, 11),
            true,
            false,
        );
        let shared = path.iter().filter(|h| first.contains(h)).count();
        assert!(
            shared >= 20,
            "path must merge onto the existing road ({shared} shared hexes): {path:?}"
        );
    }

    /// River-turn surcharge: bridges cross straight — a 60° bend inside
    /// the river costs more than extra hexes on land, so the path crosses
    /// squarely even when a bent crossing is shorter.
    #[test]
    fn river_turn_penalty_keeps_bridges_straight() {
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        for r in 0..24 {
            grid.set_terrain(HexCoord::new(12, r), Terrain::River);
            grid.set_terrain(HexCoord::new(13, r), Terrain::River);
        }
        let in_province = vec![true; 24 * 24];
        // Square crossing: (10,6)→(14,6) crosses at row 6.
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(10, 6),
            HexCoord::new(14, 6),
            true,
            false,
        );
        assert!(
            path.contains(&HexCoord::new(12, 6)) && path.contains(&HexCoord::new(13, 6)),
            "square crossing: {path:?}"
        );
        // (10,6)→(15,9): the bent route — enter (12,6) then turn 60° to
        // (13,7) inside the river — is two hexes shorter than crossing
        // squarely at row 6 and bending on land, but the surcharge makes
        // it lose. Observable: the path's River hexes stay in ONE row.
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(10, 6),
            HexCoord::new(15, 9),
            true,
            false,
        );
        let river_rows: HashSet<i32> = path
            .iter()
            .filter(|h| grid.cell(**h).unwrap().terrain == Terrain::River)
            .map(|h| h.r)
            .collect();
        assert_eq!(
            river_rows.len(),
            1,
            "bridge bends inside the river: {path:?}"
        );
    }

    /// City attraction: a path whose endpoints sit on opposite sides of a
    /// wide urban strip DIVES through it (Urban steps at ROAD_URBAN_COST
    /// beat any plains detour) — the road reads as connecting to the
    /// city's corners instead of skimming past. Far from any cluster the
    /// path is the plain straight line.
    #[test]
    fn road_path_dives_through_urban_cluster() {
        let in_province = vec![true; 32 * 24];
        let mut grid = HexGrid::new(32, 24, Terrain::Plains);
        // A 12-hex-wide, 1-hex-tall urban strip across the straight line:
        // at the terrain's 1.2 a skirt around it would be cheaper, at
        // ROAD_URBAN_COST diving through wins by far.
        for q in 6..=17 {
            grid.set_terrain(HexCoord::new(q, 12), Terrain::Urban);
        }
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(4, 12),
            HexCoord::new(20, 12),
            true,
            false,
        );
        assert!(
            path.iter()
                .any(|h| grid.cell(*h).unwrap().terrain == Terrain::Urban),
            "path must dive through the urban strip, not skirt it: {path:?}"
        );
        // Far from the cluster the straight plains line is untouched.
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(4, 4),
            HexCoord::new(20, 4),
            true,
            false,
        );
        assert!(
            !path
                .iter()
                .any(|h| grid.cell(*h).unwrap().terrain == Terrain::Urban),
            "no urban hex anywhere near: {path:?}"
        );
        assert_eq!(path.len(), 17, "straight line across the map");
    }

    /// Sea/lake neighbours get no border stub at all — a shore has no
    /// off-map road continuation — even when their border pixels are
    /// supplied. The land side gets the city's radial.
    #[test]
    fn sea_neighbour_gets_no_border_stub() {
        let mut grid = HexGrid::new(40, 40, Terrain::Plains);
        let anchor = HexCoord::new(30, 20);
        let in_province = vec![true; 40 * 40];
        let mut border_pixels: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        border_pixels.insert(2, (0..40).map(|y| (0, y)).collect());
        border_pixels.insert(3, (0..40).map(|y| (39, y)).collect());
        // Province 2 (west) is sea — only 3 (east) is land. The city sits
        // 10 hexes off the west ring: a land west neighbour WOULD get a
        // radial, so a clean west ring proves the kind filter.
        let land: HashSet<u32> = [3].into_iter().collect();
        let cell_pid = vec![1u32; 40 * 40];
        overlay_roads(
            &mut grid,
            &in_province,
            &cell_pid,
            Some(anchor),
            1,
            1.0,
            &border_pixels,
            &land,
            (0, 0, 39, 39),
        );
        // East (land) stub (39,18): the city's radial reaches it.
        assert!(
            grid.cell(HexCoord::new(39, 18)).unwrap().road,
            "land-neighbour stub must paint"
        );
        let reach = road_reachable(&grid, anchor);
        assert!(
            reach.contains(&HexCoord::new(39, 18)),
            "land stub not road-connected to the city"
        );
        // West (sea) side: no stub, no road on the west ring.
        for r in 0..40 {
            assert!(
                !grid.cell(HexCoord::new(0, r)).unwrap().road,
                "sea-side ring painted at r={r}"
            );
        }
    }

    /// True when some three hexes are pairwise linked by the given edge
    /// bits — a hex-level 3-clique (road-edge tests).
    fn hex_triangle(grid: &HexGrid, bits: impl Fn(&tactical_core::GridCell) -> u8) -> bool {
        let linked = |a: HexCoord, b: HexCoord| {
            let Some(dir) = a.direction_to(b) else {
                return false;
            };
            let (Some(ca), Some(cb)) = (grid.cell(a), grid.cell(b)) else {
                return false;
            };
            bits(ca) & dir.bit() != 0 && bits(cb) & dir.opposite().bit() != 0
        };
        grid.iter_coords().any(|h| {
            let ns: Vec<HexCoord> = h
                .neighbors()
                .into_iter()
                .filter(|&n| linked(h, n))
                .collect();
            ns.iter()
                .enumerate()
                .any(|(i, &a)| ns[i + 1..].iter().any(|&b| linked(a, b)))
        })
    }

    /// Edge-explicit painting: every consecutive path pair records the
    /// crossed edge on BOTH cells — internals carry both along-path bits,
    /// terminals exactly one; a single-hex node paint sets no bits.
    #[test]
    fn paint_road_sets_edge_bits_on_both_cells() {
        let in_province = vec![true; 24 * 24];
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let path = HexCoord::new(2, 2).line_to(HexCoord::new(6, 2));
        assert_eq!(path.len(), 5, "straight east line");
        paint_road(&mut grid, &in_province, &path, 1, 1.0);
        let bits = |q, r| grid.cell(HexCoord::new(q, r)).unwrap().road_edges;
        assert_eq!(bits(2, 2), HexDirection::E.bit(), "west terminal");
        for q in 3..=5 {
            assert_eq!(
                bits(q, 2),
                HexDirection::W.bit() | HexDirection::E.bit(),
                "internal q={q}"
            );
        }
        assert_eq!(bits(6, 2), HexDirection::W.bit(), "east terminal");
        for q in 2..=6 {
            let cell = grid.cell(HexCoord::new(q, 2)).unwrap();
            assert!(cell.road && cell.rail_edges == 0, "flag set, no rail bits");
        }
        // A single-hex node paint sets the flag but no edge bits — nodes
        // collect their edges from the paths that connect them.
        paint_road(&mut grid, &in_province, &[HexCoord::new(10, 10)], 1, 1.0);
        let node = grid.cell(HexCoord::new(10, 10)).unwrap();
        assert!(node.road && node.road_edges == 0);
    }

    /// Hex-level triangles cannot be inferred from the edge bits: three
    /// mutually ADJACENT village hexes (all road-flagged) linked by a
    /// two-path chain paint exactly two edge links — the painted
    /// road_edges graph has no 3-clique even though the road-flag hexes
    /// touch pairwise.
    #[test]
    fn clustered_villages_paint_no_hex_triangle() {
        let in_province = vec![true; 24 * 24];
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        // Pairwise adjacent: (10,10)-(11,10) E, (10,10)-(10,11) SE,
        // (10,11)-(11,10) NE.
        let villages = [
            HexCoord::new(10, 10),
            HexCoord::new(11, 10),
            HexCoord::new(10, 11),
        ];
        for &v in &villages {
            grid.set_terrain(v, Terrain::Village);
            paint_road(&mut grid, &in_province, &[v], 1, 1.0);
        }
        // A two-link chain through the middle village.
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(10, 10),
            HexCoord::new(11, 10),
            true,
            false,
        );
        paint_road(&mut grid, &in_province, &path, 1, 1.0);
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(10, 11),
            HexCoord::new(11, 10),
            true,
            false,
        );
        paint_road(&mut grid, &in_province, &path, 1, 1.0);
        for &v in &villages {
            assert!(grid.cell(v).unwrap().road, "node keeps its flag");
        }
        assert!(
            !hex_triangle(&grid, |c| c.road_edges),
            "flag-adjacent villages must not weave a triangle"
        );
    }

    /// Pass-by: a painted line running BESIDE a road-flagged node hex
    /// sets NO edge bit toward it — flag adjacency would have invented
    /// the link. (With the anti-skim surcharge the A* routes around or
    /// merges — the residual pass-by source is straight-line paint:
    /// the line_to fallback and rail lines.)
    #[test]
    fn path_passing_beside_a_node_sets_no_link_to_it() {
        let in_province = vec![true; 24 * 24];
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let village = HexCoord::new(5, 5);
        grid.set_terrain(village, Terrain::Village);
        paint_road(&mut grid, &in_province, &[village], 1, 1.0);
        // A straight line along row 4: (2,4)→(12,4); (5,5) touches
        // (5,4) and (6,4).
        let path = HexCoord::new(2, 4).line_to(HexCoord::new(12, 4));
        assert!(!path.contains(&village));
        paint_road(&mut grid, &in_province, &path, 1, 1.0);
        let v = grid.cell(village).unwrap();
        assert!(v.road, "the node keeps its flag");
        assert_eq!(v.road_edges, 0, "pass-by invented a link to the node");
        for h in [HexCoord::new(5, 4), HexCoord::new(6, 4)] {
            assert!(path.contains(&h), "test layout: {h:?} on the path");
            assert_eq!(
                grid.cell(h).unwrap().road_edges,
                HexDirection::E.bit() | HexDirection::W.bit(),
                "spurious bit toward the village at {h:?}"
            );
        }
    }

    /// Anti-skim surcharge: beside an existing road the straight row
    /// (three hexes touching it) loses to the one-row-further detour —
    /// without the surcharge the skim row wins. Control: with no road,
    /// the same endpoints take the straight row.
    #[test]
    fn road_path_keeps_clearance_or_merges() {
        let in_province = vec![true; 24 * 24];
        // Two road hexes; the straight row-9 line from (9,9) to (13,9)
        // skims them via (10,9), (11,9), (12,9) — the row-8 detour is one
        // hex longer but pays no surcharge.
        let roaded = || {
            let mut grid = HexGrid::new(24, 24, Terrain::Plains);
            paint_road(
                &mut grid,
                &in_province,
                &[HexCoord::new(10, 10), HexCoord::new(11, 10)],
                1,
                1.0,
            );
            grid
        };
        let path = road_path(
            &roaded(),
            &in_province,
            HexCoord::new(9, 9),
            HexCoord::new(13, 9),
            true,
            false,
        );
        for h in [
            HexCoord::new(10, 9),
            HexCoord::new(11, 9),
            HexCoord::new(12, 9),
        ] {
            assert!(!path.contains(&h), "skim hex {h:?} on the path: {path:?}");
        }
        // Control: no road anywhere → the straight row-9 line.
        let clear = HexGrid::new(24, 24, Terrain::Plains);
        let path = road_path(
            &clear,
            &in_province,
            HexCoord::new(9, 9),
            HexCoord::new(13, 9),
            true,
            false,
        );
        assert_eq!(path.len(), 5, "straight line when nothing is painted");
        // True merging is not priced out: a long parallel path still
        // rides the existing road (regression guard for the merge mult).
        let mut grid = roaded();
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(2, 10).line_to(HexCoord::new(8, 10)),
            1,
            1.0,
        );
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(2, 11),
            HexCoord::new(8, 11),
            true,
            false,
        );
        let shared = path
            .iter()
            .filter(|h| (2..=8).contains(&h.q) && h.r == 10)
            .count();
        assert!(shared >= 5, "the merge corridor must survive: {path:?}");
    }

    /// Branch ends never hug the existing net: a picked end must not be
    /// adjacent to any road hex of the main-net snapshot (a hugging end
    /// reads as a stub-length fork).
    #[test]
    fn branch_ends_never_touch_the_main_net() {
        let mut grid = HexGrid::new(48, 48, Terrain::Plains);
        let in_province = vec![true; 48 * 48];
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(6, 24).line_to(HexCoord::new(42, 24)),
            1,
            1.0,
        );
        let road_hexes: Vec<HexCoord> = grid
            .iter_coords()
            .filter(|&h| grid.cell(h).unwrap().road)
            .collect();
        let road_set: HashSet<HexCoord> = road_hexes.iter().copied().collect();
        let mut ends: Vec<HexCoord> = Vec::new();
        let mut painted = 0usize;
        for i in 0..8usize {
            if let Some((_, end)) = paint_branch(
                &mut grid,
                &in_province,
                1,
                1.0,
                &road_hexes,
                &ends,
                i,
                5,
                7,
                None,
            ) {
                assert!(
                    !end.neighbors().into_iter().any(|n| road_set.contains(&n)),
                    "end {end:?} hugs the main net"
                );
                ends.push(end);
                painted += 1;
            }
        }
        assert!(painted > 0, "the open board must place some branches");
    }

    /// Edge-graph triangle cleanup: a hand-built hex 3-cycle loses its
    /// lexicographically largest edge, the remaining links still connect
    /// the trio (and a downstream hex), and road FLAGS never change.
    #[test]
    fn prune_road_hex_triangles_clears_one_edge() {
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let in_province = vec![true; 24 * 24];
        // Triangle A(10,10) B(11,10) C(10,11) plus tail D(12,10) off B.
        let link = |grid: &mut HexGrid, a: HexCoord, b: HexCoord| {
            let dir = a.direction_to(b).unwrap();
            let back = dir.opposite();
            grid.cell_mut(a).unwrap().road_edges |= dir.bit();
            grid.cell_mut(b).unwrap().road_edges |= back.bit();
        };
        let (a, b, c, d) = (
            HexCoord::new(10, 10),
            HexCoord::new(11, 10),
            HexCoord::new(10, 11),
            HexCoord::new(12, 10),
        );
        let paint_flags = |grid: &mut HexGrid| {
            paint_road(grid, &in_province, &[a, b, c, d], 1, 1.0);
        };
        paint_flags(&mut grid);
        link(&mut grid, a, b);
        link(&mut grid, a, c);
        link(&mut grid, b, c);
        link(&mut grid, b, d);
        assert!(
            hex_triangle(&grid, |c| c.road_edges),
            "fixture has a triangle"
        );
        prune_road_hex_triangles(&mut grid);
        assert!(
            !hex_triangle(&grid, |c| c.road_edges),
            "the triangle must be gone"
        );
        let edge_linked = |grid: &HexGrid, x: HexCoord, y: HexCoord| {
            let dir = x.direction_to(y).unwrap();
            grid.cell(x).unwrap().road_edges & dir.bit() != 0
                && grid.cell(y).unwrap().road_edges & dir.opposite().bit() != 0
        };
        assert!(!edge_linked(&grid, b, c), "the largest edge is cleared");
        assert!(edge_linked(&grid, a, b) && edge_linked(&grid, a, c) && edge_linked(&grid, b, d));
        // Flags untouched: all four hexes keep their road flag.
        for h in [a, b, c, d] {
            assert!(grid.cell(h).unwrap().road, "flag lost at {h:?}");
        }
    }

    /// Cleanup never touches a crossing: a triangle whose largest edge
    /// touches a bridge/River hex loses a DIFFERENT edge; a fully
    /// protected triangle survives.
    #[test]
    fn prune_road_hex_triangles_respects_bridges() {
        let link = |grid: &mut HexGrid, a: HexCoord, b: HexCoord| {
            let dir = a.direction_to(b).unwrap();
            let back = dir.opposite();
            grid.cell_mut(a).unwrap().road_edges |= dir.bit();
            grid.cell_mut(b).unwrap().road_edges |= back.bit();
        };
        // Case 1: triangle with the lex-largest edge protected (bridge).
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let (a, b, c) = (
            HexCoord::new(10, 10),
            HexCoord::new(11, 10),
            HexCoord::new(10, 11),
        );
        grid.set_terrain(b, Terrain::River);
        grid.cell_mut(b).unwrap().bridge = true;
        link(&mut grid, a, b);
        link(&mut grid, a, c);
        link(&mut grid, b, c);
        prune_road_hex_triangles(&mut grid);
        let edge_linked = |grid: &HexGrid, x: HexCoord, y: HexCoord| {
            let dir = x.direction_to(y).unwrap();
            grid.cell(x).unwrap().road_edges & dir.bit() != 0
                && grid.cell(y).unwrap().road_edges & dir.opposite().bit() != 0
        };
        assert!(
            edge_linked(&grid, b, c) && edge_linked(&grid, a, b),
            "bridge-touching edges must survive"
        );
        assert!(!edge_linked(&grid, a, c), "the unprotected edge goes");
        // Case 2: fully protected triangle (bridge on one hex + River
        // terrain on another) — nothing is cleared.
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        grid.set_terrain(b, Terrain::River);
        grid.set_terrain(c, Terrain::River);
        grid.cell_mut(b).unwrap().bridge = true;
        link(&mut grid, a, b);
        link(&mut grid, a, c);
        link(&mut grid, b, c);
        prune_road_hex_triangles(&mut grid);
        assert!(
            edge_linked(&grid, a, b) && edge_linked(&grid, a, c) && edge_linked(&grid, b, c),
            "a fully protected triangle is left alone"
        );
    }

    /// Border-ford completeness: a path DYING on a River hex (no far-bank
    /// continuation) paints neither deck nor road flag on the run — the
    /// road ends at the near bank with no dangling edge bits. A complete
    /// crossing still rolls its deck (pass) or fords (fail, flags stay).
    #[test]
    fn border_ford_paints_no_partial_bridge() {
        let in_province = vec![true; 24 * 24];
        // River strip at q=12, south half only — the map's playable edge
        // cuts the crossing: path (10,22)→(12,22) dies on the river.
        let build = || {
            let mut grid = HexGrid::new(24, 24, Terrain::Plains);
            for r in 14..24 {
                grid.set_terrain(HexCoord::new(12, r), Terrain::River);
            }
            grid
        };
        let mut grid = build();
        let path = HexCoord::new(10, 22).line_to(HexCoord::new(12, 22));
        paint_road(&mut grid, &in_province, &path, 1, 1.0);
        let river_end = HexCoord::new(12, 22);
        let cell = grid.cell(river_end).unwrap();
        assert!(
            !cell.road && !cell.bridge,
            "incomplete ford painted at the border"
        );
        assert!(
            grid.cell(HexCoord::new(11, 22)).unwrap().road,
            "near bank keeps the road"
        );
        assert_eq!(
            grid.cell(HexCoord::new(11, 22)).unwrap().road_edges,
            HexDirection::W.bit(),
            "no dangling edge bit toward the dropped river hex"
        );
        // Complete crossing, roll passes (p=1): deck + flags.
        let mut grid = build();
        let path = HexCoord::new(10, 22).line_to(HexCoord::new(14, 22));
        paint_road(&mut grid, &in_province, &path, 1, 1.0);
        let cell = grid.cell(river_end).unwrap();
        assert!(cell.road && cell.bridge, "complete crossing decks at p=1");
        // Complete crossing, roll fails (p=0): ford — flags stay, no deck.
        let mut grid = build();
        paint_road(&mut grid, &in_province, &path, 1, 0.0);
        let cell = grid.cell(river_end).unwrap();
        assert!(
            cell.road && !cell.bridge,
            "the ford keeps its flag without a deck"
        );
        // Rail dying on the river: also no flag, no deck.
        let mut grid = build();
        paint_rail_line(
            &mut grid,
            &in_province,
            HexCoord::new(10, 22),
            HexCoord::new(12, 22),
        );
        let cell = grid.cell(river_end).unwrap();
        assert!(!cell.rail && !cell.bridge, "incomplete rail ford painted");
        assert!(grid.cell(HexCoord::new(11, 22)).unwrap().rail);
    }

    /// Rail lines record rail_edges (both directions per step) and never
    /// touch road bits; roads never touch rail bits.
    #[test]
    fn rail_and_road_edge_bits_stay_separate() {
        let in_province = vec![true; 24 * 24];
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        assert!(paint_rail_line(
            &mut grid,
            &in_province,
            HexCoord::new(2, 2),
            HexCoord::new(6, 2)
        ));
        let bits = |q, r| {
            let c = grid.cell(HexCoord::new(q, r)).unwrap();
            (c.rail, c.rail_edges, c.road, c.road_edges)
        };
        assert_eq!(bits(2, 2), (true, HexDirection::E.bit(), false, 0));
        assert_eq!(
            bits(4, 2),
            (
                true,
                HexDirection::E.bit() | HexDirection::W.bit(),
                false,
                0
            )
        );
        assert_eq!(bits(6, 2), (true, HexDirection::W.bit(), false, 0));
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(2, 6).line_to(HexCoord::new(4, 6)),
            1,
            1.0,
        );
        let c = grid.cell(HexCoord::new(3, 6)).unwrap();
        assert!(c.road && c.road_edges == HexDirection::E.bit() | HexDirection::W.bit());
        assert!(!c.rail && c.rail_edges == 0);
    }

    /// The full overlay inside `generate`: rural main roads connect the
    /// border stubs, villages derive from the painted net, and the whole
    /// overlay is deterministic across calls.
    #[test]
    fn generate_paints_road_network() {
        // Two land neighbours: the rect province borders province 2 (west
        // half) and province 3 (east half) — two stubs, one rural pair.
        let (w, h) = (40u32, 40u32);
        let colors: Vec<u32> = (0..h)
            .flat_map(|y| {
                (0..w).map(move |x| {
                    if (12..=27).contains(&x) && (12..=27).contains(&y) {
                        RED
                    } else if x < 20 {
                        GREEN
                    } else {
                        BLUE
                    }
                })
            })
            .collect();
        let gen = MapGenerator::new(
            ProvinceMap::from_colors(w, h, colors).unwrap(),
            defs(),
            vec![],
        );
        let tm = gen.generate(1, &[]).unwrap();
        let (cols, rows) = (tm.grid.width, tm.grid.height);
        let in_province = |c: HexCoord| !tm.grid.cell(c).unwrap().out_of_bounds;
        // Border stubs: some in-province hex touching non-province ground
        // is on the road net, on BOTH the west and the east side.
        let stub_on_side = |west: bool| {
            tm.grid.iter_coords().any(|c| {
                in_province(c)
                    && (if west {
                        c.q * 2 < cols as i32
                    } else {
                        c.q * 2 >= cols as i32
                    })
                    && tm.grid.cell(c).unwrap().road
                    && c.neighbors().into_iter().any(|n| {
                        n.q < 0
                            || n.r < 0
                            || n.q as usize >= cols
                            || n.r as usize >= rows
                            || !in_province(n)
                    })
            })
        };
        assert!(stub_on_side(true), "no west border stub painted");
        assert!(stub_on_side(false), "no east border stub painted");
        // Roads stay inside the playable mask.
        for c in tm.grid.iter_coords() {
            let cell = tm.grid.cell(c).unwrap();
            if cell.road {
                assert!(in_province(c), "road painted out of bounds at {c:?}");
            }
        }
        // Villages derive from the net: some exist, and none sits on
        // River/Water ground.
        let mut villages = 0usize;
        for c in tm.grid.iter_coords() {
            let cell = tm.grid.cell(c).unwrap();
            if cell.terrain == Terrain::Village && in_province(c) {
                villages += 1;
            }
        }
        assert!(villages > 0, "the road net must derive villages");
        // Determinism across generate() calls (roads + rails + factories).
        let tm2 = gen.generate(1, &[]).unwrap();
        for c in tm.grid.iter_coords() {
            let (a, b) = (tm.grid.cell(c).unwrap(), tm2.grid.cell(c).unwrap());
            assert_eq!(
                (
                    a.road,
                    a.rail,
                    a.bridge,
                    a.bridge_edges,
                    a.road_edges,
                    a.rail_edges,
                    a.factory
                ),
                (
                    b.road,
                    b.rail,
                    b.bridge,
                    b.bridge_edges,
                    b.road_edges,
                    b.rail_edges,
                    b.factory
                ),
                "nondeterministic overlay at {c:?}"
            );
        }
    }

    /// Factory districts: industry 9 → 9/4 = 2 hexes, Village base with
    /// factory + road flags, ring 2..=4 around the anchor; no anchor or
    /// no industry → none; industry is clamped to 3 districts.
    #[test]
    fn factory_districts_stamp_near_anchor() {
        let in_province = vec![true; 24 * 24];
        let anchor = HexCoord::new(12, 12);
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let picked = place_factories(&mut grid, &in_province, Some(anchor), 9, 1);
        assert_eq!(picked.len(), 2, "industry 9 / 4 = 2 districts");
        for h in &picked {
            let d = h.distance(anchor);
            assert!((2..=4).contains(&d), "district {h:?} at ring {d}");
            let cell = grid.cell(*h).unwrap();
            assert_eq!(cell.terrain, Terrain::Village, "factory base terrain");
            assert!(cell.factory && cell.road, "factory flags at {h:?}");
        }
        // Existing villages are preferred candidates (and keep Village).
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        grid.set_terrain(HexCoord::new(14, 12), Terrain::Village);
        let picked = place_factories(&mut grid, &in_province, Some(anchor), 12, 1);
        assert_eq!(picked.len(), 3, "industry 12 / 4 = 3 districts");
        assert!(
            picked.contains(&HexCoord::new(14, 12)),
            "the only village in range must be picked: {picked:?}"
        );
        // Guards: no anchor, no industry; clamp at 3.
        assert!(place_factories(&mut grid, &in_province, None, 9, 1).is_empty());
        assert!(place_factories(&mut grid, &in_province, Some(anchor), 0, 1).is_empty());
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        assert_eq!(
            place_factories(&mut grid, &in_province, Some(anchor), 99, 1).len(),
            3,
            "district count clamps to 3"
        );
    }

    /// A top-up factory stamped on a Hills hex must KEEP the hill's
    /// elevation (movement pricing / LoS read it) — the stamp is a direct
    /// terrain swap, never `set_terrain` (which flattens to 0).
    #[test]
    fn factory_stamp_on_hills_keeps_the_elevation() {
        let in_province = vec![true; 24 * 24];
        let anchor = HexCoord::new(12, 12);
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        // Every ring-2..=4 hex is a 2-high hill, so any pick lands on one.
        for r in 0..24i32 {
            for q in 0..24i32 {
                let h = HexCoord::new(q, r);
                if (2..=4).contains(&h.distance(anchor)) {
                    let c = grid.cell_mut(h).unwrap();
                    c.terrain = Terrain::Hills;
                    c.elevation = 2;
                }
            }
        }
        let picked = place_factories(&mut grid, &in_province, Some(anchor), 8, 1);
        assert_eq!(picked.len(), 2);
        for h in &picked {
            let cell = grid.cell(*h).unwrap();
            assert_eq!(cell.terrain, Terrain::Village);
            assert_eq!(cell.elevation, 2, "hill elevation lost at {h:?}");
        }
    }

    /// Rail painting on a synthetic grid: the straight line bridges the
    /// river strip, clips to the in-province mask, and the bridge_edges
    /// stamp covers rail hexes too.
    #[test]
    fn rail_line_bridges_rivers_and_stays_in_province() {
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        for r in 0..24 {
            grid.set_terrain(HexCoord::new(12, r), Terrain::River);
        }
        let mut in_province = vec![true; 24 * 24];
        for r in 0..24 {
            for q in 0..4 {
                in_province[r * 24 + q] = false; // west of q=4 is foreign
            }
        }
        let painted = paint_rail_line(
            &mut grid,
            &in_province,
            HexCoord::new(0, 12),
            HexCoord::new(20, 12),
        );
        assert!(painted);
        // The river hex on the line is railed + bridged; the foreign west
        // stays untouched.
        let river = grid.cell(HexCoord::new(12, 12)).unwrap();
        assert!(river.rail && river.bridge && river.terrain == Terrain::River);
        for q in 0..4 {
            assert!(!grid.cell(HexCoord::new(q, 12)).unwrap().rail);
        }
        // An adjacency river edge under a rail hex bridges.
        grid.cell_mut(HexCoord::new(14, 12)).unwrap().river_edges = river_bit(HexDirection::E);
        stamp_bridge_edges(&mut grid, &in_province, 1, 1.0);
        assert_eq!(
            grid.cell(HexCoord::new(14, 12)).unwrap().bridge_edges,
            river_bit(HexDirection::E)
        );
    }

    /// The full railway overlay inside `generate`: a static line paints
    /// between unitstacks anchors (in-province only); live levels gate
    /// segments (a level-0 endpoint drops the segment); player-built rail
    /// (live levels, no static line) connects the battle anchor to railed
    /// neighbours.
    #[test]
    fn railways_paint_and_respect_live_levels() {
        // Province 1 = 20×20 px rect in province-2 background; anchors at
        // y = 20 px (unitstacks z is bottom-up: z = 39 − y = 19).
        let stacks = || {
            [
                (1u32, (20.0f32, 19.0f32)),
                (2, (5.0, 19.0)),
                (3, (35.0, 19.0)),
            ]
            .into_iter()
            .collect::<HashMap<u32, (f32, f32)>>()
        };
        let rail_hexes = |tm: &TacticalMap| -> usize {
            tm.grid
                .iter_coords()
                .filter(|c| tm.grid.cell(*c).unwrap().rail)
                .count()
        };
        // No live levels: the static line paints across the province, and
        // every rail hex stays inside the playable mask.
        let mut gen = world(40, 40, (10, 10, 29, 29), 1);
        gen.set_unit_stacks(stacks());
        gen.set_railways(vec![RailwayLine {
            level: 1,
            path: vec![2, 1, 3],
        }]);
        let tm = gen.generate(1, &[]).unwrap();
        let n = rail_hexes(&tm);
        assert!(n >= 10, "static line should cross the province, got {n}");
        for c in tm.grid.iter_coords() {
            let cell = tm.grid.cell(c).unwrap();
            if cell.rail {
                assert!(!cell.out_of_bounds, "rail out of bounds at {c:?}");
            }
        }
        // Live levels, battle province level 0: both segments (2→1, 1→3)
        // die with their endpoint, and the player-built fallback requires
        // the battle province to have rail — nothing paints.
        let mut gen = world(40, 40, (10, 10, 29, 29), 1);
        gen.set_unit_stacks(stacks());
        gen.set_railways(vec![RailwayLine {
            level: 1,
            path: vec![2, 1, 3],
        }]);
        gen.set_rail_levels(Some([(2u32, 1u8), (1, 0), (3, 1)].into_iter().collect()));
        let tm = gen.generate(1, &[]).unwrap();
        assert_eq!(rail_hexes(&tm), 0, "level-0 endpoint must drop segments");
        // Live levels, everything alive: the whole line paints.
        let mut gen = world(40, 40, (10, 10, 29, 29), 1);
        gen.set_unit_stacks(stacks());
        gen.set_railways(vec![RailwayLine {
            level: 1,
            path: vec![2, 1, 3],
        }]);
        gen.set_rail_levels(Some([(2u32, 1u8), (1, 1), (3, 1)].into_iter().collect()));
        let tm = gen.generate(1, &[]).unwrap();
        assert!(rail_hexes(&tm) >= 10);
        // Player-built: no static line, battle province + one neighbour
        // railed → the battle anchor connects to the railed neighbour.
        let mut gen = world(40, 40, (10, 10, 29, 29), 1);
        gen.set_unit_stacks(stacks());
        gen.set_rail_levels(Some([(1u32, 1u8), (2, 1)].into_iter().collect()));
        let tm = gen.generate(1, &[]).unwrap();
        assert!(
            rail_hexes(&tm) >= 5,
            "player-built line to the railed neighbour"
        );
        // … but not to a level-0 neighbour.
        let mut gen = world(40, 40, (10, 10, 29, 29), 1);
        gen.set_unit_stacks(stacks());
        gen.set_rail_levels(Some([(1u32, 1u8), (2, 0)].into_iter().collect()));
        let tm = gen.generate(1, &[]).unwrap();
        assert_eq!(rail_hexes(&tm), 0, "no railed neighbour → no line");
    }

    /// Village density follows the battle province's state category: the
    /// roll-class candidates scale with it, so a wasteland (×0.15)
    /// derives fewer villages than rural (×1.0); an unmapped province
    /// keeps the full rate. Guaranteed branch endpoints don't scale, so
    /// the check is monotonicity, not a ratio.
    #[test]
    fn state_category_scales_village_density() {
        // Two land neighbours so a road net (and thus villages) exists.
        let with_category = |cat: Option<&str>| -> usize {
            let (w, h) = (40u32, 40u32);
            let colors: Vec<u32> = (0..h)
                .flat_map(|y| {
                    (0..w).map(move |x| {
                        if (12..=27).contains(&x) && (12..=27).contains(&y) {
                            RED
                        } else if x < 20 {
                            GREEN
                        } else {
                            BLUE
                        }
                    })
                })
                .collect();
            let mut gen = MapGenerator::new(
                ProvinceMap::from_colors(w, h, colors).unwrap(),
                defs(),
                vec![],
            );
            if let Some(cat) = cat {
                gen.set_province_to_state([(1u32, 7u32)].into_iter().collect());
                gen.set_state_categories([(7u32, cat.to_string())].into_iter().collect());
            }
            let tm = gen.generate(1, &[]).unwrap();
            tm.grid
                .iter_coords()
                .filter(|c| {
                    let cell = tm.grid.cell(*c).unwrap();
                    cell.terrain == Terrain::Village && !cell.out_of_bounds
                })
                .count()
        };
        let rural = with_category(Some("rural"));
        let unmapped = with_category(None);
        let wasteland = with_category(Some("wasteland"));
        assert_eq!(rural, unmapped, "unmapped state keeps the base rate");
        assert!(rural > 0 && wasteland > 0);
        assert!(
            wasteland < rural,
            "wasteland {wasteland} should derive fewer villages than rural {rural}"
        );
    }

    /// Branch endpoints are the structural class: every one of them is
    /// stamped a village, no roll involved.
    #[test]
    fn branch_endpoints_always_become_villages() {
        let mut grid = HexGrid::new(40, 40, Terrain::Plains);
        let anchor = HexCoord::new(20, 20);
        let in_province = vec![true; 40 * 40];
        let mut border_pixels: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        border_pixels.insert(2, (0..40).map(|y| (0, y)).collect());
        border_pixels.insert(3, (0..40).map(|y| (39, y)).collect());
        let land: HashSet<u32> = [2, 3].into_iter().collect();
        let cell_pid = vec![1u32; 40 * 40];
        let ends = overlay_roads(
            &mut grid,
            &in_province,
            &cell_pid,
            Some(anchor),
            1,
            1.0,
            &border_pixels,
            &land,
            (0, 0, 39, 39),
        );
        assert!(
            ends.len() >= 4,
            "a 1600-hex province yields the clamp minimum of branches: {ends:?}"
        );
        place_road_villages(&mut grid, &in_province, Some(anchor), &ends, 1.0, 1, 3);
        for &e in &ends {
            let cell = grid.cell(e).unwrap();
            assert_eq!(
                cell.terrain,
                Terrain::Village,
                "branch endpoint {e:?} must stamp as a village on the spot"
            );
            assert!(cell.road, "the endpoint keeps its road flag");
        }
    }

    /// The bridge deck flag of a hex (bridge-roll tests).
    fn grid_is_bridged(grid: &HexGrid, h: HexCoord) -> bool {
        grid.cell(h).unwrap().bridge
    }

    /// Junction villages: a hex with 3+ road_edges bits rolls the 0.55
    /// class — across seeds it becomes a village (deterministic per seed),
    /// stamped ON the junction hex itself.
    #[test]
    fn junction_rolls_can_stamp_village() {
        let build = || {
            let mut grid = HexGrid::new(24, 24, Terrain::Plains);
            let in_province = vec![true; 24 * 24];
            // A T junction at (15,10): east-west road plus a spur south.
            paint_road(
                &mut grid,
                &in_province,
                &HexCoord::new(10, 10).line_to(HexCoord::new(20, 10)),
                1,
                1.0,
            );
            paint_road(
                &mut grid,
                &in_province,
                &HexCoord::new(15, 10).line_to(HexCoord::new(15, 16)),
                1,
                1.0,
            );
            assert!(
                grid.cell(HexCoord::new(15, 10))
                    .unwrap()
                    .road_edges
                    .count_ones()
                    >= 3
            );
            (grid, in_province)
        };
        let junction = HexCoord::new(15, 10);
        let stamped = (1..300u32).any(|pid| {
            let (mut grid, in_province) = build();
            place_road_villages(&mut grid, &in_province, None, &[], 1.0, pid, 3);
            grid.cell(junction).unwrap().terrain == Terrain::Village
        });
        assert!(stamped, "the 0.55 junction class must hit on some seed");
        // … and the same seed stamps (or not) identically twice.
        let (mut g1, ip1) = build();
        let (mut g2, _) = build();
        place_road_villages(&mut g1, &ip1, None, &[], 1.0, 7, 3);
        place_road_villages(&mut g2, &ip1, None, &[], 1.0, 7, 3);
        assert_eq!(
            g1.cell(junction).unwrap().terrain,
            g2.cell(junction).unwrap().terrain,
            "junction roll must be deterministic"
        );
    }

    /// Waterside villages: candidates 2..=3 from water roll the 0.05
    /// class — across seeds an off-road waterside village appears; never
    /// on the water itself.
    #[test]
    fn waterside_villages_appear_off_road() {
        let build = || {
            let mut grid = HexGrid::new(24, 24, Terrain::Plains);
            let in_province = vec![true; 24 * 24];
            // A small lake, far from any road.
            grid.set_terrain(HexCoord::new(5, 5), Terrain::Water);
            (grid, in_province)
        };
        let stamped = (1..400u32).any(|pid| {
            let (mut grid, in_province) = build();
            place_road_villages(&mut grid, &in_province, None, &[], 1.0, pid, 3);
            let coords: Vec<HexCoord> = grid.iter_coords().collect();
            coords.into_iter().any(|c| {
                let cell = grid.cell(c).unwrap();
                cell.terrain == Terrain::Village && near_water(&grid, c, 2, 3)
            })
        });
        assert!(stamped, "the 0.05 waterside class must hit on some seed");
        let (mut grid, in_province) = build();
        for pid in 1..40u32 {
            place_road_villages(&mut grid, &in_province, None, &[], 1.0, pid, 3);
        }
        assert_ne!(
            grid.cell(HexCoord::new(5, 5)).unwrap().terrain,
            Terrain::Village,
            "a village must never stamp on water"
        );
    }

    /// Near the city (anchor ring ≤4) a derived village stamps as a
    /// factory district — on the structure hex itself.
    #[test]
    fn near_city_village_stamps_factory() {
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let anchor = HexCoord::new(12, 12);
        let in_province = vec![true; 24 * 24];
        // Branch endpoints are the road path's terminal hexes.
        let near = HexCoord::new(14, 13); // ring 3 from the anchor
        let far = HexCoord::new(20, 20); // ring 11
        paint_road(&mut grid, &in_province, &[near], 1, 1.0);
        paint_road(&mut grid, &in_province, &[far], 1, 1.0);
        place_road_villages(
            &mut grid,
            &in_province,
            Some(anchor),
            &[near, far],
            1.0,
            1,
            3,
        );
        let c = grid.cell(near).unwrap();
        assert_eq!(c.terrain, Terrain::Village);
        assert!(c.road, "the structural village keeps the road flag");
        assert!(c.factory, "near-city village must display as a factory");
        let c = grid.cell(far).unwrap();
        assert_eq!(c.terrain, Terrain::Village);
        assert!(!c.factory, "a far branch-end village stays a village");
    }

    /// Villages never stamp on River/Water/Urban ground, even directly on
    /// a road that crosses a river.
    #[test]
    fn villages_never_stamp_on_river_water_urban() {
        for pid in 1..30u32 {
            let mut grid = HexGrid::new(24, 24, Terrain::Plains);
            for r in 0..24 {
                grid.set_terrain(HexCoord::new(12, r), Terrain::River);
            }
            grid.set_terrain(HexCoord::new(6, 6), Terrain::Water);
            grid.set_terrain(HexCoord::new(18, 18), Terrain::Urban);
            let in_province = vec![true; 24 * 24];
            // A road through all three hazards' neighbourhood.
            paint_road(
                &mut grid,
                &in_province,
                &HexCoord::new(2, 18).line_to(HexCoord::new(22, 18)),
                1,
                1.0,
            );
            place_road_villages(&mut grid, &in_province, None, &[], 1.0, pid, 3);
            // The hazard hexes kept their terrain — a village stamp would
            // have replaced it with Village.
            assert_eq!(
                grid.cell(HexCoord::new(12, 18)).unwrap().terrain,
                Terrain::River,
                "pid {pid}: river stamped over"
            );
            assert_eq!(
                grid.cell(HexCoord::new(6, 6)).unwrap().terrain,
                Terrain::Water,
                "pid {pid}: water stamped over"
            );
            assert_eq!(
                grid.cell(HexCoord::new(18, 18)).unwrap().terrain,
                Terrain::Urban,
                "pid {pid}: urban stamped over"
            );
        }
    }

    /// Factory top-up: factory-stamped road villages in the ring count
    /// toward the target; only the difference is newly stamped, and
    /// existing factory hexes are never re-picked.
    #[test]
    fn factory_districts_top_up_existing() {
        let in_province = vec![true; 24 * 24];
        let anchor = HexCoord::new(12, 12);
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        // Two factory villages already in the ring (as place_road_villages
        // would have stamped them).
        for h in [HexCoord::new(14, 12), HexCoord::new(10, 12)] {
            grid.set_terrain(h, Terrain::Village);
            grid.cell_mut(h).unwrap().factory = true;
        }
        let picked = place_factories(&mut grid, &in_province, Some(anchor), 12, 1);
        assert_eq!(
            picked.len(),
            1,
            "industry 12 → 3 districts, 2 exist → top up 1: {picked:?}"
        );
        assert!(
            !picked.contains(&HexCoord::new(14, 12)) && !picked.contains(&HexCoord::new(10, 12)),
            "existing factory hexes must not be re-picked"
        );
        let factories = grid
            .iter_coords()
            .filter(|&c| grid.cell(c).unwrap().factory)
            .count();
        assert_eq!(factories, 3, "the ring tops up to the target");
    }

    /// Branch roads reach at least ring 5: long branches land in 8..=12,
    /// short spurs in 5..=7 — no stub-length branches.
    #[test]
    fn branch_ends_respect_the_ring_floors() {
        let mut grid = HexGrid::new(48, 48, Terrain::Plains);
        let in_province = vec![true; 48 * 48];
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(6, 24).line_to(HexCoord::new(42, 24)),
            1,
            1.0,
        );
        let road_hexes: Vec<HexCoord> = grid
            .iter_coords()
            .filter(|&h| grid.cell(h).unwrap().road)
            .collect();
        let mut ends: Vec<HexCoord> = Vec::new();
        let mut painted = 0usize;
        for i in 0..8usize {
            if let Some((start, end)) = paint_branch(
                &mut grid,
                &in_province,
                1,
                1.0,
                &road_hexes,
                &ends,
                i,
                8,
                12,
                None,
            ) {
                let d = start.distance(end);
                assert!((8..=12).contains(&d), "long branch at ring {d}");
                ends.push(end);
                painted += 1;
            }
        }
        for i in 0..8usize {
            if let Some((start, end)) = paint_branch(
                &mut grid,
                &in_province,
                1,
                1.0,
                &road_hexes,
                &ends,
                100 + i,
                5,
                7,
                None,
            ) {
                let d = start.distance(end);
                assert!(
                    (5..=7).contains(&d),
                    "short spur at ring {d} — below the 5 floor"
                );
                ends.push(end);
                painted += 1;
            }
        }
        assert!(painted > 0, "the open board must place some branches");
    }

    /// Structural villages stamp ON the structure hex (on the road, flag
    /// kept) — junctions and branch endpoints are road hexes by nature.
    #[test]
    fn structural_villages_stamp_on_the_road() {
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let in_province = vec![true; 24 * 24];
        let end = HexCoord::new(14, 10);
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(4, 10).line_to(end),
            1,
            1.0,
        );
        place_road_villages(&mut grid, &in_province, None, &[end], 1.0, 1, 3);
        let cell = grid.cell(end).unwrap();
        assert_eq!(
            cell.terrain,
            Terrain::Village,
            "the endpoint village stamps on the spot"
        );
        assert!(cell.road, "the structural village keeps the road flag");
    }

    /// The structural terrain gate: a structure hex on River/Water/Urban
    /// (or a bridge deck) grows NO village — no relocation to a
    /// neighbour, just no village.
    #[test]
    fn structural_village_needs_host_terrain() {
        let in_province = vec![true; 24 * 24];
        // Endpoint on River ground → no village anywhere from it. A
        // complete crossing paints the flag (and the deck at p=1).
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let end = HexCoord::new(14, 10);
        grid.set_terrain(end, Terrain::River);
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(12, 10).line_to(HexCoord::new(16, 10)),
            1,
            1.0,
        );
        assert!(
            grid.cell(end).unwrap().road,
            "crossing painted the endpoint"
        );
        place_road_villages(&mut grid, &in_province, None, &[end], 1.0, 1, 3);
        assert_ne!(
            grid.cell(end).unwrap().terrain,
            Terrain::Village,
            "a river endpoint grows no village"
        );
        assert!(
            end.neighbors()
                .into_iter()
                .all(|n| grid.cell(n).map(|c| c.terrain) != Some(Terrain::Village)),
            "no relocation to a neighbour either"
        );
        // Endpoint on a bridge deck → no village (bridge gate, even off
        // river ground).
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        paint_road(&mut grid, &in_province, &[end], 1, 1.0);
        grid.cell_mut(end).unwrap().bridge = true; // force the deck flag
        place_road_villages(&mut grid, &in_province, None, &[end], 1.0, 1, 3);
        assert_ne!(
            grid.cell(end).unwrap().terrain,
            Terrain::Village,
            "a bridge deck grows no village"
        );
    }

    /// Wildland scatter (ruling: road-free positions roll a LOW
    /// terrain-scaled probability, never zero): a road-free plains board
    /// still grows a few villages, far sparser than the roadside class;
    /// Mountain rates stay below Plains; the state density factor scales
    /// the count.
    #[test]
    fn wildland_scatter_is_sparse_terrain_scaled() {
        let villages_on = |terrain: Terrain, with_road: bool, density: f32| -> usize {
            let mut grid = HexGrid::new(48, 48, terrain);
            let in_province = vec![true; 48 * 48];
            if with_road {
                paint_road(
                    &mut grid,
                    &in_province,
                    &HexCoord::new(2, 24).line_to(HexCoord::new(46, 24)),
                    1,
                    1.0,
                );
            }
            place_road_villages(&mut grid, &in_province, None, &[], density, 1, 0);
            grid.iter_coords()
                .filter(|&c| grid.cell(c).unwrap().terrain == Terrain::Village)
                .count()
        };
        let plains = villages_on(Terrain::Plains, false, 1.0);
        assert!(plains > 0, "a road-free plains board grows villages");
        assert!(plains <= 30, "wildland stays sparse, got {plains}");
        let mountain = villages_on(Terrain::Mountain, false, 1.0);
        assert!(
            mountain < plains,
            "mountain {mountain} must stay below plains {plains}"
        );
        let sparse = villages_on(Terrain::Plains, false, 0.15);
        assert!(sparse < plains, "the density factor scales the count");
        // The roadside class dominates where a road runs through.
        let with_road = villages_on(Terrain::Plains, true, 1.0);
        assert!(
            with_road > plains,
            "roadside villages must outnumber the wildland-only board ({with_road} vs {plains})"
        );
    }

    /// No industry → the factory budget is zero: near-city villages come
    /// out as plain villages and the top-up stamps nothing.
    #[test]
    fn no_industry_no_factories() {
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let anchor = HexCoord::new(12, 12);
        let in_province = vec![true; 24 * 24];
        let near = HexCoord::new(14, 13);
        paint_road(&mut grid, &in_province, &[near], 1, 1.0);
        place_road_villages(&mut grid, &in_province, Some(anchor), &[near], 1.0, 1, 0);
        assert!(
            grid.cell(near).unwrap().terrain == Terrain::Village,
            "the village still stamps"
        );
        assert!(
            grid.iter_coords().all(|c| !grid.cell(c).unwrap().factory),
            "budget 0 → no factory anywhere"
        );
        assert!(
            place_factories(&mut grid, &in_province, Some(anchor), 0, 1).is_empty(),
            "industry 0 → place_factories stamps nothing"
        );
    }

    /// The factory budget is consumed by near-city villages in acceptance
    /// order; place_factories then tops up only the difference (here: 0).
    #[test]
    fn factory_budget_is_consumed_in_acceptance_order() {
        let mut grid = HexGrid::new(24, 24, Terrain::Plains);
        let anchor = HexCoord::new(12, 12);
        let in_province = vec![true; 24 * 24];
        // Two near-city endpoints (both ring 2), budget 1.
        let ends = [HexCoord::new(14, 12), HexCoord::new(10, 12)];
        for &e in &ends {
            paint_road(&mut grid, &in_province, &[e], 1, 1.0);
        }
        place_road_villages(&mut grid, &in_province, Some(anchor), &ends, 1.0, 1, 1);
        let factories = grid
            .iter_coords()
            .filter(|&c| grid.cell(c).unwrap().factory)
            .count();
        assert_eq!(factories, 1, "budget 1 → exactly one factory village");
        // industry 4 → target 1; one already exists → nothing to add.
        assert!(
            place_factories(&mut grid, &in_province, Some(anchor), 4, 1).is_empty(),
            "the top-up must see the consumed budget"
        );
        assert_eq!(
            grid.iter_coords()
                .filter(|&c| grid.cell(c).unwrap().factory)
                .count(),
            1
        );
    }

    /// Branches never cross a river: River is impassable to their A*, so
    /// a full-height strip is an absolute barrier — the river-barred
    /// search finds no route (no straight fallback), while a main road
    /// (allow_river) still crosses.
    #[test]
    fn branches_never_cross_rivers() {
        let mut grid = HexGrid::new(48, 24, Terrain::Plains);
        let in_province = vec![true; 48 * 24];
        for r in 0..24 {
            grid.set_terrain(HexCoord::new(24, r), Terrain::River);
        }
        // A river-barred branch search across the strip: no route at all.
        assert!(
            road_path(
                &grid,
                &in_province,
                HexCoord::new(20, 12),
                HexCoord::new(28, 12),
                false,
                false
            )
            .is_empty(),
            "branch search must not cross the strip"
        );
        // … while a main road crosses it (and earns its roll for a deck).
        let path = road_path(
            &grid,
            &in_province,
            HexCoord::new(20, 12),
            HexCoord::new(28, 12),
            true,
            false,
        );
        assert!(
            path.iter()
                .any(|h| grid.cell(*h).unwrap().terrain == Terrain::River),
            "main roads keep their river crossings"
        );
        // Branches off a west-bank road stay west of the strip.
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(4, 12).line_to(HexCoord::new(20, 12)),
            1,
            1.0,
        );
        let road_hexes: Vec<HexCoord> = grid
            .iter_coords()
            .filter(|&h| grid.cell(h).unwrap().road)
            .collect();
        let mut ends: Vec<HexCoord> = Vec::new();
        for i in 0..10usize {
            let _ = paint_branch(
                &mut grid,
                &in_province,
                1,
                1.0,
                &road_hexes,
                &ends,
                i,
                5,
                7,
                None,
            )
            .map(|(_, end)| ends.push(end));
        }
        for c in grid.iter_coords() {
            let cell = grid.cell(c).unwrap();
            if cell.terrain == Terrain::River {
                assert!(!cell.road, "a branch crossed the river at {c:?}");
            }
        }
    }

    /// Railways route around impassable sea: the rail A* hugs the coast
    /// instead of dead-ending at both shores, never paints Water, and
    /// still decks its river crossings. No land route at all → the line
    /// is dropped (false, nothing painted).
    #[test]
    fn rail_routes_around_sea() {
        let in_province = vec![true; 48 * 24];
        let mut grid = HexGrid::new(48, 24, Terrain::Plains);
        // A sea bay filling rows 8..=15 between q 10..=30 — the straight
        // line (2,12)→(44,12) would cross it; the route must detour.
        for r in 8..=15 {
            for q in 10..=30 {
                grid.set_terrain(HexCoord::new(q, r), Terrain::Water);
            }
        }
        // A river strip west of the bay, crossed squarely.
        for r in 0..24 {
            grid.set_terrain(HexCoord::new(5, r), Terrain::River);
        }
        assert!(paint_rail_line(
            &mut grid,
            &in_province,
            HexCoord::new(2, 12),
            HexCoord::new(44, 12)
        ));
        for c in grid.iter_coords() {
            let cell = grid.cell(c).unwrap();
            assert!(
                !cell.rail || cell.terrain != Terrain::Water,
                "rail on sea at {c:?}"
            );
        }
        // Both anchors are railed and rail-connected around the bay.
        assert!(grid.cell(HexCoord::new(2, 12)).unwrap().rail);
        assert!(grid.cell(HexCoord::new(44, 12)).unwrap().rail);
        let rail_set: HashSet<HexCoord> = grid
            .iter_coords()
            .filter(|&c| grid.cell(c).unwrap().rail)
            .collect();
        let mut seen: HashSet<HexCoord> = HashSet::new();
        let mut stack = vec![HexCoord::new(2, 12)];
        seen.insert(HexCoord::new(2, 12));
        while let Some(c) = stack.pop() {
            for n in c.neighbors() {
                if rail_set.contains(&n) && seen.insert(n) {
                    stack.push(n);
                }
            }
        }
        assert!(
            seen.contains(&HexCoord::new(44, 12)),
            "the line must connect around the bay"
        );
        // The river strip got its deck (railways always bridge).
        assert!(
            grid.cell(HexCoord::new(5, 12)).unwrap().bridge,
            "rail still bridges the river"
        );
        // No land route at all: a full-height sea band → dropped line.
        let mut grid = HexGrid::new(48, 24, Terrain::Plains);
        for r in 0..24 {
            for q in 10..=30 {
                grid.set_terrain(HexCoord::new(q, r), Terrain::Water);
            }
        }
        assert!(!paint_rail_line(
            &mut grid,
            &in_province,
            HexCoord::new(2, 12),
            HexCoord::new(44, 12)
        ));
        assert!(grid.iter_coords().all(|c| !grid.cell(c).unwrap().rail));
    }

    /// Coastal spurs end right ON the shore (a land hex adjacent to
    /// Water) and the endpoint village stamps on the spot.
    #[test]
    fn coastal_spur_ends_on_the_shore_with_a_village() {
        let mut grid = HexGrid::new(48, 48, Terrain::Plains);
        let in_province = vec![true; 48 * 48];
        // A lake block; its west shore runs along q=19.
        for r in 10..=30 {
            for q in 20..=30 {
                grid.set_terrain(HexCoord::new(q, r), Terrain::Water);
            }
        }
        // A main road along the west side to fork from.
        paint_road(
            &mut grid,
            &in_province,
            &HexCoord::new(6, 20).line_to(HexCoord::new(16, 20)),
            1,
            1.0,
        );
        let road_hexes: Vec<HexCoord> = grid
            .iter_coords()
            .filter(|&h| grid.cell(h).unwrap().road)
            .collect();
        let mut ends: Vec<HexCoord> = Vec::new();
        let mut hit = None;
        for i in 0..12usize {
            if let Some((_, end)) = paint_branch(
                &mut grid,
                &in_province,
                1,
                1.0,
                &road_hexes,
                &ends,
                i,
                5,
                8,
                Some((1, 1)),
            ) {
                ends.push(end);
                hit = Some(end);
            }
        }
        let end = hit.expect("a coastal spur must land");
        assert!(
            end.neighbors()
                .into_iter()
                .any(|n| grid.cell(n).map(|c| c.terrain) == Some(Terrain::Water)),
            "coastal end {end:?} not on the shore"
        );
        assert!(grid.cell(end).unwrap().road, "the spur paints its end");
        place_road_villages(&mut grid, &in_province, None, &ends, 1.0, 1, 0);
        assert_eq!(
            grid.cell(end).unwrap().terrain,
            Terrain::Village,
            "the shore endpoint must grow its village on the spot"
        );
    }

    /// Coastal-gate regression: a province whose water lies ENTIRELY in
    /// the out-of-bounds shore ring (the real sea) must still grow coastal
    /// spurs — the gate is grid-wide water presence, not in-province.
    #[test]
    fn coastal_spur_grows_when_sea_is_out_of_bounds() {
        let mut grid = HexGrid::new(48, 48, Terrain::Plains);
        // Left strip (q < 16) is the province; everything east is OOB sea.
        let mut in_province = vec![false; 48 * 48];
        let mut cell_pid = vec![9u32; 48 * 48];
        for r in 0..48usize {
            for q in 0..16usize {
                in_province[r * 48 + q] = true;
                cell_pid[r * 48 + q] = 1;
            }
        }
        for r in 0..48 {
            for q in 16..48 {
                grid.set_terrain(HexCoord::new(q, r), Terrain::Water);
            }
        }
        // One land neighbour north so a main road exists to fork from.
        let mut border_pixels: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        border_pixels.insert(2, (0..16).map(|x| (x, 0)).collect());
        let land: HashSet<u32> = [2].into_iter().collect();
        let anchor = HexCoord::new(8, 24);
        let ends = overlay_roads(
            &mut grid,
            &in_province,
            &cell_pid,
            Some(anchor),
            1,
            1.0,
            &border_pixels,
            &land,
            (0, 0, 47, 47),
        );
        let coastal = ends.iter().copied().find(|e| {
            e.neighbors()
                .into_iter()
                .any(|n| grid.cell(n).map(|c| c.terrain) == Some(Terrain::Water))
        });
        assert!(
            coastal.is_some(),
            "no coastal spur with OOB-only sea; ends={ends:?}"
        );
    }

    /// Coast stubs bucket by water-bearing direction: a ring island
    /// (water on all four sides) stubs three DIFFERENT coasts instead of
    /// stacking on one long shore; a single water band stays at 1–2
    /// stubs.
    #[test]
    fn coast_stubs_bucket_by_water_direction() {
        let in_province = vec![true; 48 * 48];
        // Ring island: water around a central 29×29 land block.
        let island = || {
            let mut grid = HexGrid::new(48, 48, Terrain::Water);
            for r in 10..=38 {
                for q in 10..=38 {
                    grid.set_terrain(HexCoord::new(q, r), Terrain::Plains);
                }
            }
            grid
        };
        // A hex's dominant water direction = its bucket.
        let bucket_of = |grid: &HexGrid, h: HexCoord| {
            let mut counts = [0u32; 6];
            for d in HexDirection::ALL {
                if grid.cell(h.neighbor(d)).map(|c| c.terrain) == Some(Terrain::Water) {
                    counts[d as usize] += 1;
                }
            }
            (0..6)
                .max_by_key(|&d| (counts[d], std::cmp::Reverse(d)))
                .unwrap()
        };
        let stubs = coast_stub_sites(&island(), &in_province, 1);
        assert_eq!(
            stubs.len(),
            3,
            "a 4-sided island gets the top-3 buckets: {stubs:?}"
        );
        let mut buckets: Vec<usize> = stubs.iter().map(|&s| bucket_of(&island(), s)).collect();
        buckets.sort_unstable();
        buckets.dedup();
        assert_eq!(buckets.len(), 3, "stubs face distinct coasts: {buckets:?}");
        // Integration: with a city, every coast stub is painted and
        // road-connected to the anchor.
        let mut grid = island();
        let cell_pid = vec![1u32; 48 * 48];
        let anchor = HexCoord::new(24, 24);
        overlay_roads(
            &mut grid,
            &in_province,
            &cell_pid,
            Some(anchor),
            1,
            1.0,
            &HashMap::new(),
            &HashSet::new(),
            (0, 0, 47, 47),
        );
        let reach = road_reachable(&grid, anchor);
        for &s in &stubs {
            assert!(grid.cell(s).unwrap().road, "coast stub {s:?} unpainted");
            assert!(reach.contains(&s), "coast stub {s:?} not connected");
        }
        // Single band (north): 1–2 stubs, never more.
        let mut band = HexGrid::new(48, 48, Terrain::Plains);
        for r in 6..=9 {
            for q in 0..48 {
                band.set_terrain(HexCoord::new(q, r), Terrain::Water);
            }
        }
        let stubs = coast_stub_sites(&band, &in_province, 1);
        assert!(
            (1..=2).contains(&stubs.len()),
            "single band: 1–2 stubs, got {}",
            stubs.len()
        );
    }

    /// Main-road bridges follow infrastructure: each contiguous River run
    /// on the path rolls p = level / 5 — full infrastructure decks every
    /// segment, zero decks none, and 1/5 deterministically leaves at
    /// least one ford here. The road flag stays on the River hexes
    /// either way (a ford is waded).
    #[test]
    fn main_road_bridges_follow_infrastructure() {
        let build = |p: f32| {
            let mut grid = HexGrid::new(32, 24, Terrain::Plains);
            for r in 0..24 {
                grid.set_terrain(HexCoord::new(10, r), Terrain::River);
                grid.set_terrain(HexCoord::new(20, r), Terrain::River);
            }
            let in_province = vec![true; 32 * 24];
            let path = HexCoord::new(2, 12).line_to(HexCoord::new(30, 12));
            paint_road(&mut grid, &in_province, &path, 1, p);
            grid
        };
        let river_hexes = |grid: &HexGrid| {
            grid.iter_coords()
                .filter(|&c| {
                    grid.cell(c).unwrap().terrain == Terrain::River && grid.cell(c).unwrap().road
                })
                .collect::<Vec<_>>()
        };
        let full = build(1.0);
        assert!(
            river_hexes(&full)
                .iter()
                .all(|&h| grid_is_bridged(&full, h)),
            "full infrastructure decks every crossing"
        );
        let none = build(0.0);
        assert!(
            river_hexes(&none)
                .iter()
                .all(|&h| !grid_is_bridged(&none, h)),
            "zero infrastructure builds nothing"
        );
        // Fords keep the road flag.
        assert!(!river_hexes(&none).is_empty());
        let fifth = build(0.2);
        assert!(
            river_hexes(&fifth)
                .iter()
                .any(|&h| !grid_is_bridged(&fifth, h)),
            "1/5 infrastructure deterministically leaves a ford here"
        );
        // Determinism: identical build → identical decks.
        let fifth2 = build(0.2);
        for c in fifth.iter_coords() {
            assert_eq!(
                fifth.cell(c).unwrap().bridge,
                fifth2.cell(c).unwrap().bridge,
                "nondeterministic bridge at {c:?}"
            );
        }
    }

    /// Adjacency-river bridge edges: rail hexes always stamp; road hexes
    /// roll the infrastructure probability.
    #[test]
    fn border_river_road_edges_roll_by_infrastructure() {
        let in_province = vec![true; 24 * 24];
        let build = |rail: bool, p: f32| {
            let mut grid = HexGrid::new(24, 24, Terrain::Plains);
            let h = HexCoord::new(5, 5);
            let cell = grid.cell_mut(h).unwrap();
            cell.river_edges = river_bit(HexDirection::E);
            if rail {
                cell.rail = true;
            } else {
                cell.road = true;
            }
            stamp_bridge_edges(&mut grid, &in_province, 1, p);
            grid.cell(h).unwrap().bridge_edges
        };
        assert_eq!(
            build(false, 1.0),
            river_bit(HexDirection::E),
            "full infra roads stamp"
        );
        assert_eq!(build(false, 0.0), 0, "zero infra roads never stamp");
        assert_eq!(
            build(true, 0.0),
            river_bit(HexDirection::E),
            "rail always stamps"
        );
    }
}
