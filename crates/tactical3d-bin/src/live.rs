//! Live mode: listen HOI4's game.log for tac_start, build the battle from the
//! real save + province map, and inject results back via the console (§3).
//! The battle itself is assembled by scenario::assemble_live and run by
//! battle::run_live — this module is only the console listen loop (`--live`;
//! the main-menu Live toggle runs the same loop inside the menu app).

use std::thread::sleep;
use std::time::Duration;

use tactical_listen::{LogListener, LogMessage};

/// Listen → battle → inject loop. `dry` forces injector dry-run (no SendInput).
/// Paths come from settings.json with auto-detect fallback.
pub fn run(dry: bool) {
    println!("=== Forward Command 3D — LIVE mode ===");
    let settings = crate::settings::AppSettings::load();
    let Some(log_path) = settings
        .log_path()
        .or_else(tactical_listen::detect_log_path)
    else {
        eprintln!("error: could not locate HOI4 game.log (is HOI4 installed?)");
        std::process::exit(2);
    };
    if settings.hoi4_dir().is_none() && tactical_map::detect_hoi4_dir().is_none() {
        eprintln!("error: could not locate the HOI4 installation directory");
        std::process::exit(2);
    }
    println!("game.log : {}", log_path.display());
    if dry {
        println!("injector : DRY RUN (batch files only, no SendInput)");
    }

    let mut listener = match LogListener::start_at_end(log_path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error starting log listener: {e}");
            std::process::exit(2);
        }
    };

    println!("\nWaiting for tac_start (fire the tactical decision in HOI4)…\n");
    loop {
        let msgs = listener.poll();
        let mut consumed = vec![false; msgs.len()];
        for i in 0..msgs.len() {
            if consumed[i] {
                continue;
            }
            match &msgs[i] {
                LogMessage::TacStart {
                    province,
                    tag,
                    attack_dirs,
                    is_player_attacker,
                    ts,
                    ..
                } => {
                    println!("[{ts}] tac_start: province={province} tag={tag} dirs={attack_dirs:?} player_atk={is_player_attacker}");
                    // The mod emits tac_enemy_tactic in the same tick right
                    // after tac_start — the same log flush usually lands it
                    // in THIS poll batch, so look ahead first; only
                    // grace-poll the file when it is not here.
                    let mut tactic = None;
                    for (j, later) in msgs.iter().enumerate().skip(i + 1) {
                        if let LogMessage::TacEnemyTactic { enemy_tactic, .. } = later {
                            println!("enemy tactic reported: {enemy_tactic}");
                            tactic = Some(tactical_ai::CombatTactic::from_str(enemy_tactic));
                            consumed[j] = true;
                            break;
                        }
                    }
                    let enemy_tactic =
                        tactic.unwrap_or_else(|| collect_enemy_tactic(&mut listener));
                    // The state-target entry decision marks the picked state
                    // (tac_pick=1) — resolve the real contested province from
                    // a fresh snapshot; NoPick = legacy largest-battle
                    // inference; QuietNoBattle = report, don't launch.
                    let resolved_province = {
                        let hoi4 = settings.hoi4_dir().or_else(tactical_map::detect_hoi4_dir);
                        match (hoi4, settings.saves_dir()) {
                            (Some(h), Some(s)) => {
                                let p2s = crate::snapshot::load_p2s(&h);
                                match crate::snapshot::resolve_picked_province(
                                    &tactical_inject::Injector::new(),
                                    &s,
                                    &p2s,
                                    dry,
                                ) {
                                    Ok(crate::snapshot::PickResolution::Province(p)) => {
                                        println!("picked battle resolved: province={p}");
                                        Some(p)
                                    }
                                    Ok(crate::snapshot::PickResolution::NoPick) => Some(*province),
                                    Ok(crate::snapshot::PickResolution::QuietNoBattle {
                                        state,
                                    }) => {
                                        eprintln!("the picked state {state} holds no running battle — not launching");
                                        // Notify in game + reset every map
                                        // decision (mod event tac_quiet.1 =
                                        // tac_abort payload).
                                        if let Err(e) = tactical_inject::Injector::new()
                                            .inject_commands(
                                                &[
                                                    format!("event tac_quiet.1 {tag}"),
                                                    // Force the decisions-UI
                                                    // refresh so the map icons
                                                    // hide now.
                                                    "reloadinterface".to_string(),
                                                ],
                                                None,
                                                dry,
                                            )
                                        {
                                            eprintln!("quiet-notify injection failed: {e}");
                                        }
                                        None
                                    }
                                    // Several battles in the picked state —
                                    // the player picks from a list.
                                    Ok(crate::snapshot::PickResolution::Multiple {
                                        battles,
                                        ..
                                    }) => {
                                        let names = crate::snapshot::load_vp_names(
                                            &h,
                                            settings.language()
                                                == tactical_locale::Language::SimpChinese,
                                        );
                                        prompt_battle_choice(&battles, &names)
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "pick resolution failed ({e}) — legacy inference"
                                        );
                                        Some(*province)
                                    }
                                }
                            }
                            _ => Some(*province),
                        }
                    };
                    let Some(resolved_province) = resolved_province else {
                        continue;
                    };
                    match crate::scenario::assemble_live(
                        &settings,
                        resolved_province,
                        tag,
                        attack_dirs,
                        enemy_tactic,
                        *is_player_attacker,
                    ) {
                        Ok(spec) => {
                            crate::battle::run_live(
                                spec,
                                crate::battle::LiveCtx {
                                    tag: tag.clone(),
                                    dry,
                                },
                            );
                            // Drain whatever the mod logged WHILE the battle
                            // window was open: a tac_start fired during the
                            // fight would otherwise be picked up by the next
                            // poll() and instantly replay a stale battle.
                            let _ = listener.poll();
                            println!("\nbattle window closed — waiting for next tac_start…\n");
                        }
                        Err(e) => eprintln!("failed to build battle: {e}"),
                    }
                }
                LogMessage::TacAbort { ts, tag } => {
                    println!("[{ts}] tac_abort from {tag} (the live battle window closes itself via its own abort watcher)");
                }
                LogMessage::TacEnemyTactic { enemy_tactic, .. } => {
                    println!("enemy tactic reported: {enemy_tactic}");
                }
                _ => {}
            }
        }
        sleep(Duration::from_millis(400));
    }
}

/// The mod emits `tac_enemy_tactic` in the same tick right after `tac_start`.
/// Grace-poll up to ~1 s so the battle assembles with the reported card;
/// other message types seen in the window are consumed (they only duplicate
/// the main loop's prints). Falls back to [`tactical_ai::CombatTactic::Default`].
fn collect_enemy_tactic(listener: &mut LogListener) -> tactical_ai::CombatTactic {
    for _ in 0..10 {
        for msg in listener.poll() {
            if let LogMessage::TacEnemyTactic { enemy_tactic, .. } = msg {
                println!("enemy tactic reported: {enemy_tactic}");
                return tactical_ai::CombatTactic::from_str(&enemy_tactic);
            }
        }
        sleep(Duration::from_millis(100));
    }
    tactical_ai::CombatTactic::Default
}

/// Multi-battle picker (CLI arm of the state-target entry flow): the picked
/// state holds several player battles — print a numbered list (VP name when
/// the contested province is a victory point, tags, division counts) and read
/// the choice from stdin. Empty input / EOF cancels: nothing launches (same
/// "no silent fallback" behavior as the quiet-state path).
fn prompt_battle_choice(
    battles: &[crate::snapshot::BattleChoice],
    names: &std::collections::HashMap<u32, String>,
) -> Option<u32> {
    println!("the picked state holds {} battles:", battles.len());
    for (i, b) in battles.iter().enumerate() {
        let label = match names.get(&b.province) {
            Some(name) => format!("{name} (province {})", b.province),
            None => format!("province {}", b.province),
        };
        println!(
            "  [{}] {} — {} {} div vs {} {} div",
            i + 1,
            label,
            b.attacker_tags.join("+"),
            b.attacker_units,
            b.defender_tags.join("+"),
            b.defender_units
        );
    }
    loop {
        print!("pick 1-{} (empty to cancel): ", battles.len());
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            println!("cancelled — not launching");
            return None;
        }
        match line.parse::<usize>() {
            Ok(n) if (1..=battles.len()).contains(&n) => return Some(battles[n - 1].province),
            _ => println!("invalid choice: {line}"),
        }
    }
}
