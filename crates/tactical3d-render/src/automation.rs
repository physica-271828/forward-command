//! Battle-window turn automation (DESIGN.md §6.2 turn loop, §8.1 time model,
//! §8.2 hourly sync, §9.2 command panel): a fast-play master switch plus three
//! session-scoped toggles drawn next to the End Turn button —
//!
//! 0. *RTS mode (master, experimental)*: one checkbox drives the whole RTS
//!    preset — the three toggles below forced on and combat reports never
//!    opened (the fire phase settles its FX and deferred visuals directly
//!    instead of arming the report tour) — while the turn/sync paces come
//!    from the mode's own speed slider (1.0× default). The top notice bar
//!    goes silent too: turn-handover banners (flashed and pinned) and the
//!    sync-ready hints never show, and the hourly sync summary never pops
//!    up (its numbers land in the battle log) — only the deadlock-fuse halt
//!    and the battle-resolved cue still come through. ON snapshots the
//!    driven values, OFF restores them; the slider is the mode's own knob
//!    and rides along untouched. Space pauses/resumes the flow.
//! 1. *Auto-advance quiet rounds*: a round whose settlement had no fire /
//!    assault engagement ends itself, no End Turn click needed. A round with
//!    combat pauses the auto-advance for the player; one manual End Turn
//!    resumes it (or, when reports auto-close, it simply carries on). A
//!    12-round deadlock fuse (no movement AND no combat) halts the advance
//!    with a notice-bar flash and unchecks the toggle.
//! 2. *Auto-close reports*: each combat-report window dismisses itself after
//!    its configured delay instead of waiting for [Continue].
//! 3. *Auto-sync*: the ReadyToSync hour boundary fires the existing Sync
//!    path on its own, and the sync-completion summary dismisses itself
//!    after its own configured delay.
//!
//! The toggles are session-scoped by design: never written to settings.json
//! and reset to all-off at every battle start (the Deployment →
//! TacticalActive edge). The driver is a plain Update system that issues the
//! SAME [`PlayerCommand`]s the buttons issue, driven by Bevy [`Time`] — no
//! thread sleeps, and never more than one turn per frame. The round/fuse
//! rules are pure functions (unit-tested below) so the core behaviour is
//! Bevy-free.

use bevy::prelude::*;
use tactical_core::hex::HexCoord;
use tactical_core::unit::{BattalionUnit, Side};
use tactical_sync::BattlePhase;

use crate::game::{GameController, PendingCommands, PlayerCommand};
use crate::locale::LocaleRes;
use crate::state::{
    BattleTour, DesyncAlert, FlashNotice, NoticeBar, SyncProgress, SyncStall, TacticalState,
    UiWindows,
};

/// Deadlock fuse: this many consecutive rounds with neither movement nor
/// combat halt the auto-advance (the battle is parked — nobody is gaining
/// anything by clicking through it).
pub const FUSE_LIMIT: u32 = 12;

/// Pace between a completed round's evaluation and the automatic End Turn —
/// enough frames for the handover banner and any march slides to read, while
/// an unattended battle still rolls at a sensible rate. Field-default; the
/// fast-play preset cuts it to [`FAST_END_PACE_SECS`].
const DEFAULT_END_PACE_SECS: f32 = 2.0;

/// Settle delay after entering ReadyToSync before the auto-sync fires — the
/// "Hour complete" notice gets a beat instead of vanishing under the click.
/// Field-default; the fast-play preset cuts it to [`FAST_SYNC_SETTLE_SECS`].
const DEFAULT_SYNC_SETTLE_SECS: f32 = 0.8;

/// RTS-mode paces at 1.0× speed: the base turn cadence and sync settle the
/// speed slider scales (a slider value of N divides them). The sync summary
/// is dismissed after a blink regardless of speed (combat reports never
/// open at all).
const FAST_END_PACE_SECS: f32 = 0.6;
const FAST_SYNC_SETTLE_SECS: f32 = 0.6;
const FAST_CLOSE_SECS: f32 = 0.5;

/// RTS-mode speed slider bounds and default (1.0× = the base paces above).
pub const FAST_SPEED_MIN: f32 = 0.5;
pub const FAST_SPEED_MAX: f32 = 4.0;
pub const FAST_SPEED_DEFAULT: f32 = 1.0;

/// Auto-close delay bounds (one slider per consumer: combat reports get
/// `report_close_secs`, the sync summary gets `sync_close_secs`).
pub const CLOSE_SECS_MIN: f32 = 0.5;
pub const CLOSE_SECS_MAX: f32 = 10.0;
pub const CLOSE_SECS_DEFAULT: f32 = 3.0;

/// The session-scoped automation state (DESIGN §9.2 side panel). The pub
/// fields are the player-facing toggles/sliders the UI draws; everything
/// else is the driver's bookkeeping and is deliberately not persisted anywhere.
#[derive(Resource)]
pub struct TurnAutomation {
    // ── Player-facing toggles ─────────────────────────────────────────
    /// Master switch: the whole RTS preset in one checkbox — the three
    /// toggles below forced on, the paces cut, combat reports suppressed.
    /// ON snapshots the driven values (restored on OFF). Session-scoped.
    pub fast_mode: bool,
    /// Space pause: the auto-flow holds (no auto End Turn, no auto-sync)
    /// while the board stays fully interactive — issue orders, Space resumes.
    pub paused: bool,
    /// Box 1: quiet rounds (no fire/assault engagement) end themselves.
    pub skip_quiet_turns: bool,
    /// Box 2: combat-report windows auto-close after `report_close_secs`.
    pub skip_reports: bool,
    /// Box 3: the hour boundary syncs itself; the summary auto-closes.
    pub auto_sync: bool,
    /// Combat-report auto-close delay in seconds (slider range-clamped).
    pub report_close_secs: f32,
    /// Sync-summary auto-close delay in seconds (slider range-clamped).
    pub sync_close_secs: f32,
    /// Auto End Turn pace: seconds of clean player window before the
    /// scheduled End Turn fires (fast-play preset cuts it).
    pub end_pace_secs: f32,
    /// Settle seconds at ReadyToSync before the auto-sync fires
    /// (fast-play preset cuts it).
    pub sync_settle_secs: f32,
    /// RTS-mode speed multiplier — the mode's own slider. NOT part of the
    /// ON/OFF snapshot: it only scales the fast paces, so it survives
    /// re-enabling the mode within the session.
    pub fast_speed: f32,

    // ── Driver bookkeeping ────────────────────────────────────────────
    /// Auto-advance may fire. False until the first round is evaluated, and
    /// after a round with combat whose reports need a human (box 2 off).
    armed: bool,
    /// Consecutive deadlocked rounds (no movement, no combat).
    fuse: u32,
    /// A round completed since the last evaluation (session turn/side/phase
    /// moved) and is waiting for a clean player window to judge.
    round_pending: bool,
    /// Position snapshot at the last window edge — the fuse's "did anything
    /// move" ground truth (diffed at the next evaluation).
    window_pos: Vec<(usize, HexCoord)>,
    /// `TacticalState::combat_reports` at the last window edge — the round's
    /// combat side of the activity verdict (tour-independent, so fast play's
    /// suppressed reports still count).
    window_reports: u64,
    /// Elapsed-seconds deadline for the scheduled auto End Turn.
    end_at: Option<f32>,
    /// True while the last frame's state was a clean, actionable player
    /// window — the clean edge restarts the pace countdown.
    was_clean: bool,
    /// When the current ReadyToSync phase was entered (auto-sync settle).
    sync_at: Option<f32>,
    /// When the open sync-completion prompt first appeared (auto-close).
    prompt_at: Option<f32>,
    /// The driven values snapshot at the fast-mode ON edge (restored on OFF).
    fast_snapshot: Option<FastSnapshot>,
    // ── Previous-frame watches ────────────────────────────────────────
    prev_phase: Option<BattlePhase>,
    prev_session: Option<(BattlePhase, Side, u32)>,
    prev_skip_quiet: bool,
    prev_fast_mode: bool,
    prev_fast_speed: f32,
}

/// The seven player-facing values the fast-mode master drives: snapshotted
/// at the ON edge, written back at the OFF edge.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FastSnapshot {
    skip_quiet_turns: bool,
    skip_reports: bool,
    auto_sync: bool,
    report_close_secs: f32,
    sync_close_secs: f32,
    end_pace_secs: f32,
    sync_settle_secs: f32,
}

impl FastSnapshot {
    fn of(auto: &TurnAutomation) -> Self {
        FastSnapshot {
            skip_quiet_turns: auto.skip_quiet_turns,
            skip_reports: auto.skip_reports,
            auto_sync: auto.auto_sync,
            report_close_secs: auto.report_close_secs,
            sync_close_secs: auto.sync_close_secs,
            end_pace_secs: auto.end_pace_secs,
            sync_settle_secs: auto.sync_settle_secs,
        }
    }

    fn restore(self, auto: &mut TurnAutomation) {
        auto.skip_quiet_turns = self.skip_quiet_turns;
        auto.skip_reports = self.skip_reports;
        auto.auto_sync = self.auto_sync;
        auto.report_close_secs = self.report_close_secs;
        auto.sync_close_secs = self.sync_close_secs;
        auto.end_pace_secs = self.end_pace_secs;
        auto.sync_settle_secs = self.sync_settle_secs;
    }
}

impl Default for TurnAutomation {
    fn default() -> Self {
        TurnAutomation {
            fast_mode: false,
            paused: false,
            skip_quiet_turns: false,
            skip_reports: false,
            auto_sync: false,
            report_close_secs: CLOSE_SECS_DEFAULT,
            sync_close_secs: CLOSE_SECS_DEFAULT,
            end_pace_secs: DEFAULT_END_PACE_SECS,
            sync_settle_secs: DEFAULT_SYNC_SETTLE_SECS,
            fast_speed: FAST_SPEED_DEFAULT,
            armed: false,
            fuse: 0,
            round_pending: false,
            window_pos: Vec::new(),
            window_reports: 0,
            end_at: None,
            was_clean: false,
            sync_at: None,
            prompt_at: None,
            fast_snapshot: None,
            prev_phase: None,
            prev_session: None,
            prev_skip_quiet: false,
            prev_fast_mode: false,
            prev_fast_speed: FAST_SPEED_DEFAULT,
        }
    }
}

impl TurnAutomation {
    /// The fast-play preset: every toggle on, every pace set from the speed
    /// slider (pure — the master edge's ON half; the snapshot/restore pair
    /// is unit-tested).
    fn apply_fast_preset(&mut self) {
        self.skip_quiet_turns = true;
        self.skip_reports = true;
        self.auto_sync = true;
        self.report_close_secs = FAST_CLOSE_SECS;
        self.sync_close_secs = FAST_CLOSE_SECS;
        self.apply_fast_pace();
    }

    /// The speed slider's half of the preset: base paces divided by the
    /// multiplier. Re-applied live on every slider move while the mode is on.
    fn apply_fast_pace(&mut self) {
        self.end_pace_secs = FAST_END_PACE_SECS / self.fast_speed;
        self.sync_settle_secs = FAST_SYNC_SETTLE_SECS / self.fast_speed;
    }

    /// A new round window opens at `units`' current layout: every round
    /// accumulator and the pacing deadline reset, the position snapshot
    /// refreshes. Used at battle start, toggle-on and rollback.
    /// `combat_reports` is the board's monotonic report counter — the
    /// window's combat ground truth is the delta to the next evaluation.
    fn open_window(&mut self, units: &[BattalionUnit], combat_reports: u64) {
        self.armed = false;
        self.fuse = 0;
        self.round_pending = false;
        self.window_pos = position_snapshot(units);
        self.window_reports = combat_reports;
        self.end_at = None;
        self.sync_at = None;
    }
}

/// What a completed round actually did — the two signals the automation
/// reacts to (from the existing settlement data: battle reports for combat,
/// the unit roster's positions for movement).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundActivity {
    /// The round's fire phases raised ≥1 engagement report.
    pub combat: bool,
    /// Any unit changed hex between the window edges.
    pub movement: bool,
}

/// The driver update produced by folding one completed round in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundVerdict {
    /// Whether auto-advance may fire on the next quiet round.
    pub armed: bool,
    /// Consecutive deadlocked rounds so far (0 whenever anything happened).
    pub fuse: u32,
    /// The deadlock fuse tripped — auto-advance must halt and notify.
    pub fuse_tripped: bool,
}

/// Fold one completed round into the driver state (pure — the unit tests
/// pin the whole auto-skip contract here).
///
/// * A round WITHOUT combat re-arms the auto-advance (quiet rounds and pure
///   marching rounds skip themselves).
/// * A round WITH combat disarms it — the player is expected to react, and
///   one manual End Turn resumes the skip. With report auto-close on, the
///   pause is only the report's display time, so the advance stays armed.
/// * The deadlock fuse counts rounds with neither movement nor combat and
///   trips at `fuse_limit`; any activity resets the count.
pub fn after_round(activity: RoundActivity, skip_reports: bool, fuse_before: u32, fuse_limit: u32) -> RoundVerdict {
    let armed = !activity.combat || skip_reports;
    let (fuse, fuse_tripped) = if activity.combat || activity.movement {
        (0, false)
    } else {
        let next = fuse_before + 1;
        if next >= fuse_limit {
            (0, true)
        } else {
            (next, false)
        }
    };
    RoundVerdict {
        armed,
        fuse,
        fuse_tripped,
    }
}

/// Position snapshot of every unit (id → hex) — the movement side of the
/// round activity.
pub fn position_snapshot(units: &[BattalionUnit]) -> Vec<(usize, HexCoord)> {
    units.iter().map(|u| (u.id, u.position)).collect()
}

/// Whether the roster moved since `snapshot` was taken: any unit off its
/// recorded hex counts, and so does a unit id the snapshot lacks (joined) or
/// a snapshot id no longer on the roster (left) — both are board activity.
pub fn any_unit_moved(snapshot: &[(usize, HexCoord)], units: &[BattalionUnit]) -> bool {
    use std::collections::{HashMap, HashSet};
    let at: HashMap<usize, HexCoord> = snapshot.iter().copied().collect();
    let present: HashSet<usize> = units.iter().map(|u| u.id).collect();
    units
        .iter()
        .any(|u| at.get(&u.id).map_or(true, |p| *p != u.position))
        || snapshot.iter().any(|(id, _)| !present.contains(id))
}

/// Per-frame driver (registered at the end of the game-loop chain, after the
/// UI commands drained and `tick_battle` observed this frame's transitions).
///
/// Jobs:
/// * item 0 — the fast-mode master edge (snapshot ⇄ preset ⇄ restore);
/// * item 3 — auto-sync at ReadyToSync, then auto-close the summary prompt;
/// * item 1 — judge each completed round at the player's next clean window
///   and schedule the automatic End Turn (held while `paused`);
/// * battle-start / toggle-on / rollback reset edges.
///
/// Item 2 (report auto-close) lives in the report window's draw system,
/// which is the only place that knows when a report is actually on screen.
pub fn tick_turn_automation(
    mut auto: ResMut<TurnAutomation>,
    game: Option<Res<GameController>>,
    state: Option<Res<TacticalState>>,
    tour: Res<BattleTour>,
    mut ui_win: ResMut<UiWindows>,
    mut pending: ResMut<PendingCommands>,
    mut notice: ResMut<NoticeBar>,
    time: Res<Time>,
    loc: Res<LocaleRes>,
    stall: Option<Res<SyncStall>>,
    desync: Option<Res<DesyncAlert>>,
    sync_progress: Option<Res<SyncProgress>>,
) {
    let (Some(game), Some(state)) = (game, state) else { return };
    let now = time.elapsed_secs();
    let phase = game.session.phase;
    let player = state.player_side;

    // ── Battle-start reset ─────────────────────────────────────────────
    // Every battle begins with all toggles off (session-scoped, DESIGN
    // §9.2) — the edge also covers a Restart → Begin Battle re-entry, which
    // returns to Deployment first. A fresh round window opens at once.
    let battle_started = auto.prev_phase == Some(BattlePhase::Deployment)
        && phase == BattlePhase::TacticalActive;
    if battle_started {
        *auto = TurnAutomation::default();
        auto.open_window(&state.units, state.combat_reports);
        auto.prev_phase = Some(phase);
        auto.prev_session = Some((phase, game.session.current_side, game.session.turn_number));
        auto.prev_skip_quiet = false;
        return;
    }
    auto.prev_phase = Some(phase);

    // Rollback to a checkpoint mid-battle (Restart/rollback): the turn
    // counter rewinds — re-arm from a clean slate (the toggles themselves
    // survive; only the driver state resets).
    let session = (phase, game.session.current_side, game.session.turn_number);
    let rewound = phase == BattlePhase::TacticalActive
        && auto
            .prev_session
            .is_some_and(|(_, _, t)| game.session.turn_number < t);
    if rewound {
        auto.open_window(&state.units, state.combat_reports);
    }

    // Fast-mode master edge: ON snapshots the driven values and applies the
    // RTS preset; OFF restores the snapshot. Either edge clears the Space
    // pause. The preset flips skip_quiet_turns on, so the toggle edge below
    // re-arms the round bookkeeping the same frame.
    if auto.fast_mode != auto.prev_fast_mode {
        if auto.fast_mode {
            auto.fast_snapshot = Some(FastSnapshot::of(&auto));
            auto.apply_fast_preset();
            // The top bar goes silent for the mode: drop any in-flight
            // turn/sync banner so it does not linger into the flow.
            notice.flash = None;
            notice.pending = None;
        } else if let Some(snap) = auto.fast_snapshot.take() {
            snap.restore(&mut auto);
        }
        auto.paused = false;
        auto.prev_fast_mode = auto.fast_mode;
    }
    // Speed-slider edge: re-pace live while the mode runs, restarting the
    // End Turn countdown so a speed-up is felt this turn rather than after
    // the old wait. The slider itself is not snapshotted — OFF/ON keeps it.
    if auto.fast_speed != auto.prev_fast_speed {
        if auto.fast_mode {
            auto.apply_fast_pace();
            auto.end_at = None;
        }
        auto.prev_fast_speed = auto.fast_speed;
    }
    // Any session move — a side handed over, a turn ticking, an hour
    // boundary sync — means a round (or the last half of one) just settled.
    // Mark it for evaluation at the player's next clean window. Battle-end
    // phases are excluded: automation never judges or pushes past a result.
    let session_moved = session != auto.prev_session.unwrap_or(session);
    auto.prev_session = Some(session);
    if auto.skip_quiet_turns
        && session_moved
        && matches!(phase, BattlePhase::TacticalActive | BattlePhase::ReadyToSync)
    {
        auto.round_pending = true;
    }

    // The toggle-ON edge re-arms the round bookkeeping: whatever the battle
    // did before the player enabled automation does not count towards the
    // fuse or the pause state.
    if auto.skip_quiet_turns && !auto.prev_skip_quiet {
        auto.open_window(&state.units, state.combat_reports);
    }
    auto.prev_skip_quiet = auto.skip_quiet_turns;

    // Sync-stall / desync dialogs: the sync pipe is unresolved and the
    // player must choose — freeze the automation entirely (it must not push
    // turns behind an unanswered modal). Same while a sync worker is in
    // flight: the board is frozen behind the progress modal.
    if stall.is_some() || desync.is_some() || sync_progress.is_some() {
        return;
    }

    // A "clean player window": the player's side may act, no report tour is
    // up, no modal surface blocks the board, no command is queued. This is
    // where completed rounds get judged and where an auto End Turn may land.
    let clean = phase == BattlePhase::TacticalActive
        && game.session.current_side == player
        && !tour.active
        && !ui_win.modal_open()
        && state.div_pick.is_none()
        && state.command_mode == crate::state::CommandMode::Select
        && pending.0.is_empty();

    // ── Item 1: judge the completed round at the clean window ─────────
    // A round_pending set by a turn/side/phase transition is consumed once:
    // the round's activity decides the armed state and the fuse. Combat is
    // the report-counter delta across the window — tour-independent, so
    // fast play's suppressed reports still feed the fuse.
    if auto.skip_quiet_turns && auto.round_pending && clean {
        auto.round_pending = false;
        let activity = RoundActivity {
            combat: state.combat_reports != auto.window_reports,
            movement: any_unit_moved(&auto.window_pos, &state.units),
        };
        let verdict = after_round(activity, auto.skip_reports, auto.fuse, FUSE_LIMIT);
        auto.armed = verdict.armed;
        auto.fuse = verdict.fuse;
        // New round window starts from the current layout.
        auto.window_pos = position_snapshot(&state.units);
        auto.window_reports = state.combat_reports;
        if verdict.fuse_tripped {
            // 12 deadlocked rounds: the battle is parked — stop advancing,
            // say so on the notice bar, and uncheck the toggle.
            auto.armed = false;
            auto.skip_quiet_turns = false;
            let n = FUSE_LIMIT.to_string();
            notice.flash = Some(FlashNotice::plain(
                loc.trf("battle.notice.auto.fuse", &[("n", &n)]),
                now + 8.0,
            ));
        }
    }

    // ── Item 3: auto-sync at the hour boundary ────────────────────────
    // ReadyToSync holds the whole world; once the boundary round's reports
    // are read and a beat has passed, push the same Sync command the button
    // would. Never in desync mode (the session never rests there), never
    // behind an open summary or a stall (handled above), never while paused.
    if phase == BattlePhase::ReadyToSync {
        if auto.sync_at.is_none() {
            auto.sync_at = Some(now);
        }
        let ready = auto.auto_sync
            && !auto.paused
            && !game.session.sync_disabled
            && !tour.active
            && !ui_win.modal_open()
            && pending.0.is_empty()
            && now - auto.sync_at.unwrap_or(now) >= auto.sync_settle_secs;
        if ready {
            pending.0.push(PlayerCommand::Sync);
            auto.sync_at = None;
        }
    } else {
        auto.sync_at = None;
    }

    // ── Item 3: the sync-completion summary auto-closes ───────────────
    // The prompt opens when the hour seals (tick_battle). With auto-sync on
    // it dismisses itself after the shared delay; closing it counts as the
    // player's one manual nudge, so auto-advance resumes on the spot.
    let prompt_open = ui_win.sync_prompt && !ui_win.confirm_end;
    if auto.auto_sync && prompt_open {
        if auto.prompt_at.is_none() {
            auto.prompt_at = Some(now);
        } else if now - auto.prompt_at.unwrap() >= auto.sync_close_secs {
            ui_win.sync_prompt = false;
            auto.prompt_at = None;
            if auto.skip_quiet_turns {
                auto.armed = true;
                auto.fuse = 0;
                auto.round_pending = false;
                auto.window_pos = position_snapshot(&state.units);
                auto.window_reports = state.combat_reports;
            }
        }
    } else {
        auto.prompt_at = None;
    }

    // ── Item 1: schedule the automatic End Turn ───────────────────────
    // The pace counts only clean time: any interruption (menu, report,
    // prompt) restarts the countdown from the next clean frame. The Space
    // pause holds the schedule — the board stays interactive for orders.
    if clean && !auto.was_clean {
        auto.end_at = None;
    }
    auto.was_clean = clean;
    if auto.skip_quiet_turns && auto.armed && clean && !auto.paused {
        let pace = auto.end_pace_secs;
        let at = *auto.end_at.get_or_insert(now + pace);
        if now >= at {
            pending.0.push(PlayerCommand::EndTurn);
            auto.end_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(combat: bool, movement: bool) -> RoundActivity {
        RoundActivity { combat, movement }
    }

    /// A round with neither combat nor movement is a deadlocked one — it
    /// re-arms the auto-advance (so the skip rolls on) and increments the
    /// fuse toward the halt limit.
    #[test]
    fn deadlocked_round_rearms_and_counts() {
        let v = after_round(act(false, false), false, 3, FUSE_LIMIT);
        assert!(v.armed, "auto-advance stays armed on a quiet round");
        assert_eq!(v.fuse, 4, "deadlock count grows");
        assert!(!v.fuse_tripped);
    }

    /// Pure marching rounds (movement, no combat) are exactly the rounds the
    /// auto-skip exists for: armed stays true and any deadlock progress
    /// resets.
    #[test]
    fn marching_round_rearms_and_resets_fuse() {
        let v = after_round(act(false, true), false, 11, FUSE_LIMIT);
        assert!(v.armed);
        assert_eq!(v.fuse, 0, "movement proves the battle is not parked");
        assert!(!v.fuse_tripped);
    }

    /// The combat stop: a round with ≥1 engagement disarms the auto-advance
    /// so the player can react — the next End Turn is hers, and the skip
    /// resumes from the round after it.
    #[test]
    fn combat_round_stops_auto_advance() {
        let v = after_round(act(true, true), false, 0, FUSE_LIMIT);
        assert!(!v.armed, "a combat round pauses the auto-advance");
        assert_eq!(v.fuse, 0, "combat is activity, not a deadlock");
        assert!(!v.fuse_tripped);
    }

    /// With report auto-close on, the combat pause is only the report's
    /// display time — the advance stays armed and carries on by itself.
    #[test]
    fn combat_round_with_auto_reports_keeps_advancing() {
        let v = after_round(act(true, false), true, 0, FUSE_LIMIT);
        assert!(v.armed, "auto-closed reports leave the advance armed");
        assert_eq!(v.fuse, 0);
        assert!(!v.fuse_tripped);
    }

    /// The deadlock fuse: FUSE_LIMIT consecutive no-movement-no-combat
    /// rounds trip the halt; anything before the last one merely counts up.
    #[test]
    fn twelve_deadlocked_rounds_trip_the_fuse() {
        let mut fuse = 0;
        for _ in 0..FUSE_LIMIT - 1 {
            let v = after_round(act(false, false), false, fuse, FUSE_LIMIT);
            assert!(!v.fuse_tripped, "round {} must not trip yet", fuse + 1);
            fuse = v.fuse;
        }
        let v = after_round(act(false, false), false, fuse, FUSE_LIMIT);
        assert!(v.fuse_tripped, "the 12th deadlocked round halts the advance");
        assert_eq!(v.fuse, 0, "the counter restarts after the trip");
    }

    /// Movement compares positions; a unit that joined or left the roster
    /// also counts as board activity.
    #[test]
    fn any_unit_moved_detects_roster_and_position_changes() {
        let u = |id: usize, q: i32, r: i32| {
            BattalionUnit::new(id, "X", tactical_core::unit::UnitType::Infantry, Side::Attacker, HexCoord::new(q, r))
        };
        let before = vec![u(1, 1, 1), u(2, 3, 3)];
        let snapshot = position_snapshot(&before);
        assert!(!any_unit_moved(&snapshot, &before));

        let mut moved = before.clone();
        moved[0].position = HexCoord::new(2, 1);
        assert!(any_unit_moved(&snapshot, &moved), "a hex change is movement");

        let joined = vec![before[0].clone(), u(9, 5, 5)];
        assert!(any_unit_moved(&snapshot, &joined), "a new id is movement");

        let left = vec![before[0].clone()];
        assert!(any_unit_moved(&snapshot, &left), "a departed id is movement");
    }

    /// The fast-play preset forces every toggle on and cuts every pace; the
    /// OFF edge's snapshot restore round-trips the player's prior setup.
    #[test]
    fn fast_preset_snapshot_restore_round_trips() {
        let mut auto = TurnAutomation::default();
        // A hand-tuned starting point: only box 3 on, custom delays.
        auto.auto_sync = true;
        auto.report_close_secs = 7.0;
        auto.sync_close_secs = 9.0;
        let snap = FastSnapshot::of(&auto);
        auto.apply_fast_preset();
        assert!(auto.skip_quiet_turns && auto.skip_reports && auto.auto_sync);
        assert!((auto.end_pace_secs - FAST_END_PACE_SECS).abs() < 1e-6);
        assert!((auto.sync_settle_secs - FAST_SYNC_SETTLE_SECS).abs() < 1e-6);
        assert!((auto.report_close_secs - FAST_CLOSE_SECS).abs() < 1e-6);
        assert!((auto.sync_close_secs - FAST_CLOSE_SECS).abs() < 1e-6);
        snap.restore(&mut auto);
        assert!(!auto.skip_quiet_turns && !auto.skip_reports && auto.auto_sync);
        assert!((auto.report_close_secs - 7.0).abs() < 1e-6);
        assert!((auto.sync_close_secs - 9.0).abs() < 1e-6);
        assert!((auto.end_pace_secs - DEFAULT_END_PACE_SECS).abs() < 1e-6);
        assert!((auto.sync_settle_secs - DEFAULT_SYNC_SETTLE_SECS).abs() < 1e-6);
    }

    /// The speed slider scales the RTS paces: 2.0× halves the waits, 0.5×
    /// doubles them, and 1.0× is the base pace. The slider is not part of
    /// the snapshot — restoring an OFF leaves it where the player put it.
    #[test]
    fn fast_speed_scales_paces() {
        let mut auto = TurnAutomation::default();
        auto.fast_speed = 2.0;
        auto.apply_fast_preset();
        assert!((auto.end_pace_secs - FAST_END_PACE_SECS / 2.0).abs() < 1e-6);
        assert!((auto.sync_settle_secs - FAST_SYNC_SETTLE_SECS / 2.0).abs() < 1e-6);
        auto.fast_speed = 0.5;
        auto.apply_fast_pace();
        assert!((auto.end_pace_secs - FAST_END_PACE_SECS * 2.0).abs() < 1e-6);
        assert!((auto.sync_settle_secs - FAST_SYNC_SETTLE_SECS * 2.0).abs() < 1e-6);
    }
}
