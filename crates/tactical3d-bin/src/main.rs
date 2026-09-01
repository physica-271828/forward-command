// Release builds are GUI-subsystem: double-clicking the exe must NOT pop a
// console window (the old always-on console is why "other programs don't
// show a command line first"). Panics still land in crash.log via the hook
// below; logs from a terminal run still show (the process inherits the
// parent console). Debug builds keep the console so `cargo run` prints as
// usual.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! forward-command — main entry point.
//!
//! Modes:
//!   (no args)        — main menu (Start Live Listen / Debug Battle / Settings / Exit)
//!   --demo           — demo battle (built-in scenario, no HOI4 needed)
//!   --preview <what> — render model/terrain previews into previews/ (what: terrains|units|all)
//!   --battle k=v...  — forged tac_start straight into a battle (scenario::parse_cli)
//!   --livebattle k=v… — menu live-listen child: one real tac_start, with injection
//!   --live [--dry]   — full HOI4 listen loop in-process (CLI live mode)
//!   --debug          — interactive scenario builder (no running HOI4 needed)
//!   --headless [file=<script>] [atk_tactic=<tactic>] [def_tactic=<tactic>] [turns] [seed]
//!                      — no-window AI-vs-AI run with a full action trace
//!                      for AI evaluation/tuning. Without file= the built-in
//!                      arena scenario plays; with file= a battle script
//!                      (data/battles/*.json, e.g. 1939_warsaw) runs on its
//!                      real map with BOTH sides AI-driven, so AI-vs-AI
//!                      behavior can be tuned on scripted battles.
//!                      atk_tactic=/def_tactic= set the two sides' tactic
//!                      cards.
//!   --deploycheck file=<script> [atk_tactic=] [def_tactic=]
//!                      — agent tool: assemble + AI-deploy both sides, print
//!                      placement metrics + ASCII map (deploycheck_map.txt)
//!   --eguitest        — standalone egui/font/icon smoke window (dev tool)

mod app_icon;
mod autoplay;
mod battle;
mod debug;
mod demo;
mod deploycheck;
mod diag;
mod dirs;
mod headless;
mod live;
mod menu;
mod naming;
mod pickmap;
mod preview;
mod scenario;
mod script;
mod settings;
mod single;
mod snapshot;
mod splash;
mod theme;
mod tray;
mod window;

/// Apply `hoi4_dir=` / `saves_dir=` / `log_path=` CLI overrides onto loaded
/// settings (child processes inherit the menu's configured paths).
fn apply_path_overrides(settings: &mut settings::AppSettings, args: &[String]) {
    for arg in args {
        let Some((k, v)) = arg.split_once('=') else {
            continue;
        };
        // An empty value would WIPE the loaded setting — skip it.
        if v.is_empty() {
            continue;
        }
        match k {
            "hoi4_dir" => settings.hoi4_dir = v.to_string(),
            "saves_dir" => settings.saves_dir = v.to_string(),
            "log_path" => settings.log_path = v.to_string(),
            _ => {}
        }
    }
}

/// A panic in a GUI launch looks like a silent 闪退 — the console window
/// flashes and takes the message with it. Mirror every panic into
/// `crash.log` next to the exe (append, best-effort) so post-mortem
/// evidence survives, then defer to the default hook for the normal
/// stderr message.
fn install_crash_log_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(exe) = std::env::current_exe() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let thread = std::thread::current();
            let entry = format!(
                "[{secs}] panic on thread '{}': {info}\n",
                thread.name().unwrap_or("<unnamed>")
            );
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(exe.with_file_name("crash.log"))
            {
                let _ = f.write_all(entry.as_bytes());
            }
        }
        default_hook(info);
    }));
}

fn main() {
    // Adopt per-monitor DPI awareness BEFORE any window exists (winit sets
    // the same V2 context later, but by then the splash is already up):
    // on 150%-scaling screens the splash is otherwise created while the
    // process is still DPI-unaware and visibly SHRINKS + jumps off-center
    // the moment winit flips the process.
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    install_crash_log_hook();
    // Let battle child processes take the foreground later (the menu hides
    // to the system tray while a battle runs; without this grant Windows
    // blocks the child's SetForegroundWindow and the battle window opens
    // buried behind other apps).
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    let args: Vec<String> = std::env::args().collect();
    // Single-instance guard: only the RESIDENT listener modes take the
    // guard — the menu (no args) and the CLI `--live` loop. The menu's own
    // children (--battle/--livebattle/--headless/…) must NOT be blocked —
    // they run while the menu is alive. A second menu launch summons the
    // running instance's menu window (like a tray double-click) and exits
    // without opening a second listener; a second `--live` is a hard CLI
    // error (two instances used to tail game.log at once and toggle each
    // other's injection batches off — no snapshot, no ping, console
    // flicker only).
    let mode = args.get(1).map(|s| s.as_str());
    if matches!(mode, None | Some("--live"))
        && matches!(single::guard(), single::SingleInstance::Second)
    {
        if mode.is_none() {
            single::summon_and_wait();
            std::process::exit(0);
        }
        eprintln!(
            "forward-command: another instance is already running — \
                 refusing to start a second listener"
        );
        std::process::exit(1);
    }
    // GUI modes get the native loading splash (covers the cold-start gap
    // before any Bevy window exists); CLI modes (--live/--headless/--debug/
    // --preview/--eguitest) stay textual. The splash self-closes after the
    // app's first rendered frames via splash::auto_close.
    if matches!(
        args.get(1).map(String::as_str),
        None | Some("--demo") | Some("--battle") | Some("--livebattle")
    ) {
        splash::start();
    }
    match args.get(1).map(|s| s.as_str()) {
        Some("--preview") => {
            let what = args.get(2).map(|s| s.as_str()).unwrap_or("all");
            preview::run(what);
        }
        Some("--live") => {
            let dry = args.iter().any(|a| a == "--dry");
            live::run(dry);
        }
        Some("--eguitest") => {
            use bevy::prelude::*;
            use bevy::window::WindowLevel;
            use bevy_egui::{egui, EguiContexts, EguiPlugin};
            use tactical3d_render::icons::{init_icons_once, IconId, IconSet};
            App::new()
                .add_plugins(DefaultPlugins.set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "eguitest".into(),
                        resolution: (800.0_f32, 600.0_f32).into(),
                        window_level: WindowLevel::AlwaysOnTop,
                        ..default()
                    }),
                    ..default()
                }))
                .add_plugins(EguiPlugin)
                // Honor the settings.json UI language (DESIGN §15) in the
                // font/icon test window too.
                .insert_resource(tactical3d_render::locale::LocaleRes(
                    tactical_locale::Locale::load(settings::AppSettings::load().language()),
                ))
                .init_resource::<IconSet>()
                .add_systems(Update, init_icons_once)
                .add_systems(Update, |mut contexts: EguiContexts, icons: Res<IconSet>| {
                    // try_ctx_mut: first-frame egui init panics on ctx_mut.
                    let Some(ctx) = contexts.try_ctx_mut() else { return };
                    egui::Window::new("EGUI-TEST").show(ctx, |ui| {
                        ui.label("standalone egui works");
                        ui.separator();
                        // Icon smoke test: a few bare images, icon+text
                        // labels, and icon buttons (menu_button style).
                        ui.horizontal(|ui| {
                            for id in [IconId::Attack, IconId::Gear, IconId::Door, IconId::Listen] {
                                icons.icon(ui, id, 32.0);
                            }
                        });
                        icons.label_with_icon(ui, IconId::Trophy, "label_with_icon", 16.0);
                        ui.add(icons.button(Some(IconId::Fire), "icon button", 16.0));
                        ui.add_sized(
                            [260.0, 34.0],
                            icons.button(Some(IconId::Listen), egui::RichText::new("menu-style button").size(15.0), 16.0),
                        );
                    });
                })
                .add_systems(Update, |mut t: Local<f32>, time: Res<Time>, mut commands: Commands, mut exit: EventWriter<bevy::app::AppExit>| {
                    *t += time.delta_secs();
                    if *t > 5.0 && *t < 6.0 {
                        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("..").join("..").join("previews").join("eguitest.png");
                        commands
                            .spawn(bevy::render::view::window::screenshot::Screenshot::primary_window())
                            .observe(bevy::render::view::window::screenshot::save_to_disk(path));
                    }
                    if *t > 8.0 {
                        exit.send(bevy::app::AppExit::Success);
                    }
                })
                .run();
        }
        Some("--debug") => debug::run(),
        // Agent tool: study AI deployment on a battle script — assemble +
        // AI-deploy both sides, print placement metrics + a map.
        Some("--deploycheck") => {
            let kv = |key: &str| {
                let p = format!("{key}=");
                args[2..]
                    .iter()
                    .find_map(|a| a.strip_prefix(&p))
                    .map(|s| s.to_string())
            };
            let tactic = |t: Option<String>, dflt: tactical_ai::CombatTactic| {
                t.as_deref()
                    .map(tactical_ai::CombatTactic::from_str)
                    .unwrap_or(dflt)
            };
            let sc = match scenario::parse_cli(
                &args[2..]
                    .iter()
                    .filter(|a| !a.starts_with("atk_tactic=") && !a.starts_with("def_tactic="))
                    .cloned()
                    .collect::<Vec<String>>(),
            ) {
                Ok(sc) => sc,
                Err(e) => {
                    eprintln!("--deploycheck: {e}");
                    std::process::exit(2);
                }
            };
            deploycheck::run(deploycheck::Opts {
                scenario: sc,
                atk_tactic: tactic(kv("atk_tactic"), tactical_ai::CombatTactic::Blitz),
                def_tactic: tactic(kv("def_tactic"), tactical_ai::CombatTactic::ElasticDefense),
            });
        }
        Some("--battle") => {
            // Agent automation + menu Debug Battle child: forge a tac_start
            // from key=value args and run the battle directly (no menu/stdin;
            // see scenario::parse_cli). hoi4_dir=/saves_dir= override settings.
            let mut settings = settings::AppSettings::load();
            apply_path_overrides(&mut settings, &args[2..]);
            match scenario::parse_cli(&args[2..]).and_then(|sc| scenario::assemble(&sc, &settings))
            {
                // --autoplay: hands-free self-play through the production
                // command path (agent hang repro — see autoplay.rs).
                Ok(spec) if args.iter().any(|a| a == "--autoplay") => battle::run_autoplay(spec),
                Ok(spec) => battle::run(spec),
                // Non-zero exit so the menu's reaper surfaces stderr
                // instead of a bare "battle closed".
                Err(e) => {
                    eprintln!("--battle: {e}");
                    std::process::exit(2);
                }
            }
        }
        Some("--livebattle") => {
            // Menu live-listen child: one tac_start passed as key=value args
            // (province= dirs= tag= tactic= player_atk=), assembled the live
            // way and run WITH console injection (dry=1 forces injector
            // dry-run). tactic= is the mod's tac_enemy_tactic token (empty /
            // unknown → Default); player_atk=0 flips the player to Defender
            // (the mod ships 1 until a verified attack/defend trigger
            // exists).
            let mut settings = settings::AppSettings::load();
            apply_path_overrides(&mut settings, &args[2..]);
            let get = |key: &str| {
                args[2..].iter().find_map(|a| {
                    a.split_once('=')
                        .and_then(|(k, v)| (k == key).then(|| v.to_string()))
                })
            };
            let dry = args.iter().any(|a| a == "dry=1");
            let province = get("province").and_then(|v| v.parse::<u32>().ok());
            // tag may be omitted/empty: the live assembler then takes the
            // played country from the save's `player="TAG"` key.
            let tag = get("tag").unwrap_or_default();
            let dirs: Vec<String> = get("dirs")
                .map(|d| d.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();
            let tactic = tactical_ai::CombatTactic::from_str(&get("tactic").unwrap_or_default());
            let player_atk = get("player_atk").as_deref() != Some("0");
            match province {
                Some(p) => {
                    match scenario::assemble_live(&settings, p, &tag, &dirs, tactic, player_atk) {
                        Ok(spec) => {
                            // headless=1: AI-vs-AI the live-assembled battle
                            // instead of opening the window (same assembly,
                            // no GUI iteration). turns=/seed= ride along;
                            // def_tactic= overrides the save-resolved enemy
                            // card when given.
                            if args.iter().any(|a| a == "headless=1") {
                                let num = |key: &str, dflt: u64| {
                                    get(key).and_then(|v| v.parse::<u64>().ok()).unwrap_or(dflt)
                                };
                                let atk = get("atk_tactic")
                                    .map(|t| tactical_ai::CombatTactic::from_str(&t))
                                    .unwrap_or(tactical_ai::CombatTactic::Blitz);
                                let def = get("def_tactic")
                                    .map(|t| tactical_ai::CombatTactic::from_str(&t))
                                    .unwrap_or(spec.enemy_tactic);
                                headless::run_battle(
                                    spec,
                                    false,
                                    atk,
                                    def,
                                    num("turns", 144) as u32,
                                    num("seed", 42),
                                );
                            } else {
                                battle::run_live(spec, battle::LiveCtx { tag, dry })
                            }
                        }
                        Err(e) => {
                            eprintln!("--livebattle: {e}");
                            std::process::exit(2);
                        }
                    }
                }
                _ => {
                    eprintln!("--livebattle: need province=<id> (tag= optional, inferred from the save when omitted)");
                    std::process::exit(2);
                }
            }
        }
        Some("--headless") => {
            // Any `--battle` scenario spec works (province=/dirs=/atk=/def=/
            // tactic=/file=…, parsed by scenario::parse_cli) plus the two
            // headless-only tactic cards for the AI sides. Garbage args fail
            // loudly instead of silently taking defaults.
            let kv = |key: &str| {
                let p = format!("{key}=");
                args[2..]
                    .iter()
                    .find_map(|a| a.strip_prefix(&p))
                    .map(|s| s.to_string())
            };
            let tactic = |t: Option<String>, dflt: tactical_ai::CombatTactic, what: &str| {
                let Some(s) = t else { return dflt };
                let parsed = tactical_ai::CombatTactic::from_str(&s);
                // from_str folds the 55 vanilla tokens AND silently defaults
                // on garbage — reject the garbage explicitly.
                let low = s.trim().to_ascii_lowercase();
                if parsed == tactical_ai::CombatTactic::Default
                    && !matches!(low.as_str(), "default" | "basic_attack" | "basic_defend")
                {
                    eprintln!("--headless: bad {what} '{s}' (unknown tactic token)");
                    std::process::exit(2);
                }
                parsed
            };
            // tactic= (1..9) is the --battle enemy-tactic index: in headless
            // it doubles as the DEFENDER card (the "enemy" of the attacker).
            let def_tactic = match kv("def_tactic") {
                Some(t) => tactic(
                    Some(t),
                    tactical_ai::CombatTactic::ElasticDefense,
                    "def_tactic",
                ),
                None => match kv("tactic") {
                    Some(i) => {
                        let idx = i
                            .parse::<usize>()
                            .map(|v| v.wrapping_sub(1))
                            .unwrap_or(usize::MAX);
                        scenario::TACTICS.get(idx).copied().unwrap_or_else(|| {
                            eprintln!("--headless: tactic= index '{i}' out of range");
                            std::process::exit(2);
                        })
                    }
                    None => tactical_ai::CombatTactic::ElasticDefense,
                },
            };
            // atk_tactic=/def_tactic= are headless-only — strip them before
            // parse_cli (file= exclusivity and unknown-key tolerance).
            let scenario_args: Vec<String> = args[2..]
                .iter()
                .filter(|a| !a.starts_with("atk_tactic=") && !a.starts_with("def_tactic="))
                .cloned()
                .collect();
            let sc = match scenario::parse_cli(&scenario_args) {
                Ok(sc) => sc,
                Err(e) => {
                    eprintln!("--headless: {e}");
                    std::process::exit(2);
                }
            };
            let positional: Vec<&String> = args[2..].iter().filter(|a| !a.contains('=')).collect();
            let parse_or_die = |i: usize, what: &str| -> Option<u64> {
                positional.get(i).map(|s| {
                    s.parse().unwrap_or_else(|_| {
                        eprintln!("--headless: bad {what} '{s}' (expected a number)");
                        std::process::exit(2);
                    })
                })
            };
            let turns = parse_or_die(0, "turns").unwrap_or(12) as u32;
            let seed = parse_or_die(1, "seed").unwrap_or(42);
            // Seed unification: the positional seed drives the COMBAT dice;
            // when no seed= k=v rides along, the same seed must also feed
            // the map/flag RNG — otherwise identical CLI seeds produced
            // different RELIEF across modes (`--headless 7` vs
            // `--battle seed=7`).
            let mut sc = sc;
            if sc.seed.is_none() {
                sc.seed = Some(seed);
            }
            headless::run(headless::Opts {
                scenario: sc,
                atk_tactic: tactic(
                    kv("atk_tactic"),
                    tactical_ai::CombatTactic::Blitz,
                    "atk_tactic",
                ),
                def_tactic,
                turns,
                seed,
            });
        }
        Some("--demo") => demo::run(),
        // No args = main menu (was: demo battle). An unknown mode is a CLI
        // error — the old `_` fallback silently launched a resident GUI menu
        // from a typo'd flag and blocked.
        None => menu::menu_loop(),
        Some(other) => {
            eprintln!("forward-command: unknown mode '{other}' (try no args, --demo, --battle, --live, --headless, --preview, --deploycheck, --debug)");
            std::process::exit(2);
        }
    }
}
