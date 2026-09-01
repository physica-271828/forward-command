//! §4.2 grid generation: one HOI4 province → a capped 1 km hex grid with
//! occupancy mask, seeded terrain variation, river edges and deployment
//! zones.

use std::collections::{HashMap, HashSet};

use tactical_core::{HexCoord, HexDirection, HexGrid, Terrain};

use crate::bmp::ProvinceMap;
use crate::csv::{Adjacency, AdjacencyKind, ProvinceInfo};
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
/// tactical-core ([`HexDirection::bit`]); kept here for readability.
pub fn river_bit(dir: HexDirection) -> u8 {
    dir.bit()
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

/// Seeded per-hex village scatter (deterministic per (province, q, r),
/// independent hash domain from the terrain variation).
/// Probability follows the terrain: open countryside gets villages, wilds
/// get few, Urban/River/Water never.
fn village_roll(province_id: u32, q: i32, r: i32, t: Terrain) -> bool {
    let p = match t {
        Terrain::Plains => 0.04,
        Terrain::Clearing => 0.05,
        Terrain::Hills => 0.02,
        Terrain::Forest => 0.015,
        Terrain::Desert => 0.02,
        Terrain::Marsh => 0.01,
        Terrain::Jungle => 0.01,
        Terrain::Mountain => 0.005,
        _ => return false,
    };
    hash01(province_id ^ 0x5EED_5EED, q, r) < p
}

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
}

impl MapGenerator {
    /// Combine the three §4.1 inputs. The bitmap's packed colors are resolved
    /// to province ids once, up front, via the definition.csv palette.
    pub fn new(
        map: ProvinceMap,
        definitions: HashMap<u32, ProvinceInfo>,
        adjacencies: Vec<Adjacency>,
    ) -> Self {
        let color_to_id: HashMap<u32, u32> = definitions
            .values()
            .map(|p| (pack_rgb(p.rgb), p.id))
            .collect();
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
}

fn pack_rgb(rgb: (u8, u8, u8)) -> u32 {
    ((rgb.0 as u32) << 16) | ((rgb.1 as u32) << 8) | rgb.2 as u32
}

impl MapGenerator {
    /// (width, height) of the underlying province bitmap in pixels.
    pub fn map_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Look up a province's definition.csv record.
    pub fn province_info(&self, id: u32) -> Option<&ProvinceInfo> {
        self.provinces.get(&id)
    }

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
    /// `fallback_dirs` apply when nothing resolves.
    pub fn generate_from_sources(
        &self,
        province_id: u32,
        source_provinces: &[u32],
        fallback_dirs: &[HexDirection],
        elevation_seed: u64,
    ) -> Result<TacticalMap> {
        let derived = self.dirs_from_source_provinces(province_id, source_provinces);
        let dirs = if derived.is_empty() {
            fallback_dirs
        } else {
            &derived
        };
        self.generate_impl(province_id, dirs, elevation_seed, Some(source_provinces))
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
        self.generate_impl(province_id, attack_dirs, elevation_seed, None)
    }

    /// Shared body of [`generate_with_elevation_seed`] and
    /// [`generate_from_sources`]. `strip_sources` (live battles, where the
    /// save names the attacking divisions' source provinces) stages the
    /// attacker from those provinces' full shared borders; `None` (script
    /// battles) uses the direction-sector heuristic alone.
    fn generate_impl(
        &self,
        province_id: u32,
        attack_dirs: &[HexDirection],
        elevation_seed: u64,
        strip_sources: Option<&[u32]>,
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
        // note.
        let center_x = (min_x + max_x) as f32 / 2.0;
        let center_y = (min_y + max_y) as f32 / 2.0;
        let (origins, strip_borders) =
            self.resolve_origins(&border_pixels, center_x, center_y, attack_dirs, strip_sources);
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
                    // Seeded village scatter over the countryside
                    // (probability by terrain; never on water/river/urban).
                    let t = if village_roll(pid, q as i32, r as i32, base) {
                        Terrain::Village
                    } else {
                        base
                    };
                    grid.set_terrain(coord, t);
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
        if let Some(level) = vp_level {
            // Unitstacks z is bottom-up; the bitmap ids are top-down.
            let vp_hex = self.unit_stacks.get(&province_id).map(|&(sx, sz)| {
                let px = sx;
                let py = self.height as f32 - 1.0 - sz;
                let col_step = pixel_width / cols as f32;
                let row_step = pixel_height / rows as f32;
                let mut r = ((py - min_y as f32) / row_step - 0.5).round() as i32;
                r = r.clamp(0, rows as i32 - 1);
                let mut q =
                    ((px - min_x as f32) / col_step - 0.5 - if r % 2 == 1 { 0.5 } else { 0.0 })
                        .round() as i32;
                q = q.clamp(0, cols as i32 - 1);
                HexCoord::new(q, r)
            });
            let anchor = self.place_vp_urban(&mut grid, level, is_urban, vp_hex, &in_province);
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
            // Mark every in-province hex whose neighbour across `dir` leaves
            // the province (internal border hex) or the grid (bbox edge).
            for r in 0..rows {
                for q in 0..cols {
                    if !in_province[r * cols + q] {
                        continue;
                    }
                    let coord = HexCoord::new(q as i32, r as i32);
                    let n = coord.neighbor(dir);
                    let n_inside = n.q >= 0
                        && n.r >= 0
                        && (n.q as usize) < cols
                        && (n.r as usize) < rows
                        && in_province[n.r as usize * cols + n.q as usize];
                    if !n_inside {
                        if let Some(cell) = grid.cell_mut(coord) {
                            cell.river_edges |= bit;
                        }
                    }
                }
            }
        }

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
        let zones = if origins.is_empty() {
            deployment_zones(&grid, effective_dirs, &in_province)
        } else {
            deployment_zones_stitched(&grid, province_id, &cell_pid, &in_province, &strip_hex)
        };

        Ok(TacticalMap {
            grid,
            zones,
            province_id,
            base_terrain: info.terrain,
            origin_provinces: origins,
            cell_province: cell_pid,
            vp_label,
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
        // HOI4 rivers.bmp: 254/255 = sea/land background, everything else is
        // a river stroke or a source/flow marker.
        let is_stroke = |x: i32, y: i32| -> bool {
            if x < 0 || y < 0 {
                return false;
            }
            rivers
                .index_at(x as u32, y as u32)
                .map(|v| v != 254 && v != 255)
                .unwrap_or(false)
        };
        let mut stroke: Vec<(i32, i32)> = Vec::new();
        for y in y0..=y1 {
            for x in x0..=x1 {
                if is_stroke(x, y) {
                    stroke.push((x, y));
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
                    if is_stroke(x + dx, y + dy) {
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
        // samples), NOT a hex-unit offset: hexes are always ~7.9 px (true
        // scale; the 512 cap only makes steps LARGER), so any residual
        // convention error is ≤0.5 px ≈ 0.06 hex — below rounding.
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
        let mut marked: HashSet<(i32, i32)> = HashSet::new();
        let stroke_set: HashSet<(i32, i32)> = stroke.iter().copied().collect();
        for &(x, y) in &stroke {
            let (sx, sy) = smooth(x, y);
            let h = to_hex(sx, sy);
            marked.insert((h.q, h.r));
        }
        // Connect 8-neighbouring stroke pixels with hex lines between their
        // SMOOTHED centres — cube-lerp keeps the river continuous and one
        // hex wide (earlier attempts: directional thinning deleted thin
        // rivers; neighbour-count erosion punched holes).
        for &(x, y) in &stroke {
            for (dx, dy) in [(1i32, 0i32), (0, 1), (1, 1), (-1, 1)] {
                if !stroke_set.contains(&(x + dx, y + dy)) {
                    continue;
                }
                let (ax, ay) = smooth(x, y);
                let (bx, by) = smooth(x + dx, y + dy);
                for h in to_hex(ax, ay).line_to(to_hex(bx, by)) {
                    marked.insert((h.q, h.r));
                }
            }
        }
        for (q, r) in marked {
            if q < 0 || r < 0 || q as usize >= cols || r as usize >= rows {
                continue;
            }
            let coord = HexCoord::new(q, r);
            let inside = in_province[r as usize * cols + q as usize];
            let is_water = grid
                .cell(coord)
                .map(|c| c.terrain == Terrain::Water)
                .unwrap_or(false);
            if inside || !is_water {
                grid.set_terrain(coord, Terrain::River);
                if !inside {
                    // Border river band: crossable although it lies just
                    // outside the province (deployment still barred —
                    // is_deployable excludes River).
                    if let Some(cell) = grid.cell_mut(coord) {
                        cell.is_passable = true;
                    }
                }
            }
        }
        if std::env::var("TAC_DEBUG_RIVERS").is_ok() {
            let n = grid
                .iter_coords()
                .filter(|c| {
                    grid.cell(*c)
                        .map(|c| c.terrain == Terrain::River)
                        .unwrap_or(false)
                })
                .count();
            eprintln!("[rivers] painted {n} river hexes (grid {cols}×{rows})");
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
        let gen = MapGenerator::new(ProvinceMap::from_colors(w, h, colors).unwrap(), defs(), vec![]);
        let tm = gen
            .generate_from_sources(1, &[3], &[HexDirection::W], 7)
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
}
