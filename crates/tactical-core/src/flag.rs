//! Flag-capture victory (DESIGN §6.11) — the objective conclusion
//! for battles that would otherwise grind on forever (a hidden city garrison
//! froze headless Warsaw for 300+ turns; real sieges end when the key
//! points fall, not when every battalion dies).
//!
//! Model (design rulings):
//! - City battles: ONE flag = the VP-city urban cluster (map-derived).
//! - Field battles: THREE flags (model A) — the battle script's explicit
//!   `flags:` anchors, or a fallback: three well-separated anchors sampled
//!   from the defender deployment zone's interior core (the ~40% of hexes
//!   with the largest margin to the nearest non-zone cell — attacker zone,
//!   province border, water and the map rim all count).
//! - A flag ZONE is the hex set within `flag_cluster_radius` of its anchor
//!   (≈19 hexes), clipped to the defender deployment zone and passable
//!   terrain. City clusters are the whole urban set (Warsaw 32, Berlin 62).
//! - Progress 0..=`flag_progress_cap`: attacker:defender unit-count ratio
//!   inside the zone > `flag_capture_ratio` → +1 per turn; <
//!   `flag_decay_ratio` → −1; in between → contested, unchanged.
//! - Victory trigger (end of turn): city = the single flag full; field =
//!   ALL three full at the same time. On trigger the ATTACKER WINS
//!   IMMEDIATELY: every defender battalion's org
//!   drops to 0 (strength UNTOUCHED — a surrender, not a massacre) and the
//!   battle declares the attacker's victory at once — no tactical mop-up
//!   flow. The HOI4 strategic layer resolves the retreat / province outcome
//!   itself; the tactical layer only syncs the org-zeroing (strength 0 is
//!   NEVER sent for the collapse).
//! - No flag zones (no VP city, no script `flags:`, no fallback anchors) →
//!   the annihilation path is the only conclusion.

use crate::grid::HexGrid;
use crate::hex::HexCoord;
use crate::params::CombatParams;
use crate::rng::XorShift64;
use crate::unit::{BattalionUnit, Side, UnitState};

/// §6.11: city battle (single urban-cluster flag) vs field battle (three
/// flags, model A — all must be full to trigger).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlagKind {
    #[default]
    City,
    Field,
}

impl FlagKind {
    /// Rust-enum name in snake_case (locale key `flag.kind.*`).
    pub fn name(self) -> &'static str {
        match self {
            FlagKind::City => "city",
            FlagKind::Field => "field",
        }
    }
}

/// One contested flag: its anchor, the hex zone it governs, and its capture
/// progress (battle state — checkpoint restore / rollback keeps it, §6.11).
#[derive(Debug, Clone)]
pub struct FlagZone {
    /// The key-point hex the zone radiates from (city centroid / script
    /// anchor / sampled field anchor).
    pub anchor: HexCoord,
    /// The hex set of the zone: radius-`flag_cluster_radius` cluster around
    /// the anchor ∩ defender zone ∩ passable (city battle: the urban set).
    pub zone: Vec<HexCoord>,
    /// Capture progress, 0..=params.flag_progress_cap.
    pub progress: i32,
    /// §6.11 warning: the "flag falling" banner fired once when progress
    /// crossed 2/3 of the cap (UI bookkeeping, part of battle state so a
    /// rollback re-warns cleanly).
    pub warned_high: bool,
}

impl FlagZone {
    /// Fresh flag at an anchor with its pre-computed zone (battle start).
    pub fn new(anchor: HexCoord, zone: Vec<HexCoord>) -> Self {
        FlagZone {
            anchor,
            zone,
            progress: 0,
            warned_high: false,
        }
    }

    /// The zone hex nearest to `h` (its fight goal).
    pub fn nearest_hex(&self, h: HexCoord) -> Option<HexCoord> {
        self.zone.iter().copied().min_by_key(|z| h.distance(*z))
    }
}

/// The battle's flag board (DESIGN §6.11). `None` zones on a battle = no
/// flag path at all.
#[derive(Debug, Clone, Default)]
pub struct FlagState {
    pub kind: FlagKind,
    pub flags: Vec<FlagZone>,
    /// The §6.11 collapse has fired (defender org zeroed, strength intact);
    /// fires exactly once. The caller declares the attacker's victory.
    pub collapsed: bool,
}

impl FlagState {
    /// End-of-turn bookkeeping (§6.11): recompute the per-flag control
    /// ratio, nudge the progress counters, and — when the capture threshold
    /// is crossed for the first time — zero the defender's org (strength
    /// untouched). The CALLER declares the attacker's victory immediately
    /// (the capture ends the battle, there is no rout
    /// flow to mop up).
    pub fn tick(
        &mut self,
        grid: &HexGrid,
        units: &mut [BattalionUnit],
        params: &CombatParams,
    ) -> FlagTick {
        for flag in &mut self.flags {
            update_flag_progress(flag, grid, units, params);
        }
        let mut tick = FlagTick {
            captured: self.captured(params),
            collapse_fired: false,
        };
        if tick.captured && !self.collapsed {
            self.collapsed = true;
            tick.collapse_fired = true;
            collapse_side(units, Side::Defender);
        }
        tick
    }

    /// §6.11 victory trigger: city = the single flag full; field = ALL
    /// flags full at the same time (a flag that decays back below full
    /// blocks the trigger — the A model).
    pub fn captured(&self, params: &CombatParams) -> bool {
        if self.flags.is_empty() {
            return false;
        }
        match self.kind {
            FlagKind::City => self.flags[0].progress >= params.flag_progress_cap,
            FlagKind::Field => self
                .flags
                .iter()
                .all(|f| f.progress >= params.flag_progress_cap),
        }
    }

    /// Highest per-flag progress fraction (0.0..=1.0) — the UI warning tier.
    pub fn max_progress_ratio(&self, params: &CombatParams) -> f32 {
        self.flags
            .iter()
            .map(|f| f.progress as f32 / params.flag_progress_cap as f32)
            .fold(0.0, f32::max)
    }
}

/// What one end-of-turn [`FlagState::tick`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlagTick {
    /// The capture threshold is crossed (city flag full / all field flags
    /// full) — the attacker wins this turn.
    pub captured: bool,
    /// The §6.11 collapse fired this tick: every defender battalion's org
    /// dropped to 0 (strength untouched — a surrender, not a massacre).
    pub collapse_fired: bool,
}

/// Per-flag progress nudge (§6.11): attacker:defender UNIT-COUNT RATIO
/// inside the zone > `flag_capture_ratio` → +1; < `flag_decay_ratio` → −1;
/// in between → contested, unchanged. HQs and non-combat-effective units
/// (broken, retreating, off-board) count for nothing.
pub fn update_flag_progress(
    flag: &mut FlagZone,
    grid: &HexGrid,
    units: &[BattalionUnit],
    params: &CombatParams,
) {
    let in_zone = |u: &BattalionUnit| grid.in_bounds(u.position) && flag.zone.contains(&u.position);
    let (mut atk, mut def) = (0i32, 0i32);
    for u in units
        .iter()
        .filter(|u| u.is_combat_effective() && !u.is_hq() && in_zone(u))
    {
        match u.side {
            Side::Attacker => atk += 1,
            Side::Defender => def += 1,
        }
    }
    // The designer rule operates on the raw ratio; guard the division.
    let ratio = if def == 0 {
        if atk == 0 {
            0.0
        } else {
            f32::INFINITY
        }
    } else {
        atk as f32 / def as f32
    };
    if ratio > params.flag_capture_ratio {
        flag.progress = (flag.progress + 1).min(params.flag_progress_cap);
    } else if ratio < params.flag_decay_ratio {
        flag.progress = (flag.progress - 1).max(0);
    }
}

/// §6.11 collapse: every unresolved battalion of `side`
/// drops to **org 0 only** — strength is NEVER touched (a surrender, not a
/// massacre; the strategic layer resolves the retreat). No state change, no
/// retreat flow: the caller declares the attacker's victory immediately.
/// Returns the number of battalions affected.
pub fn collapse_side(units: &mut [BattalionUnit], side: Side) -> usize {
    let mut n = 0;
    for u in units.iter_mut() {
        if u.side == side
            && !matches!(
                u.state,
                UnitState::Eliminated
                    | UnitState::Withdrawn
                    | UnitState::Surrendered
                    | UnitState::LeftBattle
            )
        {
            u.org = 0.0;
            n += 1;
        }
    }
    n
}

// ── Zone derivation ─────────────────────────────────────────────────────────

/// §6.11 flag zones — the battle's objective conclusion. Returns `None`
/// when the battle has no flags at all (no VP city, no usable script
/// anchors, no fallback) — the annihilation path is then the only
/// conclusion.
///
/// Priority: city battle (VP-city urban cluster in the defender zone) →
/// field battle with the script's explicit anchors → field fallback
/// (three well-separated anchors sampled from the defender zone's
/// interior core, `flag_deep_core_fraction`).
pub fn derive_flag_state(
    grid: &HexGrid,
    // The attacker zone no longer feeds the fallback sampling: the margin
    // metric measures from every non-defender cell, attackers included —
    // the parameter stays for the public call sites.
    _attacker_zone: &[HexCoord],
    defender_zone: &[HexCoord],
    script_anchors: &[HexCoord],
    params: &CombatParams,
    rng: &mut XorShift64,
) -> Option<FlagState> {
    // City battle: ONE flag = the VP-city urban cluster (map-derived). The
    // generator stamps Urban only at real victory points, so an
    // urban cluster inside the defender zone IS a city battle.
    if let Some(zone) = city_urban_zone(grid, defender_zone) {
        let anchor = cluster_centroid_anchor(&zone);
        return Some(FlagState {
            kind: FlagKind::City,
            flags: vec![FlagZone::new(anchor, zone)],
            collapsed: false,
        });
    }
    // Field battle: THREE flags — script anchors first, then the fallback.
    let mut anchors: Vec<HexCoord> = Vec::new();
    for a in script_anchors {
        let zone = cluster_zone(grid, defender_zone, *a, params);
        if !zone.is_empty() {
            anchors.push(*a);
        }
    }
    if anchors.is_empty() {
        anchors = sample_field_anchors(grid, defender_zone, params, rng)?;
    }
    let flags = anchors
        .into_iter()
        .map(|a| FlagZone::new(a, cluster_zone(grid, defender_zone, a, params)))
        .collect();
    Some(FlagState {
        kind: FlagKind::Field,
        flags,
        collapsed: false,
    })
}

/// The whole VP-city urban cluster inside the defender zone (Warsaw 32,
/// Berlin 62, Stalingrad 38 — DESIGN §6.11), or `None` for field battles.
fn city_urban_zone(grid: &HexGrid, defender_zone: &[HexCoord]) -> Option<Vec<HexCoord>> {
    let urban: Vec<HexCoord> = defender_zone
        .iter()
        .copied()
        .filter(|h| {
            grid.cell(*h)
                .map(|c| c.terrain == crate::terrain::Terrain::Urban)
                .unwrap_or(false)
        })
        .collect();
    if urban.is_empty() {
        None
    } else {
        Some(urban)
    }
}

/// The zone a flag governs: everything within `flag_cluster_radius` of the
/// anchor, clipped to the defender deployment zone and passable terrain.
fn cluster_zone(
    grid: &HexGrid,
    defender_zone: &[HexCoord],
    anchor: HexCoord,
    params: &CombatParams,
) -> Vec<HexCoord> {
    let r = params.flag_cluster_radius;
    let mut zone = Vec::new();
    // Axial cube ring walk over the radius-2 neighborhood (~19 hexes).
    for dq in -r..=r {
        let q1 = (-r - dq).max(-r);
        let q2 = (r - dq).min(r);
        for dr in q1..=q2 {
            let h = HexCoord::new(anchor.q + dq, anchor.r + dr);
            if defender_zone.contains(&h) && grid.cell(h).map(|c| c.is_passable).unwrap_or(false) {
                zone.push(h);
            }
        }
    }
    zone
}

/// Anchor at the centroid of a cluster of hexes (city flag: the heart of
/// the urban mass).
fn cluster_centroid_anchor(hexes: &[HexCoord]) -> HexCoord {
    let n = hexes.len() as f32;
    let (sq, sr) = hexes.iter().fold((0i64, 0i64), |(aq, ar), h| {
        (aq + h.q as i64, ar + h.r as i64)
    });
    let (cq, cr) = (sq as f32 / n, sr as f32 / n);
    hexes
        .iter()
        .copied()
        .min_by_key(|h| {
            let d2 = (h.q as f32 - cq).powi(2) + (h.r as f32 - cr).powi(2);
            (d2 * 1000.0) as i64
        })
        .unwrap_or(HexCoord::ZERO)
}

/// §6.11 fallback anchors: `field_flag_count` well-separated hexes sampled
/// from the defender zone's interior core — the ~`flag_deep_core_fraction`
/// of hexes with the largest MARGIN, where a hex's margin is its distance
/// to the nearest cell NOT in the defender zone: the inter-zone boundary
/// (attacker zone), the province border (out-of-province margin ring,
/// water) and the grid rim all count. The old depth-to-attacker metric
/// alone parked the first anchor on the province's far BORDER (the point
/// geometrically deepest from one edge of a zone is the opposite edge) and
/// the greedy separation pulled the other two into the far corners, where
/// the clipped radius-2 clusters read as "flags glued to the map edge".
/// Deepest-by-margin keeps the rear-core intent while staying clear of
/// every edge. The first anchor is the largest-margin hex; each next one
/// maximizes the minimum distance to the chosen anchors (greedy
/// farthest-point). When fewer than `field_flag_count` well-separated
/// anchors exist, there is NO fallback — the battle keeps the annihilation
/// path only.
fn sample_field_anchors(
    grid: &HexGrid,
    defender_zone: &[HexCoord],
    params: &CombatParams,
    rng: &mut XorShift64,
) -> Option<Vec<HexCoord>> {
    let need = params.field_flag_count.max(1) as usize;
    if defender_zone.is_empty() {
        return None;
    }
    // Multi-source BFS over the grid graph (hex distance == graph distance
    // on the hex lattice): seeds are every cell outside the defender zone
    // (margin 0 — attacker cells, out-of-province ring, water) and every
    // grid-rim zone cell (margin 1 — one step off the map).
    let w = grid.width as i32;
    let h = grid.height as i32;
    let in_zone: std::collections::HashSet<HexCoord> = defender_zone.iter().copied().collect();
    let mut margin = vec![u32::MAX; grid.width * grid.height];
    let mut queue = std::collections::VecDeque::new();
    for r in 0..h {
        for q in 0..w {
            let cell = HexCoord::new(q, r);
            let idx = (r * w + q) as usize;
            let on_rim = q == 0 || r == 0 || q == w - 1 || r == h - 1;
            if !in_zone.contains(&cell) {
                margin[idx] = 0;
                queue.push_back(cell);
            } else if on_rim {
                margin[idx] = 1;
                queue.push_back(cell);
            }
        }
    }
    while let Some(c) = queue.pop_front() {
        let base = margin[(c.r * w + c.q) as usize];
        for n in c.neighbors() {
            if n.q < 0 || n.r < 0 || n.q >= w || n.r >= h {
                continue;
            }
            let idx = (n.r * w + n.q) as usize;
            if margin[idx] > base + 1 {
                margin[idx] = base + 1;
                queue.push_back(n);
            }
        }
    }
    let margin_of = |cell: HexCoord| margin[(cell.r * w + cell.q) as usize] as i32;
    let mut by_margin: Vec<(i32, HexCoord)> = defender_zone
        .iter()
        .copied()
        .map(|cell| (margin_of(cell), cell))
        .collect();
    by_margin.sort_by_key(|(m, cell)| (*m, cell.q, cell.r));
    // The deepest fraction: hexes with margin ≥ the (1 − fraction) quantile.
    let cutoff_idx =
        (by_margin.len() as f32 * (1.0 - params.flag_deep_core_fraction)).max(0.0) as usize;
    let cutoff = by_margin
        .get(cutoff_idx.min(by_margin.len() - 1))
        .map(|(m, _)| *m)?;
    let band: Vec<HexCoord> = by_margin
        .into_iter()
        .rev()
        .take_while(|(m, _)| *m >= cutoff)
        .map(|(_, cell)| cell)
        .collect();
    if band.len() < need {
        return None;
    }
    // Greedy farthest-point sampling; the separation requirement makes the
    // clusters meaningfully distinct (anchors at least 2×radius+1 apart).
    let min_sep = params.flag_cluster_radius * 2 + 1;
    let mut chosen: Vec<HexCoord> = Vec::new();
    // First anchor: the largest-margin hex (deterministic (q, r) tie-break
    // for the equal-margin mass).
    let first = band
        .iter()
        .copied()
        .max_by_key(|cell| (margin_of(*cell), (cell.q, cell.r)))
        .unwrap();
    chosen.push(first);
    while chosen.len() < need {
        let next = band
            .iter()
            .copied()
            .filter(|cell| chosen.iter().all(|c| cell.distance(*c) >= min_sep))
            .max_by_key(|cell| {
                let md = chosen.iter().map(|c| cell.distance(*c)).min().unwrap_or(0);
                (md, rng.next_u64())
            });
        chosen.push(next?); // None = no further well-separated anchor
    }
    Some(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::HexGrid;
    use crate::params::CombatParams;
    use crate::rng::XorShift64;
    use crate::terrain::Terrain;
    use crate::unit::{BattalionUnit, UnitType};

    fn unit(id: usize, side: Side, pos: HexCoord) -> BattalionUnit {
        BattalionUnit::new(id, format!("U{id}"), UnitType::Infantry, side, pos)
    }

    /// 40×40 plains with an urban blob at q=30..34, r=5..10.
    fn grid_with_city() -> HexGrid {
        let mut g = HexGrid::new(40, 40, Terrain::Plains);
        for q in 30..35 {
            for r in 5..11 {
                g.set_terrain(HexCoord::new(q, r), Terrain::Urban);
            }
        }
        g
    }

    fn big_defender_zone() -> Vec<HexCoord> {
        let mut z = Vec::new();
        for q in 0..40 {
            for r in 0..40 {
                z.push(HexCoord::new(q, r));
            }
        }
        z
    }

    #[test]
    fn city_battle_single_flag_from_urban_cluster() {
        let g = grid_with_city();
        let dz = big_defender_zone();
        let az = vec![HexCoord::new(5, 5)];
        let params = CombatParams::default();
        let fs = derive_flag_state(&g, &az, &dz, &[], &params, &mut XorShift64::new(1)).unwrap();
        assert_eq!(fs.kind, FlagKind::City);
        assert_eq!(fs.flags.len(), 1);
        // The 32 urban hexes (5×6=30 here) form the whole zone.
        assert_eq!(fs.flags[0].zone.len(), 30);
        assert!(fs.flags[0]
            .zone
            .iter()
            .all(|h| g.cell(*h).unwrap().terrain == Terrain::Urban));
    }

    #[test]
    fn field_battle_three_flags_from_fallback() {
        // No urban hexes → field battle; the fallback must produce exactly
        // field_flag_count well-separated anchors in the interior core.
        let g = HexGrid::new(40, 40, Terrain::Plains);
        // The zone is the whole grid; the attacker column no longer decides
        // where the core is — every edge (map rim) counts as a boundary.
        let az: Vec<HexCoord> = (0..40).map(|r| HexCoord::new(0, r)).collect();
        let dz = big_defender_zone();
        let params = CombatParams::default();
        let fs = derive_flag_state(&g, &az, &dz, &[], &params, &mut XorShift64::new(7)).unwrap();
        assert_eq!(fs.kind, FlagKind::Field);
        assert_eq!(fs.flags.len(), 3);
        // Anchors sit in the interior (clear of the rim) and stay well
        // separated.
        let rim_dist = |h: HexCoord| h.q.min(h.r).min(39 - h.q).min(39 - h.r);
        for f in &fs.flags {
            assert!(
                rim_dist(f.anchor) >= 3,
                "anchor {:?} not in the interior core",
                f.anchor
            );
            assert!(!f.zone.is_empty());
            assert!(f.zone.len() <= 19);
        }
        assert!(fs.flags[0].anchor.distance(fs.flags[1].anchor) >= 5);
        assert!(fs.flags[0].anchor.distance(fs.flags[2].anchor) >= 5);
    }

    #[test]
    fn field_fallback_anchors_stay_clear_of_edges() {
        // Deepest-by-attacker-distance parked the first anchor on
        // the far border (with the attacker column at q=0 the deepest cells
        // were the q=39 rim) and the greedy spread pushed the rest into the
        // far corners, clipping the radius-2 clusters into half-discs.
        // Deepest-by-margin must keep every anchor clear of the zone edge.
        let g = HexGrid::new(40, 40, Terrain::Plains);
        let az: Vec<HexCoord> = (0..40).map(|r| HexCoord::new(0, r)).collect();
        let dz = big_defender_zone();
        let params = CombatParams::default();
        let fs = derive_flag_state(&g, &az, &dz, &[], &params, &mut XorShift64::new(7)).unwrap();
        assert_eq!(fs.flags.len(), 3);
        // The zone is the whole grid — distance to the nearest non-zone
        // cell is distance to the grid rim.
        let rim_dist = |h: HexCoord| h.q.min(h.r).min(39 - h.q).min(39 - h.r);
        for f in &fs.flags {
            assert!(
                rim_dist(f.anchor) >= 3,
                "anchor {:?} hugs the map edge",
                f.anchor
            );
            // Full cluster, not an edge-clipped half-disc.
            assert!(f.zone.len() >= 15, "anchor {:?} cluster clipped", f.anchor);
        }
    }

    #[test]
    fn script_anchors_take_priority_over_fallback() {
        let g = HexGrid::new(40, 40, Terrain::Plains);
        let az: Vec<HexCoord> = (0..40).map(|r| HexCoord::new(0, r)).collect();
        let dz = big_defender_zone();
        let params = CombatParams::default();
        let anchors = vec![
            HexCoord::new(35, 5),
            HexCoord::new(35, 20),
            HexCoord::new(35, 35),
        ];
        let fs =
            derive_flag_state(&g, &az, &dz, &anchors, &params, &mut XorShift64::new(7)).unwrap();
        assert_eq!(fs.kind, FlagKind::Field);
        assert_eq!(fs.flags.len(), 3);
        assert!(fs.flags.iter().all(|f| f.anchor
            == anchors
                .iter()
                .find(|a| **a == f.anchor)
                .copied()
                .unwrap_or(HexCoord::ZERO)));
        // Anchors outside the defender zone / water are dropped; when none
        // survive the fallback kicks in.
        let fs2 = derive_flag_state(
            &g,
            &az,
            &dz,
            &[HexCoord::new(-5, -5)],
            &params,
            &mut XorShift64::new(7),
        )
        .unwrap();
        assert_eq!(fs2.flags.len(), 3);
        let rim_dist = |h: HexCoord| h.q.min(h.r).min(39 - h.q).min(39 - h.r);
        assert!(fs2.flags.iter().all(|f| rim_dist(f.anchor) >= 3));
    }

    #[test]
    fn tiny_defender_zone_has_no_flags() {
        // A handful of hexes cannot host three well-separated anchors —
        // the battle keeps the annihilation path only.
        let g = HexGrid::new(20, 20, Terrain::Plains);
        let dz = vec![
            HexCoord::new(1, 1),
            HexCoord::new(2, 2),
            HexCoord::new(3, 3),
        ];
        let az = vec![HexCoord::new(0, 0)];
        let params = CombatParams::default();
        assert!(derive_flag_state(&g, &az, &dz, &[], &params, &mut XorShift64::new(1)).is_none());
    }

    #[test]
    fn progress_ticks_by_control_ratio() {
        let g = grid_with_city();
        let params = CombatParams::default();
        let mut flag = FlagZone::new(HexCoord::new(32, 7), big_defender_zone());
        // Attacker outnumbers the defender > 2:1 in the zone → +1.
        let mut units = vec![
            unit(0, Side::Attacker, HexCoord::new(30, 5)),
            unit(1, Side::Attacker, HexCoord::new(31, 5)),
            unit(2, Side::Attacker, HexCoord::new(32, 5)),
            unit(3, Side::Defender, HexCoord::new(33, 5)),
        ];
        update_flag_progress(&mut flag, &g, &units, &params);
        assert_eq!(flag.progress, 1);
        // Defender feeds units in: ratio 3:4 → contested, unchanged.
        units.push(unit(4, Side::Defender, HexCoord::new(34, 5)));
        units.push(unit(5, Side::Defender, HexCoord::new(30, 6)));
        units.push(unit(6, Side::Defender, HexCoord::new(31, 6)));
        update_flag_progress(&mut flag, &g, &units, &params);
        assert_eq!(flag.progress, 1);
        // Attacker leaves: ratio < 1:2 → decays back down.
        units.truncate(7);
        let defs: Vec<BattalionUnit> = (0..4)
            .map(|i| {
                unit(
                    (i + 4) as usize,
                    Side::Defender,
                    HexCoord::new(30 + i as i32, 6),
                )
            })
            .collect();
        units.retain(|u| u.side == Side::Attacker && u.id == 0); // 1 vs 4 → 0.25
        units.extend(defs.into_iter());
        update_flag_progress(&mut flag, &g, &units, &params);
        assert_eq!(flag.progress, 0);
        // HQs and broken units do not count.
        let mut hq = unit(9, Side::Defender, HexCoord::new(30, 5));
        hq.attrs |= crate::unit::Attrs::HQ;
        units.push(hq);
        let mut broken = unit(10, Side::Attacker, HexCoord::new(31, 5));
        broken.org = 0.0;
        units.push(broken);
        update_flag_progress(&mut flag, &g, &units, &params);
        assert_eq!(flag.progress, 0);
    }

    #[test]
    fn city_flag_captured_at_full_progress() {
        let _g = grid_with_city();
        let params = CombatParams::default();
        let mut fs = FlagState {
            kind: FlagKind::City,
            flags: vec![FlagZone::new(HexCoord::new(32, 7), big_defender_zone())],
            collapsed: false,
        };
        assert!(!fs.captured(&params));
        fs.flags[0].progress = params.flag_progress_cap - 1;
        assert!(!fs.captured(&params));
        fs.flags[0].progress = params.flag_progress_cap;
        assert!(fs.captured(&params));
        // Field model: ONE flag below full blocks the trigger.
        fs.kind = FlagKind::Field;
        fs.flags.push(FlagZone::new(
            HexCoord::new(1, 1),
            vec![HexCoord::new(1, 1)],
        ));
        fs.flags[1].progress = params.flag_progress_cap;
        assert!(fs.captured(&params));
        fs.flags[1].progress = params.flag_progress_cap - 1;
        assert!(!fs.captured(&params));
    }

    #[test]
    fn tick_collapses_defender_once() {
        let g = grid_with_city();
        let params = CombatParams::default();
        let mut units = vec![
            unit(0, Side::Attacker, HexCoord::new(30, 5)),
            unit(1, Side::Attacker, HexCoord::new(31, 5)),
            unit(2, Side::Attacker, HexCoord::new(32, 5)),
            unit(3, Side::Attacker, HexCoord::new(33, 5)),
            unit(4, Side::Attacker, HexCoord::new(34, 5)),
            unit(5, Side::Defender, HexCoord::new(30, 6)),
            unit(6, Side::Defender, HexCoord::new(31, 6)),
        ];
        let mut fs = derive_flag_state(
            &g,
            &big_defender_zone(),
            &big_defender_zone(),
            &[],
            &params,
            &mut XorShift64::new(1),
        )
        .unwrap();
        // Push the flag to full by hand, then let the tick fire.
        fs.flags[0].progress = params.flag_progress_cap - 1;
        let t1 = fs.tick(&g, &mut units, &params);
        assert!(t1.captured && t1.collapse_fired);
        assert!(fs.collapsed);
        // Org ZEROED ONLY — strength untouched, no state
        // change, no rout flow (the caller declares victory immediately).
        let defs: Vec<&BattalionUnit> = units.iter().filter(|u| u.side == Side::Defender).collect();
        assert!(defs.iter().all(|u| u.org == 0.0));
        assert!(
            defs.iter().all(|u| u.strength == u.max_strength),
            "strength must survive the surrender"
        );
        assert!(
            defs.iter().all(|u| u.state == UnitState::Active),
            "no retreat state"
        );
        assert!(units
            .iter()
            .filter(|u| u.side == Side::Attacker)
            .all(|u| u.state == UnitState::Active));
        // Second tick: already collapsed — nothing new fires.
        let t2 = fs.tick(&g, &mut units, &params);
        assert!(t2.captured && !t2.collapse_fired);
    }
}
