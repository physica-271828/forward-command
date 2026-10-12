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
    DesyncAction, DesyncAlert, FlashNotice, NoticeBar, SyncProgress, SyncStall, SyncStallAction,
    TacticalState,
};
use tactical3d_render::TacticalGamePlugin;
use tactical_core::hex::HexCoord;
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
    /// The version the mod advertised in `tac_start` (§3.3) — `None` for a
    /// pre-handshake mod, which then receives no verdict line at all.
    pub mod_ver: Option<String>,
}

/// Standalone battle (debug form / CLI / demo-style): no HOI4 injection.
/// `settings` are the CALLER's, not a fresh read of settings.json — the CLI
/// path keys (`hoi4_dir=`/`saves_dir=`/`log_path=`) must reach the injector,
/// the abort watcher and the render knobs too, or the batch file lands in
/// the wrong user dir.
pub fn run(spec: BattleSpec, settings: &AppSettings) {
    run_inner(spec, None, false, settings);
}

/// Live battle: console injection of sync results back into HOI4.
pub fn run_live(spec: BattleSpec, ctx: LiveCtx, settings: &AppSettings) {
    run_inner(spec, Some(ctx), false, settings);
}

/// Standalone battle with the hands-free autoplay driver (agent automation
/// for in-window hang repros — see autoplay.rs).
pub fn run_autoplay(spec: BattleSpec, settings: &AppSettings) {
    run_inner(spec, None, true, settings);
}

/// Run the battle window for an assembled scenario. Blocks until the window
/// closes (Esc menu → Exit Game, battle end, or OS close), then returns.
fn run_inner(spec: BattleSpec, live: Option<LiveCtx>, autoplay: bool, settings: &AppSettings) {
    // UI language (DESIGN §15) — battle children are separate processes, so
    // each loads its own settings; `settings` is that load, already carrying
    // any CLI path overrides.
    let loc = Locale::load(settings.language());
    let title = loc.trf("window.title.battle", &[("location", &spec.location)]);
    // Version handshake (§3.3): the mod's advertised version decides the one
    // verdict line every injected batch carries, plus this window's own
    // warning when the mod sits below the compatibility floor.
    let own_ver = env!("CARGO_PKG_VERSION");
    let mod_ver = live.as_ref().and_then(|c| c.mod_ver.clone());
    let verdict = crate::version::verdict(own_ver, mod_ver.as_deref());
    let notice = crate::version::notice_text(&loc, &verdict, own_ver, mod_ver.as_deref());
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
    crate::window::apply_render_quality(&mut app, settings);
    // Render-resolution scale (offscreen + upscale when < 100%).
    crate::window::apply_render_scale(&mut app, settings.render_scale_pct());
    if settings.low_power {
        crate::window::apply_low_power(&mut app);
    }
    // Esc menu → Settings — the live render/performance knobs
    // (hot-applied by the render crate, persisted here).
    crate::window::init_battle_settings(&mut app, settings);

    let mut game = GameController::new(
        spec.player_side,
        spec.enemy_tactic,
        // Deterministic default 7 for --battle reproducibility; live
        // assembly passes a varying seed (M6).
        spec.seed.unwrap_or(7),
    );
    // §7.4 settings policy: keep manually commanded battalions manual
    // after their command completes (default: auto hand-back). Checkpoint
    // snapshots are taken below, so the flag rides them from the start.
    game.keep_manual_override = settings.keep_manual_override;
    game.location = spec.location.clone();
    if let Some(p) = spec.province {
        game.province = p;
    }
    // §7.5: the script's allied AI contingents + division→tag
    // registry (outside script battles the registry is empty and the allied
    // split rides `spec.allies` — a live battle's allied countries).
    game.allies = spec.allies.clone();
    app.insert_resource(crate::theme::tag_colors(&battle_tag_set(&spec)));
    game.division_tags = spec.division_tags;
    // ENH-0158: the flag anchors ride the battle state so the defender's
    // deployment corridor can be computed from them at deploy time.
    let flag_anchors: Vec<HexCoord> = spec
        .flags
        .as_ref()
        .map(|f| f.flags.iter().map(|z| z.anchor).collect())
        .unwrap_or_default();
    // §6.11: attach the battle's flag board (derived at
    // assembly) — checkpoint snapshots carry it, so a Restart/Rollback
    // keeps the capture progress.
    game.session.set_flags(spec.flags);
    // §6.8: the fire-phase "step aside" routs score against
    // the defender zone's rim like every other retreat.
    game.combat.set_retreat_zones(Some(spec.zones.clone()));
    // Interior battles: the rout rim is the province boundary outright —
    // the zone blobs' east rims would sit mid-province.
    game.combat.set_retreat_rim(spec.retreat_rim.clone());
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
    // Version handshake (§3.3): the verdict line rides at the FRONT of
    // every injected batch, scoped to the SESSION's resolved country tag
    // (ctx.tag can be empty — the batch scope is what HOI4 receives).
    let version_line = crate::version::verdict_line(&game.session.country_tag, &verdict);
    if let Some(text) = &notice {
        // The battle window's own surfaces carry the warning for a stale
        // MOD (never an in-game popup — the mod already acks that case).
        game.log_line_icon(None, text.clone());
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
        retreat_rim: spec.retreat_rim,
        staging: spec.staging.clone(),
        flag_anchors,
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
    if let Some(text) = notice {
        // Same text as the battle-log line, as a top-bar flash (the log
        // window only opens on demand).
        app.insert_resource(NoticeBar {
            flash: Some(FlashNotice::plain(text, 12.0)),
            pending: None,
        });
    }
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
        let log_path = settings.log_path();
        let injector = build_injector(&batch_path, &log_path);
        app.insert_resource(LiveInjector {
            injector,
            dry: ctx.dry,
            batch_path,
            log_path,
            saves_dir: settings.saves_dir(),
            version_line,
        });
        // The live flow's own trail (release builds have no console): the
        // verdict, the versions it was computed from, and — for a stale-mod
        // warning — the floor row that raised it.
        let floor = match &verdict {
            crate::version::Verdict::ModStale { .. } => crate::version::floor_reason(own_ver)
                .map(|r| format!(" floor_reason={r}"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        inject_log_line(&format!(
            "version handshake: verdict={verdict:?} mod={mod_ver:?} exe={own_ver}{floor}"
        ));
        // The mid-battle reinforcement path assembles divisions
        // from the sync snapshots — same data tables as the assembly.
        match crate::scenario::DivisionTables::load_runtime() {
            Ok(t) => {
                app.insert_resource(LiveTables(t));
            }
            Err(e) => warn!("roster sync disabled (data tables): {e}"),
        }
        app.init_resource::<FinalRetry>();
        app.add_systems(
            Update,
            // The poll runs BEFORE the drain: a resolved sync's receipt seal
            // and dialog outcomes must land before any queued next batch
            // (e.g. an ending battle's cleanup) injects behind them.
            (
                poll_sync_worker.before(drain_injections),
                drain_injections,
                exit_when_ended,
            ),
        );
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
    /// game.log path — kept alongside `batch_path` so a background sync
    /// worker can build its own injector from the same pair.
    log_path: Option<std::path::PathBuf>,
    /// HOI4 "save games" dir — the post-sync battle-alive check
    /// drops its `tac_check.hoi4` snapshots here.
    saves_dir: Option<std::path::PathBuf>,
    /// The handshake verdict line (§3.3), prepended to every injected batch.
    /// None = the mod predates the handshake: inject nothing, never guess.
    version_line: Option<String>,
}

/// Build an injector from the settings-derived paths: the batch file lives
/// in the HOI4 USER DIR (the console `run` only resolves relative paths
/// there) and the ping readback needs game.log. No batch path = the
/// injector's built-in defaults. The main injector and every background
/// sync worker's private one come from here, so they always agree.
fn build_injector(
    batch_path: &Option<std::path::PathBuf>,
    log_path: &Option<std::path::PathBuf>,
) -> Injector {
    match batch_path {
        Some(p) => Injector::with_paths(p.clone(), log_path.clone()),
        None => Injector::new(),
    }
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

/// Prepend the handshake verdict to one batch (DESIGN §3.3). Every batch the
/// battle injects carries the verdict exactly ONCE, at the front; the
/// clock-probe / snapshot single-liners go through the injector directly and
/// stay untouched. An EMPTY batch stays empty: nothing is injected for it at
/// all, and a lone verdict line would turn it into a real synchronization.
fn with_version_line(version_line: &Option<String>, batch: &[String]) -> Vec<String> {
    match (batch.is_empty(), version_line) {
        (false, Some(line)) => {
            let mut out = Vec::with_capacity(batch.len() + 1);
            out.push(line.clone());
            out.extend_from_slice(batch);
            out
        }
        _ => batch.to_vec(),
    }
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
/// skipped and injection stays fire-and-forget. Takes the injector by
/// reference so a background sync worker can drive its own.
fn current_clock_prefix(injector: &Injector, dry: bool) -> Option<String> {
    if dry {
        return None;
    }
    let log = injector.log_path()?.to_path_buf();
    injector
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
/// wedging the flow. Takes the injector by reference so a background sync
/// worker can drive its own.
fn wait_clock_advance(
    injector: &Injector,
    probe_prefix: &str,
    expected: Option<&str>,
) -> ClockReceipt {
    let Some(log) = injector.log_path().map(|p| p.to_path_buf()) else {
        return ClockReceipt::Timeout;
    };
    use tactical_sync::desync::{format_game_prefix, parse_game_prefix};
    let probe = parse_game_prefix(probe_prefix);
    let expected = expected.and_then(parse_game_prefix);
    let deadline = std::time::Instant::now() + CLOCK_ADVANCE_TIMEOUT;
    loop {
        match injector.inject_commands(&[tactical_inject::clock_probe_line()], None, false) {
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

/// The worker half of a post-sync follow-up (§8.2), returned as pure data:
/// the desync guard's verdict when a probe fired, the strategic winner when
/// the mirrored HOI4 battle ended under the tactical one, or the parsed
/// snapshot for the roster diff. Silent by design — every line the inline
/// path would have written to fc_inject.log is collected in `log_lines` for
/// the main thread to emit verbatim (two threads must never interleave
/// writes there).
#[derive(Default)]
struct FollowupOutcome {
    /// The desync guard fired — the main thread opens the guard dialog and
    /// skips everything else this round.
    verdict: Option<tactical_sync::desync::DesyncVerdict>,
    /// The `land_combat` record is gone — the mirrored HOI4 battle ended;
    /// carries the STRATEGIC winner.
    ended: Option<tactical_core::Side>,
    /// The HOI4 battle still runs and the snapshot parsed — the main thread
    /// diffs the roster against it and applies the result to the board.
    save: Option<tactical_save::SaveGame>,
    /// The fc_inject.log lines the inline path would have written.
    log_lines: Vec<String>,
}

/// The session snapshot the follow-up worker runs against, captured at
/// spawn time — the worker thread never touches ECS state.
struct FollowupSeed {
    country_tag: String,
    battle_ctx: Option<tactical_sync::BattleContext>,
    /// Pre-computed roster gate (running board + assembly tables + grid all
    /// present) so the worker skips the expensive save parse when the main
    /// thread would discard the result anyway. The apply side re-checks the
    /// phase — the board may have resolved while the worker was in flight.
    roster_wanted: bool,
}

/// Capture the follow-up seed from the live ECS state. `None` without a
/// game controller (the inline path no-ops there too).
fn followup_seed(
    game: &Option<ResMut<GameController>>,
    state: &Option<ResMut<TacticalState>>,
    tables: &Option<Res<LiveTables>>,
) -> Option<FollowupSeed> {
    let game = game.as_ref()?;
    Some(FollowupSeed {
        country_tag: game.session.country_tag.clone(),
        battle_ctx: game.session.battle_ctx.clone(),
        roster_wanted: game.session.phase == tactical_sync::BattlePhase::TacticalActive
            && tables.is_some()
            && state.as_ref().is_some_and(|s| s.grid.is_some()),
    })
}

/// §8.2: after a successful hourly sync the mirrored HOI4 battle
/// may have ended under the tactical one — vanilla forces 0-org divisions
/// out of combat, and the injected `damage_units` lines grind org down.
/// Vanilla damage is frozen and injected damage only lands at syncs, so the
/// ending can only happen at a sync boundary: snapshot the save and scan
/// for the `land_combat` (tactical-sync `savecheck`). Every inconclusive
/// probe fails open (a false ending would strand the player mid-battle).
///
/// The desync guard runs BEFORE the battle-alive judgement (DESIGN.md
/// §3.2/§8.2): the snapshot's played country must still be this battle's
/// country, otherwise a loaded save would misjudge endings and book
/// damage against the wrong world.
///
/// Runs on the background sync worker: this is the heavy I/O half (forced
/// savegame + full save parse). The board mutations ride back in the
/// returned [`FollowupOutcome`] for the main thread to apply.
fn run_sync_followup(
    injector: &Injector,
    dry: bool,
    saves_dir: Option<std::path::PathBuf>,
    seed: FollowupSeed,
) -> FollowupOutcome {
    use tactical_sync::desync::check_player_tag;
    use tactical_sync::savecheck::{check_hoi4_battle, root_player_tag, Hoi4BattleCheck};
    let mut out = FollowupOutcome::default();
    if dry {
        return out;
    }
    let Some(ctx) = seed.battle_ctx else {
        return out;
    };
    let Some(saves_dir) = saves_dir else {
        return out;
    };
    let path = match crate::snapshot::take_check_snapshot(injector, &saves_dir, false) {
        Ok(p) => p,
        Err(e) => {
            out.log_lines
                .push(format!("battle-alive check skipped: {e}"));
            return out;
        }
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            out.log_lines
                .push(format!("battle-alive check read failed: {e}"));
            return out;
        }
    };
    // Desync guard, played-country probe — before ANY judgement below.
    if !seed.country_tag.is_empty() {
        let verdict = check_player_tag(&seed.country_tag, root_player_tag(&text).as_deref());
        if let tactical_sync::desync::DesyncVerdict::TagMismatch { expected, found } = &verdict {
            out.log_lines.push(format!(
                "DESYNC GUARD fired: save player={found}, battle country={expected} — checks skipped"
            ));
            out.verdict = Some(verdict);
            return out;
        }
    }
    let def_tags: Vec<&str> = ctx.defender_tags.iter().map(String::as_str).collect();
    match check_hoi4_battle(&text, ctx.contested_province, &def_tags) {
        Hoi4BattleCheck::Alive => {
            if seed.roster_wanted {
                match tactical_save::SaveParser::parse_save(&path) {
                    Ok(save) => out.save = Some(save),
                    Err(e) => out
                        .log_lines
                        .push(format!("roster sync skipped (save parse): {e}")),
                }
            }
        }
        Hoi4BattleCheck::Unknown => {
            out.log_lines.push(
                "battle-alive check inconclusive (unreadable save) — battle continues".to_string(),
            );
        }
        Hoi4BattleCheck::Ended { winner } => {
            out.log_lines.push(format!(
                "HOI4-side battle over — strategic winner {winner:?} (post-sync save check)"
            ));
            out.ended = Some(winner);
        }
    }
    out
}

/// The main-thread half of the follow-up: replays the worker's log trail,
/// then applies the outcome with the inline path's mapping — a fired guard
/// returns the verdict (the caller opens the desync dialog and skips the
/// rest), an externally ended battle resolves the session, a still-running
/// battle diffs the roster onto the board.
#[allow(clippy::too_many_arguments)]
fn apply_sync_followup(
    outcome: FollowupOutcome,
    game: &mut Option<ResMut<GameController>>,
    state: &mut Option<ResMut<TacticalState>>,
    checkpoints: &mut Option<ResMut<Checkpoints>>,
    notice: &mut Option<ResMut<NoticeBar>>,
    tables: &Option<Res<LiveTables>>,
    loc: &Option<Res<LocaleRes>>,
    now_secs: f32,
) -> Option<tactical_sync::desync::DesyncVerdict> {
    for line in &outcome.log_lines {
        inject_log_line(line);
    }
    if let Some(verdict) = outcome.verdict {
        return Some(verdict);
    }
    let (Some(game), Some(loc)) = (game.as_mut(), loc.as_ref()) else {
        return None;
    };
    if let Some(winner) = outcome.ended {
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
        return None;
    }
    if let Some(save) = outcome.save {
        // Only a RUNNING board takes roster changes — a tactically
        // resolved battle (ReadyToEnd) is already counting down to the
        // exit. Re-checked here: the board may have resolved while the
        // worker was in flight.
        if game.session.phase == tactical_sync::BattlePhase::TacticalActive {
            roster_sync_now(
                game,
                state,
                checkpoints,
                notice,
                tables,
                loc,
                &save,
                now_secs,
            );
        }
    }
    None
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
/// it must not rewind). The snapshot arrives pre-parsed from the sync
/// worker — the parse is the expensive half and runs off the UI thread.
#[allow(clippy::too_many_arguments)]
fn roster_sync_now(
    game: &mut GameController,
    state: &mut Option<ResMut<TacticalState>>,
    checkpoints: &mut Option<ResMut<Checkpoints>>,
    notice: &mut Option<ResMut<NoticeBar>>,
    tables: &Option<Res<LiveTables>>,
    loc: &LocaleRes,
    save: &tactical_save::SaveGame,
    now: f32,
) {
    let (Some(state), Some(tables)) = (state.as_mut(), tables.as_ref()) else {
        return;
    };
    let Some(grid) = state.grid.clone() else {
        return;
    };
    let naming = crate::naming::localized_unit_naming(&loc.0);
    let zones = state.deployment_zones.clone().unwrap_or_default();
    let report = crate::scenario::apply_roster_sync(
        &mut game.session,
        &grid,
        &zones,
        &mut state.units,
        save,
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
    state.prune_selection();
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

// ── Background sync worker (§8.4) ────────────────────────────────────
// The hourly sync's blocking half — console injection, the clock-receipt
// wait (up to CLOCK_ADVANCE_TIMEOUT), the forced-savegame battle-alive
// check and the roster save parse — runs on a one-shot worker thread so
// the battle window stays responsive behind the `SyncProgress` modal
// (drain_injections owns the spawn, poll_sync_worker the outcome). FINAL
// batches stay synchronous: the window closes right after anyway. The
// worker is SILENT — every fc_inject.log line lands on the main thread
// from the outcome data, in the synchronous path's order (two threads
// must never interleave writes there).

/// A result slot shared with a one-shot sync worker thread.
type SyncSlot = std::sync::Arc<std::sync::Mutex<Option<Result<SyncOutcome, String>>>>;

/// One background sync job. Carries only owned data — the worker thread
/// never touches ECS state.
enum SyncJob {
    /// Regular hourly sync: probe the clock, inject the batch, wait for
    /// the receipt, then run the post-sync follow-up.
    Hourly {
        /// The batch as it will be sent (verdict line already prepended).
        batch1: Vec<String>,
        /// The session's authoritative clock hour + 1 — the desync
        /// guard's exact-match expectation (§3.2/§8.2).
        expected_prefix: Option<String>,
    },
    /// Stall dialog "Retry Sync": re-wait for the clock ONLY (the damage
    /// lines already landed — re-sending them would double-apply), with a
    /// catch-up reading counting as success without re-sending the clock
    /// command; the deferred follow-up runs on success.
    StallRetry {
        /// Game-hour prefix captured before the stalled batch.
        prev_prefix: String,
        /// The session's last sealed receipt hour — the expectation is
        /// never the stale pre-batch probe.
        session_last: Option<String>,
    },
    /// Stall dialog "Cancel Sync": accept the unconfirmed hour — only the
    /// deferred follow-up runs.
    FollowupOnly,
}

/// The job shape [`poll_sync_worker`] needs to map a resolved outcome: an
/// hourly receipt timeout OPENS the stall dialog; a stall-retry timeout
/// bumps the attempt count and re-opens it.
#[derive(Clone)]
enum SyncJobKind {
    Hourly,
    StallRetry { prev_prefix: String, attempts: u32 },
    FollowupOnly,
}

/// A background sync in flight (§8.4). Present only while the worker runs
/// — the drain holds all injection work behind it (ending-battle cleanup
/// excepted) and the render side draws the progress modal.
#[derive(Resource)]
struct SyncWorker {
    slot: SyncSlot,
    kind: SyncJobKind,
}

/// The worker's pure-data return: every log line and state mutation is
/// applied on the main thread from these fields.
#[derive(Default)]
struct SyncOutcome {
    /// Injection failure detail (batch or clock-command) — the main
    /// thread writes the fc_inject.log failure trail from this.
    inject_err: Option<String>,
    /// The batch as SENT (verdict line included); empty when nothing went
    /// out. Written to the reconciliation log by the main thread.
    sent_batch: Vec<String>,
    /// The pre-batch clock probe — a stall dialog re-waits against it.
    prev_prefix: Option<String>,
    /// The clock-receipt wait's verdict; None when no wait ran (no clock
    /// command in the batch, dry run, or a failed pre-probe).
    receipt: Option<ClockReceipt>,
    /// The post-sync follow-up's worker half; None while deferred (a
    /// receipt timeout/mismatch holds it for the dialog's resolution).
    followup: Option<FollowupOutcome>,
}

/// Spawn `job` on a background thread and return its result slot (the
/// menu's worker pattern). A panicking worker comes back as Err — the
/// mutex must never poison (the main thread `.lock().unwrap()`s the
/// slot), and the battle flow must never silently lose the outcome.
fn spawn_sync_worker(live: &LiveInjector, job: SyncJob, seed: Option<FollowupSeed>) -> SyncSlot {
    // The worker injects through its OWN injector, built from the same
    // paths — the resource's injector is never shared across threads.
    let injector = build_injector(&live.batch_path, &live.log_path);
    let dry = live.dry;
    let saves_dir = live.saves_dir.clone();
    let slot: SyncSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let writer = slot.clone();
    std::thread::spawn(move || {
        let outcome: Result<SyncOutcome, String> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_sync_job(&injector, dry, saves_dir, job, seed)
            }))
            .map_err(|payload| {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string payload>".to_string());
                format!("sync worker panicked: {msg}")
            });
        *writer.lock().unwrap() = Some(outcome);
    });
    slot
}

/// The worker body: I/O and parsing only, in exactly the synchronous
/// path's sequence — probe BEFORE the batch (its trailing clock command
/// must move AWAY from this reading), inject, wait for the receipt, then
/// the follow-up. A receipt timeout/mismatch DEFERS the follow-up to the
/// dialog's resolution, so it does not run here.
fn run_sync_job(
    injector: &Injector,
    dry: bool,
    saves_dir: Option<std::path::PathBuf>,
    job: SyncJob,
    seed: Option<FollowupSeed>,
) -> SyncOutcome {
    let mut out = SyncOutcome::default();
    match job {
        SyncJob::Hourly {
            batch1,
            expected_prefix,
        } => {
            // The verdict line rides the FRONT of the batch, so the last
            // line is still the clock-advance command when there is one.
            let advances_clock = batch1
                .last()
                .is_some_and(|l| l == tactical_sync::CLOCK_ADVANCE_COMMAND);
            let prev_prefix = if advances_clock {
                current_clock_prefix(injector, dry)
            } else {
                None
            };
            if !batch1.is_empty() {
                if let Err(e) = injector.inject_commands(&batch1, None, dry) {
                    // A failed hourly batch ends the sync there — no
                    // receipt wait, no follow-up (the inline Err arm).
                    out.inject_err = Some(e.to_string());
                    return out;
                }
            }
            out.sent_batch = batch1;
            out.prev_prefix = prev_prefix.clone();
            if let Some(prev) = &prev_prefix {
                let receipt = wait_clock_advance(injector, prev, expected_prefix.as_deref());
                let advanced = matches!(receipt, ClockReceipt::Advanced { .. });
                out.receipt = Some(receipt);
                if !advanced {
                    return out;
                }
            }
            out.followup = seed.map(|s| run_sync_followup(injector, dry, saves_dir, s));
        }
        SyncJob::StallRetry {
            prev_prefix,
            session_last,
        } => {
            // The hour may have caught up on its own — a stale
            // `pause_in_hours` timer fires once the player leaves the
            // menu. A moved prefix counts as success WITHOUT re-sending
            // the clock command — and the desync guard's exact match
            // applies to the catch-up reading too (the unpause that
            // fired the timer may have run the clock ahead). The
            // expectation is the SESSION's authoritative hour,
            // never the stale pre-batch probe.
            let receipt = match current_clock_prefix(injector, dry) {
                Some(cur) if prefix_moved(&prev_prefix, &cur) => match session_last.as_deref() {
                    Some(last) => match tactical_sync::desync::check_clock_receipt(last, &cur) {
                        tactical_sync::desync::DesyncVerdict::Ok => {
                            ClockReceipt::Advanced { found: cur }
                        }
                        tactical_sync::desync::DesyncVerdict::HourMismatch { expected, found } => {
                            ClockReceipt::Mismatch { expected, found }
                        }
                        tactical_sync::desync::DesyncVerdict::TagMismatch { .. } => {
                            ClockReceipt::Timeout
                        }
                    },
                    None => ClockReceipt::Advanced { found: cur },
                },
                _ => {
                    let expected = session_last
                        .as_deref()
                        .and_then(tactical_sync::desync::next_game_hour);
                    match injector.inject_commands(
                        &[tactical_sync::CLOCK_ADVANCE_COMMAND.to_string()],
                        None,
                        dry,
                    ) {
                        Ok(()) => wait_clock_advance(injector, &prev_prefix, expected.as_deref()),
                        Err(e) => {
                            out.inject_err = Some(e.to_string());
                            ClockReceipt::Timeout
                        }
                    }
                }
            };
            let advanced = matches!(receipt, ClockReceipt::Advanced { .. });
            out.receipt = Some(receipt);
            if advanced {
                out.followup = seed.map(|s| run_sync_followup(injector, dry, saves_dir, s));
            }
        }
        SyncJob::FollowupOnly => {
            out.followup = seed.map(|s| run_sync_followup(injector, dry, saves_dir, s));
        }
    }
    out
}

/// What a resolved background sync means for the session and the dialogs
/// — the pure mapping out of [`poll_sync_worker`], unit-tested directly;
/// the system only applies it to ECS state and writes the log trail.
#[derive(Default)]
struct SyncResolution {
    /// fc_inject.log trail lines, in the synchronous path's order.
    log_lines: Vec<String>,
    /// Injection failure detail for [`inject_failure_log`].
    inject_err: Option<String>,
    /// Reconciliation batch-log payload (the batch as sent); empty when
    /// the job injected none.
    sent_batch: Vec<String>,
    /// Seal the session's authoritative clock hour — the next sync
    /// expects this + 1 game hour.
    seal_prefix: Option<String>,
    /// (Re-)open the sync-stall dialog with this state.
    stall: Option<SyncStall>,
    /// Open the desync guard dialog with this verdict.
    desync: Option<tactical_sync::desync::DesyncVerdict>,
    /// Stall-retry success extras (receipt-confirmed trail + the
    /// battle-log line).
    retry_confirmed: bool,
    /// The follow-up to apply on the main thread.
    followup: Option<FollowupOutcome>,
}

/// Map a finished worker's outcome exactly as the synchronous path did:
/// Advanced seals the session's clock hour; an hourly Timeout opens the
/// stall dialog (a stall retry's re-opens it with the attempt count
/// bumped); a Mismatch raises the desync guard; the follow-up applies
/// unless the receipt deferred it.
fn resolve_sync_outcome(kind: &SyncJobKind, outcome: SyncOutcome) -> SyncResolution {
    let mut res = SyncResolution {
        inject_err: outcome.inject_err,
        sent_batch: outcome.sent_batch,
        followup: outcome.followup,
        ..default()
    };
    match (kind, outcome.receipt) {
        (_, Some(ClockReceipt::Advanced { found })) => {
            res.seal_prefix = Some(found);
            if matches!(kind, SyncJobKind::StallRetry { .. }) {
                res.retry_confirmed = true;
                res.log_lines
                    .push("clock receipt confirmed after player retry".to_string());
            }
        }
        (SyncJobKind::Hourly, Some(ClockReceipt::Timeout)) => {
            match outcome.prev_prefix {
                Some(prev) => {
                    res.log_lines.push(format!(
                        "clock receipt TIMEOUT (prev={prev}, final=false) — player dialog opened"
                    ));
                    res.stall = Some(SyncStall {
                        prev_prefix: prev,
                        attempts: 1,
                        action: SyncStallAction::Pending,
                    });
                    // The follow-up waits for the dialog's resolution.
                    res.followup = None;
                }
                None => {
                    // Defensive: without a probe baseline a retry would
                    // have nothing to re-wait against — log-and-continue
                    // instead of opening the dialog.
                    res.log_lines.push(
                        "clock receipt TIMEOUT (no probe baseline, final=false) — flow continued"
                            .to_string(),
                    );
                }
            }
        }
        (
            SyncJobKind::StallRetry {
                prev_prefix,
                attempts,
            },
            Some(ClockReceipt::Timeout),
        ) => {
            let attempts = *attempts + 1;
            res.log_lines.push(format!(
                "clock receipt TIMEOUT on retry (attempts={attempts}, prev={prev_prefix})"
            ));
            res.stall = Some(SyncStall {
                prev_prefix: prev_prefix.clone(),
                attempts,
                action: SyncStallAction::Pending,
            });
            res.followup = None;
        }
        // FollowupOnly runs no receipt wait — unreachable, kept for
        // exhaustiveness.
        (SyncJobKind::FollowupOnly, Some(ClockReceipt::Timeout)) => {}
        (_, Some(ClockReceipt::Mismatch { expected, found })) => {
            if matches!(kind, SyncJobKind::StallRetry { .. }) {
                res.log_lines.push(format!(
                    "clock receipt MISMATCH on retry (expected {expected}, found {found}) — guard dialog opened"
                ));
            }
            res.desync =
                Some(tactical_sync::desync::DesyncVerdict::HourMismatch { expected, found });
            res.followup = None;
        }
        (_, None) => {}
    }
    res
}

/// Main-thread drain of a finished background sync (§8.4): applies the
/// resolved outcome with exactly the synchronous path's mapping — the log
/// trail first (the worker stays silent), then the receipt seal / stall
/// dialog / desync guard, then the post-sync follow-up's board mutations.
/// Registered BEFORE drain_injections so a landed receipt seal precedes
/// any queued batch's injection.
#[allow(clippy::too_many_arguments)]
fn poll_sync_worker(
    mut commands: Commands,
    live: Res<LiveInjector>,
    worker: Option<ResMut<SyncWorker>>,
    mut game: Option<ResMut<GameController>>,
    mut state: Option<ResMut<TacticalState>>,
    mut checkpoints: Option<ResMut<Checkpoints>>,
    mut notice: Option<ResMut<NoticeBar>>,
    tables: Option<Res<LiveTables>>,
    loc: Option<Res<LocaleRes>>,
    time: Res<Time>,
) {
    let Some(worker) = worker else { return };
    let Some(result) = worker.slot.lock().unwrap().take() else {
        return;
    };
    let kind = worker.kind.clone();
    // Resolved: the progress modal closes and the drain gate re-opens.
    commands.remove_resource::<SyncWorker>();
    commands.remove_resource::<SyncProgress>();

    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            // A panicked worker leaves the sync's fate unknown — fail
            // open (log-and-continue), like an injection failure: the
            // battle flow must never wedge on the receipt.
            warn!("sync worker failed: {e} — flow continued");
            inject_log_line(&format!("sync worker failed: {e} — flow continued"));
            return;
        }
    };
    let hourly = matches!(kind, SyncJobKind::Hourly);
    let res = resolve_sync_outcome(&kind, outcome);

    // The fc_inject.log trail, in the synchronous path's order: the
    // failure detail, the reconciliation batch dump, then the receipt /
    // follow-up lines the worker collected.
    if let Some(e) = &res.inject_err {
        inject_failure_log(&live, e, false);
    }
    if !res.sent_batch.is_empty() && !live.dry {
        inject_batch_log("hourly sync", &res.sent_batch);
    }
    for line in &res.log_lines {
        inject_log_line(line);
    }
    if let Some(found) = res.seal_prefix {
        if let Some(game) = game.as_mut() {
            game.session.last_receipt_prefix = Some(found);
        }
    }
    if res.retry_confirmed {
        info!("clock advance confirmed on player retry");
        if let (Some(game), Some(loc)) = (&mut game, &loc) {
            game.log_line_icon(Some(IconId::Sync), loc.tr("log.sync.stall_retry"));
        }
    }
    if let Some(stall) = res.stall {
        // A fresh stall surfaces to the player; a re-stall after a failed
        // retry only bumps the dialog's attempt count.
        if stall.attempts == 1 {
            warn!("clock-advance receipt timed out — sync-stall dialog opened");
            if let (Some(game), Some(loc)) = (&mut game, &loc) {
                game.log_line_icon(Some(IconId::Warning), loc.tr("log.sync.stalled"));
            }
        }
        commands.insert_resource(stall);
        return;
    }
    if let Some(verdict) = res.desync {
        if hourly {
            warn!("clock receipt mismatch — desync guard dialog opened");
        }
        raise_desync_alert(&mut commands, &mut game, &loc, verdict);
        return;
    }
    if let Some(followup) = res.followup {
        if let Some(verdict) = apply_sync_followup(
            followup,
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

/// Drain pending injection requests and push them into HOI4 (§3.2).
///
/// Async hourly sync (§8.4): a HOURLY batch's whole blocking half — the
/// pre-batch clock probe, the injection, the receipt wait and the
/// post-sync follow-up — runs on a one-shot background worker
/// ([`SyncWorker`]) while the `SyncProgress` modal keeps the board frozen
/// and the window responsive; [`poll_sync_worker`] then applies the
/// outcome on the main thread with exactly the synchronous mapping. A
/// worker in flight holds ALL further injection work (the ending-battle
/// exception below excepted), so a queued batch can never inject ahead of
/// an unconfirmed hour.
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
/// collapse): only phase 2 goes out then. FINAL batches stay fully
/// synchronous here — the window closes right after, so the wait cannot
/// strand a responsive window.
///
/// Roster sync (§8.2): the state/tables params feed the follow-up seed
/// the worker carries (the roster save parse runs off-thread; the board
/// mutations land from [`poll_sync_worker`]).
///
/// Sync stall (§8.4): a HOURLY batch whose receipt wait times out suspends
/// the flow behind the sync-stall dialog (the `SyncStall` resource) — the
/// player checks HOI4, then picks Retry (clock-command only: the damage
/// lines already landed, re-sending them would double-apply) or Cancel
/// (the plain log-and-continue path). Both arms spawn a worker too — the
/// retry re-waits up to the full receipt timeout. The deferred post-sync
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
    state: Option<ResMut<TacticalState>>,
    mut notice: Option<ResMut<NoticeBar>>,
    tables: Option<Res<LiveTables>>,
    loc: Option<Res<LocaleRes>>,
    mut stall: Option<ResMut<SyncStall>>,
    mut desync: Option<ResMut<DesyncAlert>>,
    worker: Option<Res<SyncWorker>>,
    time: Res<Time>,
) {
    // While a stall dialog is unanswered, hold ALL injection
    // work; resolve it here once the player clicks through.
    // EXCEPTION: an ending battle (Ended/ReadyToEnd) never holds back the
    // queued cleanup batch — the exit countdown does not wait for the
    // dialog, and the mod must still get its unfreeze/abort lines.
    let session_ending = game
        .as_ref()
        .map(|g| {
            matches!(
                g.session.phase,
                tactical_sync::BattlePhase::Ended | tactical_sync::BattlePhase::ReadyToEnd
            )
        })
        .unwrap_or(false);
    // A sync worker in flight holds all injection work behind the
    // progress modal — the poll lands the outcome first (it runs before
    // this system), so a Some worker here is still genuinely running.
    // Same ending-battle exception as the dialogs.
    if worker.is_some() && !session_ending {
        return;
    }
    if let Some(stall) = &mut stall {
        match stall.action {
            SyncStallAction::Pending if !session_ending => return,
            SyncStallAction::Pending => {}
            // An ending battle never re-waits the receipt: the follow-up
            // it protects is moot on a dying session, and a worker here
            // would inject concurrently with the synchronous cleanup
            // batch. Close the dialog and fall through to the final batch.
            SyncStallAction::Retry | SyncStallAction::Cancel if session_ending => {
                inject_log_line("sync stall dialog dismissed by battle end — flow continued");
                commands.remove_resource::<SyncStall>();
            }
            SyncStallAction::Retry => {
                // The retry's re-wait can hold for the full receipt
                // timeout — run it on the background worker like the
                // hourly path (the dialog closes for the flight and
                // re-opens on another timeout; poll_sync_worker maps the
                // outcome exactly as the inline arm did).
                let session_last = game
                    .as_ref()
                    .and_then(|g| g.session.last_receipt_prefix.clone());
                let job = SyncJob::StallRetry {
                    prev_prefix: stall.prev_prefix.clone(),
                    session_last,
                };
                let slot = spawn_sync_worker(&live, job, followup_seed(&game, &state, &tables));
                commands.insert_resource(SyncWorker {
                    slot,
                    kind: SyncJobKind::StallRetry {
                        prev_prefix: stall.prev_prefix.clone(),
                        attempts: stall.attempts,
                    },
                });
                commands.remove_resource::<SyncStall>();
                commands.insert_resource(SyncProgress);
                return;
            }
            SyncStallAction::Cancel => {
                inject_log_line("sync receipt wait dismissed by player — flow continued");
                if let (Some(game), Some(loc)) = (&mut game, &loc) {
                    game.log_line_icon(Some(IconId::Warning), loc.tr("log.sync.stall_cancel"));
                }
                // Only the deferred post-sync follow-up remains — the
                // worker runs it (the save parse is the expensive half).
                let slot = spawn_sync_worker(
                    &live,
                    SyncJob::FollowupOnly,
                    followup_seed(&game, &state, &tables),
                );
                commands.insert_resource(SyncWorker {
                    slot,
                    kind: SyncJobKind::FollowupOnly,
                });
                commands.remove_resource::<SyncStall>();
                commands.insert_resource(SyncProgress);
                return;
            }
        }
    }
    // While the desync guard's dialog is unanswered, hold ALL injection
    // work; resolve the player's choice here. Same ending-battle
    // exception as the stall dialog above.
    if let Some(alert) = &mut desync {
        match alert.action {
            DesyncAction::Pending if !session_ending => return,
            DesyncAction::Pending => {}
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
    if !req.is_final {
        // Hourly sync: the whole blocking half (pre-batch probe → inject →
        // receipt wait → post-sync follow-up) moves to a background worker
        // so the window stays responsive; the progress modal and the
        // gameplay gates freeze the board for the flight, and
        // poll_sync_worker applies the outcome with the synchronous
        // mapping. The EXPECTED receipt is the session's authoritative
        // clock hour + 1 — never a fresh probe: a drifted clock (player
        // unpaused mid-battle) would validate its own drift.
        let batch1 = with_version_line(&live.version_line, &req.batch);
        let expected_prefix = game
            .as_ref()
            .and_then(|g| g.session.last_receipt_prefix.clone())
            .and_then(|p| tactical_sync::desync::next_game_hour(&p));
        let job = SyncJob::Hourly {
            batch1,
            expected_prefix,
        };
        let slot = spawn_sync_worker(&live, job, followup_seed(&game, &state, &tables));
        commands.insert_resource(SyncWorker {
            slot,
            kind: SyncJobKind::Hourly,
        });
        commands.insert_resource(SyncProgress);
        return;
    }
    // FINAL batches stay fully synchronous — the window closes right
    // after, so the wait cannot strand a responsive window.
    let advances_clock = req
        .batch
        .last()
        .is_some_and(|l| l == tactical_sync::CLOCK_ADVANCE_COMMAND);
    let prev_prefix = if advances_clock {
        current_clock_prefix(&live.injector, live.dry)
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
    let batch1 = with_version_line(&live.version_line, &req.batch);
    let phase1 = if req.batch.is_empty() {
        Ok(())
    } else {
        live.injector.inject_commands(&batch1, None, live.dry)
    };
    match phase1 {
        Ok(()) => {
            info!(
                "injected {} console commands (final={})",
                req.batch.len(),
                req.is_final
            );
            // The sent values land in fc_inject.log for
            // reconciliation (empty phase 1 injected nothing). The batch
            // logged is the one SENT — verdict line included.
            if !batch1.is_empty() && !live.dry {
                inject_batch_log("final phase 1", &batch1);
            }
            if let Some(prev) = prev_prefix {
                match wait_clock_advance(&live.injector, &prev, expected_prefix.as_deref()) {
                    ClockReceipt::Advanced { found } => {
                        // Seal the session's authoritative clock hour —
                        // the next sync expects found + 1 game hour.
                        if let Some(game) = game.as_mut() {
                            game.session.last_receipt_prefix = Some(found);
                        }
                    }
                    ClockReceipt::Timeout => {
                        // Log-and-continue for finals: phase 2 (unfreeze +
                        // cleanup) must never wait on a dialog.
                        warn!("clock-advance receipt timed out — continuing anyway");
                        inject_log_line(&format!(
                            "clock receipt TIMEOUT (prev={prev}, final=true) — flow continued"
                        ));
                    }
                    ClockReceipt::Mismatch { expected, found } => {
                        // A final batch's phase 2 (unfreeze + cleanup)
                        // must not be held hostage by a dialog.
                        warn!("clock receipt mismatch on final batch — continuing anyway");
                        inject_log_line(&format!(
                            "clock receipt MISMATCH (expected {expected}, found {found}, final=true) — flow continued"
                        ));
                    }
                }
            }
            // Final phase 2 (unfreeze + cleanup): no retry on failure —
            // FinalRetry covers phase 1 only; the inject.log trail is the
            // post-mortem.
            if let Some(phase2) = &req.phase2 {
                let batch2 = with_version_line(&live.version_line, phase2);
                match live.injector.inject_commands(&batch2, None, live.dry) {
                    Ok(()) => {
                        info!("injected final phase 2 ({} commands)", phase2.len());
                        if !batch2.is_empty() && !live.dry {
                            inject_batch_log("final phase 2", &batch2);
                        }
                    }
                    Err(e) => inject_failure_log(&live, &format!("final phase 2: {e}"), true),
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

/// The set of country tags whose base plate this battle colors: every tag it
/// fields a division for (the script's division→tag table; the allied
/// contingents, which live battles use instead), every live belligerent
/// (`belligerent_tags` — an enemy-side co-belligerent is in nobody's
/// `allies` entry), plus both side tags.
fn battle_tag_set(spec: &BattleSpec) -> std::collections::HashSet<String> {
    spec.division_tags
        .values()
        .chain(spec.allies.iter().map(|a| &a.tag))
        .chain(spec.belligerent_tags.iter())
        .chain(std::iter::once(&spec.atk_tag))
        .chain(std::iter::once(&spec.def_tag))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_is_prepended_to_non_empty_batches_only() {
        let line = Some("eval_effect GER = { d_tac_ver_ok = yes }".to_string());
        let batch = vec!["eval_effect ITA = { d_tac_sync_hourly = yes }".to_string()];
        assert_eq!(
            with_version_line(&line, &batch),
            vec![
                "eval_effect GER = { d_tac_ver_ok = yes }".to_string(),
                "eval_effect ITA = { d_tac_sync_hourly = yes }".to_string(),
            ]
        );
        // An empty batch injects nothing at all — a lone verdict line would
        // turn a no-op batch into a real synchronization.
        assert!(with_version_line(&line, &[]).is_empty());
        // A pre-handshake mod (no verdict) leaves every batch untouched.
        assert_eq!(with_version_line(&None, &batch), batch);
        assert!(with_version_line(&None, &[]).is_empty());
    }

    /// A spec with nothing but the fields `battle_tag_set` reads: a live
    /// battle carries no `division_tags` and no `allies` for an enemy-side
    /// co-belligerent, and such a tag must reach the color table all the
    /// same.
    fn tag_spec(belligerents: &[&str]) -> BattleSpec {
        BattleSpec {
            grid: tactical_core::HexGrid::new(2, 2, tactical_core::Terrain::Plains),
            units: Vec::new(),
            zones: (Vec::new(), Vec::new()),
            player_side: tactical_core::Side::Attacker,
            enemy_tactic: tactical_ai::CombatTactic::Default,
            atk_tag: "GER".to_string(),
            def_tag: "SOV".to_string(),
            location: String::new(),
            vp_label: None,
            province: None,
            flags: None,
            seed: None,
            division_tags: std::collections::HashMap::new(),
            allies: Vec::new(),
            belligerent_tags: belligerents.iter().map(|t| t.to_string()).collect(),
            battle_ctx: None,
            retreat_rim: None,
            staging: Vec::new(),
            writeback_mode: tactical_sync::WritebackMode::default(),
            start_datetime: None,
            roster: tactical_sync::BattleRoster::default(),
        }
    }

    #[test]
    fn battle_tag_set_covers_every_belligerent() {
        let mut spec = tag_spec(&["HUN"]);
        // The script path's per-division registry and the player-side
        // contingents both keep their say.
        spec.division_tags
            .insert("2. Inf".to_string(), "ITA".to_string());
        spec.allies = vec![tactical3d_render::game::AllyContingent {
            tag: "ROU".to_string(),
            tactic: tactical_ai::CombatTactic::Assault,
            divisions: vec!["3. Inf".to_string()],
        }];

        let tags = battle_tag_set(&spec);

        for expected in ["HUN", "ITA", "ROU", "GER", "SOV"] {
            assert!(tags.contains(expected), "{expected} missing from {tags:?}");
        }
        assert_eq!(tags.len(), 5, "nothing but belligerents: {tags:?}");
    }

    /// A finished hourly job carrying a follow-up, for outcome-mapping
    /// tests.
    fn hourly_outcome(receipt: Option<ClockReceipt>) -> SyncOutcome {
        SyncOutcome {
            receipt,
            prev_prefix: Some("1939.09.01.04".to_string()),
            sent_batch: vec!["eval_effect GER = { d_tac_ver_ok = yes }".to_string()],
            followup: Some(FollowupOutcome::default()),
            ..default()
        }
    }

    #[test]
    fn hourly_advanced_seals_clock() {
        let res = resolve_sync_outcome(
            &SyncJobKind::Hourly,
            hourly_outcome(Some(ClockReceipt::Advanced {
                found: "1939.09.01.05".to_string(),
            })),
        );
        assert_eq!(res.seal_prefix.as_deref(), Some("1939.09.01.05"));
        assert!(res.stall.is_none() && res.desync.is_none());
        assert!(!res.retry_confirmed);
        assert!(res.followup.is_some(), "the follow-up applies");
        assert!(res.log_lines.is_empty(), "no extra trail on success");
    }

    #[test]
    fn hourly_timeout_opens_stall_and_defers_followup() {
        let res = resolve_sync_outcome(
            &SyncJobKind::Hourly,
            hourly_outcome(Some(ClockReceipt::Timeout)),
        );
        let stall = res.stall.expect("an hourly timeout opens the stall dialog");
        assert_eq!(stall.prev_prefix, "1939.09.01.04");
        assert_eq!(stall.attempts, 1);
        assert_eq!(stall.action, SyncStallAction::Pending);
        assert!(
            res.followup.is_none(),
            "the follow-up waits for the dialog's resolution"
        );
        assert!(
            res.log_lines
                .iter()
                .any(|l| l.contains("TIMEOUT") && l.contains("player dialog opened")),
            "the timeout trail line: {:?}",
            res.log_lines
        );
    }

    #[test]
    fn retry_timeout_restalls_with_bumped_attempts() {
        let kind = SyncJobKind::StallRetry {
            prev_prefix: "1939.09.01.04".to_string(),
            attempts: 1,
        };
        let outcome = SyncOutcome {
            receipt: Some(ClockReceipt::Timeout),
            followup: Some(FollowupOutcome::default()),
            ..default()
        };
        let res = resolve_sync_outcome(&kind, outcome);
        let stall = res.stall.expect("a failed retry re-opens the stall dialog");
        assert_eq!(stall.prev_prefix, "1939.09.01.04");
        assert_eq!(stall.attempts, 2, "the attempt count bumps");
        assert!(res.followup.is_none());
        assert!(
            res.log_lines
                .iter()
                .any(|l| l.contains("TIMEOUT on retry (attempts=2")),
            "the retry-timeout trail line: {:?}",
            res.log_lines
        );
    }

    #[test]
    fn retry_advanced_seals_with_confirmation_trail() {
        let kind = SyncJobKind::StallRetry {
            prev_prefix: "1939.09.01.04".to_string(),
            attempts: 1,
        };
        let outcome = SyncOutcome {
            receipt: Some(ClockReceipt::Advanced {
                found: "1939.09.01.05".to_string(),
            }),
            followup: Some(FollowupOutcome::default()),
            ..default()
        };
        let res = resolve_sync_outcome(&kind, outcome);
        assert_eq!(res.seal_prefix.as_deref(), Some("1939.09.01.05"));
        assert!(res.retry_confirmed);
        assert!(
            res.log_lines
                .iter()
                .any(|l| l.contains("confirmed after player retry")),
            "the retry-confirmed trail line: {:?}",
            res.log_lines
        );
        assert!(res.followup.is_some(), "the deferred follow-up runs");
    }

    #[test]
    fn hourly_mismatch_raises_desync_guard() {
        let res = resolve_sync_outcome(
            &SyncJobKind::Hourly,
            hourly_outcome(Some(ClockReceipt::Mismatch {
                expected: "1939.09.01.05".to_string(),
                found: "1939.09.01.07".to_string(),
            })),
        );
        assert_eq!(
            res.desync,
            Some(tactical_sync::desync::DesyncVerdict::HourMismatch {
                expected: "1939.09.01.05".to_string(),
                found: "1939.09.01.07".to_string(),
            })
        );
        assert!(res.stall.is_none());
        assert!(
            res.followup.is_none(),
            "a mismatch skips the round's checks"
        );
        assert!(res.log_lines.is_empty(), "the guard writes its own trail");
    }

    #[test]
    fn retry_mismatch_raises_desync_guard_with_trail() {
        let kind = SyncJobKind::StallRetry {
            prev_prefix: "1939.09.01.04".to_string(),
            attempts: 1,
        };
        let outcome = SyncOutcome {
            receipt: Some(ClockReceipt::Mismatch {
                expected: "1939.09.01.05".to_string(),
                found: "1939.09.01.07".to_string(),
            }),
            ..default()
        };
        let res = resolve_sync_outcome(&kind, outcome);
        assert!(matches!(
            res.desync,
            Some(tactical_sync::desync::DesyncVerdict::HourMismatch { .. })
        ));
        assert!(
            res.log_lines
                .iter()
                .any(|l| l.contains("MISMATCH on retry")),
            "the retry-mismatch trail line: {:?}",
            res.log_lines
        );
    }

    #[test]
    fn followup_only_passes_the_followup_through() {
        let outcome = SyncOutcome {
            followup: Some(FollowupOutcome::default()),
            ..default()
        };
        let res = resolve_sync_outcome(&SyncJobKind::FollowupOnly, outcome);
        assert!(res.followup.is_some());
        assert!(res.seal_prefix.is_none() && res.stall.is_none() && res.desync.is_none());
        assert!(!res.retry_confirmed);
    }

    #[test]
    fn hourly_inject_failure_carries_the_error_and_moves_on() {
        let outcome = SyncOutcome {
            inject_err: Some("SendInput failed".to_string()),
            ..default()
        };
        let res = resolve_sync_outcome(&SyncJobKind::Hourly, outcome);
        assert_eq!(res.inject_err.as_deref(), Some("SendInput failed"));
        assert!(res.stall.is_none() && res.desync.is_none() && res.followup.is_none());
        assert!(res.sent_batch.is_empty(), "a failed inject logs no batch");
    }
}
