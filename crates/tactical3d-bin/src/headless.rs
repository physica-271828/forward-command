//! Headless AI-vs-AI battle runner: no Bevy, no window — a battle plays out
//! at full speed with a per-turn action trace, so AI behavior can be
//! evaluated and tuned in tight loops.
//!
//!   forward-command --headless [turns] [seed]
//!   forward-command --headless file=1939_warsaw atk_tactic=overwhelming_fire def_tactic=elastic_defense 30 7
//!   forward-command --headless province=9206 dirs=E,NE atk=1 def=2 18 3
//!
//! Any `--battle` scenario spec is accepted (province=/dirs=/atk=/def=/
//! tactic=/file=/map=synthetic), plus the headless-only `atk_tactic=` /
//! `def_tactic=` tactic cards for the two AI sides (`tactic=` doubles as
//! the defender card). A battle script (data/battles/*.json) runs on its
//! real province map with BOTH sides AI-driven; preset forces on any real
//! province give the generalization battery for the AI tuning: per-side
//! tactic cards, both sides AI-deployed, and a per-turn compact line with
//! each side's strength and city occupancy.
//!
//! Both sides are driven by `TacticalAi` planning against a fog-of-war view
//! of the enemy — the same limit the player has. Execution
//! (movement/contact/combat) stays omniscient, exactly like the in-game loop.
//!
//! Run-time rule: runs of ≤144 turns (one game day) may use the debug
//! binary; LONGER runs MUST use the release binary — debug compiles
//! workspace members at opt-level 0 and ~100-unit scripted battles crawl
//! there.

use std::collections::HashSet;

use tactical_ai::{AiAction, CombatTactic, TacticalAi};
use tactical_combat::{
    apply_oob_leaving, retreat_step_zoned, AttackOrder, AttackTarget, CombatEngine,
};
use tactical_core::flag::FlagState;
use tactical_core::fog::{FogOfWar, VisibilityState};
use tactical_core::grid::HexGrid;
use tactical_core::hex::HexCoord;
use tactical_core::movement::{advance_move_orders, refresh_move_order, MovementEvent};
use tactical_core::unit::{BattalionUnit, MoveOrder, Side, UnitState};
use tactical_core::{CombatParams, Terrain};

use crate::scenario::{MapChoice, Scenario};

/// `--headless` options (main.rs parses the CLI).
pub struct Opts {
    /// The scenario: battle script, real province + preset forces, or the
    /// built-in arena (default).
    pub scenario: Scenario,
    /// Attacker tactic card (default Blitz).
    pub atk_tactic: CombatTactic,
    /// Defender tactic card (default ElasticDefense).
    pub def_tactic: CombatTactic,
    pub turns: u32,
    pub seed: u64,
}

/// One command slice's AI driver: planner, its fog-of-war, its standing
/// attack orders, the divisions it commands (None = the whole side), and a
/// trace label. Per DESIGN §7.5 the player side of a script battle may
/// split into the player proxy + one driver per allied contingent — each
/// plans its own slice against the player's fog with the REST of the side
/// as passive friendlies (the interactive `plan_allied_nations` in
/// miniature).
struct SideDriver {
    side: Side,
    label: String,
    divisions: Option<Vec<String>>,
    ai: TacticalAi,
    fog: FogOfWar,
    orders: Vec<AttackOrder>,
    /// Pre-battle intel: the enemy deployment zone the side marches on when
    /// it sees no enemy (attacker → the defender's zone; the defender
    /// holds). Each unit aims at its nearest intel hex.
    objective: Option<Vec<HexCoord>>,
}

impl SideDriver {
    #[allow(clippy::too_many_arguments)]
    fn new(
        side: Side,
        label: String,
        divisions: Option<Vec<String>>,
        tactic: CombatTactic,
        seed: u64,
        width: usize,
        height: usize,
        reveal_turns: u32,
        objective: Option<Vec<HexCoord>>,
    ) -> Self {
        SideDriver {
            side,
            label,
            divisions,
            ai: TacticalAi::new(side, tactic, seed),
            fog: FogOfWar::new(width, height, reveal_turns),
            orders: Vec::new(),
            objective,
        }
    }

    /// The division slice this driver commands (None = every division).
    fn in_slice(&self, division: &str) -> bool {
        self.divisions
            .as_ref()
            .map(|d| d.iter().any(|n| n == division))
            .unwrap_or(true)
    }
}

pub fn run(opts: Opts) {
    // --- Build the battle: assemble the scenario (script / province /
    // synthetic — the same pipeline the interactive game uses).
    // A script file overrides the map, so "synthetic" means: no script AND
    // the arena map choice.
    let synthetic =
        opts.scenario.file.is_none() && matches!(opts.scenario.map, MapChoice::Arena);
    let settings = crate::settings::AppSettings::load();
    let spec = match crate::scenario::assemble(&opts.scenario, &settings) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("--headless: {e}");
            std::process::exit(2);
        }
    };
    run_battle(
        spec,
        synthetic,
        opts.atk_tactic,
        opts.def_tactic,
        opts.turns,
        opts.seed,
    );
}

/// Drive an ALREADY-ASSEMBLED battle AI-vs-AI: the live-save assembly path
/// (`--livebattle headless=1`) builds the spec from the real HOI4 save
/// instead of a script/preset scenario.
pub fn run_battle(
    spec: crate::scenario::BattleSpec,
    synthetic: bool,
    atk_tactic: CombatTactic,
    def_tactic: CombatTactic,
    turns: u32,
    seed: u64,
) {
    let grid = spec.grid;
    let zones = spec.zones.clone();
    let mut units = spec.units;
    // §6.11: the battle's flag board (city = 1 urban flag, field = 3
    // flags) — derived at assembly; `None` = annihilation-only.
    let mut flags = spec.flags;
    let player = spec.player_side;
    let enemy = player.opponent();
    let (player_zone, enemy_zone) = if player == Side::Attacker {
        (&zones.0, &zones.1)
    } else {
        (&zones.1, &zones.0)
    };
    // The player side's command slices — the player proxy owns every
    // player-side division NOT in an allied contingent; each contingent
    // owns its divisions (script battles with a `divisions:` block). Empty
    // allies = one whole-side driver (legacy behavior).
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
    // The full player-side division order shared by the sector partition
    // (player divisions first, then each nation's in order).
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
    let split = !spec.allies.is_empty();
    // Both sides AI-deploy. The interactive flow only AI-deploys the enemy
    // at Begin Battle (the player's deploy is a human unknown), so AI-vs-AI
    // tuning exercises BOTH deployment behaviors. The synthetic map
    // AI-deploys its attacker too — assemble() wipes the player side's
    // hand-placed positions into the OFFBOARD sentinel, so "keep the
    // hand-placed spawn" had nothing left to keep and the whole attacker
    // fought from off-map. With a command split the player side deploys
    // exactly like BeginBattle — the proxy's divisions first, then each
    // nation through its own tactic card, accumulating `pre_used` so later
    // slices spread instead of stacking (deploy_allied_nations semantics).
    // The ordered division list rides along so the sector partition gives
    // each division its own slice of the band; and the pre_used
    // accumulation filters on `u.undeployed` (BEFORE the flag is cleared —
    // the old `!u.undeployed` inverted test matched nothing while every
    // unit was still flagged, so Narvik-style multi-division battles
    // stacked divisions on the same hexes).
    if !split {
        tactical_ai::ai_deploy(
            &grid,
            &mut units,
            player_zone,
            enemy_zone,
            player,
            if player == Side::Attacker {
                atk_tactic
            } else {
                def_tactic
            },
            &HashSet::new(),
            None,
        );
    } else {
        let player_card = if player == Side::Attacker {
            atk_tactic
        } else {
            def_tactic
        };
        let mut pre_used: HashSet<(i32, i32)> = HashSet::new();
        let mut deploy_division =
            |pre_used: &mut HashSet<(i32, i32)>, div: &str, card: CombatTactic| {
                tactical_ai::ai_deploy_impl(
                    &grid,
                    &mut units,
                    player_zone,
                    enemy_zone,
                    player,
                    card,
                    pre_used,
                    Some(div),
                    Some(&all_divisions),
                    false,
                );
                for u in units.iter_mut().filter(|u| {
                    u.side == player
                        && u.division == div
                        && u.undeployed
                        && u.position != BattalionUnit::OFFBOARD
                }) {
                    u.undeployed = false;
                    pre_used.insert((u.position.q, u.position.r));
                }
            };
        for div in &player_divs {
            deploy_division(&mut pre_used, div, player_card);
        }
        let allies = spec.allies.clone();
        for contingent in &allies {
            for div in &contingent.divisions {
                deploy_division(&mut pre_used, div, contingent.tactic);
            }
        }
    }
    tactical_ai::ai_deploy(
        &grid,
        &mut units,
        enemy_zone,
        player_zone,
        enemy,
        if enemy == Side::Attacker {
            atk_tactic
        } else {
            def_tactic
        },
        &HashSet::new(),
        None,
    );
    // scenario::assemble flags the player's battalions undeployed (the
    // interactive flow places them from the OOB); headless has no such
    // phase — the AI just deployed everyone, so the flag must not linger.
    for u in &mut units {
        u.undeployed = false;
    }
    let location = spec.location;
    let params = CombatParams::default();
    let mut combat = CombatEngine::new(params.clone(), seed);
    // §6.8: retreats (incl. the fire-phase step-aside) score against the
    // defender zone's rim — the reachable province boundary.
    combat.set_retreat_zones(Some(zones.clone()));
    // Pre-battle intel: the enemy deployment zone. Each unit marches on its
    // NEAREST zone hex (the whole front advances, not one centroid point —
    // the Warsaw strip's centroid sits 25 hexes off the border). The old
    // synthetic Sedan zones (W+NW corner strips) were degenerate for the
    // nearest-intel model: its boundary pocket packed the whole AI line into
    // a deadlocked blob (replays with the centroid objective develop the
    // battle normally), so the demo keeps the centroid. The arena's straight
    // edge strips are not degenerate, but script battles (file=) take the
    // zone-intel arm anyway — this path only serves the bare default run, so
    // the centroid stays.
    let intel: Option<Vec<HexCoord>> = if synthetic {
        centroid(&zones.1).map(|h| vec![h])
    } else {
        Some(zones.1.clone())
    };
    let intel_len = intel.as_ref().map(|z| z.len()).unwrap_or(0);
    let urban_total = grid
        .iter_coords()
        .filter(|h| {
            grid.cell(*h)
                .map(|c| c.terrain == Terrain::Urban)
                .unwrap_or(false)
        })
        .count();
    let mut drivers: Vec<SideDriver> = Vec::new();
    if !split {
        drivers.push(SideDriver::new(
            player,
            tag(player).to_string(),
            None,
            if player == Side::Attacker {
                atk_tactic
            } else {
                def_tactic
            },
            seed ^ 0xA11A,
            grid.width,
            grid.height,
            params.fog_reveal_duration_turns,
            if player == Side::Attacker {
                intel.clone()
            } else {
                None
            },
        ));
    } else {
        // Player proxy first, then each nation in contingent order — the
        // same order plan_allied_nations plans them.
        drivers.push(SideDriver::new(
            player,
            tag(player).to_string(),
            Some(player_divs.clone()),
            if player == Side::Attacker {
                atk_tactic
            } else {
                def_tactic
            },
            seed ^ 0xA11A,
            grid.width,
            grid.height,
            params.fog_reveal_duration_turns,
            if player == Side::Attacker {
                intel.clone()
            } else {
                None
            },
        ));
        let allies = spec.allies.clone();
        for (i, contingent) in allies.iter().enumerate() {
            // Per-nation deterministic seed — the interactive FNV-1a mix
            // over the tag, with the driver index as an extra tie-breaker.
            let mut h = 0xcbf29ce484222325u64;
            for b in contingent.tag.bytes() {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
            let seed = seed.wrapping_mul(0x9E3779B97F4A7C15)
                ^ h
                ^ (i as u64).wrapping_mul(0x517CC1B727220A95);
            drivers.push(SideDriver::new(
                player,
                contingent.tag.clone(),
                Some(contingent.divisions.clone()),
                contingent.tactic,
                seed,
                grid.width,
                grid.height,
                params.fog_reveal_duration_turns,
                if player == Side::Attacker {
                    intel.clone()
                } else {
                    None
                },
            ));
        }
    }
    drivers.push(SideDriver::new(
        enemy,
        tag(enemy).to_string(),
        None,
        if enemy == Side::Attacker {
            atk_tactic
        } else {
            def_tactic
        },
        seed ^ 0xDE7A,
        grid.width,
        grid.height,
        params.fog_reveal_duration_turns,
        // The march-to-contact intel belongs to the ATTACKER side, not
        // the player label — with `--headless side=2` the real attacker
        // driver used to get None and never marched to contact.
        if enemy == Side::Attacker { intel } else { None },
    ));

    println!(
        "=== HEADLESS {location}: Attacker={atk_tactic:?} vs Defender={def_tactic:?}, seed {}, {} turns, map {}x{} ({} urban hexes) ===",
        seed, turns, grid.width, grid.height, urban_total
    );
    // Print the command split when the script carries one.
    if split {
        let divs = |ds: &[String]| {
            if ds.is_empty() {
                "no divisions".to_string()
            } else {
                format!("{} division(s)", ds.len())
            }
        };
        let allies = spec.allies.clone();
        println!(
            "  command split ({:?}): player {} | {}",
            player,
            divs(&player_divs),
            allies
                .iter()
                .map(|a| format!("{}: {}", a.tag, divs(&a.divisions)))
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    let bounds = |z: &[HexCoord]| {
        if z.is_empty() {
            return "empty".to_string();
        }
        let (q0, r0, q1, r1) = z.iter().fold(
            (i32::MAX, i32::MAX, i32::MIN, i32::MIN),
            |(q0, r0, q1, r1), h| (q0.min(h.q), r0.min(h.r), q1.max(h.q), r1.max(h.r)),
        );
        format!("q{q0}..{q1} r{r0}..{r1} ({} hexes)", z.len())
    };
    println!(
        "  zones: ATK {} | DEF {} | attacker blind intel {}",
        bounds(&zones.0),
        bounds(&zones.1),
        if intel_len > 1 {
            format!("DEF zone ({} hexes)", intel_len)
        } else {
            "DEF centroid".to_string()
        }
    );
    match &flags {
        Some(fs) => {
            println!(
                "  flags: {:?} battle, {} flag(s){} — capture at 2:1 in-zone, progress cap {}",
                fs.kind,
                fs.flags.len(),
                if fs.collapsed { " (COLLAPSED)" } else { "" },
                params.flag_progress_cap,
            );
            for f in &fs.flags {
                println!(
                    "    flag anchor ({},{}): zone {} hexes",
                    f.anchor.q,
                    f.anchor.r,
                    f.zone.len()
                );
            }
        }
        None => println!("  flags: none — annihilation-only battle"),
    }
    print_state_line("start", &units);

    for turn in 1..=turns {
        println!("-- Turn {turn} {}", "-".repeat(40));
        // The per-side half-turn order is preserved from the two-driver
        // era: every driver of the active side plans, then the whole side
        // marches + fires ONCE (the interactive plays the player side the
        // same way — allied plans register silently, then all units of the
        // side move and resolve their fire phase together).
        for side in [Side::Attacker, Side::Defender] {
            for d in drivers.iter_mut().filter(|d| d.side == side) {
                plan_driver(d, &grid, &mut units, turn, flags.as_ref());
                if declare(&units, turn, "plan").is_some() {
                    return;
                }
            }
            let mut side_drivers: Vec<&mut SideDriver> =
                drivers.iter_mut().filter(|d| d.side == side).collect();
            march_and_fire(
                &mut side_drivers,
                &grid,
                &mut units,
                &mut combat,
                &params,
                turn,
            );
            if declare(&units, turn, "fire phase").is_some() {
                return;
            }
        }
        full_turn_upkeep(&grid, &mut units, &mut combat, &params, &zones, &mut flags);
        // Compact tuning metric: strength totals + who holds the city (urban
        // hexes). The verbose state line below carries the full position
        // dump for small battles; scripted ones (~100 units) rely on this.
        let line = |s: Side| {
            let (mut org, mut str_, mut n) = (0.0, 0.0, 0);
            for u in units.iter().filter(|u| u.side == s) {
                if u.is_combat_effective() {
                    org += u.org;
                    str_ += u.strength;
                    n += 1;
                }
            }
            format!("{s:?} {n} up org {org:.0} str {str_:.0}")
        };
        let city = |s: Side| {
            units
                .iter()
                .filter(|u| {
                    u.side == s
                        && u.state != UnitState::Eliminated
                        && grid
                            .cell(u.position)
                            .map(|c| c.terrain == Terrain::Urban)
                            .unwrap_or(false)
                })
                .count()
        };
        // Front movement: the mean position of each side's in-battle units
        // and the closest defender-to-attacker gap. LeftBattle units sit on
        // the OFFBOARD sentinel (-1000,-1000) and must not skew the front
        // mean (same in-battle filter as `winner`).
        let mean_pos = |s: Side| -> String {
            let list: Vec<(i32, i32)> = units
                .iter()
                .filter(|u| {
                    u.side == s && (u.is_combat_effective() || u.state == UnitState::Retreating)
                })
                .map(|u| (u.position.q, u.position.r))
                .collect();
            if list.is_empty() {
                return "gone".to_string();
            }
            let n = list.len() as f32;
            let (sq, sr) = list.iter().fold((0i64, 0i64), |(aq, ar), &(q, r)| {
                (aq + q as i64, ar + r as i64)
            });
            format!("({:.0},{:.0})", sq as f32 / n, sr as f32 / n)
        };
        let gap = units
            .iter()
            .filter(|u| {
                u.side == Side::Attacker
                    && (u.is_combat_effective() || u.state == UnitState::Retreating)
            })
            .flat_map(|a| {
                units
                    .iter()
                    .filter(|d| {
                        d.side == Side::Defender
                            && (d.is_combat_effective() || d.state == UnitState::Retreating)
                    })
                    .map(move |d| a.position.distance(d.position))
            })
            .min()
            .unwrap_or(0);
        println!(
            "  [T{turn}] {} | {} | city ATK {}/{} DEF {}/{} | fronts ATK {} DEF {} gap {gap}",
            line(Side::Attacker),
            line(Side::Defender),
            city(Side::Attacker),
            urban_total,
            city(Side::Defender),
            urban_total,
            mean_pos(Side::Attacker),
            mean_pos(Side::Defender),
        );
        // §6.11: per-turn flag progress trace.
        if let Some(fs) = &flags {
            let progress: Vec<String> = fs
                .flags
                .iter()
                .map(|f| format!("{}/{}", f.progress, params.flag_progress_cap))
                .collect();
            println!(
                "  flags {}: [{}]{}",
                fs.kind.name(),
                progress.join(" "),
                if fs.collapsed {
                    " — DEFENDER SURRENDERED"
                } else {
                    ""
                },
            );
        }
        print_state_line(&format!("end of turn {turn}"), &units);
        // §6.11: flag capture ends the battle IMMEDIATELY — the attacker
        // wins this turn, no unit-based mop-up (pre-collapse combat routs
        // must not delay the declaration).
        if flags
            .as_ref()
            .map(|fs| fs.captured(&params))
            .unwrap_or(false)
        {
            println!("*** Attacker wins after turn {turn} upkeep (flag capture) ***");
            print_summary(&units);
            return;
        }
        if declare(&units, turn, "upkeep").is_some() {
            return;
        }
    }
    println!("-- {} turns reached {}", turns, "-".repeat(30));
    print_summary(&units);
}

/// One driver's plan: fog-limited view, slice-scoped own/passive split,
/// plan, order issue (standing move orders + fire orders + emplace/limber/
/// retreat states). With a command split, the rest of the driver's own
/// side becomes `passive_friendlies` — they block pathing and count in the
/// statistics but never receive actions (DESIGN §7.5).
fn plan_driver(
    d: &mut SideDriver,
    grid: &HexGrid,
    units: &mut Vec<BattalionUnit>,
    turn: u32,
    flags: Option<&FlagState>,
) {
    let side = d.side;
    d.fog.update(grid, units, side, turn);
    // The AI plans against its own fog view — hidden enemies do not exist
    // for planning purposes (execution below stays physical/omniscient).
    let view: Vec<BattalionUnit> = units
        .iter()
        .filter(|u| u.side == side || d.fog.state(u.position, turn) == VisibilityState::Visible)
        .cloned()
        .collect();
    let own: Vec<BattalionUnit> = view
        .iter()
        .filter(|u| u.side == side && d.in_slice(&u.division))
        .cloned()
        .collect();
    // Same-side units OUTSIDE the driver's slice — the player's units (for
    // allied planners) or the other nations' slices — join the
    // occupancy/statistics view as passive friendlies, never as command
    // targets. Whole-side drivers see an empty list (= None semantics).
    let passive: Vec<BattalionUnit> = view
        .iter()
        .filter(|u| u.side == side && !d.in_slice(&u.division))
        .cloned()
        .collect();
    let foe: Vec<BattalionUnit> = view.iter().filter(|u| u.side != side).cloned().collect();
    let visible_foes = foe.iter().filter(|u| u.is_combat_effective()).count();

    let actions = d
        .ai
        // The physical foe list feeds the fog-wall blind-assault probe
        // (dark-fog defenders that halt the march get stormed when
        // beaten/overwhelmed — e.g. the Ioannina script battle).
        .plan_turn_full(
            grid,
            &own,
            &foe,
            d.objective.as_deref(),
            flags,
            (!passive.is_empty()).then_some(passive.as_slice()),
            Some(units),
        );
    let count = |f: fn(&AiAction) -> bool| actions.iter().filter(|a| f(a)).count();
    println!(
        "[{}] sees {visible_foes} enemy — plan: {} move, {} assault, {} fire, {} emp/limb, {} hold, {} retreat",
        d.label,
        count(|a| matches!(a, AiAction::MoveUnit { .. })),
        count(|a| matches!(a, AiAction::Assault { .. })),
        count(|a| matches!(a, AiAction::FireSupport { .. })),
        count(|a| matches!(a, AiAction::Emplace { .. } | AiAction::Limber { .. })),
        count(|a| matches!(a, AiAction::Hold { .. })),
        count(|a| matches!(a, AiAction::Retreat { .. })),
    );

    for a in actions {
        match a {
            AiAction::MoveUnit { unit_id, path } => {
                if path.is_empty() {
                    continue;
                }
                if let Some(u) = units.iter_mut().find(|x| x.id == unit_id) {
                    if !u.is_emplaced {
                        let dest = *path.last().unwrap();
                        // Re-affirming the same destination keeps the standing
                        // order and its invested hours (hours are valid only
                        // while the first step matches). Re-issuing a fresh
                        // order every turn would reset progress and pin any
                        // unit slower than 1 hex/turn (medium tanks in
                        // forest, towed guns, river crossings).
                        match &mut u.move_order {
                            Some(o) if o.path.last() == Some(&dest) => {
                                // A same-destination re-affirm keeps the
                                // WHOLE standing order (path + invested
                                // hours). Adopting the planner's freshly
                                // recomputed path here reset hours whenever
                                // the first step flapped, pinning any unit
                                // slower than 1 hex/turn.
                                // refresh_move_order already heals the
                                // standing path each side-turn under
                                // anti-oscillation rules.
                            }
                            _ => {
                                println!(
                                    "  {} {} -> move ({},{})",
                                    d.label, u.name, dest.q, dest.r
                                );
                                u.move_order = Some(MoveOrder { path, hours: 0.0 });
                            }
                        }
                    }
                }
            }
            AiAction::Emplace { unit_id } => {
                if let Some(u) = units.iter_mut().find(|x| x.id == unit_id) {
                    if !u.acted && u.requires_emplacement() && !u.is_emplaced {
                        u.is_emplaced = true;
                        u.acted = true;
                        u.move_order = None;
                        println!("  {} {} emplaces", d.label, u.name);
                    }
                }
            }
            AiAction::Limber { unit_id } => {
                if let Some(u) = units.iter_mut().find(|x| x.id == unit_id) {
                    if !u.acted && u.is_emplaced {
                        u.is_emplaced = false;
                        u.acted = true;
                        println!("  {} {} limbers", d.label, u.name);
                    }
                }
            }
            AiAction::Assault {
                attacker_id,
                target_id,
            } => {
                if shocked(units, attacker_id) {
                    continue;
                }
                d.orders.retain(|o| o.attacker != attacker_id);
                d.orders.push(AttackOrder {
                    attacker: attacker_id,
                    target: AttackTarget::Assault(target_id),
                });
                if let Some(u) = units.iter_mut().find(|x| x.id == attacker_id) {
                    u.move_order = None;
                    // Attacking breaks the hunker — mirrors game.rs.
                    u.is_holding = false;
                }
                println!(
                    "  {} {} -> assault {} (registered)",
                    d.label,
                    name(units, attacker_id),
                    name(units, target_id)
                );
            }
            AiAction::FireSupport {
                attacker_id,
                target_hex,
            } => {
                if shocked(units, attacker_id) {
                    continue;
                }
                // Mirrors the interactive fire-mission rule: precise = a
                // combat-effective enemy stands at the aim hex AND this
                // side's fog currently sees it (the AI equivalent of
                // right-clicking a visible enemy); rockets are always area
                // fire. Direct-fire guns (AT/AA) are always precision and
                // never register without a combat-effective enemy exactly
                // on the aim hex (no zone saturation / self-splash).
                let direct_gun = units
                    .iter()
                    .find(|u| u.id == attacker_id)
                    .map(|u| u.is_direct_gun())
                    .unwrap_or(false);
                let enemy_at_hex = units.iter().any(|u| {
                    u.side != side && u.is_combat_effective() && u.position == target_hex
                });
                if direct_gun && !enemy_at_hex {
                    continue;
                }
                let hex_visible = d.fog.state(target_hex, turn) == VisibilityState::Visible;
                let is_rocket = units
                    .iter()
                    .find(|u| u.id == attacker_id)
                    .map(|u| u.is_rocket())
                    .unwrap_or(false);
                let precise = if direct_gun {
                    true
                } else {
                    !is_rocket && hex_visible && enemy_at_hex
                };
                d.orders.retain(|o| o.attacker != attacker_id);
                d.orders.push(AttackOrder {
                    attacker: attacker_id,
                    target: AttackTarget::FireMission {
                        hex: target_hex,
                        precise,
                    },
                });
                if let Some(u) = units.iter_mut().find(|x| x.id == attacker_id) {
                    u.move_order = None;
                }
                println!(
                    "  {} {} -> fire mission ({},{}){} (registered)",
                    d.label,
                    name(units, attacker_id),
                    target_hex.q,
                    target_hex.r,
                    if precise { " precise" } else { " area" }
                );
            }
            AiAction::Hold { unit_id } => {
                // Mirrors the interactive game.rs handling: "no action this
                // turn" doubles as the Hold stance for AI units — an idle
                // unit hunkers (+hold_defense_bonus). The old player-only
                // rationale is gone (can_assault no longer reads the stance;
                // movement drops it on the first step; assault/retreat clear
                // it). Still traced: a silently skipped action makes
                // lock-ups undebuggable.
                if let Some(u) = units.iter_mut().find(|x| x.id == unit_id) {
                    if u.is_combat_effective() {
                        u.is_holding = true;
                    }
                }
                println!("  {} {} holds position", d.label, name(units, unit_id));
            }
            AiAction::Retreat { unit_id } => {
                if let Some(u) = units.iter_mut().find(|x| x.id == unit_id) {
                    u.state = UnitState::Retreating;
                    u.is_holding = false;
                    println!("  {} {} retreats!", d.label, u.name);
                }
            }
            AiAction::EndTurn => break,
        }
    }
}

/// The side's march + unified fire phase, ONCE per side per turn: every
/// unit of the side advances its standing order (routes refreshed against
/// the first driver's fog view, mirroring the interactive execute_movement
/// handling), then the side's fire orders (all drivers merged, in plan
/// order) resolve together. Trace lines carry the owning driver's label.
fn march_and_fire(
    drivers: &mut [&mut SideDriver],
    grid: &HexGrid,
    units: &mut Vec<BattalionUnit>,
    combat: &mut CombatEngine,
    params: &CombatParams,
    turn: u32,
) {
    let side = drivers[0].side;
    let label_of = |drivers: &[&mut SideDriver], units: &[BattalionUnit], id: usize| -> String {
        units
            .iter()
            .find(|u| u.id == id)
            .and_then(|u| drivers.iter().find(|d| d.in_slice(&u.division)))
            .map(|d| d.label.clone())
            .unwrap_or_else(|| tag(side).to_string())
    };
    let view: Vec<BattalionUnit> = units
        .iter()
        .filter(|u| {
            u.side == side || drivers[0].fog.state(u.position, turn) == VisibilityState::Visible
        })
        .cloned()
        .collect();
    for u in units
        .iter_mut()
        .filter(|u| u.side == side && u.is_combat_effective() && u.move_order.is_some())
    {
        refresh_move_order(grid, u, &view, params);
    }
    let events = advance_move_orders(grid, units, side, params);
    for ev in &events {
        match *ev {
            MovementEvent::Arrived { unit_id } => {
                println!(
                    "  {} {} arrives ({},{})",
                    label_of(drivers, units, unit_id),
                    name(units, unit_id),
                    pos(units, unit_id).q,
                    pos(units, unit_id).r
                )
            }
            MovementEvent::MadeContact { unit_id } => {
                println!(
                    "  {} {} makes contact — halts",
                    label_of(drivers, units, unit_id),
                    name(units, unit_id)
                )
            }
            MovementEvent::Intercepted { unit_id, enemy_id } => {
                println!(
                    "  {} {} intercepted by {} — both halt",
                    label_of(drivers, units, unit_id),
                    name(units, unit_id),
                    name(units, enemy_id)
                )
            }
            MovementEvent::Blocked {
                unit_id,
                blocker_id,
            } => {
                println!(
                    "  {} {} blocked by {} — waits",
                    label_of(drivers, units, unit_id),
                    name(units, unit_id),
                    name(units, blocker_id)
                )
            }
            MovementEvent::Advanced { .. } | MovementEvent::Progress { .. } => {}
        }
    }

    // Unified fire phase: the side's orders resolve together — driver
    // order preserved for determinism.
    let mut orders: Vec<AttackOrder> = Vec::new();
    for d in drivers.iter_mut() {
        orders.append(&mut d.orders);
    }
    if !orders.is_empty() {
        let results = combat.resolve_fire_phase(grid, units, &orders);
        for r in &results {
            let owner = label_of(drivers, units, r.attacker_id);
            if r.target_lost {
                println!("  {} {}: target lost", owner, name(units, r.attacker_id));
                continue;
            }
            println!(
                "  {} {} vs {}: -{:.1} org / -{:.1} str{}{}{}{}",
                owner,
                name(units, r.attacker_id),
                name(units, r.defender_id),
                r.org_damage_dealt,
                r.str_damage_dealt,
                if r.shocked_defender { " SHOCKED" } else { "" },
                if r.defender_broken { " BROKEN" } else { "" },
                if r.surrendered { " SURRENDERS" } else { "" },
                if r.eliminated { " ANNIHILATED" } else { "" },
            );
            if r.org_damage_taken > 0.0 || r.str_damage_taken > 0.0 {
                println!(
                    "    - counter: {} -{:.1} org / -{:.1} str{}",
                    name(units, r.attacker_id),
                    r.org_damage_taken,
                    r.str_damage_taken,
                    if r.shocked_attacker { " SHOCKED" } else { "" },
                );
            }
        }
    }
    // §6.13: report HQ annihilations (division command collapse).
    for ev in combat.take_hq_events() {
        println!(
            "  {} HQ destroyed — division command collapses!",
            ev.division
        );
    }
    // Shock expiry at the turn end: shocks from this turn-end's fire phase
    // persist; older ones wear off.
    combat.expire_shocks(units);
}

/// End of the full turn: attachment regen, command (HQ) org regen, retreat
/// steps, §6.14 out-of-bounds leaving, encirclement attrition, flag
/// progress + collapse trigger (§6.11), per-turn flag resets (mirrors
/// finish_full_turn).
#[allow(clippy::too_many_arguments)]
fn full_turn_upkeep(
    grid: &HexGrid,
    units: &mut Vec<BattalionUnit>,
    combat: &mut CombatEngine,
    params: &CombatParams,
    zones: &(Vec<HexCoord>, Vec<HexCoord>),
    flags: &mut Option<FlagState>,
) {
    // Normalization fallback first — an Active unit with org/strength ≤ 0
    // (transient or forged) is untargetable yet counts for victory;
    // normalize before anything else reads the roster.
    for u in units.iter_mut() {
        u.normalize_broken_state();
    }
    for u in units.iter_mut() {
        let regen = u.support_str_regen();
        if regen > 0.0 && u.is_combat_effective() {
            u.strength = (u.strength + regen).min(u.max_strength);
        }
    }
    // §6.13: in-command battalions regenerate org near their HQ.
    combat.apply_command_regen(units);
    let retreating: Vec<usize> = units
        .iter()
        .filter(|u| u.state == UnitState::Retreating)
        .map(|u| u.id)
        .collect();
    // §6.8: retreats score against the defender zone's eastern rim (the
    // reachable province boundary) — see retreat_step_zoned.
    for id in retreating {
        retreat_step_zoned(
            grid,
            units,
            id,
            true,
            Some((zones.0.as_slice(), zones.1.as_slice())),
        );
    }
    // §6.14: a unit lingering oob_leaving_turns full turns in the
    // out-of-bounds ring leaves the battle (org 0, strength frozen).
    for d in apply_oob_leaving(grid, units, params) {
        println!(
            "  *** {} ({:?}) lingered out of bounds and LEFT THE BATTLE (org 0, strength frozen)",
            d.name, d.side
        );
    }
    combat.apply_encirclement_attrition(grid, units);
    // §6.11: end-of-turn flag tick — control ratio → progress, full
    // capture → the ATTACKER WINS immediately: the defender's org is zeroed
    // (strength untouched), no rout flow — the explicit `captured()` check
    // after this upkeep closes the battle at once (the org-zeroed defenders
    // stay Active and still count for victory — the predicate is unified
    // on state, not org).
    if let Some(fs) = flags {
        let tick = fs.tick(grid, units, params);
        if tick.collapse_fired {
            println!(
                "*** FLAG CAPTURE — the ATTACKER WINS! The defender's org is zeroed ({} battalions, strength intact) — no mop-up, the strategic layer resolves the retreat ***",
                units
                    .iter()
                    .filter(|u| u.side == Side::Defender && u.org <= 0.0)
                    .count()
            );
        }
    }
    for u in units.iter_mut() {
        u.refresh_turn();
    }
}

/// A side is beaten when nothing counting for victory remains — the
/// predicate is single-sourced in tactical-core
/// (`BattalionUnit::counts_for_victory`): Active or Retreating, non-HQ.
/// This fn previously mirrored tactical-sync `check_victory` with
/// `is_combat_effective() || Retreating`, which diverged on an Active unit
/// with org/strength ≤ 0 (beaten here, alive in the interactive battle).
/// Mutual annihilation is a terminal draw, not an attacker win (the
/// interactive battle ends the same way).
fn winner(units: &[BattalionUnit]) -> tactical_sync::VictoryOutcome {
    let alive = |s: Side| units.iter().any(|u| u.side == s && u.counts_for_victory());
    match (alive(Side::Attacker), alive(Side::Defender)) {
        (true, true) => tactical_sync::VictoryOutcome::Undecided,
        (true, false) => tactical_sync::VictoryOutcome::Winner(Side::Attacker),
        (false, true) => tactical_sync::VictoryOutcome::Winner(Side::Defender),
        (false, false) => tactical_sync::VictoryOutcome::Draw,
    }
}

/// Terminal declaration when the battle has ended — prints the result and
/// summary, returns `Some(())` to signal "stop the run".
fn declare(units: &[BattalionUnit], turn: u32, stage: &str) -> Option<()> {
    match winner(units) {
        tactical_sync::VictoryOutcome::Undecided => None,
        tactical_sync::VictoryOutcome::Winner(w) => {
            println!("*** {w:?} wins after turn {turn} {stage} ***");
            print_summary(units);
            Some(())
        }
        tactical_sync::VictoryOutcome::Draw => {
            println!("*** draw — mutual annihilation after turn {turn} {stage} ***");
            print_summary(units);
            Some(())
        }
    }
}

fn print_state_line(prefix: &str, units: &[BattalionUnit]) {
    let line = |s: Side| {
        let (mut org, mut str_, mut n, mut ret) = (0.0, 0.0, 0, 0);
        for u in units.iter().filter(|u| u.side == s) {
            if u.is_combat_effective() {
                org += u.org;
                str_ += u.strength;
                n += 1;
            } else if u.state == UnitState::Retreating {
                ret += 1;
            }
        }
        format!("{s:?} {n} up ({ret} retreating) org {org:.0} str {str_:.0}")
    };
    println!(
        "  [{prefix}] {} | {}",
        line(Side::Attacker),
        line(Side::Defender)
    );
    // Compact positions: the behavior trace's map view. Scripted battles
    // (~100 units) skip the per-unit dump — the compact T-turn line above
    // carries their state; small demo battles keep the full trace.
    if units.len() <= 40 {
        for s in [Side::Attacker, Side::Defender] {
            let mut cells: Vec<String> = units
                .iter()
                .filter(|u| u.side == s && u.state != UnitState::Eliminated)
                .map(|u| format!("{}({},{})", u.name, u.position.q, u.position.r))
                .collect();
            cells.sort();
            println!("    {:?}: {}", s, cells.join(" "));
        }
    }
}

fn print_summary(units: &[BattalionUnit]) {
    println!("== summary ==");
    for u in units {
        println!(
            "  {:?} {:<7} {:?} at ({},{}) org {:.0}/{:.0} str {:.0}/{:.0}",
            u.side,
            u.name,
            u.state,
            u.position.q,
            u.position.r,
            u.org,
            u.max_org,
            u.strength,
            u.max_strength
        );
    }
}

fn tag(side: Side) -> &'static str {
    match side {
        Side::Attacker => "ATK",
        Side::Defender => "DEF",
    }
}

/// The zone hex nearest the zone's centroid (the march objective).
fn centroid(zone: &[HexCoord]) -> Option<HexCoord> {
    if zone.is_empty() {
        return None;
    }
    let (sq, sr) = zone.iter().fold((0i64, 0i64), |(aq, ar), h| {
        (aq + h.q as i64, ar + h.r as i64)
    });
    let n = zone.len() as f32;
    let (cq, cr) = (sq as f32 / n, sr as f32 / n);
    zone.iter().copied().min_by(|a, b| {
        let d2 = |h: &HexCoord| (h.q as f32 - cq).powi(2) + (h.r as f32 - cr).powi(2);
        d2(a)
            .partial_cmp(&d2(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn name(units: &[BattalionUnit], id: usize) -> String {
    units
        .iter()
        .find(|u| u.id == id)
        .map(|u| u.name.clone())
        .unwrap_or_else(|| format!("#{id}"))
}

fn pos(units: &[BattalionUnit], id: usize) -> HexCoord {
    units
        .iter()
        .find(|u| u.id == id)
        .map(|u| u.position)
        .unwrap_or(HexCoord::new(0, 0))
}

fn shocked(units: &[BattalionUnit], id: usize) -> bool {
    units
        .iter()
        .find(|u| u.id == id)
        .map(|u| u.shocked)
        .unwrap_or(true)
}
