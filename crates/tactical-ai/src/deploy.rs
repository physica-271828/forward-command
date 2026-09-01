//! AI deployment planner (§11.1.5): runs when the
//! player finishes deploying, arranging the opposing force inside its own
//! zone while out of the player's sight. Zones already hold only deployable
//! hexes. Moved out of the renderer into tactical-ai for
//! unit-testability.
//!
//! Pass 1 — direct-fire battalions (attack_range ≤ 1) hold the front. Each
//! picks its hex by TERRAIN SCORE within its DIVISION SECTOR (the old
//! greedy whole-zone best-hex + cohesion pull formed
//! random-looking blobs: divisions clumped around their first member's hex,
//! the first member's (q,r) tie-break parked whole formations at one end of
//! the front, and a packed attacker stacked at the single nearest boundary
//! point — observed on a whole-province map: three infantry divisions
//! stacked into one 3-wide column, the armored vanguard anchored 30 hexes
//! away at the L-corner tip):
//!
//! ```text
//! defender: score = −|distance − target_band| × FRONT_W   (target_band =
//!               DEF_FRONT_BAND + the unit's echelon offset)
//!           + terrain + city bonus + river shield − crowding × loose
//! attacker: same, target_band = ATK_FRONT_BAND (+ echelon offset)
//! ```
//!
//! The distance term is LINEAR, not squared:
//! the old −d² pinned every battalion to the single hex closest to the
//! attacker — on a whole-province defender zone (4661 hexes) that shoved
//! the whole line into a 2–3 hex cluster instead of spreading along the
//! threatened border. Linear distance + the adjacency penalty makes good
//! ground (urban 35/hex vs ~8 per hex of frontage) and line-spreading
//! competitive.
//!
//! SIDES BEHAVE DIFFERENTLY: the attacker closes
//! for the assault (front at any cost); the DEFENDER lays a proper defence —
//! a band ~3 hexes off the attacker (not hugging the razor edge), with a
//! strong city attraction so garrison forces occupy the VP city instead of
//! leaving it empty (Warsaw: the 32-hex city sat unoccupied because the
//! distance term dragged everyone to the border).
//!
//! TACTIC SHAPES THE DEPLOYMENT:
//! the enemy's tactic card enters the scoring AND the formation profile —
//! UrbanDefense moves the WHOLE line into the city (garrison, not a third),
//! Ambush doubles the cover weight and loosens the line (lurk in forest /
//! hills / ruins), RiverDefense doubles the river-shield bonus and adds a
//! bank-hugging term, Blitz concentrates on the centre of the front with a
//! deep second wave, Encirclement splits the force into two wing sectors,
//! ElasticDefense/Counterattack hold a band plus an armor reserve echelon,
//! TacticalWithdrawal deploys in three fallback echelons, Delay in two. The
//! other cards deploy by the side rule. The profiles mirror the §7.2 card
//! table so the deployment's initial positions and the planner's doctrine
//! echo each other (a reserve echelon is where the counter-punch starts
//! from, the TW echelons are the fallback lines the retreat uses).
//!
//! DIVISION SECTORS: pass 1 partitions the front band
//! into contiguous arc sectors — one per division with line troops, width
//! proportional to the division's line-unit count — and each division places
//! its battalions INSIDE its own sector (plus a 2-arc slack for terrain).
//! The old cohesion pull is gone: the sector IS the cohesion (a division
//! cannot scatter across the map because its pool is the sector; it cannot
//! blob either, because its front echelon spreads along the sector's arc
//! with the adjacency penalty and its rear echelons sit in distinct bands
//! behind it). Divisions self-sector in roster order; `sector_divisions`
//! lets a caller deploying division-by-division (allied contingents, sector
//! deployment) pass the shared division order so each call still lands in
//! ITS slice of the band.
//!
//! Echelons: a unit's target band = the front band plus its echelon's
//! offset — the front rank holds the band, the reserve echelons form
//! ordered fallback lines behind it (defender depth) or follow-up waves
//! (attacker). Pass 2 anchors ranged support to its own division's sector
//! line instead of the global front centroid.
//!
//! SUPPORT BAND: pass 2 is measured from the FRONT,
//! not the line centroid — each support battalion aims at
//! `line_front + standoff` where `line_front` is the division's placed-line
//! distance to the enemy zone and the standoff is per class (AT 2 = the
//! infantry's second line, AA 3 = with the guns/command, artillery
//! clamp(range/2, 1, 4)). AA (attack range 1) is NO LONGER a pass-1 line
//! unit — a flak battery never holds the razor edge; the umbrella radius 3
//! still covers the line from 3 hexes back. Support units stay inside their
//! own division's sector (support-only divisions own a slice too) and spread
//! along the band by the crowd penalty — no more guns clustered at the
//! global front centroid or chained into deep columns.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use tactical_core::{Attrs, BattalionUnit, HexCoord, HexGrid, Side, Terrain};

use crate::tactic::CombatTactic;

/// Pass-1 weights: terrain quality counts about as much as one hex
/// of frontage (forest ≈ 17.5, hills ≈ 20, urban ≈ 35 vs ~2d+1 per hex at
/// the front), so good ground one hex back beats a bare front hex without
/// flattening the line. The river shield dominates local frontage but
/// cannot drag the line into the deep rear.
/// The deploy score now reads COVER only (the
/// terrain defense column is retired), and the v3.3 cover values run lower
/// than the old combined column — the DEFENDER weight doubles to ~120 so
/// the established behavior survives the rescale (forest ≈ 18 / hills 24 /
/// village 36 / mountain 48 / urban 60; river −60 now actively repels,
/// which the fords deserve). The ATTACKER keeps the old weight (50): he is
/// about to step off the line, and the wider v3.3 spread (urban 0.5 vs
/// forest 0.15) would otherwise let a deep city drag the assault rearward.
const TERRAIN_W: f32 = 120.0;
const TERRAIN_W_ATK: f32 = 50.0;
const RIVER_SHIELD_W: f32 = 80.0;
/// Linear distance weight per hex of frontage (was the squared
/// distance, which collapsed the line into a single closest point).
const FRONT_W: f32 = 8.0;
/// Defender's preferred standoff from the attacker, in hexes:
/// a defence band ~3 hexes out, so the line is NOT razor-edged — city and
/// terrain scores then decide within the band.
const DEF_FRONT_BAND: f32 = 3.0;
/// Attacker's band centre: the attacker hugs the front — its
/// zone's own edge sits ~3 hexes off the enemy zone (MIN_ZONE_DISTANCE), so
/// the razor band is 3.5 ± 1.5.
const ATK_FRONT_BAND: f32 = 3.5;
/// Extra score per city hex for the DEFENDER: garrison forces
/// must occupy the VP city, not just the nearest border hex. 60 ≈ 7.5 hex
/// of frontage, big enough to pull the line onto a 32-hex city but not to
/// stack every battalion inside it (crowding still applies).
const DEF_URBAN_BONUS: f32 = 60.0;
/// Penalty for a hex that adjoins an already-taken hex (spread the line
/// along the border instead of stacking; 0 for d ≥ 2).
const CROWD_W: f32 = 12.0;
/// §6.13: pass-3 HQ deployment stays within this distance of the
/// division anchor (deliberately tighter than the 3-hex aura — deployment
/// parks the HQ close; ai_deploy takes no params, hardcoded per §12.2).
const HQ_DEPLOY_LEASH: i32 = 2;
/// Half-width of the echelon band term: a unit's pool is the front band
/// ± this many hexes of distance to the enemy zone.
const BAND_HALF: f32 = 1.5;
/// Beyond the band the distance penalty steepens to [`DEEP_W`] per hex — a
/// river shield (+80) can still draw the line ~6 hexes deeper, but bare
/// terrain (+35 max) cannot beat the band (the flat band term
/// alone let an attacker's deep urban hex beat the razor edge, `attacker
/// _hugs` test).
const DEEP_W: f32 = 12.0;
/// Pool extension beyond the band (the distance where the score's deep
/// penalty still allows the shield/city pulls to compete).
const DEEP_PULL: f32 = 10.0;
/// A band whose arc span is below this is degenerate (its PCA axis may run
/// perpendicular to the boundary) — the sector partition then tiles the
/// whole zone instead.
const MIN_BAND_SPAN: f32 = 8.0;
/// Arc slack across sector borders: a division may use hexes
/// whose arc projection falls up to this far outside its own interval —
/// terrain one hex over a boundary still competes, but a division cannot
/// wander into a foreign sector.
const SECTOR_SLACK: f32 = 2.0;
/// Max arc gap between adjacent band hexes that still counts as ONE
/// contiguous run: adjacent hexes project ~1-1.5 arc units
/// apart, a two-strip attack zone leaves tens of units between its strips —
/// the runs feed the coverage-aware sector partition.
const BAND_RUN_GAP: f32 = 4.0;
/// Support-band standoffs: how far BEHIND the line
/// each support class deploys, measured from the line's own front_of
/// (distance to the enemy zone) — a gun can never land on the razor edge.
/// Towed AT (PAK) sat in the infantry's second line covering approach lanes;
/// divisional flak protected the command/artillery positions (and the
/// umbrella radius 3 still covers the line from 3 back).
const AT_SUPPORT_BAND: f32 = 2.0;
const AA_SUPPORT_BAND: f32 = 3.0;
/// Support-gun cluster spacing: a
/// division's guns deploy around their own line's arc CENTRE (the likeliest
/// contact axis), the k-th gun this many arc-hexes off it, alternating
/// sides — massed near the fight without stacking one hex. The previous
/// even arc-spread parked guns a screen's width off the actual engagement
/// on wide sparse fronts (observed: the lone howitzer sat at the
/// sector's 5/6 slice, 19 hexes from the panzer lane, and never fired).
const SUPPORT_CLUSTER_STEP: f32 = 2.5;
/// Reverse-slope score bonus: a hex whose own
/// step toward the enemy stands HIGHER is defiladed from indirect fire
/// (§6.6 crest, defilade_mult 0.5) — the line and the guns prefer it the
/// way they prefer a river shield, but smaller: protection from fire, not a
/// barrier. 45 ≈ 5.6 hexes of frontage — enough to pull the line behind a
/// ridge, never enough to leave the band outright.
const REVERSE_SLOPE_W: f32 = 45.0;
/// Crest observation posts: one post per this
/// many defender line units (thin by design — observers sit EXPOSED on the
/// crest ×1.5 while the line holds the reverse slope).
const OBSERVER_QUOTA_DIV: usize = 8;

/// Pass-1 LINE class: direct-fire battalions that HOLD GROUND —
/// infantry, cavalry, armor. AA (attack range 1) is excluded: a flak battery
/// is a support asset, not a line weapon — it must sit behind the line with
/// the guns (or garrison the city) so infantry assault cannot overrun it at
/// battle start.
fn is_line_unit(u: &BattalionUnit) -> bool {
    u.is_combat_effective() && u.attack_range <= 1 && !u.is_hq() && !u.attrs.has(Attrs::AA)
}

/// How the front band is divided into division sectors.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    /// The whole band, tiled by the divisions in roster order.
    Full,
    /// Only the middle `p` of the band is used (blitz: concentration on
    /// the axis, the flanks stay empty).
    CenterFocus(f32),
    /// Two wing intervals of `w` each — the first half of the divisions
    /// takes the left wing, the second half the right (encirclement).
    TwoWings(f32),
}

/// Tactic → formation profile for pass 1.
#[derive(Debug, Clone, Copy)]
struct DeployProfile {
    /// Band centre: distance to the enemy zone the FRONT rank targets.
    band: f32,
    /// (echelon offset behind the band, share of the division's line units)
    /// — the first echelon is the front rank, later ones the reserve /
    /// fallback / follow-up lines.
    echelons: &'static [(i32, f32)],
    shape: Shape,
    /// Crowd-penalty multiplier — loose cards (ambush / guerrilla) scatter.
    loose: f32,
    /// Armor units go to the deepest echelon (the counter-punch reserve).
    armor_reserve: bool,
}

impl DeployProfile {
    fn default_attacker() -> Self {
        DeployProfile {
            band: ATK_FRONT_BAND,
            echelons: &[(0, 0.75), (2, 0.25)],
            shape: Shape::Full,
            loose: 1.0,
            armor_reserve: false,
        }
    }

    fn default_defender() -> Self {
        DeployProfile {
            band: DEF_FRONT_BAND,
            echelons: &[(0, 0.7), (2, 0.3)],
            shape: Shape::Full,
            loose: 1.0,
            armor_reserve: false,
        }
    }
}

/// The tactic card's deployment profile. The §7.2
/// card table maps to formation shapes so the opening positions mirror the
/// doctrine the planner then executes.
fn profile_for(defending: bool, tactic: CombatTactic) -> DeployProfile {
    if !defending {
        return match tactic {
            // Deep penetration: concentrate on the axis, keep a deep
            // follow-up wave (the breach-widening infantry).
            CombatTactic::Blitz => DeployProfile {
                band: ATK_FRONT_BAND,
                echelons: &[(0, 0.6), (2, 0.4)],
                shape: Shape::CenterFocus(0.7),
                loose: 1.0,
                armor_reserve: false,
            },
            // Full frontal assault: everyone on the razor edge, no depth.
            CombatTactic::MassCharge => DeployProfile {
                band: ATK_FRONT_BAND,
                echelons: &[(0, 1.0)],
                shape: Shape::Full,
                loose: 1.0,
                armor_reserve: false,
            },
            // Pincer: two wing sectors, the centre stays empty.
            CombatTactic::Encirclement => DeployProfile {
                band: ATK_FRONT_BAND,
                echelons: &[(0, 0.8), (2, 0.2)],
                shape: Shape::TwoWings(0.3),
                loose: 1.0,
                armor_reserve: false,
            },
            // Exploit gaps: thin, loose, a little deeper.
            CombatTactic::InfiltrationAssault => DeployProfile {
                band: ATK_FRONT_BAND + 1.0,
                echelons: &[(0, 1.0)],
                shape: Shape::Full,
                loose: 0.5,
                armor_reserve: false,
            },
            _ => DeployProfile::default_attacker(),
        };
    }
    match tactic {
        // The whole line garrisons the city / holds the bank — the special
        // paths in ai_deploy handle these; the profile is a pass-1 fallback.
        CombatTactic::UrbanDefense | CombatTactic::RiverDefense => {
            DeployProfile::default_defender()
        }
        // Lurk: deeper band, loose spacing (no contiguous line).
        CombatTactic::Ambush | CombatTactic::GuerrillaTactics => DeployProfile {
            band: DEF_FRONT_BAND + 1.0,
            echelons: &[(0, 1.0)],
            shape: Shape::Full,
            loose: 0.5,
            armor_reserve: false,
        },
        // Delay & preserve: a holding band + a counter-punch reserve of the
        // mobile troops.
        CombatTactic::ElasticDefense => DeployProfile {
            band: DEF_FRONT_BAND,
            echelons: &[(0, 0.65), (2, 0.35)],
            shape: Shape::Full,
            loose: 1.0,
            armor_reserve: true,
        },
        // Hold + strike: the reserve echelon IS the counter-punch force.
        CombatTactic::Counterattack => DeployProfile {
            band: DEF_FRONT_BAND,
            echelons: &[(0, 0.6), (2, 0.4)],
            shape: Shape::Full,
            loose: 1.0,
            armor_reserve: true,
        },
        // Systematic retreat: a thin screening echelon then two fallback
        // lines the retreat steps through.
        CombatTactic::TacticalWithdrawal => DeployProfile {
            band: DEF_FRONT_BAND,
            echelons: &[(0, 0.35), (3, 0.35), (6, 0.3)],
            shape: Shape::Full,
            loose: 1.0,
            armor_reserve: false,
        },
        // Moving screen: two thin echelons, the rear one ready to relieve.
        CombatTactic::Delay => DeployProfile {
            band: DEF_FRONT_BAND,
            echelons: &[(0, 0.5), (2, 0.5)],
            shape: Shape::Full,
            loose: 1.0,
            armor_reserve: false,
        },
        // Attrition warfare: a dense line, little reserve (all guns on the
        // line's side anyway).
        CombatTactic::OverwhelmingFire => DeployProfile {
            band: DEF_FRONT_BAND,
            echelons: &[(0, 0.85), (2, 0.15)],
            shape: Shape::Full,
            loose: 1.0,
            armor_reserve: false,
        },
        // Pincer on defence: hold the two shoulders, the centre screens.
        CombatTactic::Encirclement => DeployProfile {
            band: DEF_FRONT_BAND,
            echelons: &[(0, 0.75), (2, 0.25)],
            shape: Shape::TwoWings(0.35),
            loose: 1.0,
            armor_reserve: false,
        },
        _ => DeployProfile::default_defender(),
    }
}

/// Arrange `enemy`'s combat-effective units inside `e_zone` (the whole-side
/// call: sector order derived from the roster). See [`ai_deploy_impl`].
#[allow(clippy::too_many_arguments)]
pub fn ai_deploy(
    grid: &HexGrid,
    units: &mut [BattalionUnit],
    e_zone: &[HexCoord],
    p_zone: &[HexCoord],
    enemy: Side,
    tactic: CombatTactic,
    pre_used: &HashSet<(i32, i32)>,
    only_division: Option<&str>,
) {
    ai_deploy_impl(
        grid,
        units,
        e_zone,
        p_zone,
        enemy,
        tactic,
        pre_used,
        only_division,
        None,
        false, // the AI/whole-side flow — `only_undeployed` is the player knob
    )
}

/// Arrange `enemy`'s combat-effective units inside `e_zone`. `p_zone` is the
/// player's zone — "front" means closer to its centroid. `tactic` is the
/// enemy's tactic card (it shapes the deployment).
/// `pre_used` marks hexes already occupied by the OTHER side's hand-placed
/// units (the player Auto Deploy flow): the planner never re-takes them,
/// so a human layout survives the AI filling the remainder.
/// `only_division` (sector deployment): when Some, only that
/// division's battalions are arranged — the OOB "deploy to sector" flow
/// hands one division at a time to a player-drawn rectangle.
/// `sector_divisions`: the ordered list of divisions
/// that will share the zone, used for the sector partition when deploying
/// division-by-division (allied contingents / headless splits). None =
/// derive the order from the roster (the whole-side call); with
/// `only_division` set and no list, the division takes the whole band.
/// `only_undeployed` (the player's Auto Deploy):
/// arrange ONLY the units still waiting in the OOB (`undeployed`); a
/// hand-placed unit stays exactly where the player put it (the button's
/// tooltip has always promised this — the filter was missing). AI /
/// headless / tooling callers pass false (arrange the whole side).
#[allow(clippy::too_many_arguments)]
pub fn ai_deploy_impl(
    grid: &HexGrid,
    units: &mut [BattalionUnit],
    e_zone: &[HexCoord],
    p_zone: &[HexCoord],
    enemy: Side,
    tactic: CombatTactic,
    pre_used: &HashSet<(i32, i32)>,
    only_division: Option<&str>,
    sector_divisions: Option<&[String]>,
    only_undeployed: bool,
) {
    if e_zone.is_empty() || p_zone.is_empty() {
        return;
    }
    let defending = enemy == Side::Defender;
    let in_division =
        |u: &BattalionUnit| -> bool { only_division.map(|d| u.division == d).unwrap_or(true) };
    // Auto Deploy fills only the OOB waiters — a hand-placed unit
    // is sacred (its hex is also covered by `pre_used`). Pass-3's member
    // anchor below deliberately does NOT filter: a placed battalion is a
    // valid leash anchor wherever it came from.
    let u_ok = |u: &BattalionUnit| !only_undeployed || u.undeployed;
    let profile = profile_for(defending, tactic);

    // "Front" = the distance to the NEAREST enemy-zone hex (the inter-zone
    // boundary). The old enemy-centroid anchor collapses on large stitched
    // maps: Warsaw's origin strip centroid sits 10+ hexes off the border
    // and the whole line formed at one point instead of along the boundary.
    // Precomputed once; O(e_zone × p_zone).
    let front_of: HashMap<(i32, i32), f32> = e_zone
        .iter()
        .map(|h| {
            let d = p_zone.iter().map(|p| h.distance(*p)).min().unwrap_or(0) as f32;
            ((h.q, h.r), d)
        })
        .collect();
    // River-shield direction: the enemy zone's centroid (coarse by design —
    // the sampled line just needs to point at the enemy's side).
    let (sq, sr) = p_zone.iter().fold((0i64, 0i64), |(aq, ar), h| {
        (aq + h.q as i64, ar + h.r as i64)
    });
    let n = p_zone.len() as f32;
    let (cq, cr) = (sq as f32 / n, sr as f32 / n);

    let mut used: HashSet<(i32, i32)> = pre_used.clone();

    // Defender city garrison quota.
    // The flat DEF_URBAN_BONUS only wins within ~7 hex of the front band; on
    // big provinces (Warsaw: 32-hex city ~15+ hex deep) the band's
    // distance term buries it and the VP city stays empty (the garrison
    // came out 0/32). Split the garrison instead: a quota of the WEAKEST
    // direct-fire battalions (militia / remnant types) is placed inside the
    // city nearest-first around its centroid; the rest take the front band.
    // A whole weak division must not
    // be gutted — each division contributes at most a third of its own line
    // units to the garrison, so the rest of the division still holds its
    // sector (an un-capped weak division's battalion once landed 57 hexes
    // from the front).
    let mut city_assigned: HashSet<usize> = HashSet::new();
    let city_hexes: Vec<HexCoord> = if defending {
        e_zone
            .iter()
            .filter(|h| {
                grid.cell(**h)
                    .map(|c| c.terrain == Terrain::Urban)
                    .unwrap_or(false)
            })
            .copied()
            .collect()
    } else {
        Vec::new()
    };
    if city_hexes.len() >= 3 {
        let mut line_per_div: HashMap<String, usize> = HashMap::new();
        let mut pool: Vec<usize> = units
            .iter()
            .enumerate()
            .filter(|(_, u)| {
                u.side == enemy
                    && u.is_combat_effective()
                    && u.attack_range <= 1
                    && !u.is_hq() // §6.13: HQs never garrison
                    && in_division(u)
                    && u_ok(u)
            })
            .map(|(i, u)| {
                *line_per_div.entry(u.division.clone()).or_default() += 1;
                i
            })
            .collect();
        // Weakest first: the garrison troops defend the city; the strongest
        // take the border line. Per-division cap: ≤ ⅓ of its own line units.
        pool.sort_by(|&a, &b| {
            units[a]
                .org_ratio()
                .partial_cmp(&units[b].org_ratio())
                .unwrap_or(Ordering::Equal)
                .then(a.cmp(&b))
        });
        let mut div_city: HashMap<String, usize> = HashMap::new();
        let mut accepted: Vec<usize> = Vec::new();
        // Urban defense: the WHOLE line garrisons the city — the
        // per-division cap below is waived (everyone into the city).
        let urban_full = defending && tactic == CombatTactic::UrbanDefense;
        for &i in &pool {
            let d = units[i].division.clone();
            let cap = if urban_full {
                usize::MAX
            } else {
                (line_per_div.get(&d).copied().unwrap_or(1) / 3).max(1)
            };
            if div_city.get(&d).copied().unwrap_or(0) < cap {
                *div_city.entry(d.clone()).or_default() += 1;
                accepted.push(i);
            }
        }
        // Urban defense: the WHOLE line garrisons the city (the
        // quota below is the default defender split — a third, at least 3,
        // never more than half the line troops).
        if pool.len() >= 6 || urban_full {
            let quota = if urban_full {
                pool.len().min(city_hexes.len())
            } else {
                (city_hexes.len() / 3).clamp(3, pool.len() / 2)
            };
            let (sq, sr) = city_hexes.iter().fold((0i64, 0i64), |(aq, ar), h| {
                (aq + h.q as i64, ar + h.r as i64)
            });
            let n = city_hexes.len() as f32;
            let (cq, cr) = (sq as f32 / n, sr as f32 / n);
            for &i in accepted.iter().take(quota) {
                let best = city_hexes
                    .iter()
                    .filter(|h| !used.contains(&(h.q, h.r)))
                    .min_by(|a, b| {
                        let score = |h: &&HexCoord| {
                            let (dq, dr) = (h.q as f32 - cq, h.r as f32 - cr);
                            (dq * dq + dr * dr).sqrt() + crowd_penalty(h, &used)
                        };
                        score(a)
                            .partial_cmp(&score(b))
                            .unwrap_or(Ordering::Equal)
                            .then(a.q.cmp(&b.q))
                            .then(a.r.cmp(&b.r))
                    });
                if let Some(&h) = best {
                    used.insert((h.q, h.r));
                    units[i].position = h;
                    city_assigned.insert(units[i].id);
                }
            }
        }
    }

    // ── Division sectors ───────────────────────────────────────────────────
    // The band hexes anchor the axis: their centroid + PCA principal
    // direction; `arc(h)` = the projection of a zone hex onto the axis. The
    // divisions then tile the covered arc interval(s), each owning a
    // contiguous slice proportional to its line-unit count.
    //
    // The band is anchored to the zone's ACTUAL front edge (`edge` = the
    // smallest front_of in the zone) plus the profile's standoff — real
    // maps keep the zones ≥3 apart (MIN_ZONE_DISTANCE) so the edge sits at
    // 3 and the bands match the classic numbers (defender 3, attacker 3.5,
    // lurk 4); synthetic test zones often sit 5+ apart and the relative
    // anchor keeps the echelon pools non-empty there.
    let edge = e_zone
        .iter()
        .map(|h| front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0))
        .fold(f32::INFINITY, f32::min);
    let band_base = edge + (profile.band - DEF_FRONT_BAND).max(0.0);
    let band_hexes: Vec<HexCoord> = e_zone
        .iter()
        .copied()
        .filter(|h| {
            let d = front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0);
            (d - band_base).abs() <= BAND_HALF
        })
        .collect();
    let (axis, arc_min, arc_max) = if band_hexes.is_empty() {
        // Degenerate band (tiny/crowded zones): fall back to a flat
        // partition over the whole zone's projection — sectors still bound
        // divisions, just less usefully.
        let (sq, sr) = e_zone.iter().fold((0i64, 0i64), |(aq, ar), h| {
            (aq + h.q as i64, ar + h.r as i64)
        });
        let n = e_zone.len() as f32;
        let (bq, br) = (sq as f32 / n, sr as f32 / n);
        let axis = (1.0f32, 0.0f32);
        let (a0, a1) = e_zone
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), h| {
                let a = (h.q as f32 - bq) * axis.0 + (h.r as f32 - br) * axis.1;
                (lo.min(a), hi.max(a))
            });
        (axis, a0, a1)
    } else {
        let (sq, sr) = band_hexes.iter().fold((0i64, 0i64), |(aq, ar), h| {
            (aq + h.q as i64, ar + h.r as i64)
        });
        let n = band_hexes.len() as f32;
        let (bq, br) = (sq as f32 / n, sr as f32 / n);
        let (mut sxx, mut syy, mut sxy) = (0f32, 0f32, 0f32);
        for h in &band_hexes {
            let (dx, dy) = (h.q as f32 - bq, h.r as f32 - br);
            sxx += dx * dx;
            syy += dy * dy;
            sxy += dx * dy;
        }
        // 2×2 covariance principal eigenvector (the direction of maximum
        // spread along the band). Axis-aligned bands (sxy ≈ 0) take the
        // higher-variance axis directly — the (l1 − syy)/sxy form divides
        // by zero there.
        let trace = sxx + syy;
        let det = sxx * syy - sxy * sxy;
        let disc = ((trace * trace / 4.0 - det).max(0.0)).sqrt();
        let l1 = trace / 2.0 + disc;
        let (ex, ey) = if sxy.abs() < 1e-9 {
            if syy >= sxx {
                (0.0, 1.0)
            } else {
                (1.0, 0.0)
            }
        } else {
            ((l1 - syy) / sxy, 1.0f32)
        };
        let len = (ex * ex + ey * ey).sqrt().max(1e-6);
        let axis = (ex / len, ey / len);
        // A degenerate band (arc span ≤ MIN_BAND_SPAN) has no meaningful
        // "along" direction — its PCA axis can point PERPENDICULAR to the
        // boundary (single-column bands), which would fold the sector
        // partition. Fall back to the whole zone's arc span then (the
        // sectors tile the zone; the band filter still holds the line).
        let (a0, a1) = band_hexes
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), h| {
                let a = (h.q as f32 - bq) * axis.0 + (h.r as f32 - br) * axis.1;
                (lo.min(a), hi.max(a))
            });
        if a1 - a0 < MIN_BAND_SPAN {
            let axis = (1.0f32, 0.0f32);
            let (za0, za1) =
                e_zone
                    .iter()
                    .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), h| {
                        let a = (h.q as f32 - bq) * axis.0 + (h.r as f32 - br) * axis.1;
                        (lo.min(a), hi.max(a))
                    });
            (axis, za0, za1)
        } else {
            (axis, a0, a1)
        }
    };
    // Band centroid (captured by `arc`; O(1) per call).
    let (bq, br) = if band_hexes.is_empty() {
        let (sq, sr) = e_zone.iter().fold((0i64, 0i64), |(aq, ar), h| {
            (aq + h.q as i64, ar + h.r as i64)
        });
        (
            sq as f32 / e_zone.len().max(1) as f32,
            sr as f32 / e_zone.len().max(1) as f32,
        )
    } else {
        let (sq, sr) = band_hexes.iter().fold((0i64, 0i64), |(aq, ar), h| {
            (aq + h.q as i64, ar + h.r as i64)
        });
        (
            sq as f32 / band_hexes.len() as f32,
            sr as f32 / band_hexes.len() as f32,
        )
    };
    let arc = |h: &HexCoord| -> f32 { (h.q as f32 - bq) * axis.0 + (h.r as f32 - br) * axis.1 };

    // ── Band coverage runs ───────────────────────────────────────────────
    // The sector partition must tile the arc intervals that ACTUALLY hold
    // band hexes. A two-strip attack zone (NW + SE source provinces) leaves
    // the arc between its strips empty — slicing the raw arc_min..arc_max
    // handed whole divisions empty sectors and they never deployed
    // (observed on a real two-strip battle: 5 of 7 attacker divisions
    // stayed on the OFFBOARD sentinel). Merge the band hexes' arc values
    // into runs; `cov_arc` maps a fraction of the COVERAGE (0..1) back onto
    // an arc position through the runs, so every division's proportional
    // slice lands on real frontage. The degenerate band falls back to the
    // whole zone's arcs (same source as the bounds above).
    let run_hexes: Vec<HexCoord> = if band_hexes.is_empty() {
        e_zone.to_vec()
    } else {
        let (ba0, ba1) =
            band_hexes
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), h| {
                    let a = arc(h);
                    (lo.min(a), hi.max(a))
                });
        if ba1 - ba0 < MIN_BAND_SPAN {
            e_zone.to_vec()
        } else {
            band_hexes.clone()
        }
    };
    let mut arcs_sorted: Vec<f32> = run_hexes.iter().map(&arc).collect();
    arcs_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let mut runs: Vec<(f32, f32)> = Vec::new();
    for a in arcs_sorted {
        match runs.last_mut() {
            Some((_, hi)) if a - *hi <= BAND_RUN_GAP => *hi = a,
            _ => runs.push((a, a)),
        }
    }
    if runs.is_empty() {
        runs.push((arc_min, arc_max));
    }
    let cov_len: f32 = runs.iter().map(|(lo, hi)| hi - lo).sum();
    let cov_arc = |frac: f32| -> f32 {
        if cov_len <= 0.0 {
            return arc_min;
        }
        let mut f = frac.clamp(0.0, 1.0) * cov_len;
        for &(lo, hi) in &runs {
            let len = hi - lo;
            if f <= len {
                return lo + f;
            }
            f -= len;
        }
        arc_max
    };

    // Division order for the sector partition.
    let line_divs: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut divs: Vec<String> = Vec::new();
        if let Some(list) = sector_divisions {
            for d in list {
                // Divisions with ANY deployable units (line or
                // support) own a slice — an AT/AA-only group is not left
                // to the global fallback.
                let has_units = units.iter().any(|u| {
                    u.side == enemy
                        && u.is_combat_effective()
                        && !u.is_hq()
                        && u.division == *d
                        && u_ok(u)
                });
                if has_units && seen.insert(d.clone()) {
                    divs.push(d.clone());
                }
            }
        } else if let Some(d) = only_division {
            divs.push(d.to_string());
        } else {
            for u in units.iter() {
                if u.side == enemy
                    && u.is_combat_effective()
                    && !u.is_hq()
                    && !u.division.is_empty()
                    && u_ok(u)
                    && seen.insert(u.division.clone())
                {
                    divs.push(u.division.clone());
                }
            }
        }
        divs
    };
    // Per-division counts for the sector partition: ALL
    // deployable units — line AND support — so a support-only division
    // (army-level AT/AA groups) still owns a contiguous slice of the band
    // instead of falling back to the global front centroid. The empty
    // division name = unattached battalions, counted as one implicit
    // division. `line_counts` (line units only) drives the pass-1 echelon
    // and slot fractions — the sector may be widened by support units, but
    // the LINE units' placement order within it must not shift.
    let div_counts: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for u in units.iter() {
            if u.side == enemy
                && u.is_combat_effective()
                && !u.is_hq()
                && !city_assigned.contains(&u.id)
                && u_ok(u)
            {
                *m.entry(u.division.clone()).or_default() += 1;
            }
        }
        m
    };
    let line_counts: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for u in units.iter() {
            if u.side == enemy && is_line_unit(u) && !city_assigned.contains(&u.id) && u_ok(u) {
                *m.entry(u.division.clone()).or_default() += 1;
            }
        }
        m
    };
    let support_counts: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for u in units.iter() {
            if u.side == enemy
                && u.is_combat_effective()
                && (u.attack_range > 1 || u.attrs.has(Attrs::AA))
                && !u.is_hq()
                && !city_assigned.contains(&u.id)
                && u_ok(u)
            {
                *m.entry(u.division.clone()).or_default() += 1;
            }
        }
        m
    };
    // Covered arc interval(s) per shape — in COVERAGE-fraction space
    // (the sector partition tiles `cov_arc`, so the shape clips
    // the REAL frontage, not the raw arc — a two-strip band's middle is a
    // gap, and "concentrate on the centre" degrades to both strips).
    let covered: Vec<(f32, f32)> = match profile.shape {
        Shape::Full => vec![(0.0, 1.0)],
        Shape::CenterFocus(p) => {
            if runs.len() >= 2 {
                // No single centre on a two-strip front: cover both strips.
                vec![(0.0, 1.0)]
            } else {
                let mid = 0.5;
                let half = p / 2.0;
                vec![(mid - half, mid + half)]
            }
        }
        Shape::TwoWings(w) => {
            vec![(0.0, w), (1.0 - w, 1.0)]
        }
    };
    // Assign each line division its arc interval: divisions tile the
    // covered fraction(s) in roster order, proportionally to their counts.
    // TwoWings splits the division list by total count — the first half
    // takes the left wing, the second half the right.
    let mut div_sectors: HashMap<String, (f32, f32)> = HashMap::new();
    {
        let line_divs: Vec<&String> = line_divs
            .iter()
            .filter(|d| div_counts.get(*d).copied().unwrap_or(0) > 0)
            .collect();
        if covered.len() == 2 {
            let total: usize = line_divs.iter().map(|d| div_counts.get(*d).unwrap()).sum();
            let half = (total / 2).max(1);
            let mut cum = 0usize;
            for d in line_divs {
                let cnt = div_counts.get(d).copied().unwrap_or(0);
                let (lo, hi) = if cum + cnt <= half || (cum < half && cum + cnt >= half) {
                    covered[0]
                } else {
                    covered[1]
                };
                div_sectors.insert(d.clone(), (cov_arc(lo), cov_arc(hi)));
                cum += cnt;
            }
        } else {
            // Full / CenterFocus: the divisions tile the single covered
            // fraction interval proportionally to their line-unit counts.
            let total: usize = line_divs.iter().map(|d| div_counts.get(*d).unwrap()).sum();
            let total = total.max(1);
            let (lo0, hi0) = covered[0];
            let cspan = hi0 - lo0;
            let mut cum = 0usize;
            for d in line_divs {
                let cnt = div_counts.get(d).copied().unwrap_or(0);
                let lo = lo0 + (cum as f32 / total as f32) * cspan;
                let hi = lo0 + ((cum + cnt) as f32 / total as f32) * cspan;
                div_sectors.insert(d.clone(), (cov_arc(lo), cov_arc(hi)));
                cum += cnt;
            }
        }
    }

    // Pass 1: direct-fire units take the front — best-scored available hex
    // each, within their own division's sector and echelon band. HQs are
    // excluded (§6.13): they deploy behind their division in
    // pass 3. `div_placed` tracks each division's placed members for the
    // pass-2 artillery anchor and pass-3 HQ leash.
    let mut front_line: Vec<HexCoord> = Vec::new();
    let mut div_placed: HashMap<String, Vec<HexCoord>> = HashMap::new();
    let mut unit_order: Vec<usize> = units
        .iter()
        .enumerate()
        .filter(|(_, u)| {
            u.side == enemy
                && is_line_unit(u)
                && !city_assigned.contains(&u.id)
                && in_division(u)
                && u_ok(u)
        })
        .map(|(i, _)| i)
        .collect();
    // armor_reserve: move the division's armor to the TAIL of its roster
    // slice — the deepest echelon takes the counter-punch troops.
    if profile.armor_reserve {
        let mut head: Vec<usize> = Vec::new();
        let mut tail: Vec<usize> = Vec::new();
        let mut last_div = String::new();
        for &i in &unit_order {
            let d = units[i].division.clone();
            if d != last_div && !last_div.is_empty() {
                head.append(&mut tail);
            }
            last_div = d;
            if units[i].unit_type.is_armor() {
                tail.push(i);
            } else {
                head.push(i);
            }
        }
        head.append(&mut tail);
        unit_order = head;
    }
    let mut placed_in_div: HashMap<String, usize> = HashMap::new();
    for i in unit_order {
        let division = units[i].division.clone();
        // The division's sector interval; the fallback is the shape's
        // covered interval (the whole band for TwoWings — an unnamed
        // division may take either wing).
        let (s_lo, s_hi) = div_sectors
            .get(&division)
            .copied()
            .or_else(|| {
                if matches!(profile.shape, Shape::TwoWings(_)) {
                    Some((arc_min, arc_max))
                } else {
                    // covered is fraction-space — map back onto the arc.
                    covered
                        .first()
                        .copied()
                        .map(|(lo, hi)| (cov_arc(lo), cov_arc(hi)))
                }
            })
            .unwrap_or((arc_min, arc_max));
        // Echelon: cumulative share cutoffs over the division's placement
        // order (armor_reserve reordered armor to the tail → the deepest
        // echelon takes the counter-punch troops). The LAST echelon's band
        // has no upper bound (spill).
        let div_idx = placed_in_div.get(&division).copied().unwrap_or(0);
        *placed_in_div.entry(division.clone()).or_default() += 1;
        // Echelon/slot fractions use the LINE count — the sector
        // may have been widened by the division's support units.
        let div_total = line_counts.get(&division).copied().unwrap_or(1).max(1);
        // The unit's fraction of the division's placement order (midpoint
        // of its slot, so a single-unit division lands in the FIRST
        // echelon) compared against the CUMULATIVE echelon shares.
        let frac = (div_idx as f32 + 0.5) / div_total as f32;
        let mut echelon = profile.echelons.len() - 1;
        let mut target = band_base + profile.echelons[echelon].0 as f32;
        let mut cum = 0.0f32;
        for (e, (offset, share)) in profile.echelons.iter().enumerate() {
            cum += share;
            if frac <= cum {
                echelon = e;
                target = band_base + *offset as f32;
                break;
            }
        }
        let band_lo = target - BAND_HALF;
        // The echelon pool extends DEEP_PULL beyond the band so terrain
        // pulls (river shield +80, urban +60) can still draw a unit a few
        // hexes deeper — the score's DEEP_W penalty then keeps the line
        // near the band unless the terrain legitimately outweighs it.
        // The last echelon spills unbounded; loose cards (ambush /
        // guerrilla) also lurk deep — the cover term keeps them near.
        let band_hi = if echelon == profile.echelons.len() - 1 || profile.loose < 1.0 {
            f32::INFINITY
        } else {
            target + BAND_HALF + DEEP_PULL
        };
        let cand_score = |h: &HexCoord, used: &HashSet<(i32, i32)>| {
            let base = front_score(h, grid, &front_of, cq, cr, defending, tactic, target);
            let d = front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0);
            // Beyond the band the distance penalty steepens —
            // a river shield / city bonus may still pull the line deeper,
            // bare terrain may not (loose cards lurk free).
            let deep = if profile.loose < 1.0 {
                0.0
            } else {
                (d - (target + BAND_HALF)).max(0.0) * DEEP_W
            };
            base - deep - crowd_penalty(h, used) * profile.loose
        };
        let mut cands: Vec<(HexCoord, f32)> = e_zone
            .iter()
            .filter(|h| !used.contains(&(h.q, h.r)))
            .filter(|h| {
                let a = arc(h);
                a >= s_lo - SECTOR_SLACK && a <= s_hi + SECTOR_SLACK
            })
            .filter(|h| {
                let d = front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0);
                d >= band_lo && d <= band_hi
            })
            .map(|h| (*h, cand_score(h, &used)))
            .collect();
        // The unit's arc SLOT inside its division's sector: the k-th unit
        // of n aims at the k-th equal slice of the sector —
        // the tie-break fills the sector evenly along the arc instead of
        // chaining after the previous unit (the old `.reverse()` made the
        // NEAREST-to-used hex win on ties).
        let slot = s_lo + (div_idx as f32 + 0.5) / div_total as f32 * (s_hi - s_lo);
        let best = cands.drain(..).max_by(|(a, sa), (b, sb)| {
            // The score rides ON the candidate (computed once at
            // collection with the same `used` set — nothing mutates it
            // before this comparison). Recomputing cand_score() here
            // was O(used) per comparison, O(units × zone × used) total
            // on big zones.
            sa.partial_cmp(sb)
                .unwrap_or(Ordering::Equal)
                // The k-th unit aims at its arc SLOT: the
                // tie-break prefers the hex NEAREST the slot (reversed
                // — max_by picks the greatest element, so the ascending
                // distance comparison must flip), so a division fills
                // its sector evenly along the arc instead of chaining
                // after the previous unit.
                .then(
                    (arc(a) - slot)
                        .abs()
                        .partial_cmp(&(arc(b) - slot).abs())
                        .unwrap_or(Ordering::Equal)
                        .reverse(),
                )
                .then(a.q.cmp(&b.q))
                .then(a.r.cmp(&b.r))
        });
        if let Some((h, _)) = best {
            used.insert((h.q, h.r));
            units[i].position = h;
            front_line.push(h);
            if !division.is_empty() {
                div_placed.entry(division).or_default().push(h);
            }
        }
    }

    // CREST OBSERVATION POSTS — a thin screen
    // of the WEAKEST defender line units re-placed onto the ridge line,
    // where the ridge rule lets them see the attacker's approach before it
    // crests (peaks see over saddles). The side's fog view is shared, so a
    // post genuinely extends the whole side's spotting — a thin crest-line
    // observation screen (峰线薄观察哨). Thin by design: 1 post per
    // OBSERVER_QUOTA_DIV line
    // units, weakest first (observers sit EXPOSED on the crest ×1.5 when
    // the enemy guns answer — the historical trade; the line itself holds
    // the reverse slope behind them). A post stays in its division's sector
    // and as close to the line as possible; no crest near the front → the
    // unit keeps its line spot. The old hex stays marked in `used` (the
    // line is already placed — nobody else needs it).
    if defending {
        let line_n = units
            .iter()
            .filter(|u| {
                u.side == enemy && is_line_unit(u) && !city_assigned.contains(&u.id) && u_ok(u)
            })
            .count();
        let quota = (line_n / OBSERVER_QUOTA_DIV).max(1).min(line_n);
        let mut obs: Vec<usize> = units
            .iter()
            .enumerate()
            .filter(|(_, u)| {
                u.side == enemy
                    && is_line_unit(u)
                    && !city_assigned.contains(&u.id)
                    && in_division(u)
                    && u_ok(u)
            })
            .map(|(i, _)| i)
            .collect();
        obs.sort_by(|&a, &b| {
            units[a]
                .org_ratio()
                .partial_cmp(&units[b].org_ratio())
                .unwrap_or(Ordering::Equal)
                .then(a.cmp(&b))
        });
        for &i in obs.iter().take(quota) {
            let division = units[i].division.clone();
            let (s_lo, s_hi) = div_sectors
                .get(&division)
                .copied()
                .or_else(|| {
                    covered
                        .first()
                        .copied()
                        .map(|(lo, hi)| (cov_arc(lo), cov_arc(hi)))
                })
                .unwrap_or((arc_min, arc_max));
            let cur = units[i].position;
            let best = e_zone
                .iter()
                .filter(|h| !used.contains(&(h.q, h.r)))
                .filter(|h| {
                    let a = arc(h);
                    a >= s_lo - SECTOR_SLACK && a <= s_hi + SECTOR_SLACK
                })
                .filter(|h| on_crest(h, grid, cq, cr))
                .min_by(|a, b| {
                    let score = |h: &&HexCoord| {
                        // Nearest to the unit's line spot first — the posts
                        // stand ON the front ridge, not a deep peak.
                        let d = h.distance(cur);
                        // Ties: the higher crest sees farther.
                        let e = grid.cell(**h).map(|c| c.elevation).unwrap_or(0);
                        (d, -e)
                    };
                    score(a).cmp(&score(b))
                });
            if let Some(&h) = best {
                used.insert((h.q, h.r));
                units[i].position = h;
            }
        }
    }

    // Pass 2: AT / AA / artillery deploy in a SUPPORT
    // BAND behind the line, measured from the FRONT — the placed line's own
    // distance to the enemy zone (`front_of`) — not from the line centroid,
    // which let guns land ON the razor edge (AT's old standoff 1 ring around
    // the centroid straddled the line) or chain into deep columns. Each
    // class aims at `line_front + standoff`: AT 2 = the infantry's second
    // line (the classic PAK siting), AA 3 = with the guns/command (the
    // umbrella radius 3 still covers the line), artillery
    // clamp(range/2, 1, 4). The gun stays inside its own division's SECTOR
    // (support-only divisions own a slice — the same partition) and spreads
    // along the band by the crowd penalty. AA (range 1) joins pass 2 here —
    // a flak battery is a support asset, never a line weapon.
    let line_front = |hexes: &[HexCoord]| -> f32 {
        if hexes.is_empty() {
            return band_base;
        }
        let sum: f32 = hexes
            .iter()
            .map(|h| front_of.get(&(h.q, h.r)).copied().unwrap_or(band_base))
            .sum();
        sum / hexes.len() as f32
    };
    let mut support_in_div: HashMap<String, usize> = HashMap::new();
    for u in units.iter_mut().filter(|u| {
        u.side == enemy
            && u.is_combat_effective()
            && (u.attack_range > 1 || u.attrs.has(Attrs::AA))
            && !city_assigned.contains(&u.id)
            && in_division(u)
            && u_ok(u)
    }) {
        let division = u.division.clone();
        // The support band's front anchor: the division's own placed line
        // (its mean front_of), else the whole side's line, else the city
        // garrison (urban defense — everyone is in the city), else the
        // profile band edge.
        let reference = div_placed
            .get(&division)
            .filter(|v| !v.is_empty())
            .map(|v| line_front(v))
            .or_else(|| (!front_line.is_empty()).then(|| line_front(&front_line)))
            .or_else(|| (!city_hexes.is_empty()).then(|| line_front(&city_hexes)))
            .unwrap_or(band_base);
        let standoff = if u.attrs.has(Attrs::AA) {
            AA_SUPPORT_BAND
        } else if u.attrs.has(Attrs::AT) {
            AT_SUPPORT_BAND
        } else {
            (u.attack_range / 2).clamp(1, 4) as f32
        };
        let target = reference + standoff;
        // The division's sector interval (the same fallbacks as pass 1).
        let (s_lo, s_hi) = div_sectors
            .get(&division)
            .copied()
            .or_else(|| {
                if matches!(profile.shape, Shape::TwoWings(_)) {
                    Some((arc_min, arc_max))
                } else {
                    covered
                        .first()
                        .copied()
                        .map(|(lo, hi)| (cov_arc(lo), cov_arc(hi)))
                }
            })
            .unwrap_or((arc_min, arc_max));
        // The unit's arc SLOT inside the division's sector. The DEFENDER's
        // guns CLUSTER behind their own line's
        // arc centre — the likeliest contact axis — the k-th gun offsetting
        // ±1, ±2… cluster steps around it, still bounded by the sector; a
        // division with no placed line (support-only, or the whole line
        // garrisoning a city) keeps the even spread. The defender is a
        // fire base: it emplaces where it stands, so its stand IS its
        // coverage — the even spread parked a lone howitzer at the sector's
        // 5/6 slice, 19 hexes off the panzer lane, and it
        // never fired. The ATTACKER keeps the even spread deliberately:
        // its guns creep toward the nearest enemy, and clustered guns all
        // converge on ONE flapping creep goal and never settle (observed:
        // attacker fire fell 111→0 / 106→0 when this
        // clustering briefly applied to both sides).
        let div_idx = support_in_div.get(&division).copied().unwrap_or(0);
        *support_in_div.entry(division.clone()).or_default() += 1;
        let div_total = support_counts.get(&division).copied().unwrap_or(1).max(1);
        let own_line: &[HexCoord] = div_placed
            .get(&division)
            .filter(|v| !v.is_empty())
            .map(Vec::as_slice)
            .unwrap_or(if division.is_empty() {
                front_line.as_slice()
            } else {
                &[]
            });
        let slot = if !defending || own_line.is_empty() {
            s_lo + (div_idx as f32 + 0.5) / div_total as f32 * (s_hi - s_lo)
        } else {
            let centre =
                own_line.iter().map(|h| arc(h)).sum::<f32>() / own_line.len() as f32;
            let step = ((div_idx + 1) / 2) as f32 * SUPPORT_CLUSTER_STEP;
            let dir = if div_idx % 2 == 0 { 1.0 } else { -1.0 };
            (centre + dir * step).clamp(s_lo, s_hi)
        };
        let band_lo = target - BAND_HALF;
        let band_hi = if profile.loose < 1.0 {
            f32::INFINITY
        } else {
            target + BAND_HALF + DEEP_PULL
        };
        let cand_score = |h: &HexCoord, used: &HashSet<(i32, i32)>| {
            let base = front_score(h, grid, &front_of, cq, cr, defending, tactic, target);
            let d = front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0);
            // Beyond the band the distance penalty steepens (river shield /
            // cover can still pull a gun deeper — reverse-slope siting).
            let deep = if profile.loose < 1.0 {
                0.0
            } else {
                (d - (target + BAND_HALF)).max(0.0) * DEEP_W
            };
            base - deep - crowd_penalty(h, used) * profile.loose
        };
        let mut cands: Vec<(HexCoord, f32)> = e_zone
            .iter()
            .filter(|h| !used.contains(&(h.q, h.r)))
            .filter(|h| {
                let a = arc(h);
                a >= s_lo - SECTOR_SLACK && a <= s_hi + SECTOR_SLACK
            })
            .filter(|h| {
                let d = front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0);
                d >= band_lo && d <= band_hi
            })
            .map(|h| (*h, cand_score(h, &used)))
            .collect();
        if cands.is_empty() {
            // Degenerate zone (the support band falls beyond the zone):
            // fall back to the sector's nearest ground — a gun must never
            // stay OFFBOARD.
            cands = e_zone
                .iter()
                .filter(|h| !used.contains(&(h.q, h.r)))
                .filter(|h| {
                    let a = arc(h);
                    a >= s_lo - SECTOR_SLACK && a <= s_hi + SECTOR_SLACK
                })
                .map(|h| {
                    let d = front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0);
                    (*h, -(d - target).abs() * FRONT_W)
                })
                .collect();
        }
        let best = cands.drain(..).max_by(|(a, sa), (b, sb)| {
            // Stored scores (see pass 2: computed once at collection;
            // the fallback path scores -|d-target|×FRONT_W — a different
            // metric, but self-consistent within this candidate set).
            sa.partial_cmp(sb)
                .unwrap_or(Ordering::Equal)
                .then(
                    (arc(a) - slot)
                        .abs()
                        .partial_cmp(&(arc(b) - slot).abs())
                        .unwrap_or(Ordering::Equal)
                        .reverse(),
                )
                .then(a.q.cmp(&b.q))
                .then(a.r.cmp(&b.r))
        });
        if let Some((h, _)) = best {
            used.insert((h.q, h.r));
            u.position = h;
        }
    }

    // Pass 3 (§6.13): HQs deploy BEHIND their division — on the
    // command leash of the division's placed battalions, as far off the
    // front as the leash allows. Runs last so the battalions are placed.
    let hq_ids: Vec<usize> = units
        .iter()
        .filter(|u| u.side == enemy && u.is_hq() && in_division(u) && u_ok(u))
        .map(|u| u.id)
        .collect();
    for id in hq_ids {
        let division = units
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.division.clone())
            .unwrap_or_default();
        // Division anchor: centroid of the placed same-division battalions
        // (OFFBOARD = not placed yet, never pollutes the centroid).
        let members: Vec<HexCoord> = units
            .iter()
            .filter(|u| {
                u.side == enemy
                    && !u.is_hq()
                    && u.division == division
                    && u.position != BattalionUnit::OFFBOARD
            })
            .map(|u| u.position)
            .collect();
        let anchor: Option<HexCoord> = if members.is_empty() {
            None
        } else {
            let (sq, sr) = members.iter().fold((0i64, 0i64), |(aq, ar), h| {
                (aq + h.q as i64, ar + h.r as i64)
            });
            let n = members.len() as f32;
            Some(HexCoord::new(
                (sq as f32 / n).round() as i32,
                (sr as f32 / n).round() as i32,
            ))
        };
        let best = e_zone
            .iter()
            .filter(|h| !used.contains(&(h.q, h.r)))
            .min_by(|a, b| {
                let score = |h: &&HexCoord| {
                    let leash = anchor
                        .map(|c| (h.distance(c) - HQ_DEPLOY_LEASH).max(0))
                        .unwrap_or(0);
                    // On the leash first; then the deeper rear wins
                    // (front_of = distance to the enemy zone — bigger = safer).
                    let rear = -(front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0) as i64);
                    (leash, rear)
                };
                score(a).cmp(&score(b))
            });
        if let (Some(&h), Some(u)) = (best, units.iter_mut().find(|u| u.id == id)) {
            used.insert((h.q, h.r));
            u.position = h;
        }
    }
}

/// Spread penalty: a hex adjoining an already-taken one loses
/// `CROWD_W` so the line runs ALONG the threatened border instead of
/// stacking into a cluster. 0 for d ≥ 2.
fn crowd_penalty(h: &HexCoord, used: &HashSet<(i32, i32)>) -> f32 {
    if used.is_empty() {
        return 0.0;
    }
    let d = used
        .iter()
        .map(|&(q, r)| h.distance(HexCoord::new(q, r)))
        .min()
        .unwrap();
    CROWD_W * (2 - d).max(0) as f32
}

/// Pass-1 hex score; higher is better. Both sides aim at their echelon
/// target band [`target`] — the attacker's razor band (~[`ATK_FRONT_BAND`]),
/// the defender's defence band ([`DEF_FRONT_BAND`] + the unit's echelon
/// offset) — within the band terrain decides, and the city bonus pulls the
/// garrison into the VP city. "Front" is the distance to the nearest
/// enemy-zone hex (the boundary), not a centroid (a strip-zone
/// centroid sits far off the border on stitched maps).
/// The tactic card re-weights cover and river ground.
/// `target` is per-echelon (the echelon bands).
#[allow(clippy::too_many_arguments)]
fn front_score(
    h: &HexCoord,
    grid: &HexGrid,
    front_of: &HashMap<(i32, i32), f32>,
    cq: f32,
    cr: f32,
    defending: bool,
    tactic: CombatTactic,
    target: f32,
) -> f32 {
    let dist = front_of.get(&(h.q, h.r)).copied().unwrap_or(0.0);
    let mut score = -(dist - target).abs() * FRONT_W;
    let t = grid.cell(*h).map(|c| c.terrain).unwrap_or(Terrain::Plains);
    // Ambush: cover quality counts THREE times — the ambusher
    // lurks in forest / hills / ruins instead of spreading along the bare
    // border. v3.3: cover alone carries ground quality (the
    // terrain defense column is retired); the weight splits by side (the
    // attacker keeps the old pull — he steps off the line anyway).
    let terrain_w = if defending { TERRAIN_W } else { TERRAIN_W_ATK };
    let cover_w = if defending && tactic == CombatTactic::Ambush {
        terrain_w * 3.0
    } else {
        terrain_w
    };
    score += t.cover_percent() * cover_w;
    if defending && t == Terrain::Urban {
        score += DEF_URBAN_BONUS;
    }
    // River defense: the shield doubles AND the banks themselves
    // are prime ground — the line forms in the water's shadow, one step
    // from the fords it will deny.
    let shield_w = if defending && tactic == CombatTactic::RiverDefense {
        RIVER_SHIELD_W * 2.0
    } else {
        RIVER_SHIELD_W
    };
    if river_between(h, grid, cq, cr) {
        score += shield_w;
    }
    if defending && tactic == CombatTactic::RiverDefense && adjacent_river(h, grid) {
        score += RIVER_SHIELD_W * 0.75;
    }
    // Reverse-slope siting — a hex whose own
    // step toward the enemy stands higher is defiladed from indirect fire
    // (×0.5, §6.6 crest). Both sides' lines and guns prefer it; within the
    // band it beats bare ground but never a river shield, and the DEEP_W
    // penalty still caps how far it can drag a line behind the ridge.
    if defiladed(h, grid, cq, cr) {
        score += REVERSE_SLOPE_W;
    }
    score
}

/// True when any neighbour of `h` is a river hex (the bank strip the
/// river-defense line wants to hold).
fn adjacent_river(h: &HexCoord, grid: &HexGrid) -> bool {
    h.neighbors().iter().any(|n| {
        grid.cell(*n)
            .map(|c| c.terrain == Terrain::River)
            .unwrap_or(false)
    })
}

/// True when the straight axial line from `h` to the enemy centroid crosses
/// a river hex — i.e. the river runs BETWEEN this position and the attacker
/// (a shield), not behind it (背水). Sampled at ~1 hex spacing: approximate
/// by design, exact hex raycasting is overkill here. Endpoints are skipped
/// (the candidate hex is deployable and the centroid sits in the enemy zone,
/// so neither is river anyway). Shared with the planner — river
/// discipline for second-line positioning.
pub(crate) fn river_between(h: &HexCoord, grid: &HexGrid, cq: f32, cr: f32) -> bool {
    let (dq, dr) = (cq - h.q as f32, cr - h.r as f32);
    let steps = dq.abs().max(dr.abs()).ceil() as i32;
    if steps < 2 {
        return false;
    }
    for i in 1..steps {
        let t = i as f32 / steps as f32;
        let q = (h.q as f32 + dq * t).round() as i32;
        let r = (h.r as f32 + dr * t).round() as i32;
        if grid
            .cell(HexCoord::new(q, r))
            .map(|c| c.terrain == Terrain::River)
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// True when the hex's OWN STEP toward the
/// enemy — the hex immediately before it on the enemy-centroid line —
/// stands strictly higher: reverse-slope siting. Indirect fire from the
/// enemy side lands at ×0.5 (§6.6 crest, defilade_mult) — the ridge
/// shoulder throttles the impact angle — so the line and the guns prefer
/// it the way they prefer a river shield ("照抄河盾"). Coarse by design:
/// the same enemy-centroid sampling as [`river_between`].
fn defiladed(h: &HexCoord, grid: &HexGrid, cq: f32, cr: f32) -> bool {
    let (dq, dr) = (cq - h.q as f32, cr - h.r as f32);
    let steps = dq.abs().max(dr.abs()).ceil() as i32;
    if steps < 2 {
        return false;
    }
    let he = grid.cell(*h).map(|c| c.elevation).unwrap_or(0);
    let q = (h.q as f32 + dq / steps as f32).round() as i32;
    let r = (h.r as f32 + dr / steps as f32).round() as i32;
    let se = grid
        .cell(HexCoord::new(q, r))
        .map(|c| c.elevation)
        .unwrap_or(0);
    se > he
}

/// The hex stands on a CREST toward the enemy
/// — it rises above the battlefield (elevation ≥ 1: hills/mountains) AND
/// every sampled intermediate hex on the enemy-centroid line is strictly
/// lower, so the §6.6 ridge rule gives it unobstructed sight over the
/// approach (peaks see over saddles): the thin observation screen's spot.
/// (A crest hex is EXPOSED to indirect fire ×1.5 — the observers' historical
/// trade, and why the screen is thin. The elevation floor keeps flat ground
/// — where nothing stands higher than anything — from qualifying.)
fn on_crest(h: &HexCoord, grid: &HexGrid, cq: f32, cr: f32) -> bool {
    let he = grid.cell(*h).map(|c| c.elevation).unwrap_or(0);
    if he < 1 {
        return false;
    }
    let (dq, dr) = (cq - h.q as f32, cr - h.r as f32);
    let steps = dq.abs().max(dr.abs()).ceil() as i32;
    if steps < 2 {
        return false;
    }
    for i in 1..steps.min(5) {
        let t = i as f32 / steps as f32;
        let q = (h.q as f32 + dq * t).round() as i32;
        let r = (h.r as f32 + dr * t).round() as i32;
        let e = grid
            .cell(HexCoord::new(q, r))
            .map(|c| c.elevation)
            .unwrap_or(0);
        if e > he {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_core::UnitType;

    fn unit(id: usize, ty: UnitType, side: Side, range: i32) -> BattalionUnit {
        let mut u = BattalionUnit::new(id, format!("U{id}"), ty, side, HexCoord::new(0, 0));
        u.attack_range = range;
        u.max_org = 100.0;
        u.org = 100.0;
        u.max_strength = 100.0;
        u.strength = 100.0;
        u
    }

    /// River shield: with the Meuse between the zones, the line
    /// must settle on the FAR bank — never west of the river with the water
    /// at the defenders' backs (the Sedan 背水列阵 bug). The zone mirrors
    /// production (river hexes are never deployable — excluded
    /// here the way the generator builds zones).
    #[test]
    fn direct_fire_deploys_behind_river_shield() {
        let mut g = HexGrid::new(18, 8, Terrain::Plains);
        for r in 0..8 {
            g.set_terrain(HexCoord::new(8, r), Terrain::River);
        }
        let p_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| [HexCoord::new(0, r), HexCoord::new(1, r)])
            .collect();
        // Defender zone straddles the river (q 6..=14, the q8 ford column
        // excluded), as on Sedan.
        let e_zone: Vec<HexCoord> = (6..=14)
            .filter(|q| *q != 8)
            .map(|q| HexCoord::new(q, 3))
            .collect();
        let mut units = vec![
            unit(1, UnitType::Infantry, Side::Defender, 1),
            unit(2, UnitType::Infantry, Side::Defender, 1),
            unit(3, UnitType::Infantry, Side::Defender, 1),
        ];
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        for u in &units {
            assert!(
                u.position.q > 8,
                "must hold the far (east) bank, got {:?}",
                u.position
            );
        }
        // And the line hugs the river rather than the deep rear.
        assert!(
            units.iter().any(|u| u.position.q == 9),
            "line should hug the east bank"
        );
    }

    /// Terrain preference: good defensive ground one hex back beats
    /// a bare front hex.
    #[test]
    fn direct_fire_prefers_defensive_terrain() {
        let mut g = HexGrid::new(16, 6, Terrain::Plains);
        g.set_terrain(HexCoord::new(9, 3), Terrain::Forest);
        let p_zone = vec![HexCoord::new(0, 3), HexCoord::new(1, 3)];
        let e_zone = vec![
            HexCoord::new(8, 3),
            HexCoord::new(9, 3),
            HexCoord::new(10, 3),
        ];
        let mut units = vec![unit(1, UnitType::Infantry, Side::Defender, 1)];
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        assert_eq!(
            units[0].position,
            HexCoord::new(9, 3),
            "forest one hex back beats bare front"
        );
    }

    /// Standoff band regression: artillery deploys ~range/2 behind
    /// the front centroid, not in the extreme rear.
    #[test]
    fn artillery_sits_in_standoff_band() {
        let g = HexGrid::new(22, 6, Terrain::Plains);
        let p_zone = vec![HexCoord::new(0, 3), HexCoord::new(1, 3)];
        let e_zone: Vec<HexCoord> = (8..=20).map(|q| HexCoord::new(q, 3)).collect();
        let mut units = vec![
            unit(1, UnitType::Infantry, Side::Defender, 1),
            unit(2, UnitType::ArtilleryBrigade, Side::Defender, 6),
        ];
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let front = units[0].position;
        let arty = units[1].position;
        assert_eq!(
            arty.distance(front),
            3,
            "range 6 → standoff 3 behind the line"
        );
        assert!(arty.q > front.q, "behind the line (deeper rear)");
    }

    /// §6.13: the HQ deploys behind its division, inside command
    /// range of the anchor — never on the front line (pass 3).
    #[test]
    fn hq_deploys_behind_division_on_leash() {
        let g = HexGrid::new(22, 6, Terrain::Plains);
        let p_zone = vec![HexCoord::new(0, 3), HexCoord::new(1, 3)];
        let e_zone: Vec<HexCoord> = (8..=20).map(|q| HexCoord::new(q, 3)).collect();
        let mut units = vec![
            unit(1, UnitType::Infantry, Side::Defender, 1),
            unit(2, UnitType::Infantry, Side::Defender, 1),
            unit(3, UnitType::Headquarters, Side::Defender, 1),
        ];
        for u in units.iter_mut() {
            u.division = "D".to_string();
        }
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let hq = units[2].position;
        let front_q = units[..2].iter().map(|u| u.position.q).min().unwrap();
        assert!(
            hq.q > front_q,
            "HQ behind the line: {hq:?} vs front q={front_q}"
        );
        let leash_d = units[..2]
            .iter()
            .map(|u| hq.distance(u.position))
            .min()
            .unwrap();
        assert!(leash_d <= 2, "on the leash (≤2 of a member), got {leash_d}");
    }

    /// With a whole-province defender zone
    /// the line must spread ALONG the threatened border. The old squared
    /// distance pinned every battalion to the single closest point.
    #[test]
    fn line_spreads_along_the_threatened_border() {
        let g = HexGrid::new(16, 8, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        // 9-wide defender band west edge; 12 battalions must not cluster.
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (7..=13).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=12)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let rows: HashSet<i32> = units.iter().map(|u| u.position.r).collect();
        let cols: HashSet<i32> = units.iter().map(|u| u.position.q).collect();
        assert!(
            rows.len() >= 5,
            "line should run along the border, rows={rows:?}"
        );
        assert!(
            cols.len() >= 2,
            "at least a second (rear) rank, cols={cols:?}"
        );
        // And nothing may stack on the very same hex.
        let pos: HashSet<(i32, i32)> = units.iter().map(|u| (u.position.q, u.position.r)).collect();
        assert_eq!(pos.len(), 12);
    }

    /// On a LONG band of equidistant
    /// ground — every hex ties the distance term — the old easternmost
    /// tie-break stacked the whole defender line at one end of the band,
    /// 14+ hexes from the attacker (never in contact). The sector partition
    /// must spread the divisions across the band — each owns a
    /// contiguous arc slice, so the anchors cannot pile at one end.
    #[test]
    fn division_anchors_fan_out_on_an_equidistant_band() {
        // The full distance-3 ring around the enemy hex: 18 candidates, all
        // tying the front-band term (the ring is the band).
        let g = HexGrid::new(9, 9, Terrain::Plains);
        let p_zone = vec![HexCoord::new(4, 4)];
        let e_zone: Vec<HexCoord> = (0..9)
            .flat_map(|q| (0..9).map(move |r| HexCoord::new(q, r)))
            .filter(|h| h.distance(HexCoord::new(4, 4)) == 3)
            .collect();
        assert_eq!(e_zone.len(), 18, "the distance-3 ring");
        let mut units: Vec<BattalionUnit> = (1..=6)
            .map(|i| {
                let mut u = unit(i, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("D{i}");
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        // The six anchors must spread around the ring — the old easternmost
        // tie-break would leave every unit at (7,4) (rows = 1, cols = 1).
        let rows: HashSet<i32> = units.iter().map(|u| u.position.r).collect();
        let cols: HashSet<i32> = units.iter().map(|u| u.position.q).collect();
        assert!(rows.len() >= 3, "anchors fan out vertically, rows={rows:?}");
        assert!(
            cols.len() >= 2,
            "anchors fan out horizontally, cols={cols:?}"
        );
        // Each division holds a distinct hex on the ring (no stacking, no
        // anchor sharing a sector).
        let pos: HashSet<(i32, i32)> = units.iter().map(|u| (u.position.q, u.position.r)).collect();
        assert_eq!(pos.len(), 6, "each division takes its own hex, got {pos:?}");
    }

    /// Good ground must matter again with the linear distance —
    /// an urban strip two hexes behind the front draws the line onto it.
    #[test]
    fn line_occupies_city_ground() {
        let mut g = HexGrid::new(16, 8, Terrain::Plains);
        for r in 0..8 {
            g.set_terrain(HexCoord::new(9, r), Terrain::Urban);
        }
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (7..=13).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=4)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        assert!(
            units.iter().any(|u| u.position.q == 9),
            "defender must occupy the city, got {:?}",
            units.iter().map(|u| u.position).collect::<Vec<_>>()
        );
    }

    /// Side-dependent behaviour: the ATTACKER hugs the front —
    /// a distant city must NOT pull the assault line off the razor edge.
    /// The attacker's second wave sits a band behind the line
    /// (q9-11 here), but the deep urban column (q13) still must not draw
    /// anyone off the front.
    #[test]
    fn attacker_hugs_the_front_not_the_city() {
        let mut g = HexGrid::new(16, 8, Terrain::Plains);
        for r in 0..8 {
            g.set_terrain(HexCoord::new(13, r), Terrain::Urban);
        }
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (7..=13).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=3)
            .map(|i| unit(i, UnitType::Infantry, Side::Attacker, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Attacker,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        assert!(
            units.iter().all(|u| u.position.q < 12),
            "attacker must hug the front, not the deep city, got {:?}",
            units.iter().map(|u| u.position).collect::<Vec<_>>()
        );
    }

    /// A VP city far
    /// behind the front band needs a garrison QUOTA — the flat +60 urban
    /// bonus loses to the band's distance term beyond ~7 hex of frontage
    /// (observed on a whole-province map: city garrison 0/32).
    #[test]
    fn deep_city_gets_a_garrison_quota() {
        let mut g = HexGrid::new(24, 10, Terrain::Plains);
        // A 6-hex city 8+ hexes behind the front band.
        for r in 2..8 {
            g.set_terrain(HexCoord::new(18, r), Terrain::Urban);
        }
        let p_zone: Vec<HexCoord> = (0..10).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..10)
            .flat_map(|r| (4..=20).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=18)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let in_city = units
            .iter()
            .filter(|u| u.position.q >= 18)
            .map(|u| u.position)
            .collect::<Vec<_>>();
        assert!(
            in_city.len() >= 3,
            "the deep city needs a garrison quota, got {:?}",
            units.iter().map(|u| u.position).collect::<Vec<_>>()
        );
    }

    /// Origin-strip geometry: the defender band follows
    /// the inter-zone BOUNDARY. The old enemy-centroid anchor clusters the
    /// line where the centroid happens to sit — for a long enemy strip the
    /// centroid is far off the border and the whole line bunches mid-strip.
    #[test]
    fn line_follows_the_boundary_not_the_enemy_centroid() {
        let g = HexGrid::new(14, 12, Terrain::Plains);
        // Enemy: a long 2-row strip across the top — its centroid (5.5, 0.5)
        // sits nowhere near the defender zone 4+ rows below.
        let p_zone: Vec<HexCoord> = (0..=11)
            .flat_map(|q| [HexCoord::new(q, 0), HexCoord::new(q, 1)])
            .collect();
        let e_zone: Vec<HexCoord> = (0..=11)
            .flat_map(|q| (5..=10).map(move |r| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=12)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let cols: HashSet<i32> = units.iter().map(|u| u.position.q).collect();
        assert!(
            cols.len() >= 8,
            "line must spread along the full boundary, cols={cols:?}",
        );
    }

    /// Urban defense: the WHOLE line garrisons the city — not the
    /// one-third quota of the default defender.
    #[test]
    fn urban_defense_garrisons_the_whole_line_in_the_city() {
        let mut g = HexGrid::new(24, 10, Terrain::Plains);
        for r in 0..10 {
            g.set_terrain(HexCoord::new(10, r), Terrain::Urban);
        }
        let p_zone: Vec<HexCoord> = (0..10).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..10)
            .flat_map(|r| (3..=12).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=8)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::UrbanDefense,
            &HashSet::new(),
            None,
        );
        let positions = units.iter().map(|u| u.position).collect::<Vec<_>>();
        assert!(
            units.iter().all(|u| u.position.q == 10),
            "urban defense puts the WHOLE line in the city, got {positions:?}"
        );
    }

    /// Ambush: cover quality counts triple — a forest column well
    /// beyond the plain front band draws the lurkers (the default line
    /// would hug the bare border instead).
    #[test]
    fn ambush_lurks_in_cover_not_on_the_bare_border() {
        let mut g = HexGrid::new(24, 8, Terrain::Plains);
        for r in 0..8 {
            g.set_terrain(HexCoord::new(10, r), Terrain::Forest);
        }
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (4..=10).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=3)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Ambush,
            &HashSet::new(),
            None,
        );
        let positions = units.iter().map(|u| u.position).collect::<Vec<_>>();
        assert!(
            units.iter().any(|u| u.position.q == 10),
            "ambush lurks in the forest column, got {positions:?}"
        );
    }

    /// Player Auto Deploy: hexes the human already occupies are
    /// sacred — the planner must never double-book them while filling the
    /// remainder of the zone.
    #[test]
    fn pre_placed_hexes_are_never_retaken() {
        let g = HexGrid::new(16, 8, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (7..=13).map(move |q| HexCoord::new(q, r)))
            .collect();
        // The human put a unit on (7,0) — the best front hex by far.
        let mut pre = HashSet::new();
        pre.insert((7, 0));
        let mut units: Vec<BattalionUnit> = (1..=6)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &pre,
            None,
        );
        for u in &units {
            assert!(
                !pre.contains(&(u.position.q, u.position.r)),
                "AI must not re-take a pre-placed hex, put {:?} on it",
                u.position
            );
        }
        let pos: HashSet<(i32, i32)> = units.iter().map(|u| (u.position.q, u.position.r)).collect();
        assert_eq!(pos.len(), 6, "planner still spreads across the zone");
    }

    /// Sector deployment: `only_division` confines the planner
    /// to one division — the other division's battalions never move.
    #[test]
    fn only_division_leaves_other_divisions_untouched() {
        let g = HexGrid::new(16, 8, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (7..=13).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=6)
            .map(|i| {
                let mut u = unit(i, UnitType::Infantry, Side::Defender, 1);
                u.division = if i % 2 == 0 {
                    "A Div".to_string()
                } else {
                    "B Div".to_string()
                };
                u
            })
            .collect();
        let before: Vec<HexCoord> = units.iter().map(|u| u.position).collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            Some("A Div"),
        );
        for (i, u) in units.iter().enumerate() {
            if u.division == "B Div" {
                assert_eq!(
                    u.position, before[i],
                    "B Div must stay untouched, got {:?}",
                    u.position
                );
            } else {
                assert_ne!(
                    u.position, before[i],
                    "A Div should have been placed (was OFFBOARD sentinel)"
                );
            }
        }
        // And A Div's three battalions took three distinct hexes.
        let pos: HashSet<(i32, i32)> = units
            .iter()
            .filter(|u| u.division == "A Div")
            .map(|u| (u.position.q, u.position.r))
            .collect();
        assert_eq!(pos.len(), 3);
    }

    /// A division deploys
    /// TOGETHER — the line still spreads along the border (each division's
    /// FIRST battalion is unconstrained), but later battalions cluster near
    /// their own anchor instead of scattering across the whole front.
    #[test]
    fn division_members_deploy_cohesively() {
        let g = HexGrid::new(24, 12, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..12).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..12)
            .flat_map(|r| (5..=20).map(move |q| HexCoord::new(q, r)))
            .collect();
        // Three divisions of four, INTERLEAVED in the roster on purpose —
        // cohesion must come from the division label, not list order.
        let mut units: Vec<BattalionUnit> = (0..12)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 3) + 1);
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let diameter = |name: &str| {
            let pos: Vec<HexCoord> = units
                .iter()
                .filter(|u| u.division == name)
                .map(|u| u.position)
                .collect();
            pos.iter()
                .flat_map(|a| pos.iter().map(move |b| a.distance(*b)))
                .max()
                .unwrap_or(0)
        };
        for name in ["Div 1", "Div 2", "Div 3"] {
            assert!(
                diameter(name) <= 8,
                "{name} scattered across the front: diameter {}",
                diameter(name)
            );
        }
        // …while the force as a whole still spreads along the border.
        let all: Vec<HexCoord> = units.iter().map(|u| u.position).collect();
        let global = all
            .iter()
            .flat_map(|a| all.iter().map(move |b| a.distance(*b)))
            .max()
            .unwrap_or(0);
        assert!(global >= 9, "the line must still spread, diameter {global}");
    }

    /// Cohesion, pass 2: divisional artillery anchors to ITS OWN
    /// division's line, not the global front centroid — with two divisions
    /// sectoring far apart, each gun sits behind its own flag.
    #[test]
    fn artillery_anchors_to_its_own_division() {
        let g = HexGrid::new(30, 12, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..12).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..12)
            .flat_map(|r| (5..=25).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..6)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 2) + 1);
                u
            })
            .collect();
        for (i, div) in [(6, "Div 1"), (7, "Div 2")].into_iter() {
            let mut a = unit(i + 1, UnitType::ArtilleryBrigade, Side::Defender, 6);
            a.division = div.to_string();
            units.push(a);
        }
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        for i in [6usize, 7] {
            let div = units[i].division.clone();
            let own_line: Vec<HexCoord> = units[..6]
                .iter()
                .filter(|u| u.division == div)
                .map(|u| u.position)
                .collect();
            let d_own = own_line
                .iter()
                .map(|h| units[i].position.distance(*h))
                .min()
                .unwrap();
            let d_other: i32 = units[..6]
                .iter()
                .filter(|u| u.division != div)
                .map(|u| units[i].position.distance(u.position))
                .min()
                .unwrap();
            assert!(
                d_own <= d_other,
                "{div} gun must sit behind its own line: own {d_own} vs other {d_other}"
            );
        }
    }

    // ---- Adversarial battery: scenarios built to
    // BREAK the cohesion logic, plus the user-visible quality gate (command
    // coverage at turn 0). They are the tuning instrument for the layout
    // weights — when a weight changes, these are what must stay green.

    /// Adversarial: a NARROW front — more divisions than the corridor holds
    /// in one rank. Cohesion must stack each division in DEPTH (rear ranks
    /// behind its own anchor), never smear it sideways along the corridor.
    #[test]
    fn cohesion_stacks_in_depth_on_a_narrow_front() {
        let g = HexGrid::new(14, 12, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..12).map(|r| HexCoord::new(0, r)).collect();
        // Only TWO columns of deployable ground — 9 battalions cannot all
        // take the front rank.
        let e_zone: Vec<HexCoord> = (0..12)
            .flat_map(|r| (5..=6).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..9)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 3) + 1);
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        for name in ["Div 1", "Div 2", "Div 3"] {
            let pos: Vec<HexCoord> = units
                .iter()
                .filter(|u| u.division == name)
                .map(|u| u.position)
                .collect();
            let d = pos
                .iter()
                .flat_map(|a| pos.iter().map(move |b| a.distance(*b)))
                .max()
                .unwrap_or(0);
            assert!(
                d <= 5,
                "{name} smeared along the corridor: diameter {d} ({pos:?})"
            );
        }
    }

    /// Adversarial: only THREE river-shielded hexes for four battalions.
    /// The shield (+80) must always beat the cohesion pull — no battalion
    /// may be dragged back across the river toward its anchor (背水 regression).
    /// The zone mirrors production (the q8 ford column is never
    /// deployable).
    #[test]
    fn cohesion_never_overrides_the_river_shield() {
        let mut g = HexGrid::new(18, 8, Terrain::Plains);
        for r in 0..8 {
            g.set_terrain(HexCoord::new(8, r), Terrain::River);
        }
        let p_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| [HexCoord::new(0, r), HexCoord::new(1, r)])
            .collect();
        let e_zone: Vec<HexCoord> = (6..=14)
            .filter(|q| *q != 8)
            .map(|q| HexCoord::new(q, 3))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..4)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 2) + 1);
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        for u in &units {
            assert!(
                u.position.q > 8,
                "river shield must beat the cohesion drag: {:?} west of the river",
                u.position
            );
        }
        for name in ["Div 1", "Div 2"] {
            let pos: Vec<HexCoord> = units
                .iter()
                .filter(|u| u.division == name)
                .map(|u| u.position)
                .collect();
            assert!(
                pos[0].distance(pos[1]) <= 3,
                "{name}'s pair split by the shield race: {pos:?}"
            );
        }
    }

    /// Adversarial: the city-garrison quota detaches a division's WEAKEST
    /// battalions into a deep city. The division's remaining LINE members
    /// must still cluster (the garrison itself is a deliberate detachment
    /// and is exempt from cohesion by design).
    #[test]
    fn garrison_split_keeps_the_rest_of_the_division_cohesive() {
        let mut g = HexGrid::new(24, 10, Terrain::Plains);
        for r in 2..8 {
            g.set_terrain(HexCoord::new(18, r), Terrain::Urban);
        }
        let p_zone: Vec<HexCoord> = (0..10).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..10)
            .flat_map(|r| (4..=20).map(move |q| HexCoord::new(q, r)))
            .collect();
        // 3 divisions × 4; Div 1 is militia-grade (low org) → quota bait.
        let mut units: Vec<BattalionUnit> = (0..12)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 3) + 1);
                if i % 3 == 0 {
                    u.org = 40.0;
                }
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        for name in ["Div 1", "Div 2", "Div 3"] {
            let line: Vec<HexCoord> = units
                .iter()
                .filter(|u| u.division == name && u.position.q < 18)
                .map(|u| u.position)
                .collect();
            if line.len() < 2 {
                continue; // a fully-garrisoned division has no line to scatter
            }
            let d = line
                .iter()
                .flat_map(|a| line.iter().map(move |b| a.distance(*b)))
                .max()
                .unwrap_or(0);
            assert!(
                d <= 6,
                "{name}'s line scattered after the garrison split: diameter {d}"
            );
        }
    }

    /// The user-visible quality gate: after a FULL deployment (HQs placed by
    /// pass 3), most battalions must START the battle in command — the
    /// design bar is "not necessarily all in command range, but not far".
    /// This is
    /// the metric the cohesion weights answer to; if it dips, tune
    /// COHESION_FREE / COHESION_W / HQ_DEPLOY_LEASH, not this threshold.
    #[test]
    fn most_battalions_start_in_command() {
        let g = HexGrid::new(24, 14, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..14).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..14)
            .flat_map(|r| (5..=20).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..16)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 4) + 1);
                u
            })
            .collect();
        let mut next_id = 100;
        tactical_core::synthesize_hqs(&mut units, &mut next_id, Side::Defender, |_| {
            BattalionUnit::OFFBOARD
        });
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let params = tactical_core::CombatParams::default();
        let links = tactical_core::compute_command_links(&units, &params);
        let (mut in_cmd, mut total) = (0usize, 0usize);
        for (i, u) in units.iter().enumerate() {
            if u.is_hq() {
                continue;
            }
            total += 1;
            if tactical_core::in_command(links[i]) {
                in_cmd += 1;
            }
        }
        let frac = in_cmd as f32 / total as f32;
        eprintln!(
            "command coverage at turn 0: {in_cmd}/{total} = {:.0}%",
            frac * 100.0
        );
        assert!(
            frac >= 0.60,
            "only {in_cmd}/{total} ({:.0}%) start in command — cohesion tuning regressed",
            frac * 100.0
        );
    }

    // ── Sector partition + echelon profiles ────────────────────────────────

    /// The core anti-bias contract: every division owns a CONTIGUOUS slice
    /// of the front band (in arc order), so the line tiles the whole
    /// threatened boundary instead of clumping at one end. Verifies the
    /// sector order matches the roster order.
    #[test]
    fn divisions_own_disjoint_sectors_in_roster_order() {
        let g = HexGrid::new(20, 16, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..16).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..16)
            .flat_map(|r| (5..=14).map(move |q| HexCoord::new(q, r)))
            .collect();
        // 4 divisions of 4, interleaved in the roster on purpose.
        let mut units: Vec<BattalionUnit> = (0..16)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 4) + 1);
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        // Arc parameter: the band runs along r (the boundary is vertical).
        // Div 1 (roster-first) must hold the NORTH of the band, Div 4 the
        // south — a division's members may not straddle another division's
        // slice beyond the ±SECTOR_SLACK overlap.
        let mean_r = |name: &str| {
            let pos: Vec<HexCoord> = units
                .iter()
                .filter(|u| u.division == name)
                .map(|u| u.position)
                .collect();
            pos.iter().map(|h| h.r as f32).sum::<f32>() / pos.len().max(1) as f32
        };
        let m1 = mean_r("Div 1");
        let m2 = mean_r("Div 2");
        let m3 = mean_r("Div 3");
        let m4 = mean_r("Div 4");
        assert!(
            m1 < m2 && m2 < m3 && m3 < m4,
            "sectors must follow roster order along the band: {m1:.1} {m2:.1} {m3:.1} {m4:.1}"
        );
        // …and the whole force still spans the band (no end-piling).
        let all: Vec<HexCoord> = units.iter().map(|u| u.position).collect();
        let rows: HashSet<i32> = all.iter().map(|h| h.r).collect();
        assert!(
            rows.len() >= 10,
            "the line must cover the band, rows={rows:?}"
        );
    }

    /// A two-strip attack zone: the front band's arc is
    /// DISCONTINUOUS (NW + SE strips leave the arc between them empty).
    /// The sector partition must tile the real coverage — slicing the raw
    /// arc handed every division after the second an empty sector and they
    /// never deployed (observed on a real two-strip battle: 5 of 7
    /// divisions stuck on the OFFBOARD sentinel). Regression: EVERY
    /// division deploys, and
    /// the first/last divisions land on OPPOSITE strips (roster order tiles
    /// the coverage).
    #[test]
    fn two_strip_zone_deploys_every_division() {
        let g = HexGrid::new(40, 18, Terrain::Plains);
        // Enemy zone: the central block (like the besieged city province).
        let p_zone: Vec<HexCoord> = (2..16)
            .flat_map(|r| (16..=22).map(move |q| HexCoord::new(q, r)))
            .collect();
        // The attacker's two source-province strips, left and right — both
        // at the SAME standoff (nearest enemy hex 3 away), so neither strip
        // wins the distance score. The band's q-spread dominates the PCA →
        // the arc runs vertically = two DISJOINT runs (the strips' arcs do
        // not overlap).
        let e_zone: Vec<HexCoord> = (2..16)
            .flat_map(|r| (9..=13).map(move |q| HexCoord::new(q, r)))
            .chain((2..16).flat_map(|r| (25..=29).map(move |q| HexCoord::new(q, r))))
            .collect();
        // 6 divisions x 2 line battalions, blitz (CenterFocus → both strips).
        let mut units: Vec<BattalionUnit> = (0..12)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Attacker, 1);
                u.division = format!("Div {}", (i % 6) + 1);
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Attacker,
            CombatTactic::Blitz,
            &HashSet::new(),
            None,
        );
        for d in 1..=6 {
            let name = format!("Div {d}");
            let members: Vec<HexCoord> = units
                .iter()
                .filter(|u| u.division == name)
                .map(|u| u.position)
                .collect();
            assert_eq!(
                members.len(),
                2,
                "{name} must deploy both battalions (got {members:?})"
            );
            assert!(
                members.iter().all(|h| h != &BattalionUnit::OFFBOARD),
                "{name} left a battalion OFFBOARD"
            );
        }
        // First and last division on opposite strips (coverage tiling).
        let qs = |d: usize| {
            units
                .iter()
                .filter(|u| u.division == format!("Div {d}"))
                .map(|u| u.position.q)
                .collect::<Vec<i32>>()
        };
        let (q1, q6) = (qs(1), qs(6));
        assert!(
            q1.iter().all(|q| *q <= 19) != q6.iter().all(|q| *q <= 19),
            "Div 1 {q1:?} and Div 6 {q6:?} must sit on opposite strips"
        );
        // TwoWings (encirclement) must place everyone too.
        let mut units2: Vec<BattalionUnit> = (0..12)
            .map(|i| {
                let mut u = unit(100 + i, UnitType::Infantry, Side::Attacker, 1);
                u.division = format!("Div {}", (i % 6) + 1);
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units2,
            &e_zone,
            &p_zone,
            Side::Attacker,
            CombatTactic::Encirclement,
            &HashSet::new(),
            None,
        );
        assert!(
            units2.iter().all(|u| u.position != BattalionUnit::OFFBOARD),
            "encirclement (TwoWings) must deploy every unit on a two-strip zone"
        );
    }

    /// The sector order list (`sector_divisions`): when deploying
    /// division-by-division (allied contingents), the caller's division
    /// order must drive the partition — Div B must land in ITS slice even
    /// though it is deployed in a separate call.
    #[test]
    fn sector_divisions_orders_the_slices() {
        let g = HexGrid::new(20, 16, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..16).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..16)
            .flat_map(|r| (5..=14).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..8)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = format!("Div {}", (i % 2) + 1);
                u
            })
            .collect();
        let order = vec!["Div 1".to_string(), "Div 2".to_string()];
        // Deploy Div 2 first (a separate call, the allied-nations flow).
        ai_deploy_impl(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            Some("Div 2"),
            Some(&order),
            false,
        );
        let div2_rows: HashSet<i32> = units
            .iter()
            .filter(|u| u.division == "Div 2")
            .map(|u| u.position.r)
            .collect();
        let div2_mean = units
            .iter()
            .filter(|u| u.division == "Div 2")
            .map(|u| u.position.r as f32)
            .sum::<f32>()
            / 4.0;
        assert!(
            div2_mean > 6.0,
            "Div 2 (the second sector) must deploy to the SOUTH half of the band, mean r {div2_mean:.1}, rows {div2_rows:?}"
        );
    }

    /// Echelon depth: the defender's second echelon (elastic: 35%) sits a
    /// band BEHIND the front rank — a fallback line, not a second blob.
    #[test]
    fn defender_deploys_a_second_echelon() {
        let g = HexGrid::new(24, 16, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..16).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..16)
            .flat_map(|r| (5..=20).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..20)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = "D".to_string();
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::ElasticDefense,
            &HashSet::new(),
            None,
        );
        let front_rank: Vec<i32> = units.iter().map(|u| u.position.q).collect();
        let min_q = *front_rank.iter().min().unwrap();
        let max_q = *front_rank.iter().max().unwrap();
        // Front band at dist 5 (q5-6), reserve echelon at dist +2 (q7+).
        let rear = units.iter().filter(|u| u.position.q >= min_q + 2).count();
        assert!(
            rear >= 5,
            "elastic defense needs a second echelon behind the line (min_q {min_q}, max_q {max_q}), rear units {rear}"
        );
        assert!(
            max_q >= min_q + 2,
            "the echelon must reach a real second band"
        );
    }

    /// TacticalWithdrawal echelons in depth: a thin screen + two fallback
    /// lines — the retreat has somewhere to go.
    #[test]
    fn tactical_withdrawal_echelons_in_depth() {
        let g = HexGrid::new(30, 12, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..12).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..12)
            .flat_map(|r| (5..=26).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..18)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = "D".to_string();
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::TacticalWithdrawal,
            &HashSet::new(),
            None,
        );
        let qs: Vec<i32> = units.iter().map(|u| u.position.q).collect();
        let min_q = *qs.iter().min().unwrap();
        let max_q = *qs.iter().max().unwrap();
        assert!(
            max_q - min_q >= 5,
            "tactical withdrawal must echelon in depth, q span {} (min {min_q}, max {max_q})",
            max_q - min_q
        );
        // Three distinct bands: screen, first fallback, second fallback.
        let bands: HashSet<i32> = qs.iter().map(|q| (q - min_q) / 3).collect();
        assert!(bands.len() >= 3, "three echelons expected, bands={bands:?}");
    }

    /// Blitz concentrates on the centre of the band — the flanks stay empty
    /// (the deep-penetration axis, not a broad line).
    #[test]
    fn blitz_concentrates_on_the_centre() {
        let g = HexGrid::new(20, 24, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..24).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..24)
            .flat_map(|r| (5..=14).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..12)
            .map(|i| unit(i + 1, UnitType::Infantry, Side::Attacker, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Attacker,
            CombatTactic::Blitz,
            &HashSet::new(),
            None,
        );
        let rs: Vec<i32> = units.iter().map(|u| u.position.r).collect();
        let min_r = *rs.iter().min().unwrap();
        let max_r = *rs.iter().max().unwrap();
        let span = max_r - min_r;
        assert!(
            span <= 19,
            "blitz concentrates on the axis: occupied r-span {span} (min {min_r}, max {max_r}) of 24 rows"
        );
    }

    /// The garrison per-division cap: a whole weak division is NOT gutted —
    /// at most a third of its own line units go to the city (without the
    /// cap a weak division's battalion landed 57 hexes from the front).
    #[test]
    fn garrison_cap_keeps_weak_divisions_on_the_line() {
        let mut g = HexGrid::new(24, 10, Terrain::Plains);
        for r in 2..8 {
            g.set_terrain(HexCoord::new(18, r), Terrain::Urban);
        }
        let p_zone: Vec<HexCoord> = (0..10).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..10)
            .flat_map(|r| (4..=20).map(move |q| HexCoord::new(q, r)))
            .collect();
        // Div 1 = 6 militia (org 40) — the weakest division by far.
        let mut units: Vec<BattalionUnit> = (0..12)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = if i < 6 {
                    "Weak".to_string()
                } else {
                    "Strong".to_string()
                };
                if i < 6 {
                    u.org = 40.0;
                }
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let weak_in_city = units
            .iter()
            .filter(|u| u.division == "Weak" && u.position.q >= 18)
            .count();
        assert!(
            weak_in_city <= 2,
            "at most a third of the weak division may garrison ({weak_in_city} in the city of 6)"
        );
    }

    // ── Support band — AA/AT behind the line ──────────────────────────────

    /// AA (attack range 1) is NOT a line weapon — it must deploy
    /// in the support band behind the line, never on the razor edge where
    /// infantry assault overruns it (the umbrella radius 3 still covers the
    /// line from 3 hexes back). Under the old `attack_range <= 1` pass-1
    /// filter the flak battery sat ON the line with the infantry.
    #[test]
    fn aa_deploys_behind_the_line_not_on_it() {
        let g = HexGrid::new(22, 8, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (6..=18).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units = vec![
            unit(1, UnitType::Infantry, Side::Defender, 1),
            unit(2, UnitType::AntiAirBrigade, Side::Defender, 1),
        ];
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let front = |h: HexCoord| p_zone.iter().map(|p| h.distance(*p)).min().unwrap();
        let inf_f = front(units[0].position);
        let aa_f = front(units[1].position);
        assert!(
            aa_f >= inf_f + 1,
            "AA must sit behind the line: line at {inf_f}, AA at {aa_f} ({:?})",
            units[1].position
        );
        assert!(
            aa_f <= inf_f + 5,
            "AA must stay in the support band, not the deep rear (line {inf_f}, AA {aa_f})"
        );
    }

    /// AT guns take the SECOND LINE — the front band + 2 — the
    /// classic PAK siting covering the approach lanes, never the razor edge
    /// (the old centroid-relative standoff 1 ring straddled the line).
    #[test]
    fn at_guns_sit_in_the_second_line() {
        let g = HexGrid::new(22, 8, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (6..=18).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units = vec![
            unit(1, UnitType::Infantry, Side::Defender, 1),
            unit(2, UnitType::AntiTankBrigade, Side::Defender, 2),
        ];
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let front = |h: HexCoord| p_zone.iter().map(|p| h.distance(*p)).min().unwrap();
        let inf_f = front(units[0].position);
        let at_f = front(units[1].position);
        assert!(
            at_f >= inf_f + 1,
            "AT must sit in the second line: line at {inf_f}, AT at {at_f} ({:?})",
            units[1].position
        );
        assert!(
            at_f <= inf_f + 4,
            "AT must stay near the line (second-line siting), not the deep rear (line {inf_f}, AT {at_f})"
        );
    }

    /// On a WIDE sparse front the
    /// even arc spread parked a division's lone howitzer at the sector's far
    /// slice — 19 hexes from the lane the enemy actually used; it never
    /// fired. Guns now cluster behind their own line's arc centre.
    #[test]
    fn support_guns_cluster_behind_the_line_centre() {
        let g = HexGrid::new(30, 40, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..40).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..40)
            .flat_map(|r| (5..=25).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..8)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = "Div".to_string();
                u
            })
            .collect();
        for i in 0..3 {
            let mut a = unit(9 + i, UnitType::ArtilleryBrigade, Side::Defender, 9);
            a.division = "Div".to_string();
            units.push(a);
        }
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        // The line's centre of mass along the front (the r axis).
        let line_rc = units[..8].iter().map(|u| u.position.r as f32).sum::<f32>() / 8.0;
        for (k, i) in [8usize, 9, 10].into_iter().enumerate() {
            let off = (units[i].position.r as f32 - line_rc).abs();
            let limit = 3.0 + k as f32 * SUPPORT_CLUSTER_STEP;
            assert!(
                off <= limit,
                "gun {i} must cluster behind the line centre ({line_rc:.1}), got r={} (off {off:.1}, limit {limit:.1})",
                units[i].position.r
            );
        }
    }

    /// The ATTACKER keeps the even arc spread — its guns creep
    /// toward the nearest enemy and pair with their local line segment;
    /// clustering them converged every tube on one flapping creep goal and
    /// they never settled (observed: attacker fire collapsed when the
    /// clustering briefly applied to both sides).
    #[test]
    fn attacker_guns_keep_the_even_spread() {
        let g = HexGrid::new(30, 40, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..40).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..40)
            .flat_map(|r| (5..=25).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (0..8)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Attacker, 1);
                u.division = "Div".to_string();
                u
            })
            .collect();
        for i in 0..3 {
            let mut a = unit(9 + i, UnitType::ArtilleryBrigade, Side::Attacker, 9);
            a.division = "Div".to_string();
            units.push(a);
        }
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Attacker,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let line_rc = units[..8].iter().map(|u| u.position.r as f32).sum::<f32>() / 8.0;
        let max_off = [8usize, 9, 10]
            .into_iter()
            .map(|i| (units[i].position.r as f32 - line_rc).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_off > 6.0,
            "attacker guns must spread along the sector, not cluster at the line centre ({line_rc:.1}): {:?}",
            [8usize, 9, 10].map(|i| units[i].position.r)
        );
    }

    /// A support-only division (army-level AT/AA group, no line
    /// troops) owns a CONTIGUOUS sector slice of the band — its guns deploy
    /// in ITS third of the line instead of clustering at the global front
    /// centroid (the old pass-2 fallback reference).
    #[test]
    fn support_only_division_gets_a_sector() {
        let g = HexGrid::new(22, 20, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..20).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..20)
            .flat_map(|r| (5..=18).map(move |q| HexCoord::new(q, r)))
            .collect();
        // Div A: 4 infantry. Div B: 2 AT guns (no line troops).
        let mut units: Vec<BattalionUnit> = (0..4)
            .map(|i| {
                let mut u = unit(i + 1, UnitType::Infantry, Side::Defender, 1);
                u.division = "Div A".to_string();
                u
            })
            .collect();
        for i in 0..2 {
            let mut at = unit(5 + i, UnitType::AntiTankBrigade, Side::Defender, 2);
            at.division = "Div B".to_string();
            units.push(at);
        }
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let mean_r = |name: &str| {
            let pos: Vec<HexCoord> = units
                .iter()
                .filter(|u| u.division == name)
                .map(|u| u.position)
                .collect();
            pos.iter().map(|h| h.r as f32).sum::<f32>() / pos.len().max(1) as f32
        };
        // Div B (roster-second, 2 of 6 deployable units) owns the SOUTH
        // third of the band — its guns must NOT land at the line's centre.
        let a = mean_r("Div A");
        let b = mean_r("Div B");
        assert!(
            b > a + 3.0,
            "Div B's guns must deploy in THEIR slice: A mean r {a:.1}, B {b:.1}"
        );
        // …and the guns sit behind the line, not on it.
        let front = |h: HexCoord| p_zone.iter().map(|p| h.distance(*p)).min().unwrap();
        let line_f = units[..4].iter().map(|u| front(u.position)).min().unwrap();
        for u in &units[4..] {
            assert!(
                front(u.position) >= line_f + 1,
                "Div B gun on the line: line at {line_f}, gun at {} ({:?})",
                front(u.position),
                u.position
            );
        }
    }

    // ── Reverse slope + crest observers ────────────────────────────────────

    /// A ridge between the zones pulls the defender's line onto
    /// the REVERSE slope — the hex whose step toward the enemy stands higher
    /// is defiladed from indirect fire (×0.5 §6.6) — not the exposed crest
    /// itself (which eats ×1.5 from the enemy guns). The thin observation
    /// quota (1 of 6) may man the crest; the line itself holds q6.
    #[test]
    fn defender_deploys_behind_the_ridge_not_on_the_crest() {
        let mut g = HexGrid::new(24, 8, Terrain::Plains);
        // A pure-elevation ridge at q5 (terrain stays plains so the test
        // isolates the elevation term; the ridge is the defender zone's
        // front edge, exactly where the band sits).
        for r in 0..8 {
            g.cell_mut(HexCoord::new(5, r)).unwrap().elevation = 2;
        }
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (4..=18).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=6)
            .map(|i| unit(i, UnitType::Infantry, Side::Defender, 1))
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        let on_slope = units.iter().filter(|u| u.position.q == 6).count();
        assert!(
            on_slope >= 5,
            "the line must hold the reverse slope (q6, step = the q5 ridge), got {:?}",
            units.iter().map(|u| u.position).collect::<Vec<_>>()
        );
        // Nobody may sit EXPOSED in front of the ridge (q4) or behind it
        // without the step cover — every unit is on the crest or its slope.
        assert!(
            units.iter().all(|u| u.position.q == 5 || u.position.q == 6),
            "units must be on the crest (q5, observer) or the reverse slope (q6), got {:?}",
            units.iter().map(|u| u.position).collect::<Vec<_>>()
        );
    }

    /// The defender posts a THIN observation screen on the
    /// crest — a small quota of the WEAKEST line units stands where the
    /// ridge rule gives unobstructed sight over the approach (peaks see
    /// over saddles); the rest of the line holds the reverse slope.
    #[test]
    fn defender_posts_thin_observation_screen_on_the_crest() {
        let mut g = HexGrid::new(24, 8, Terrain::Plains);
        for r in 0..8 {
            g.cell_mut(HexCoord::new(5, r)).unwrap().elevation = 2;
        }
        let p_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (0..8)
            .flat_map(|r| (4..=18).map(move |q| HexCoord::new(q, r)))
            .collect();
        let mut units: Vec<BattalionUnit> = (1..=8)
            .map(|i| {
                let mut u = unit(i, UnitType::Infantry, Side::Defender, 1);
                if i == 1 {
                    u.org = 40.0; // the weakest — observer bait
                }
                u
            })
            .collect();
        ai_deploy(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            None,
        );
        // Quota = 8/8 = 1 post; the weakest unit stands ON the crest (q5),
        // the other seven hold the reverse slope (q6).
        let weak = units.iter().find(|u| u.org == 40.0).unwrap();
        assert_eq!(
            weak.position.q, 5,
            "the weakest unit must man the crest observation post, got {:?}",
            weak.position
        );
        assert!(
            units
                .iter()
                .filter(|u| u.id != weak.id)
                .all(|u| u.position.q == 6),
            "the line holds the reverse slope, got {:?}",
            units
                .iter()
                .map(|u| (u.name.clone(), u.position))
                .collect::<Vec<_>>()
        );
    }

    /// The player's Auto Deploy (`only_undeployed = true`) fills
    /// ONLY the OOB waiters — a hand-placed unit (undeployed cleared) stays
    /// exactly where the player put it, and its hex stays blocked.
    #[test]
    fn auto_deploy_leaves_hand_placed_units_untouched() {
        let g = HexGrid::new(16, 6, Terrain::Plains);
        let p_zone: Vec<HexCoord> = (0..6).map(|r| HexCoord::new(0, r)).collect();
        let e_zone: Vec<HexCoord> = (10..=14).map(|q| HexCoord::new(q, 2)).collect();
        let anchor = HexCoord::new(14, 2); // in-zone corner, player-placed
        let mut units = vec![
            unit(1, UnitType::Infantry, Side::Defender, 1),
            unit(2, UnitType::Infantry, Side::Defender, 1),
            unit(3, UnitType::Infantry, Side::Defender, 1),
        ];
        units[0].position = anchor;
        units[0].undeployed = false; // hand-placed
        units[1].undeployed = true; // waiting in the OOB
        units[2].undeployed = true;
        let pre_used: HashSet<(i32, i32)> = [(anchor.q, anchor.r)].into_iter().collect();
        ai_deploy_impl(
            &g,
            &mut units,
            &e_zone,
            &p_zone,
            Side::Defender,
            CombatTactic::Default,
            &pre_used,
            None,
            None,
            true,
        );
        assert_eq!(
            units[0].position, anchor,
            "hand-placed unit must stay put, got {:?}",
            units[0].position
        );
        assert!(
            units[1..]
                .iter()
                .all(|u| u.position != anchor && e_zone.contains(&u.position)),
            "the OOB waiters deploy inside the zone off the taken hex: {:?}",
            units.iter().map(|u| (u.id, u.position)).collect::<Vec<_>>()
        );
    }
}
