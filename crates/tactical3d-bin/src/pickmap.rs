//! The GRAPHICAL battle-picker map behind the menu's
//! multi-battle dialog (DESIGN.md §10.1). Built off the UI thread from the
//! vanilla `map/provinces.bmp` + `definition.csv`: the picked state's
//! provinces are cropped to their bounding box; battle provinces are FILLED
//! (player attacking = dark red, defending = dark green — the HOI4 battle
//! bubble pairing), internal province borders are SOLID GRAY (the BFS
//! arc-length dash was retired), and the state's
//! outer border is a SOLID bright single texel. Hover emphasis = brightened
//! fill + a solid outline. Hit testing is pixel-exact via the per-texel
//! province buffer — no polygon extraction.

use std::collections::HashMap;
use std::path::Path;

use tactical_map::{ProvinceInfo, ProvinceMap};

/// Margin around the state's bounding box (breathing room around the
/// solid outline).
const MARGIN: u32 = 4;

/// Player attacks — dark red fill (the HOI4 battle-bubble pairing —
/// attacker red, defender green).
pub const ATK_FILL: [u8; 4] = [150, 52, 48, 220];
/// Player defends — dark green fill.
pub const DEF_FILL: [u8; 4] = [58, 112, 60, 220];
/// Hover emphasis: brightened fills.
const ATK_FILL_HI: [u8; 4] = [208, 88, 78, 240];
const DEF_FILL_HI: [u8; 4] = [96, 168, 100, 240];
/// Internal province borders (solid gray).
const PROV_EDGE: [u8; 4] = [156, 150, 140, 175];
/// State outer border (solid): bright parchment.
const STATE_EDGE: [u8; 4] = [236, 230, 214, 255];
/// Hovered battle province outline: near-white.
const HOVER_EDGE: [u8; 4] = [255, 250, 235, 255];

/// Fill color for a battle province by the player's side (legend swatches
/// use this too).
pub fn side_fill(player_attacker: bool) -> [u8; 4] {
    if player_attacker {
        ATK_FILL
    } else {
        DEF_FILL
    }
}

/// Per-texel border classification, precomputed at build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    None,
    /// Internal border between two in-state provinces (solid gray line).
    Province,
    /// The picked state's outer border (solid bright line).
    State,
}

/// The picker's map: one texel per provinces.bmp pixel inside the picked
/// state's (cropped + margined) bounding box.
pub struct PickMap {
    pub width: u32,
    pub height: u32,
    /// Province id per texel (0 = outside the picked state), row-major top.
    prov_at: Vec<u32>,
    edge: Vec<Edge>,
    /// Battle provinces: province id → player-is-attacker.
    battles: HashMap<u32, bool>,
    /// Label anchor per battle province (texel-space centroid).
    centroids: HashMap<u32, (f32, f32)>,
}

impl PickMap {
    /// Province id at a texel (0 outside the state / out of bounds).
    pub fn province_at(&self, x: u32, y: u32) -> u32 {
        if x < self.width && y < self.height {
            self.prov_at[(y * self.width + x) as usize]
        } else {
            0
        }
    }

    /// `Some(player_attacker)` when the province hosts a pickable battle.
    pub fn battle_side(&self, province: u32) -> Option<bool> {
        self.battles.get(&province).copied()
    }

    /// Battle provinces with their label anchors (texel-space centroids).
    pub fn labels(&self) -> impl Iterator<Item = (u32, bool, (f32, f32))> + '_ {
        self.battles
            .iter()
            .filter_map(|(p, atk)| self.centroids.get(p).map(|c| (*p, *atk, *c)))
    }

    /// Rasterize the map to RGBA8 (`hover` = the battle province under the
    /// cursor, rendered emphasized). Re-run on hover change; the menu
    /// re-uploads the returned texture.
    pub fn render(&self, hover: Option<u32>) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let mut img = vec![0u8; self.prov_at.len() * 4];
        // 1) battle-province fills (non-battle provinces stay transparent —
        //    only provinces with battles are filled, per spec).
        for (i, &p) in self.prov_at.iter().enumerate() {
            let Some(atk) = self.battles.get(&p) else {
                continue;
            };
            let c = match (atk, hover == Some(p)) {
                (true, false) => ATK_FILL,
                (true, true) => ATK_FILL_HI,
                (false, false) => DEF_FILL,
                (false, true) => DEF_FILL_HI,
            };
            img[i * 4..i * 4 + 4].copy_from_slice(&c);
        }
        // 2) solid gray internal province borders.
        for (i, e) in self.edge.iter().enumerate() {
            if *e == Edge::Province {
                img[i * 4..i * 4 + 4].copy_from_slice(&PROV_EDGE);
            }
        }
        // 3) solid state border — single texel (the 3×3 stamp read far too
        //    thick once the map is upscaled).
        for (i, e) in self.edge.iter().enumerate() {
            if *e == Edge::State {
                let (x, y) = (i as u32 % w, i as u32 / w);
                put(&mut img, w, h, x, y, STATE_EDGE);
            }
        }
        // 4) hover emphasis: solid single-texel outline around the hovered
        //    battle province (its own border texels, both kinds).
        if let Some(p) = hover {
            for (i, e) in self.edge.iter().enumerate() {
                if *e != Edge::None && self.prov_at[i] == p {
                    let (x, y) = (i as u32 % w, i as u32 / w);
                    put(&mut img, w, h, x, y, HOVER_EDGE);
                }
            }
        }
        img
    }
}

fn put(img: &mut [u8], w: u32, h: u32, x: u32, y: u32, c: [u8; 4]) {
    if x < w && y < h {
        let i = ((y * w + x) * 4) as usize;
        img[i..i + 4].copy_from_slice(&c);
    }
}

fn pack_rgb(rgb: (u8, u8, u8)) -> u32 {
    ((rgb.0 as u32) << 16) | ((rgb.1 as u32) << 8) | rgb.2 as u32
}

/// Build the picker map. `battles` = (province, player_attacker) pairs
/// inside `state`. None when the state has no texels at all (the caller
/// falls back to the plain list).
pub fn build_pick_map(
    bmp: &ProvinceMap,
    defs: &HashMap<u32, ProvinceInfo>,
    p2s: &HashMap<u32, u32>,
    state: u32,
    battles: &[(u32, bool)],
) -> Option<PickMap> {
    let color_to_id: HashMap<u32, u32> = defs.values().map(|p| (pack_rgb(p.rgb), p.id)).collect();
    let (w, h) = (bmp.width as usize, bmp.height as usize);
    let lookup = |x: usize, y: usize| -> u32 {
        color_to_id
            .get(&bmp.colors()[y * w + x])
            .copied()
            .unwrap_or(0)
    };
    // Pass 1 (full map): the picked state's bounding box. No full-size temp
    // buffer — the per-pixel id resolution is redone on the crop in pass 2.
    let mut bbox: Option<(usize, usize, usize, usize)> = None;
    for y in 0..h {
        for x in 0..w {
            let id = lookup(x, y);
            if id != 0 && p2s.get(&id) == Some(&state) {
                let b = bbox.get_or_insert((x, y, x, y));
                b.0 = b.0.min(x);
                b.1 = b.1.min(y);
                b.2 = b.2.max(x);
                b.3 = b.3.max(y);
            }
        }
    }
    let (minx, miny, maxx, maxy) = bbox?;
    let m = MARGIN as usize;
    let minx = minx.saturating_sub(m);
    let miny = miny.saturating_sub(m);
    let maxx = (maxx + m).min(w - 1);
    let maxy = (maxy + m).min(h - 1);
    let (cw, ch) = (maxx - minx + 1, maxy - miny + 1);
    // Pass 2 (crop only): per-texel province ids (0 = outside the state) +
    // per-province pixel sums for the battle-province centroids.
    let mut prov_at = vec![0u32; cw * ch];
    let mut sums: HashMap<u32, (u64, u64, u64)> = HashMap::new();
    for cy in 0..ch {
        for cx in 0..cw {
            let id = lookup(minx + cx, miny + cy);
            if id != 0 && p2s.get(&id) == Some(&state) {
                prov_at[cy * cw + cx] = id;
                let s = sums.entry(id).or_default();
                s.0 += cx as u64;
                s.1 += cy as u64;
                s.2 += 1;
            }
        }
    }
    let battles: HashMap<u32, bool> = battles
        .iter()
        .filter(|(p, _)| p2s.get(p) == Some(&state))
        .copied()
        .collect();
    let centroids: HashMap<u32, (f32, f32)> = battles
        .keys()
        .filter_map(|p| {
            sums.get(p)
                .map(|(sx, sy, n)| (*p, (*sx as f32 / *n as f32, *sy as f32 / *n as f32)))
        })
        .collect();
    // Edge classification (4-neighborhood; out-of-crop counts as outside —
    // margin guarantees a ring, except states touching the bitmap edge,
    // where the solid edge along the crop boundary is the wanted look).
    let mut edge = vec![Edge::None; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let i = cy * cw + cx;
            let p = prov_at[i];
            if p == 0 {
                continue;
            }
            let mut state_edge = false;
            let mut prov_edge = false;
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let (nx, ny) = (cx as i32 + dx, cy as i32 + dy);
                let q = if nx >= 0 && ny >= 0 && (nx as usize) < cw && (ny as usize) < ch {
                    prov_at[ny as usize * cw + nx as usize]
                } else {
                    0
                };
                if q == 0 {
                    state_edge = true;
                } else if q != p {
                    prov_edge = true;
                }
            }
            edge[i] = if state_edge {
                Edge::State
            } else if prov_edge {
                Edge::Province
            } else {
                Edge::None
            };
        }
    }
    Some(PickMap {
        width: cw as u32,
        height: ch as u32,
        prov_at,
        edge,
        battles,
        centroids,
    })
}

/// Load + build straight from the HOI4 install. Runs on the worker thread —
/// the bitmap read is tens of MB. None on any failure (the caller falls
/// back to the plain list).
pub fn build_pick_map_for(
    hoi4_dir: &Path,
    p2s: &HashMap<u32, u32>,
    state: u32,
    battles: &[(u32, bool)],
) -> Option<PickMap> {
    let map_dir = hoi4_dir.join("map");
    let bmp = ProvinceMap::load_bmp(&map_dir.join("provinces.bmp")).ok()?;
    let defs = tactical_map::load_definition_csv(&map_dir.join("definition.csv")).ok()?;
    build_pick_map(&bmp, &defs, p2s, state, battles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_core::Terrain;
    use tactical_map::ProvinceKind;

    // Synthetic 24×24 map: province 1 (x 3..=10, y 3..=20) and province 2
    // (x 11..=18, y 3..=20) form state 100 side by side; province 3
    // (x 3..=18, y 21..=23) below them is state 200. The provinces are
    // chunky so interior assertions stay clear of every border texel.
    fn fixture() -> (ProvinceMap, HashMap<u32, ProvinceInfo>, HashMap<u32, u32>) {
        let (w, h) = (24usize, 24usize);
        let mut colors = vec![0u32; w * h];
        for y in 0..h {
            for x in 0..w {
                let c = match (x, y) {
                    (3..=10, 3..=20) => 0x0A0000,  // province 1
                    (11..=18, 3..=20) => 0x140000, // province 2
                    (3..=18, 21..=23) => 0x1E0000, // province 3 (other state)
                    _ => 0,
                };
                colors[y * w + x] = c;
            }
        }
        let bmp = ProvinceMap::from_colors(w as u32, h as u32, colors).unwrap();
        let info = |id: u32, rgb: (u8, u8, u8)| ProvinceInfo {
            id,
            kind: ProvinceKind::Land,
            terrain: Terrain::Plains,
            is_coastal: false,
            continent_id: 1,
            rgb,
        };
        let defs: HashMap<u32, ProvinceInfo> = [
            (1, info(1, (10, 0, 0))),
            (2, info(2, (20, 0, 0))),
            (3, info(3, (30, 0, 0))),
        ]
        .into_iter()
        .collect();
        let p2s: HashMap<u32, u32> = [(1, 100), (2, 100), (3, 200)].into_iter().collect();
        (bmp, defs, p2s)
    }

    fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    #[test]
    fn build_crops_and_maps_provinces() {
        let (bmp, defs, p2s) = fixture();
        let map = build_pick_map(&bmp, &defs, &p2s, 100, &[(1, true), (2, false)]).unwrap();
        // Margin 4 around bbox x 3..=18 / y 3..=20, clamped to the bitmap:
        // x 0..=22, y 0..=23.
        assert_eq!((map.width, map.height), (23, 24));
        assert_eq!(map.province_at(6, 10), 1);
        assert_eq!(map.province_at(14, 10), 2);
        assert_eq!(map.province_at(7, 22), 0, "other state is outside");
        assert_eq!(map.province_at(0, 0), 0, "unmapped color is outside");
        assert_eq!(map.battle_side(1), Some(true));
        assert_eq!(map.battle_side(2), Some(false));
        assert_eq!(map.battle_side(3), None);
        // Label anchors at the province centroids (texel space).
        let labels: HashMap<u32, (f32, f32)> = map.labels().map(|(p, _, c)| (p, c)).collect();
        let (cx, cy) = labels[&1];
        assert!(
            (cx - 6.5).abs() < 0.01 && (cy - 11.5).abs() < 0.01,
            "{cx},{cy}"
        );
    }

    #[test]
    fn render_fills_borders_and_hover() {
        let (bmp, defs, p2s) = fixture();
        let map = build_pick_map(&bmp, &defs, &p2s, 100, &[(1, true), (2, false)]).unwrap();
        let img = map.render(None);
        let at = |x, y| px(&img, map.width, x, y);
        // Only battle provinces are filled (by side); everything else is
        // transparent (state-200 province 3, unmapped background).
        assert_eq!(at(6, 10), ATK_FILL);
        assert_eq!(at(14, 10), DEF_FILL);
        assert_eq!(at(7, 22)[3], 0);
        assert_eq!(at(0, 0)[3], 0);
        // Solid state border along the state-200 boundary — single texel,
        // no outward spill.
        assert_eq!(at(6, 20), STATE_EDGE);
        assert_eq!(
            at(6, 21)[3],
            0,
            "single-texel edge leaves the neighbor transparent"
        );
        // Solid gray internal border between provinces 1 and 2:
        // province 1's x=10 column (y 4..=19 — the corner texels y=3/y=20
        // touch the background / state 200, so they classify as STATE edge).
        assert_eq!(
            at(10, 3),
            STATE_EDGE,
            "corner where the internal border meets the state edge"
        );
        assert_eq!(at(10, 5), PROV_EDGE);
        assert_eq!(at(10, 9), PROV_EDGE);
        assert_eq!(at(10, 13), PROV_EDGE);
        // Hover emphasis: brightened fill + solid outline on the hovered
        // battle province only.
        let hi = map.render(Some(1));
        let at_hi = |x, y| px(&hi, map.width, x, y);
        assert_eq!(at_hi(6, 10), ATK_FILL_HI);
        assert_eq!(at_hi(10, 9), HOVER_EDGE, "hover outline overrides the gap");
        assert_eq!(
            at_hi(6, 20),
            HOVER_EDGE,
            "state edge of the hovered province"
        );
        assert_eq!(at_hi(14, 10), DEF_FILL, "other battle province unaffected");
    }

    #[test]
    fn unknown_state_yields_none() {
        let (bmp, defs, p2s) = fixture();
        assert!(build_pick_map(&bmp, &defs, &p2s, 999, &[]).is_none());
    }
}
