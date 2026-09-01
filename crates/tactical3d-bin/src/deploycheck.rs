//! `--deploycheck` — deployment analysis tool: runs
//! the AI deployment for both sides of a battle and prints a structured
//! dump (per-division positions, front coverage metrics, an ASCII map) so
//! deployment quality can be studied on the historical battle scripts
//! without launching the game.
//!
//!   forward-command --deploycheck file=1939_warsaw [atk_tactic=...] [def_tactic=...]
//!
//! Output: zone stats, per-division placement lines, coverage metrics, and a
//! combined ASCII map (saved to `deploycheck_map.txt` next to the exe).

use std::collections::HashSet;

use tactical_ai::CombatTactic;
use tactical_core::hex::HexCoord;
use tactical_core::unit::{BattalionUnit, Side};
use tactical_core::{HexGrid, Terrain};

use crate::scenario::Scenario;

pub struct Opts {
    pub scenario: Scenario,
    pub atk_tactic: CombatTactic,
    pub def_tactic: CombatTactic,
}

/// ~80% of the line battalions form the front band; the rest are the
/// reserve echelon (defender) / follow-up (attacker) — mirror of ai_deploy.
fn band_units(units: &[BattalionUnit], side: Side) -> Vec<&BattalionUnit> {
    units
        .iter()
        .filter(|u| {
            u.side == side
                && u.is_combat_effective()
                && u.attack_range <= 1
                && !u.is_hq()
                && u.position != BattalionUnit::OFFBOARD
        })
        .collect()
}

fn report(
    prefix: &str,
    grid: &HexGrid,
    units: &[BattalionUnit],
    side: Side,
    zone: &[HexCoord],
    foe: &[HexCoord],
) {
    let band = band_units(units, side);
    let front_of = |h: HexCoord| foe.iter().map(|p| h.distance(*p)).min().unwrap_or(0);
    let dists: Vec<i32> = band.iter().map(|u| front_of(u.position)).collect();
    let n = dists.len();
    if n == 0 {
        println!("  [{prefix}] no line battalions deployed");
        return;
    }
    let mean_d = dists.iter().map(|d| *d as f64).sum::<f64>() / n as f64;
    let d_min = *dists.iter().min().unwrap();
    let d_max = *dists.iter().max().unwrap();
    // Front coverage: project line positions onto the principal axis of the
    // band (the front line's own extent), measure the covered span vs the
    // band's span, and count gaps > 3 hexes between consecutive projections.
    let (sq, sr) = zone.iter().fold((0i64, 0i64), |(aq, ar), h| {
        (aq + h.q as i64, ar + h.r as i64)
    });
    let nz = zone.len().max(1) as f64;
    let (cz, cr) = (sq as f64 / nz, sr as f64 / nz);
    let (uq, ur) = {
        let mut sx = 0f64;
        let mut sy = 0f64;
        for u in &band {
            let (dx, dy) = (u.position.q as f64 - cz, u.position.r as f64 - cr);
            sx += dx;
            sy += dy;
        }
        let len = (sx * sx + sy * sy).sqrt().max(1e-6);
        (sx / len, sy / len)
    };
    let proj = |h: HexCoord| (h.q as f64 - cz) * uq + (h.r as f64 - cr) * ur;
    let mut band_proj: Vec<f64> = zone.iter().map(|h| proj(*h)).collect();
    band_proj.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let band_span = band_proj.last().unwrap_or(&0.0) - band_proj.first().unwrap_or(&0.0);
    let mut pos_proj: Vec<f64> = band.iter().map(|u| proj(u.position)).collect();
    pos_proj.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let covered = pos_proj.last().unwrap_or(&0.0) - pos_proj.first().unwrap_or(&0.0);
    let mut gaps = 0usize;
    for w in pos_proj.windows(2) {
        if w[1] - w[0] > 3.0 {
            gaps += 1;
        }
    }
    // Per-division diameter.
    let mut divs: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for u in &band {
        if !seen.contains(&u.division) {
            seen.push(u.division.clone());
        }
    }
    for d in seen {
        let pos: Vec<HexCoord> = band
            .iter()
            .filter(|u| u.division == d)
            .map(|u| u.position)
            .collect();
        let diam = pos
            .iter()
            .flat_map(|a| pos.iter().map(move |b| a.distance(*b)))
            .max()
            .unwrap_or(0);
        divs.push(format!("{d}=diam{diam}x{}", pos.len()));
    }
    let terr: Vec<Terrain> = band
        .iter()
        .filter_map(|u| grid.cell(u.position))
        .map(|c| c.terrain)
        .collect();
    let count = |t: Terrain| terr.iter().filter(|x| **x == t).count();
    println!(
        "  [{prefix}] {n} line bns | band dist mean {mean_d:.1} (min {d_min}, max {d_max}) | \
         cover {covered:.0}/{band_span:.0} ({:.0}%) | gaps>{gaps} | terrain P{} F{} H{} M{} U{} R{}",
        if band_span > 0.0 { covered / band_span * 100.0 } else { 0.0 },
        count(Terrain::Plains),
        count(Terrain::Forest),
        count(Terrain::Hills),
        count(Terrain::Mountain),
        count(Terrain::Urban),
        count(Terrain::River),
    );
    println!("    divs: {}", divs.join(" | "));
    let mut rows: Vec<String> = band
        .iter()
        .map(|u| {
            format!(
                "  {:>24} ({},{}) d{}",
                u.name,
                u.position.q,
                u.position.r,
                front_of(u.position)
            )
        })
        .collect();
    rows.sort();
    for r in rows {
        println!("{r}");
    }
}

fn ascii_map(
    grid: &HexGrid,
    units: &[BattalionUnit],
    zones: &(Vec<HexCoord>, Vec<HexCoord>),
) -> String {
    let a_set: HashSet<(i32, i32)> = zones.0.iter().map(|h| (h.q, h.r)).collect();
    let d_set: HashSet<(i32, i32)> = zones.1.iter().map(|h| (h.q, h.r)).collect();
    let mut out = String::new();
    out.push_str(&format!("grid {}x{}\n", grid.width, grid.height));
    for r in 0..grid.height {
        let mut line = String::new();
        for q in 0..grid.width {
            let h = HexCoord::new(q as i32, r as i32);
            let (qq, rr) = (q as i32, r as i32);
            let c = grid.cell(h);
            let unit = units.iter().find(|u| u.position == h);
            let ch = match unit {
                Some(u) if u.side == Side::Attacker => {
                    if u.is_hq() {
                        'H'
                    } else if u.is_indirect_artillery() {
                        'A'
                    } else if u.unit_type.is_armor() {
                        'T'
                    } else {
                        'a'
                    }
                }
                Some(u) if u.side == Side::Defender => {
                    if u.is_hq() {
                        'h'
                    } else if u.is_indirect_artillery() {
                        'g'
                    } else if u.unit_type.is_armor() {
                        't'
                    } else {
                        'd'
                    }
                }
                Some(_) => '?',
                None => {
                    let t = c.map(|c| c.terrain).unwrap_or(Terrain::Plains);
                    if a_set.contains(&(qq, rr)) && d_set.contains(&(qq, rr)) {
                        '*'
                    } else if a_set.contains(&(qq, rr)) {
                        '#'
                    } else if d_set.contains(&(qq, rr)) {
                        '='
                    } else {
                        match t {
                            Terrain::River => '~',
                            Terrain::Forest => 'T',
                            Terrain::Hills => '+',
                            Terrain::Mountain => '^',
                            Terrain::Urban => 'U',
                            Terrain::Marsh => 'm',
                            Terrain::Water => '.',
                            _ => ' ',
                        }
                    }
                }
            };
            line.push(ch);
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn run(opts: Opts) {
    let settings = crate::settings::AppSettings::load();
    let spec = match crate::scenario::assemble(&opts.scenario, &settings) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("--deploycheck: {e}");
            std::process::exit(2);
        }
    };
    let grid = spec.grid;
    let zones = spec.zones.clone();
    let mut units = spec.units;
    let player = spec.player_side;
    let enemy = player.opponent();
    let (player_zone, enemy_zone) = if player == Side::Attacker {
        (&zones.0, &zones.1)
    } else {
        (&zones.1, &zones.0)
    };
    let atk_tactic = opts.atk_tactic;
    let def_tactic = opts.def_tactic;
    let (player_card, enemy_card) = if player == Side::Attacker {
        (atk_tactic, def_tactic)
    } else {
        (def_tactic, atk_tactic)
    };
    // Split-deploy semantics like headless: player slices by contingent,
    // then the enemy whole-side.
    let allied_divs: HashSet<String> = spec
        .allies
        .iter()
        .flat_map(|a| a.divisions.iter().cloned())
        .collect();
    let player_divs: Vec<String> = if allied_divs.is_empty() {
        Vec::new()
    } else {
        let mut seen: HashSet<String> = HashSet::new();
        units
            .iter()
            .filter(|u| {
                u.side == player && !u.division.is_empty() && !allied_divs.contains(&u.division)
            })
            .map(|u| u.division.clone())
            .filter(|d| seen.insert(d.clone()))
            .collect()
    };
    let split = !spec.allies.is_empty();
    // The full player-side division order for the sector
    // partition (player divisions first, then each nation's in order).
    let all_divisions: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for d in player_divs
            .iter()
            .chain(spec.allies.iter().flat_map(|a| a.divisions.iter()))
        {
            if seen.insert(d.clone()) {
                out.push(d.clone());
            }
        }
        out
    };
    let deploy_division = |units: &mut Vec<BattalionUnit>,
                           div: &str,
                           card: CombatTactic,
                           pre_used: &mut HashSet<(i32, i32)>,
                           zone: &[HexCoord],
                           foe: &[HexCoord],
                           side: Side| {
        tactical_ai::ai_deploy_impl(
            &grid,
            units,
            zone,
            foe,
            side,
            card,
            pre_used,
            Some(div),
            Some(&all_divisions),
            false, // tooling mirror of the AI flow — arrange everyone
        );
        for u in units.iter_mut() {
            if u.side == side
                && u.division == div
                && u.undeployed
                && u.position != BattalionUnit::OFFBOARD
            {
                u.undeployed = false;
                pre_used.insert((u.position.q, u.position.r));
            }
        }
    };
    if !split {
        tactical_ai::ai_deploy(
            &grid,
            &mut units,
            player_zone,
            enemy_zone,
            player,
            player_card,
            &HashSet::new(),
            None,
        );
    } else {
        let mut pre_used: HashSet<(i32, i32)> = HashSet::new();
        for div in &player_divs {
            deploy_division(
                &mut units,
                div,
                player_card,
                &mut pre_used,
                player_zone,
                enemy_zone,
                player,
            );
        }
        for cont in &spec.allies {
            for div in &cont.divisions {
                deploy_division(
                    &mut units,
                    div,
                    cont.tactic,
                    &mut pre_used,
                    player_zone,
                    enemy_zone,
                    player,
                );
            }
        }
    }
    tactical_ai::ai_deploy(
        &grid,
        &mut units,
        enemy_zone,
        player_zone,
        enemy,
        enemy_card,
        &HashSet::new(),
        None,
    );
    for u in &mut units {
        u.undeployed = false;
    }

    println!(
        "== {} | ATK {} vs DEF {} | zones: ATK {} hexes, DEF {} hexes ==",
        spec.location,
        atk_tactic.token(),
        def_tactic.token(),
        zones.0.len(),
        zones.1.len()
    );
    report("ATK", &grid, &units, Side::Attacker, &zones.0, &zones.1);
    report("DEF", &grid, &units, Side::Defender, &zones.1, &zones.0);
    // Support units (ranged) — where did the guns land?
    for (label, side, zone) in [
        ("ATK", Side::Attacker, &zones.0),
        ("DEF", Side::Defender, &zones.1),
    ] {
        let foe = if side == Side::Attacker {
            &zones.1
        } else {
            &zones.0
        };
        let guns: Vec<&BattalionUnit> = units
            .iter()
            .filter(|u| u.side == side && u.is_combat_effective() && u.attack_range > 1)
            .collect();
        for u in guns {
            let d = foe
                .iter()
                .map(|p| u.position.distance(*p))
                .min()
                .unwrap_or(0);
            println!(
                "  [{label} gun] {:>24} ({},{}) dist-to-foe {d} range {}",
                u.name, u.position.q, u.position.r, u.attack_range
            );
        }
        let _ = zone;
    }
    let map = ascii_map(&grid, &units, &zones);
    // Write next to the exe, not the CWD — CWD writes land wherever the
    // caller happened to stand and can fail silently.
    let out_path = std::env::current_exe()
        .map(|p| p.with_file_name("deploycheck_map.txt"))
        .unwrap_or_else(|_| std::path::PathBuf::from("deploycheck_map.txt"));
    if let Err(e) = std::fs::write(&out_path, &map) {
        eprintln!("deploycheck: cannot write {}: {e}", out_path.display());
    } else {
        println!("map -> {}", out_path.display());
    }
}
