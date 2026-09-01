//! Unified battle window: one app runner for every scenario
//! source — menu Debug Battle form, `--battle` CLI, the stdin debug builder,
//! and the live listen (menu toggle or `--live`). Returns when the battle
//! window closes so the caller can drop back to the main menu / listen loop.

use bevy::prelude::*;
use tactical3d_render::game::{
    BattleSnapshot, Checkpoints, GameController, InjectionRequest, PendingInjection,
};
use tactical3d_render::icons::IconId;
use tactical3d_render::locale::LocaleRes;
use tactical3d_render::state::{
    DesyncAction, DesyncAlert, FlashNotice, NoticeBar, SyncStall, SyncStallAction, TacticalState,
};
use tactical3d_render::TacticalGamePlugin;
use tactical_inject::Injector;
use tactical_locale::Locale;

use crate::scenario::BattleSpec;
use crate::settings::AppSettings;

/// Live-injection context: sync results back into HOI4 (§3.2).
pub struct LiveCtx {
    /// Player country tag (injection batch `scope`, DESIGN.md §3.2).
    pub tag: String,
    /// Dry-run: write batch files only, no SendInput keystrokes.
    pub dry: bool,
}

/// Standalone battle (debug form / CLI / demo-style): no HOI4 injection.
pub fn run(spec: BattleSpec) {
    run_inner(spec, None, false);
}

/// Live battle: console injection of sync results back into HOI4.
pub fn run_live(spec: BattleSpec, ctx: LiveCtx) {
    run_inner(spec, Some(ctx), false);
}

/// Standalone battle with the hands-free autoplay driver (agent automation
/// for in-window hang repros — see autoplay.rs).
pub fn run_autoplay(spec: BattleSpec) {
    run_inner(spec, None, true);
}

/// Run the battle window for an assembled scenario. Blocks until the window
/// closes (Esc menu → Exit Game, battle end, or OS close), then returns.
fn run_inner(spec: BattleSpec, live: Option<LiveCtx>, autoplay: bool) {
    // UI language from settings.json (DESIGN §15) — battle children are
    // separate processes, so each loads the setting itself at startup.
    let settings = AppSettings::load();
    let loc = Locale::load(settings.language());
    let title = loc.trf("window.title.battle", &[("location", &spec.location)]);
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title,
            resolution: (1440.0_f32, 900.0_f32).into(),
            // Fifo = true vsync (see demo.rs): even frame pacing fixes the
            // orbit judder from unsynced ~190 fps presentation.
            present_mode: bevy::window::PresentMode::Fifo,
            // Center explicitly — OS cascade placement drifts down-right.
            position: bevy::window::WindowPosition::Centered(
                bevy::window::MonitorSelection::Primary,
            ),
            ..default()
        }),
        ..default()
    }));
    app.add_plugins(TacticalGamePlugin);
    // The game plugin registers an English LocaleRes default; insert-after-
    // init wins (locale.rs).
    app.insert_resource(LocaleRes(loc));
    crate::window::start_maximized(&mut app);
    // Take the foreground once the OS window exists: a battle child
    // spawned from live flow would otherwise open buried behind HOI4.
    crate::window::bring_to_foreground(&mut app);
    // Render quality + idle frame-saver from settings.json.
    crate::window::apply_render_quality(&mut app, &settings);
    // Render-resolution scale (offscreen + upscale when < 100%).
    crate::window::apply_render_scale(&mut app, settings.render_scale_pct());
    if settings.low_power {
        crate::window::apply_low_power(&mut app);
    }
    // Esc menu → Settings — the live render/performance knobs
    // (hot-applied by the render crate, persisted here).
    crate::window::init_battle_settings(&mut app, &settings);

    let mut game = GameController::new(
        spec.player_side,
        spec.enemy_tactic,
        // Deterministic default 7 for --battle reproducibility; live
        // assembly passes a varying seed (M6).
        spec.seed.unwrap_or(7),
    );
    game.location = spec.location.clone();
    if let Some(p) = spec.province {
        game.province = p;
    }
    game.template = spec.template.clone();
    // §7.5: the script's allied AI contingents + division→tag
    // registry (empty outside script battles — the player commands her
    // whole side). Checkpoint snapshots carry these, so Restart keeps them.
    game.allies = spec.allies.clone();
    // Per-country base-plate colors — every tag the battle's
    // division→tag table fields (script battles; empty = side colors only).
    let tags: std::collections::HashSet<String> = spec
        .division_tags
        .values()
        .chain(std::iter::once(&spec.atk_tag))
        .chain(std::iter::once(&spec.def_tag))
        .cloned()
        .collect();
    app.insert_resource(crate::theme::tag_colors(&tags));
    game.division_tags = spec.division_tags;
    // §6.11: attach the battle's flag board (derived at
    // assembly) — checkpoint snapshots carry it, so a Restart/Rollback
    // keeps the capture progress.
    game.session.set_flags(spec.flags);
    // §6.8: the fire-phase "step aside" routs score against
    // the defender zone's rim like every other retreat.
    game.combat.set_retreat_zones(Some(spec.zones.clone()));
    // Invalid-phase errors here mean a session-state bug — log, don't
    // silently swallow with .ok().
    if let Err(e) = game.session.start_launching() {
        warn!("session start_launching rejected: {e}");
    }
    if let Err(e) = game.session.start_deployment() {
        warn!("session start_deployment rejected: {e}");
    }
    // Tag the session BEFORE the battle-start checkpoint: the checkpoint is
    // the Restart target, and a restarted live battle must keep the real
    // country tag for console injection (was: assigned after the snapshot,
    // silently reverting to the default tag on Restart).
    if let Some(ctx) = &live {
        // The mod can no longer print the tag (interpolation
        // limits), so ctx.tag may be EMPTY — fall back to the tag the live
        // assembly resolved from the save's `player=` key, carried on the
        // spec's side tags (the player's country = the side she commands).
        game.session.country_tag = if ctx.tag.is_empty() {
            match spec.player_side {
                tactical_core::Side::Attacker => spec.atk_tag.clone(),
                tactical_core::Side::Defender => spec.def_tag.clone(),
            }
        } else {
            ctx.tag.clone()
        };
    }
    // HOI4 battle context + damage writeback mode for the
    // sync/end batches (None for non-live battles → no damage
    // lines, and no injector is armed anyway).
    game.session.battle_ctx = spec.battle_ctx.clone();
    game.session.writeback_mode = spec.writeback_mode;
    // §8.2: the mid-battle HOI4 division roster (live
    // assemblies; empty elsewhere → every roster method a no-op).
    game.session.roster = spec.roster.clone();
    // Live battles carry the save's in-game start datetime — the
    // battle clock displays absolute game time (None = elapsed-only clock).
    game.session.start_datetime = spec.start_datetime;
    // The desync guard's expected-receipt baseline is the battle's
    // authoritative clock hour — the save's start datetime —
    // never a fresh probe (a drifted clock would validate its own drift).
    game.session.last_receipt_prefix = spec
        .start_datetime
        .map(tactical_sync::desync::format_game_prefix);
    let state = TacticalState {
        grid: Some(std::sync::Arc::new(spec.grid)),
        units: spec.units,
        deployment_zones: Some(spec.zones),
        player_side: spec.player_side,
        board_colors_dirty: true,
        units_dirty: true,
        ..default()
    };
    // Battle-start checkpoint (restart target until the first sync).
    app.insert_resource(tactical3d_render::game::Checkpoints {
        battle_start: Some(tactical3d_render::game::BattleSnapshot::take(&game, &state)),
        ..default()
    });
    app.insert_resource(game);
    app.insert_resource(crate::theme::side_colors(&spec.atk_tag, &spec.def_tag));
    app.insert_resource(tactical3d_render::fx::VpLabel(spec.vp_label));
    app.insert_resource(state);
    if let Some(ctx) = live {
        // The battle window watches game.log itself for the
        // mod's tac_abort (Force Exit) — the parent listener (menu / --live)
        // is blocked while this window runs, so HOI4-side aborts would
        // otherwise never reach us. On abort the session jumps to Ended and
        // exit_when_ended closes the window moments later (§10.4).
        if let Some(log_path) = settings
            .log_path()
            .or_else(tactical_listen::detect_log_path)
        {
            match tactical_listen::LogListener::start_at_end(log_path) {
                Ok(listener) => {
                    app.insert_resource(AbortWatcher { listener });
                    app.add_systems(Update, watch_tac_abort);
                }
                Err(e) => warn!("abort watcher disabled (cannot listen to game.log): {e}"),
            }
        }
        // Postmsg background injection (no foreground steal). The
        // batch file must live in the HOI4 USER DIR — the console `run`
        // only resolves relative paths there; the ping readback needs
        // game.log; both come from settings with sane fallbacks.
        // Per-process name: two concurrent battles (menu child + an
        // externally launched --livebattle) used to clobber one shared
        // tac_inject.txt.
        let batch_path = settings.saves_dir().and_then(|d| {
            d.parent()
                .map(|p| p.join(format!("tac_inject_{}.txt", std::process::id())))
        });
        let injector = match &batch_path {
            Some(p) => Injector::with_paths(p.clone(), settings.log_path()),
            None => Injector::new(),
        };
        app.insert_resource(LiveInjector {
            injector,
            dry: ctx.dry,
            batch_path,
            saves_dir: settings.saves_dir(),
        });
        // The mid-battle reinforcement path assembles divisions
        // from the sync snapshots — same data tables as the assembly.
        match crate::scenario::DivisionTables::load_runtime() {
            Ok(t) => {
                app.insert_resource(LiveTables(t));
            }
            Err(e) => warn!("roster sync disabled (data tables): {e}"),
        }
        app.init_resource::<FinalRetry>();
        app.add_systems(Update, (drain_injections, exit_when_ended));
    }
    if autoplay {
        app.add_plugins(crate::autoplay::AutoplayPlugin);
    }
    app.add_systems(Update, (crate::splash::auto_close, crate::app_icon::apply));
    app.run();
}

#[derive(Resource)]
struct LiveInjector {
    injector: Injector,
    dry: bool,
    /// Batch file path (for the failure hint — the player can paste it into
    /// the HOI4 console by hand). None = the injector's built-in default.
    batch_path: Option<std::path::PathBuf>,
    /// HOI4 "save games" dir — the post-sync battle-alive check
    /// drops its `tac_check.hoi4` snapshots here.
    saves_dir: Option<std::path::PathBuf>,
}

/// Division-assembly tables for the mid-battle roster sync
/// (loaded once per live battle window; absent when the load failed —
/// roster maintenance then skips with a log trail).
#[derive(Resource)]
struct LiveTables(crate::scenario::DivisionTables);

/// One-shot retry for a failed FINAL batch: its loss is permanent damage
/// loss (the battle window closes right after), so it gets one retry after
/// a short settle delay, plus a file-log trail — release builds have no
/// console and a bare `warn!` would be invisible.
#[derive(Resource, Default)]
struct FinalRetry {
    item: Option<(InjectionRequest, f32)>,
    /// One-shot means ONE-SHOT: the re-queued batch keeps
    /// `is_final`, so without this guard a second failure would re-arm the
    /// timer forever (every 1.5 s, masked only by the 3 s exit timer). Once
    /// armed, no re-arm — a second failure leaves only the inject.log trail.
    spent: bool,
}

/// Append one line to %TEMP%\fc_inject.log — post-mortem evidence for a
/// writeback that (maybe) never reached HOI4; release builds have no
/// console and a bare `warn!` is invisible. The same file also records
/// every SUCCESSFUL batch via [`inject_batch_log`], so the log reads as
/// the full injection trail.
fn inject_log_line(line: &str) {
    use std::io::Write;
    let log = std::env::temp_dir().join("fc_inject.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {line}");
    }
}

/// Log a SUCCESSFULLY injected batch's lines verbatim — the
/// reconciliation trail pairing each sync's tactical books with the values
/// HOI4 actually received. (The per-process batch file itself gets
/// overwritten by the trailing clock probes, so without this the sent
/// values were unrecoverable after the fact.) Dry runs are skipped by the
/// caller: nothing was sent, and the batch file stays on disk there anyway.
fn inject_batch_log(label: &str, lines: &[String]) {
    inject_log_line(&format!("{label} batch sent ({} lines):", lines.len()));
    for line in lines {
        inject_log_line(&format!("    {line}"));
    }
}

/// Append an injection failure to %TEMP%\fc_inject.log (see
/// [`inject_log_line`]), naming the batch file so the player can paste it
/// into the HOI4 console by hand.
fn inject_failure_log(live: &LiveInjector, err: &str, is_final: bool) {
    let batch = live
        .batch_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<HOI4 user dir>\\tac_inject.txt".to_string());
    warn!("injection failed: {err} — batch left at {batch} for manual console run");
    inject_log_line(&format!(
        "injection FAILED (final={is_final}): {err} — batch at {batch}"
    ));
}

// ── Clock-advance receipt (DESIGN §8.4) ────────────────────────────

/// Total wait for the clock-advance receipt before logging and continuing
/// anyway — a stuck freeze must never wedge the battle flow (a final
/// batch's phase 2 fires regardless).
const CLOCK_ADVANCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Interval between clock probes while waiting for the receipt.
const CLOCK_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);
/// Tail window scanned for the probe line — game.log can take hourly-pulse
/// spam while the clock runs, so this is wider than the injector's ping
/// window.
const CLOCK_TAIL_BYTES: u64 = 64 * 1024;

/// Probe-inject a clock line and read back its game-date prefix — the
/// game hour HOI4 currently sits at (DESIGN §8.4). `None` in dry mode,
/// without a log path, or on any probe failure; the receipt wait is then
/// skipped and injection stays fire-and-forget.
fn current_clock_prefix(live: &LiveInjector) -> Option<String> {
    if live.dry {
        return None;
    }
    let log = live.injector.log_path()?.to_path_buf();
    live.injector
        .inject_commands(&[tactical_inject::clock_probe_line()], None, false)
        .ok()?;
    let tail = tactical_inject::read_log_tail(&log, CLOCK_TAIL_BYTES).ok()?;
    tactical_listen::marker_date_prefix(&tail, tactical_inject::CLOCK_PROBE_TOKEN)
}

/// Padding-tolerant movement check of two game-date prefixes: parsed
/// tuples compare when both parse, raw strings otherwise. The desync
/// guard's exact-match probes use this so a differently padded (but
/// identical) date is never mistaken for a moved clock.
fn prefix_moved(prev_prefix: &str, found_prefix: &str) -> bool {
    match (
        tactical_sync::desync::parse_game_prefix(prev_prefix),
        tactical_sync::desync::parse_game_prefix(found_prefix),
    ) {
        (Some(p), Some(f)) => p != f,
        _ => found_prefix != prev_prefix,
    }
}

/// Wait until game.log proves the batch's trailing `pause_in_hours 1`
/// advanced the clock: probe lines are injected and the newest one's
/// `[yyyy.mm.dd.hh]` prefix compared against `probe_prefix` (the pre-batch
/// reading — movement detection only).
///
/// The desync guard (DESIGN.md §3.2/§8.2) makes the receipt EXACT against
/// the SESSION's authoritative clock hour (`expected` = last confirmed
/// receipt + 1 game hour): a manual unpause runs the clock ahead
/// (+2h or more), an earlier save moves it backward, and either lands in
/// [`ClockReceipt::Mismatch`] — a drifted clock must never validate its
/// own drift via a fresh probe baseline. `expected = None` (no session
/// baseline — defensive) degrades to the legacy any-move receipt, and an
/// unparsable probe prefix likewise keeps the guard silent rather than
/// wedging the flow.
fn wait_clock_advance(
    live: &LiveInjector,
    probe_prefix: &str,
    expected: Option<&str>,
) -> ClockReceipt {
    let Some(log) = live.injector.log_path().map(|p| p.to_path_buf()) else {
        return ClockReceipt::Timeout;
    };
    use tactical_sync::desync::{format_game_prefix, parse_game_prefix};
    let probe = parse_game_prefix(probe_prefix);
    let expected = expected.and_then(parse_game_prefix);
    let deadline = std::time::Instant::now() + CLOCK_ADVANCE_TIMEOUT;
    loop {
        match live
            .injector
            .inject_commands(&[tactical_inject::clock_probe_line()], None, false)
        {
            Ok(()) => {
                let prefix = tactical_inject::read_log_tail(&log, CLOCK_TAIL_BYTES)
                    .ok()
                    .and_then(|tail| {
                        tactical_listen::marker_date_prefix(
                            &tail,
                            tactical_inject::CLOCK_PROBE_TOKEN,
                        )
                    });
                if let Some(prefix) = prefix {
                    let found = parse_game_prefix(&prefix);
                    let moved = match (found, probe) {
                        (Some(f), Some(p)) => f != p,
                        _ => prefix != probe_prefix,
                    };
                    if moved {
                        let confirmed = match (expected, found) {
                            // No authoritative baseline (legacy fallback) —
                            // any move confirms.
                            (None, _) => true,
                            (Some(e), Some(f)) => f == e,
                            // Baseline present but the receipt unparseable —
                            // cannot confirm the exact hour, treat as drift.
                            (Some(_), None) => false,
                        };
                        if confirmed {
                            info!("clock advance confirmed: {probe_prefix} → {prefix}");
                            return ClockReceipt::Advanced { found: prefix };
                        }
                        let exp = expected
                            .map(format_game_prefix)
                            .unwrap_or_else(|| probe_prefix.to_string());
                        warn!("clock receipt mismatch: expected {exp}, found {prefix}");
                        return ClockReceipt::Mismatch {
                            expected: exp,
                            found: prefix,
                        };
                    }
                }
            }
            Err(e) => {
                warn!("clock probe injection failed ({e}) — giving up on the receipt wait");
                return ClockReceipt::Timeout;
            }
        }
        if std::time::Instant::now() >= deadline {
            return ClockReceipt::Timeout;
        }
        std::thread::sleep(CLOCK_PROBE_INTERVAL);
    }
}

/// The outcome of a clock-advance receipt wait (§8.4 + the desync guard).
enum ClockReceipt {
    /// The freshest probe prefix reads exactly the expected next game hour;
    /// `found` seals the session's authoritative clock hour.
    Advanced { found: String },
    /// The prefix moved but NOT to the expected hour — the guard's hour
    /// check fires and the post-sync checks are skipped this round.
    Mismatch { expected: String, found: String },
    /// The prefix never moved within the timeout (menu/pause stall or a
    /// dead game).
    Timeout,
}

/// The post-sync battle-alive verdict.
enum PostSyncBattle {
    /// The `land_combat` for the contested province still exists — the
    /// snapshot path rides along for the roster diff.
    Alive { save_path: std::path::PathBuf },
    /// The record is gone — the mirrored HOI4 battle ended under the
    /// tactical one; carries the STRATEGIC winner.
    Ended(tactical_core::Side),
    /// Inconclusive (dry run / unreadable save / no snapshot) — fail open,
    /// the battle continues.
    Inconclusive,
}

/// §8.2: after a successful hourly sync the mirrored HOI4 battle
/// may have ended under the tactical one — vanilla forces 0-org divisions
/// out of combat, and the injected `damage_units` lines grind org down.
/// Vanilla damage is frozen and injected damage only lands at syncs, so the
/// ending can only happen at a sync boundary: snapshot the save and scan
/// for the `land_combat` (tactical-sync `savecheck`). Alive carries the
/// snapshot path on for the roster diff; Inconclusive fails open
/// (a false ending would strand the player mid-battle).
///
/// The desync guard runs BEFORE the battle-alive judgement (DESIGN.md
/// §3.2/§8.2): the snapshot's played country must still be this battle's
/// country, otherwise a loaded save would misjudge endings and book
/// damage against the wrong world. A fired guard returns the verdict as
/// `Err` and the caller skips everything else this round.
fn post_sync_battle_check(
    live: &LiveInjector,
    session: &tactical_sync::BattleSession,
) -> Result<PostSyncBattle, tactical_sync::desync::DesyncVerdict> {
    use tactical_sync::desync::check_player_tag;
    use tactical_sync::savecheck::{check_hoi4_battle, root_player_tag, Hoi4BattleCheck};
    if live.dry {
        return Ok(PostSyncBattle::Inconclusive);
    }
    let Some(ctx) = session.battle_ctx.as_ref() else {
        return Ok(PostSyncBattle::Inconclusive);
    };
    let Some(saves_dir) = live.saves_dir.clone() else {
        return Ok(PostSyncBattle::Inconclusive);
    };
    let path = match crate::snapshot::take_check_snapshot(&live.injector, &saves_dir, false) {
        Ok(p) => p,
        Err(e) => {
            inject_log_line(&format!("battle-alive check skipped: {e}"));
            return Ok(PostSyncBattle::Inconclusive);
        }
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            inject_log_line(&format!("battle-alive check read failed: {e}"));
            return Ok(PostSyncBattle::Inconclusive);
        }
    };
    // Desync guard, played-country probe — before ANY judgement below.
    if !session.country_tag.is_empty() {
        let verdict = check_player_tag(&session.country_tag, root_player_tag(&text).as_deref());
        if let tactical_sync::desync::DesyncVerdict::TagMismatch { expected, found } = &verdict {
            inject_log_line(&format!(
                "DESYNC GUARD fired: save player={found}, battle country={expected} — checks skipped"
            ));
            return Err(verdict);
        }
    }
    let def_tags: Vec<&str> = ctx.defender_tags.iter().map(String::as_str).collect();
    match check_hoi4_battle(&text, ctx.contested_province, &def_tags) {
        Hoi4BattleCheck::Alive => Ok(PostSyncBattle::Alive { save_path: path }),
        Hoi4BattleCheck::Unknown => {
            inject_log_line("battle-alive check inconclusive (unreadable save) — battle continues");
            Ok(PostSyncBattle::Inconclusive)
        }
        Hoi4BattleCheck::Ended { winner } => {
            inject_log_line(&format!(
                "HOI4-side battle over — strategic winner {winner:?} (post-sync save check)"
            ));
            Ok(PostSyncBattle::Ended(winner))
        }
    }
}

/// The deferred half of an hourly sync (§8.2): the
/// ground-truth battle-alive check, and — while the HOI4 battle still runs
/// — the mid-battle roster diff. Runs inline right after a clean clock
/// receipt; on a receipt timeout it waits for the stall dialog's
/// resolution (retry-success or cancel) instead. Returns the desync
/// guard's verdict when a probe fired — the caller opens the guard dialog
/// and skips everything else this round.
#[allow(clippy::too_many_arguments)]
fn post_sync_followup(
    live: &LiveInjector,
    game: &mut Option<ResMut<GameController>>,
    state: &mut Option<ResMut<TacticalState>>,
    checkpoints: &mut Option<ResMut<Checkpoints>>,
    notice: &mut Option<ResMut<NoticeBar>>,
    tables: &Option<Res<LiveTables>>,
    loc: &Option<Res<LocaleRes>>,
    now_secs: f32,
) -> Option<tactical_sync::desync::DesyncVerdict> {
    let (Some(game), Some(loc)) = (game.as_mut(), loc.as_ref()) else {
        return None;
    };
    match post_sync_battle_check(live, &game.session) {
        Err(verdict) => Some(verdict),
        Ok(PostSyncBattle::Ended(winner)) => {
            match game.session.resolve_externally(winner) {
                Ok(()) => {
                    let player_won = winner == game.session.player_side;
                    game.log_line_icon(
                        Some(if player_won {
                            IconId::Trophy
                        } else {
                            IconId::Dove
                        }),
                        loc.tr(if player_won {
                            "log.hoi4_end.victory"
                        } else {
                            "log.hoi4_end.defeat"
                        }),
                    );
                }
                Err(e) => {
                    warn!("external resolution dropped ({e}) — already resolved?")
                }
            }
            None
        }
        Ok(PostSyncBattle::Alive { save_path }) => {
            // Only a RUNNING board takes roster changes — a tactically
            // resolved battle (ReadyToEnd) is already counting down to the
            // exit.
            if game.session.phase == tactical_sync::BattlePhase::TacticalActive {
                roster_sync_now(
                    game,
                    state,
                    checkpoints,
                    notice,
                    tables,
                    loc,
                    &save_path,
                    now_secs,
                );
            }
            None
        }
        Ok(PostSyncBattle::Inconclusive) => None,
    }
}

/// Open the desync guard dialog (the render-side `DesyncAlert`) and log the
/// verdict both ways — the battle log for the player, the fc_inject.log
/// trail as the post-mortem record.
fn raise_desync_alert(
    commands: &mut Commands,
    game: &mut Option<ResMut<GameController>>,
    loc: &Option<Res<LocaleRes>>,
    verdict: tactical_sync::desync::DesyncVerdict,
) {
    let (key, expected, found) = match &verdict {
        tactical_sync::desync::DesyncVerdict::HourMismatch { expected, found } => {
            ("log.desync.hour", expected, found)
        }
        tactical_sync::desync::DesyncVerdict::TagMismatch { expected, found } => {
            ("log.desync.tag", expected, found)
        }
        tactical_sync::desync::DesyncVerdict::Ok => return,
    };
    inject_log_line(&format!(
        "DESYNC GUARD fired ({key}): expected {expected}, found {found}"
    ));
    if let (Some(game), Some(loc)) = (game.as_mut(), loc.as_ref()) {
        game.log_line_icon(
            Some(IconId::Warning),
            loc.trf(key, &[("expected", expected), ("found", found)]),
        );
    }
    commands.insert_resource(DesyncAlert {
        verdict,
        action: DesyncAction::Pending,
    });
}

/// §8.2: the HOI4 battle still runs — diff the session roster
/// against the fresh snapshot's `land_combat` and apply the result to the
/// board (departures → `LeftBattle`, reinforcements → map-edge entry).
/// Every change is announced (fc_inject.log trail + localized battle-log
/// line + notice flash), and the sync checkpoint is re-taken so a rollback
/// never resurrects a departed division (the roster IS the HOI4 truth —
/// it must not rewind).
#[allow(clippy::too_many_arguments)]
fn roster_sync_now(
    game: &mut GameController,
    state: &mut Option<ResMut<TacticalState>>,
    checkpoints: &mut Option<ResMut<Checkpoints>>,
    notice: &mut Option<ResMut<NoticeBar>>,
    tables: &Option<Res<LiveTables>>,
    loc: &LocaleRes,
    save_path: &std::path::Path,
    now: f32,
) {
    let (Some(state), Some(tables)) = (state.as_mut(), tables.as_ref()) else {
        return;
    };
    let Some(grid) = state.grid.clone() else {
        return;
    };
    let save = match tactical_save::SaveParser::parse_save(save_path) {
        Ok(s) => s,
        Err(e) => {
            inject_log_line(&format!("roster sync skipped (save parse): {e}"));
            return;
        }
    };
    let naming = crate::naming::localized_unit_naming(&loc.0);
    let zones = state.deployment_zones.clone().unwrap_or_default();
    let report = crate::scenario::apply_roster_sync(
        &mut game.session,
        &grid,
        &zones,
        &mut state.units,
        &save,
        &tables.0,
        &naming,
    );
    if report.is_empty() {
        return;
    }
    // Post-mortem trail (fc_inject.log, same as the injection batches).
    for c in &report.joined {
        inject_log_line(&format!(
            "roster join: {} ({}) {:?} dir={:?} battalions={}",
            c.name, c.tag, c.side, c.dir, c.battalions
        ));
    }
    for c in &report.left {
        inject_log_line(&format!(
            "roster leave: {} ({}) {:?} battalions={}",
            c.name, c.tag, c.side, c.battalions
        ));
    }
    // Localized battle-log lines.
    for c in &report.joined {
        match c.dir {
            Some(d) => {
                let dir = d.to_string();
                game.log_line_icon(
                    Some(IconId::Deploy),
                    loc.trf(
                        "log.roster.joined",
                        &[("name", &c.name), ("tag", &c.tag), ("dir", &dir)],
                    ),
                );
            }
            None => game.log_line_icon(
                Some(IconId::Deploy),
                loc.trf(
                    "log.roster.joined_unknown_dir",
                    &[("name", &c.name), ("tag", &c.tag)],
                ),
            ),
        }
    }
    for c in &report.left {
        game.log_line_icon(
            Some(IconId::Door),
            loc.trf("log.roster.left", &[("name", &c.name), ("tag", &c.tag)]),
        );
    }
    // A selected unit that just left the board must not dangle.
    if let Some(sel) = state.selected_unit {
        if !state
            .units
            .iter()
            .any(|u| u.id == sel && u.position != tactical_core::BattalionUnit::OFFBOARD)
        {
            state.selected_unit = None;
        }
    }
    state.units_dirty = true;
    if let Some(notice) = notice.as_mut() {
        let joined = report.joined.len().to_string();
        let left = report.left.len().to_string();
        notice.flash = Some(FlashNotice::plain(
            loc.trf(
                "notice.roster.changed",
                &[("joined", &joined), ("left", &left)],
            ),
            now + 5.0,
        ));
    }
    // Re-take the sync checkpoint with the roster change applied.
    if let Some(checkpoints) = checkpoints.as_mut() {
        checkpoints.last_sync = Some(BattleSnapshot::take(game, state));
    }
}

/// Live battles only: tails game.log for the mod's `tac_abort`.
#[derive(Resource)]
struct AbortWatcher {
    listener: tactical_listen::LogListener,
}

/// §10.4: the player fired "Force Exit Tactical Mode" in HOI4 — wrap the
/// battle window up immediately (unsynced damage is lost, §11.3). Polled at
/// 2 Hz; other message types are ignored here (the parent listener drains
/// them after the window closes).
fn watch_tac_abort(
    watcher: Option<ResMut<AbortWatcher>>,
    game: Option<ResMut<GameController>>,
    mut timer: Local<f32>,
    time: Res<Time>,
) {
    let (Some(mut watcher), Some(mut game)) = (watcher, game) else {
        return;
    };
    *timer += time.delta_secs();
    if *timer < 0.5 {
        return;
    }
    *timer = 0.0;
    for msg in watcher.listener.poll() {
        if let tactical_listen::LogMessage::TacAbort { ts, tag } = msg {
            info!("[{ts}] tac_abort from {tag} — closing the battle window (unsynced damage lost)");
            game.session.abort();
        }
    }
}

/// Drain pending injection requests and push them into HOI4 (§3.2).
///
/// Clock receipt (§8.4): a batch whose last line is the clock-advance command
/// gets a receipt wait — the current game-hour prefix is probe-read BEFORE
/// the batch, and after a successful injection [`wait_clock_advance`]
/// polls until the prefix reads exactly the expected next game hour (the
/// desync guard's exact match — §3.2/§8.2 below; on a plain stall: timeout,
/// log + continue). A FINAL batch
/// is two-phase: phase 1 (damage + collapse + clock advance) first, then
/// — receipt or timeout alike — phase 2 (unfreeze + cleanup), so the
/// freeze can never be left behind by a stuck clock. Phase 1 can
/// legitimately be EMPTY (battle ended with no unsynced turns, damage or
/// collapse): only phase 2 goes out then.
///
/// Roster sync (§8.2): the extra state/checkpoints/notice/tables params feed
/// the post-sync roster maintenance (join/leave on the live board).
///
/// Sync stall (§8.4): a HOURLY batch whose receipt wait times out suspends
/// the flow behind the sync-stall dialog (the `SyncStall` resource) — the
/// player checks HOI4, then picks Retry (clock-command only: the damage
/// lines already landed, re-sending them would double-apply) or Cancel
/// (the plain log-and-continue path). The deferred post-sync
/// follow-up runs on either resolution. FINAL batches never stall: their
/// phase 2 (unfreeze + cleanup) must not be held hostage by a dialog.
///
/// Desync guard (DESIGN.md §3.2/§8.2): a clock receipt that moved but not
/// to the exact expected hour, or a snapshot whose played country no longer
/// matches, suspends the flow behind the desync dialog (the `DesyncAlert`
/// resource) — End Battle queues the abort cleanup batch
/// (`build_early_exit_batch(false)`: unfreeze + flags + popup, no damage,
/// no clock), Continue Unsynced disables the sync pipeline for the rest of
/// the battle (the session seals hours locally; every exit then sends the
/// same cleanup-only batch). While the dialog is unanswered ALL injection
/// work holds, like the stall dialog.
#[allow(clippy::too_many_arguments)]
fn drain_injections(
    mut commands: Commands,
    live: Res<LiveInjector>,
    mut pending: ResMut<PendingInjection>,
    mut retry: ResMut<FinalRetry>,
    mut game: Option<ResMut<GameController>>,
    mut state: Option<ResMut<TacticalState>>,
    mut checkpoints: Option<ResMut<Checkpoints>>,
    mut notice: Option<ResMut<NoticeBar>>,
    tables: Option<Res<LiveTables>>,
    loc: Option<Res<LocaleRes>>,
    mut stall: Option<ResMut<SyncStall>>,
    mut desync: Option<ResMut<DesyncAlert>>,
    time: Res<Time>,
) {
    // While a stall dialog is unanswered, hold ALL injection
    // work; resolve it here once the player clicks through.
    if let Some(stall) = &mut stall {
        match stall.action {
            SyncStallAction::Pending => return,
            SyncStallAction::Retry => {
                stall.action = SyncStallAction::Pending;
                // The hour may have caught up on its own — a stale
                // `pause_in_hours` timer fires once the player leaves the
                // menu. A moved prefix counts as success WITHOUT re-sending
                // the clock command — and the desync guard's exact match
                // applies to the catch-up reading too (the unpause that
                // fired the timer may have run the clock ahead). The
                // expectation is the SESSION's authoritative hour,
                // never the stale pre-batch probe.
                let session_last = game
                    .as_ref()
                    .and_then(|g| g.session.last_receipt_prefix.clone());
                let receipt = match current_clock_prefix(&live) {
                    Some(cur) if prefix_moved(&stall.prev_prefix, &cur) => {
                        match session_last.as_deref() {
                            Some(last) => {
                                match tactical_sync::desync::check_clock_receipt(last, &cur) {
                                    tactical_sync::desync::DesyncVerdict::Ok => {
                                        ClockReceipt::Advanced { found: cur }
                                    }
                                    tactical_sync::desync::DesyncVerdict::HourMismatch {
                                        expected,
                                        found,
                                    } => ClockReceipt::Mismatch { expected, found },
                                    tactical_sync::desync::DesyncVerdict::TagMismatch { .. } => {
                                        ClockReceipt::Timeout
                                    }
                                }
                            }
                            None => ClockReceipt::Advanced { found: cur },
                        }
                    }
                    _ => {
                        let expected = session_last
                            .as_deref()
                            .and_then(tactical_sync::desync::next_game_hour);
                        match live.injector.inject_commands(
                            &[tactical_sync::CLOCK_ADVANCE_COMMAND.to_string()],
                            None,
                            live.dry,
                        ) {
                            Ok(()) => wait_clock_advance(
                                &live,
                                &stall.prev_prefix,
                                expected.as_deref(),
                            ),
                            Err(e) => {
                                inject_failure_log(&live, &e.to_string(), false);
                                ClockReceipt::Timeout
                            }
                        }
                    }
                };
                match receipt {
                    ClockReceipt::Advanced { found } => {
                        if let Some(game) = game.as_mut() {
                            game.session.last_receipt_prefix = Some(found);
                        }
                        info!("clock advance confirmed on player retry");
                        inject_log_line("clock receipt confirmed after player retry");
                        if let (Some(game), Some(loc)) = (&mut game, &loc) {
                            game.log_line_icon(Some(IconId::Sync), loc.tr("log.sync.stall_retry"));
                        }
                        commands.remove_resource::<SyncStall>();
                        if let Some(verdict) = post_sync_followup(
                            &live,
                            &mut game,
                            &mut state,
                            &mut checkpoints,
                            &mut notice,
                            &tables,
                            &loc,
                            time.elapsed_secs(),
                        ) {
                            raise_desync_alert(&mut commands, &mut game, &loc, verdict);
                        }
                    }
                    ClockReceipt::Mismatch { expected, found } => {
                        // The retry proved the clock off by other than one
                        // hour — the stall dialog gives way to the guard.
                        inject_log_line(&format!(
                            "clock receipt MISMATCH on retry (expected {expected}, found {found}) — guard dialog opened"
                        ));
                        commands.remove_resource::<SyncStall>();
                        raise_desync_alert(
                            &mut commands,
                            &mut game,
                            &loc,
                            tactical_sync::desync::DesyncVerdict::HourMismatch { expected, found },
                        );
                    }
                    ClockReceipt::Timeout => {
                        stall.attempts += 1;
                        inject_log_line(&format!(
                            "clock receipt TIMEOUT on retry (attempts={}, prev={})",
                            stall.attempts, stall.prev_prefix
                        ));
                    }
                }
                return;
            }
            SyncStallAction::Cancel => {
                inject_log_line("sync receipt wait dismissed by player — flow continued");
                if let (Some(game), Some(loc)) = (&mut game, &loc) {
                    game.log_line_icon(Some(IconId::Warning), loc.tr("log.sync.stall_cancel"));
                }
                commands.remove_resource::<SyncStall>();
                if let Some(verdict) = post_sync_followup(
                    &live,
                    &mut game,
                    &mut state,
                    &mut checkpoints,
                    &mut notice,
                    &tables,
                    &loc,
                    time.elapsed_secs(),
                ) {
                    raise_desync_alert(&mut commands, &mut game, &loc, verdict);
                }
                return;
            }
        }
    }
    // While the desync guard's dialog is unanswered, hold ALL injection
    // work; resolve the player's choice here.
    if let Some(alert) = &mut desync {
        match alert.action {
            DesyncAction::Pending => return,
            DesyncAction::EndBattle => {
                inject_log_line(
                    "desync guard: player ended the battle — abort cleanup batch queued",
                );
                if let Some(game) = game.as_mut() {
                    // The existing early-exit cleanup path, carry=false:
                    // unfreeze + flags + abort popup, NO damage lines and
                    // NO clock advance — a wrong-world target turns the
                    // cleanup into a no-op, which is exactly right.
                    let batches = game.session.build_early_exit_batch(false);
                    pending.0 = Some(InjectionRequest {
                        batch: batches.phase1,
                        is_final: true,
                        result: None,
                        phase2: Some(batches.phase2),
                    });
                    if let Some(loc) = &loc {
                        game.log_line_icon(Some(IconId::Door), loc.tr("log.desync.ended"));
                    }
                    // Ended only NOW, with the cleanup queued: the exit
                    // countdown must not outrun the batch it exists to send.
                    game.session.abort();
                }
                commands.remove_resource::<DesyncAlert>();
                return;
            }
            DesyncAction::ContinueUnsynced => {
                inject_log_line(
                    "desync guard: player chose unsynced mode — sync disabled for this battle",
                );
                if let Some(game) = game.as_mut() {
                    game.session.sync_disabled = true;
                    if let Some(loc) = &loc {
                        game.log_line_icon(Some(IconId::Warning), loc.tr("log.desync.entered"));
                    }
                }
                if let (Some(notice), Some(loc)) = (&mut notice, &loc) {
                    notice.flash = Some(FlashNotice::plain(
                        loc.tr("notice.desync.mode").into_owned(),
                        time.elapsed_secs() + 6.0,
                    ));
                }
                commands.remove_resource::<DesyncAlert>();
                return;
            }
        }
    }
    // A failed FINAL batch re-queues here and retries once after a settle
    // delay (see FinalRetry).
    if let Some((_, delay)) = &mut retry.item {
        *delay -= time.delta_secs();
        if *delay <= 0.0 {
            let (req, _) = retry.item.take().expect("checked Some above");
            pending.0 = Some(req);
        }
        return;
    }
    let Some(req) = pending.0.take() else { return };
    let advances_clock = req
        .batch
        .last()
        .is_some_and(|l| l == tactical_sync::CLOCK_ADVANCE_COMMAND);
    let prev_prefix = if advances_clock {
        current_clock_prefix(&live)
    } else {
        None
    };
    // The EXPECTED receipt is the session's
    // authoritative clock hour + 1 — never the fresh probe above: a
    // drifted clock (player unpaused mid-battle) would validate its own
    // drift. The pre-batch probe only detects that the clock responded.
    let expected_prefix = if advances_clock {
        game.as_ref()
            .and_then(|g| g.session.last_receipt_prefix.clone())
            .and_then(|p| tactical_sync::desync::next_game_hour(&p))
    } else {
        None
    };
    let phase1 = if req.batch.is_empty() {
        Ok(())
    } else {
        live.injector.inject_commands(&req.batch, None, live.dry)
    };
    match phase1 {
        Ok(()) => {
            info!(
                "injected {} console commands (final={})",
                req.batch.len(),
                req.is_final
            );
            // The sent values land in fc_inject.log for
            // reconciliation (empty phase 1 injected nothing).
            if !req.batch.is_empty() && !live.dry {
                inject_batch_log(
                    if req.is_final {
                        "final phase 1"
                    } else {
                        "hourly sync"
                    },
                    &req.batch,
                );
            }
            if let Some(prev) = prev_prefix {
                match wait_clock_advance(&live, &prev, expected_prefix.as_deref()) {
                    ClockReceipt::Advanced { found } => {
                        // Seal the session's authoritative clock hour —
                        // the next sync expects found + 1 game hour.
                        if let Some(game) = game.as_mut() {
                            game.session.last_receipt_prefix = Some(found);
                        }
                    }
                    ClockReceipt::Timeout => {
                        if req.is_final {
                            // Log-and-continue for finals: phase 2 (unfreeze +
                            // cleanup) must never wait on a dialog.
                            warn!("clock-advance receipt timed out — continuing anyway");
                            inject_log_line(&format!(
                                "clock receipt TIMEOUT (prev={prev}, final=true) — flow continued"
                            ));
                        } else {
                            // An hourly stall surfaces to the player —
                            // suspend here (the post-sync follow-up included) and
                            // let the dialog branch above resolve it.
                            warn!("clock-advance receipt timed out — sync-stall dialog opened");
                            inject_log_line(&format!(
                                "clock receipt TIMEOUT (prev={prev}, final=false) — player dialog opened"
                            ));
                            if let (Some(game), Some(loc)) = (&mut game, &loc) {
                                game.log_line_icon(
                                    Some(IconId::Warning),
                                    loc.tr("log.sync.stalled"),
                                );
                            }
                            commands.insert_resource(SyncStall {
                                prev_prefix: prev,
                                attempts: 1,
                                action: SyncStallAction::Pending,
                            });
                            return;
                        }
                    }
                    ClockReceipt::Mismatch { expected, found } => {
                        // The desync guard's hour probe: the clock does not
                        // sit where this battle left it. The post-sync
                        // checks are skipped entirely this round; the
                        // player chooses end-vs-unsynced.
                        if req.is_final {
                            // A final batch's phase 2 (unfreeze + cleanup)
                            // must not be held hostage by a dialog.
                            warn!("clock receipt mismatch on final batch — continuing anyway");
                            inject_log_line(&format!(
                                "clock receipt MISMATCH (expected {expected}, found {found}, final=true) — flow continued"
                            ));
                        } else {
                            warn!("clock receipt mismatch — desync guard dialog opened");
                            raise_desync_alert(
                                &mut commands,
                                &mut game,
                                &loc,
                                tactical_sync::desync::DesyncVerdict::HourMismatch {
                                    expected,
                                    found,
                                },
                            );
                            return;
                        }
                    }
                }
            }
            // Final phase 2 (unfreeze + cleanup): no retry on failure —
            // FinalRetry covers phase 1 only; the inject.log trail is the
            // post-mortem.
            if let Some(phase2) = &req.phase2 {
                match live.injector.inject_commands(phase2, None, live.dry) {
                    Ok(()) => {
                        info!("injected final phase 2 ({} commands)", phase2.len());
                        if !phase2.is_empty() && !live.dry {
                            inject_batch_log("final phase 2", phase2);
                        }
                    }
                    Err(e) => inject_failure_log(&live, &format!("final phase 2: {e}"), true),
                }
            }
            // Battle-alive check + roster diff ride along
            // on hourly syncs (a stall dialog defers them — the timeout path
            // above already returned).
            if !req.is_final {
                if let Some(verdict) = post_sync_followup(
                    &live,
                    &mut game,
                    &mut state,
                    &mut checkpoints,
                    &mut notice,
                    &tables,
                    &loc,
                    time.elapsed_secs(),
                ) {
                    raise_desync_alert(&mut commands, &mut game, &loc, verdict);
                }
            }
        }
        // The batch file stays on disk after a failure — name it so the
        // player can paste it into the HOI4 console by hand. A failed final
        // batch retries once (its loss is permanent damage loss).
        Err(e) => {
            inject_failure_log(&live, &e.to_string(), req.is_final);
            if req.is_final && !retry.spent {
                retry.spent = true;
                retry.item = Some((req, 1.5));
            }
        }
    }
}

/// Close the app shortly after the battle is fully resolved (returns to the
/// menu / listen loop instead of hanging on the result screen).
fn exit_when_ended(
    game: Option<Res<GameController>>,
    mut timer: Local<Option<f32>>,
    time: Res<Time>,
    mut exit: EventWriter<bevy::app::AppExit>,
) {
    let Some(game) = game else { return };
    if game.session.phase == tactical_sync::BattlePhase::Ended {
        let t = timer.get_or_insert(3.0);
        *t -= time.delta_secs();
        if *t <= 0.0 {
            exit.send(bevy::app::AppExit::Success);
        }
    }
}
