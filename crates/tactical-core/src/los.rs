//! Line of sight across the hex grid (used by fog of war and precision fire §6.3).
//!
//! The ONLY blocker is elevation — an intermediate hex blocks when
//! it stands strictly higher than BOTH endpoints (ridge rule). Terrain hard
//! blocks were removed for Mountain (relief now comes from per-hex elevation
//! noise, §4.2 step 8) and never existed for the rest; Urban keeps blocking
//! because buildings block sight regardless of ground height. Sight is still
//! hard-capped by `effective_sight` — LoS only shortens it, never extends it.

use crate::grid::HexGrid;
use crate::hex::HexCoord;
use crate::terrain::Terrain;

/// True if a straight hex line between `from` and `to` is unobstructed:
/// no intermediate hex is strictly higher than the higher of the two
/// endpoints. Equal elevation never blocks (a plateau is open ground).
pub fn has_line_of_sight(grid: &HexGrid, from: HexCoord, to: HexCoord) -> bool {
    if from == to {
        return true;
    }
    let from_elev = grid.cell(from).map(|c| c.elevation).unwrap_or(0);
    let to_elev = grid.cell(to).map(|c| c.elevation).unwrap_or(0);
    let endpoint_max = from_elev.max(to_elev);

    let line = from.line_to(to);
    // The slice already excludes both endpoints.
    for h in &line[1..line.len().saturating_sub(1).max(1)] {
        if let Some(c) = grid.cell(*h) {
            // Only Urban still hard-blocks by terrain (buildings);
            // mountain relief is carried by the elevation comparison.
            if c.terrain.blocks_los() || c.elevation > endpoint_max {
                return false;
            }
        }
    }
    true
}

/// Effective sight: unit sight plus terrain modifier relative to the
/// plains baseline of 2 (§6.1 unit sight × §6.6 sight mod).
/// Recon(4) on plains → 4, infantry(2) on hills(3) → 3, anyone in
/// forest/jungle → plains-level (they conceal the occupant from
/// observers — fog.rs — instead of blinding it); urban(1) still blinds.
pub fn effective_sight(unit_sight: i32, terrain_sight: i32) -> i32 {
    (unit_sight + terrain_sight - 2).max(1)
}

/// All hexes a unit can currently see: within `sight` and with clear LoS.
pub fn visible_hexes(grid: &HexGrid, from: HexCoord, sight: i32) -> Vec<HexCoord> {
    from.hexes_in_range(sight)
        .into_iter()
        .filter(|h| grid.in_bounds(*h) && has_line_of_sight(grid, from, *h))
        .collect()
}

/// Melee height-difference gain: for MELEE
/// fights (distance ≤ 1) each level of elevation advantage over the
/// defender adds `gain`, clamped at `cap` (defaults 0.15 / 0.45 = 3 levels).
/// The crest occupant hits downhill at ×(1+gain)/level and shrugs the
/// uphill assault; flat or non-contact fights are neutral (×1.0).
/// Symmetric by construction — the counter-fire strike gives the uphill
/// unit the edge both ways. Shared by combat resolution (strike) and the
/// AI's expected-damage mirror (est_org_damage).
pub fn melee_elevation_mult(
    grid: &HexGrid,
    a_pos: HexCoord,
    d_pos: HexCoord,
    distance: i32,
    gain: f32,
    cap: f32,
) -> f32 {
    if distance > 1 {
        return 1.0;
    }
    let ae = grid.cell(a_pos).map(|c| c.elevation).unwrap_or(0);
    let de = grid.cell(d_pos).map(|c| c.elevation).unwrap_or(0);
    let diff = ((ae - de) as f32 * gain).clamp(-cap, cap);
    1.0 + diff
}

/// Indirect-fire crest factor between a gun and its target:
/// the shell flies a high arc so the ridge line NEVER blocks it
/// (no LOS check here) — what decides the damage is the TARGET'S OWN STEP,
/// the hex on the gun-target line immediately before the target hex:
///
/// - neighbour HIGHER than the target → the target sits on the reverse slope
///   (defilade): the ridge shoulder throttles the shell's impact angle —
///   Korean-war reverse-slope defence, the guns cannot reach the back of
///   the hill → ×0.5.
/// - neighbour LOWER than the target → the target sits ON the crest: its
///   whole body is above the skyline, the ridge line is the death line
///   under plunging fire → ×1.5.
/// - equal → neutral (×1.0).
///
/// URBAN targets are always neutral: a victory-
/// point city is large enough to flatten its site, and the buildings
/// absorb the shell regardless of the near step — city fights read as flat
/// ground for fire support (sieges must grind on assault, not be throttled
/// by the valley the town happens to sit in).
///
/// Distance must be ≥ 2 (the caller guards: at distance 1 the melee
/// elevation gain applies instead, and at distance 2 the single
/// intermediate hex IS the target's step — the definitions coincide).
pub fn indirect_crest_mult(
    grid: &HexGrid,
    from: HexCoord,
    to: HexCoord,
    exposed_mult: f32,
    defilade_mult: f32,
) -> f32 {
    let target = grid.cell(to).map(|c| c.terrain).unwrap_or(Terrain::Plains);
    if target == Terrain::Urban {
        return 1.0;
    }
    let line = from.line_to(to);
    if line.len() < 3 {
        return 1.0;
    }
    let step = line[line.len() - 2];
    let te = grid.cell(to).map(|c| c.elevation).unwrap_or(0);
    let ne = grid.cell(step).map(|c| c.elevation).unwrap_or(0);
    if ne > te {
        defilade_mult
    } else if ne < te {
        exposed_mult
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Terrain;

    fn elev(g: &mut HexGrid, h: HexCoord, e: i32) {
        g.cell_mut(h).unwrap().elevation = e;
    }

    #[test]
    fn clear_plains_visible() {
        let g = HexGrid::new(10, 10, Terrain::Plains);
        assert!(has_line_of_sight(
            &g,
            HexCoord::new(0, 0),
            HexCoord::new(5, 0)
        ));
    }

    #[test]
    fn flat_plateau_never_blocks() {
        // Same elevation everywhere (incl. an all-mountain province before
        // noise): equal elevation never blocks — no terrain hard block.
        let mut g = HexGrid::new(10, 10, Terrain::Mountain);
        let coords: Vec<_> = g.iter_coords().collect();
        for h in coords {
            elev(&mut g, h, 2);
        }
        assert!(has_line_of_sight(
            &g,
            HexCoord::new(0, 0),
            HexCoord::new(4, 0)
        ));
    }

    #[test]
    fn ridge_blocks_valley_to_valley() {
        let mut g = HexGrid::new(10, 10, Terrain::Mountain);
        elev(&mut g, HexCoord::new(0, 0), 0);
        elev(&mut g, HexCoord::new(4, 0), 0);
        elev(&mut g, HexCoord::new(2, 0), 3);
        assert!(!has_line_of_sight(
            &g,
            HexCoord::new(0, 0),
            HexCoord::new(4, 0)
        ));
    }

    #[test]
    fn peaks_see_over_saddle() {
        let mut g = HexGrid::new(10, 10, Terrain::Mountain);
        elev(&mut g, HexCoord::new(0, 0), 4);
        elev(&mut g, HexCoord::new(4, 0), 3);
        elev(&mut g, HexCoord::new(2, 0), 2);
        assert!(has_line_of_sight(
            &g,
            HexCoord::new(0, 0),
            HexCoord::new(4, 0)
        ));
    }

    #[test]
    fn peak_overlooks_valley() {
        let mut g = HexGrid::new(10, 10, Terrain::Mountain);
        elev(&mut g, HexCoord::new(0, 0), 4);
        elev(&mut g, HexCoord::new(4, 0), 0);
        // Intermediate valley floor 1 < peak 4 -> clear (symmetric rule).
        assert!(has_line_of_sight(
            &g,
            HexCoord::new(0, 0),
            HexCoord::new(4, 0)
        ));
    }

    #[test]
    fn urban_still_blocks() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(2, 0), Terrain::Urban);
        assert!(!has_line_of_sight(
            &g,
            HexCoord::new(0, 0),
            HexCoord::new(4, 0)
        ));
    }

    #[test]
    fn standing_on_mountain_sees_over_plains() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(0, 0), Terrain::Mountain);
        // endpoint elevation 2 > intermediate plains 0 -> clear
        assert!(has_line_of_sight(
            &g,
            HexCoord::new(0, 0),
            HexCoord::new(4, 0)
        ));
    }

    #[test]
    fn sight_range_still_caps_vision() {
        // LoS may only SHORTEN the sight radius, never extend it: a peak
        // does not see beyond `effective_sight` hexes.
        let mut g = HexGrid::new(20, 20, Terrain::Plains);
        elev(&mut g, HexCoord::new(10, 10), 6);
        let seen = visible_hexes(&g, HexCoord::new(10, 10), 4);
        assert!(seen.iter().all(|h| HexCoord::new(10, 10).distance(*h) <= 4));
        assert!(!seen.contains(&HexCoord::new(10, 15)));
    }

    #[test]
    fn melee_gain_levels_and_cap() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        elev(&mut g, HexCoord::new(0, 0), 2);
        elev(&mut g, HexCoord::new(1, 0), 0);
        let (gain, cap) = (0.15, 0.45);
        // 2 levels up: ×1.30; the valley defender: ×0.70.
        let up = melee_elevation_mult(&g, HexCoord::new(0, 0), HexCoord::new(1, 0), 1, gain, cap);
        let down = melee_elevation_mult(&g, HexCoord::new(1, 0), HexCoord::new(0, 0), 1, gain, cap);
        assert!((up - 1.30).abs() < 1e-5);
        assert!((down - 0.70).abs() < 1e-5);
        // Beyond contact: neutral.
        assert_eq!(
            melee_elevation_mult(&g, HexCoord::new(0, 0), HexCoord::new(2, 0), 2, gain, cap),
            1.0
        );
        // 4 levels (out of range of the terrain noise) clamp at the cap.
        elev(&mut g, HexCoord::new(0, 0), 4);
        assert_eq!(
            melee_elevation_mult(&g, HexCoord::new(0, 0), HexCoord::new(1, 0), 1, gain, cap),
            1.45
        );
    }

    #[test]
    fn crest_factor_reads_the_targets_own_step() {
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        let gun = HexCoord::new(0, 0);
        let target = HexCoord::new(3, 0);
        // Near step higher than the target → defilade ×0.5.
        elev(&mut g, HexCoord::new(2, 0), 2);
        elev(&mut g, target, 0);
        assert!((indirect_crest_mult(&g, gun, target, 1.5, 0.5) - 0.5).abs() < 1e-5);
        // Near step lower → exposed crest ×1.5.
        elev(&mut g, HexCoord::new(2, 0), 0);
        elev(&mut g, target, 3);
        assert!((indirect_crest_mult(&g, gun, target, 1.5, 0.5) - 1.5).abs() < 1e-5);
        // Level ground → neutral.
        elev(&mut g, HexCoord::new(2, 0), 1);
        elev(&mut g, target, 1);
        assert_eq!(indirect_crest_mult(&g, gun, target, 1.5, 0.5), 1.0);
        // Distance 1 (no intermediate hex): neutral — melee gain owns it.
        assert_eq!(
            indirect_crest_mult(&g, HexCoord::new(0, 0), HexCoord::new(1, 0), 1.5, 0.5),
            1.0
        );
    }

    #[test]
    fn urban_target_never_reads_the_crest() {
        // A VP city is flat ground for fire support —
        // the near step cannot defilade it even in a mountain bowl (the
        // valley-town siege would otherwise be throttled forever).
        let mut g = HexGrid::new(10, 10, Terrain::Plains);
        g.set_terrain(HexCoord::new(3, 0), Terrain::Urban);
        let gun = HexCoord::new(0, 0);
        let city = HexCoord::new(3, 0);
        // Mountainous near step towering over the city → defilade would
        // fire — the urban-target rule says neutral.
        elev(&mut g, HexCoord::new(2, 0), 3);
        assert_eq!(indirect_crest_mult(&g, gun, city, 1.5, 0.5), 1.0);
        // And never exposed either.
        elev(&mut g, HexCoord::new(2, 0), 0);
        elev(&mut g, HexCoord::new(0, 0), 0);
        assert_eq!(indirect_crest_mult(&g, gun, city, 1.5, 0.5), 1.0);
    }
}
