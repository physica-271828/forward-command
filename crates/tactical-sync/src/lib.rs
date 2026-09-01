//! tactical-sync — battle lifecycle state machine + tactical time system.
//!
//! Spec: DESIGN.md §8 (time system: 1 tactical turn = 10 min, 6 turns = 1
//! strategic hour, sync loop §8.2, phase flow §8.3), §6.11 (victory
//! conditions), §6.12 (turn order: attacker first), §3.2 (console injection
//! batches).

use std::fmt;

use tactical_core::{BattalionUnit, CombatParams, FlagState, Side};

pub mod desync;
pub mod roster;
pub mod savecheck;

pub use roster::{BattleRoster, RosterEntry};

/// §8.1: 1 tactical turn = 10 minutes of battle time.
pub const MINUTES_PER_TURN: u32 = 10;

/// §8.1 / §12 (`turns_per_strategic_hour`): 6 tactical turns = 1 strategic hour.
pub const DEFAULT_TURNS_PER_STRATEGIC_HOUR: u32 = 6;

// ── Phases ──────────────────────────────────────────────────────────────────

/// Battle lifecycle phases (DESIGN §8.3).
///
/// §8.3 names 8 states in its diagram: WAITING_FOR_TRIGGER → LAUNCHING →
/// DEPLOYMENT → TACTICAL_ACTIVE → (every 6 turns) READY_TO_SYNC →
/// SYNCHRONIZING → TACTICAL_ACTIVE … and, once the battle resolves,
/// READY_TO_END → INJECTING_FINAL → (loop back to) WAITING_FOR_TRIGGER, with
/// a `tac_abort` escape path from anywhere.
///
/// This crate models **9 phases total**: the 8 §8.3 states plus a terminal
/// [`BattlePhase::Ended`]. The diagram's "loop back to WAITING_FOR_TRIGGER"
/// is represented as `Ended` because one [`BattleSession`] is exactly one
/// battle; the application creates a fresh session for the next `tac_start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BattlePhase {
    /// Idle, listening to game.log for `tac_start` (§3.1).
    WaitingForTrigger,
    /// Save parsed, map generated, units initialized.
    Launching,
    /// Player adjusts auto-deployed positions (§9 Deployment).
    Deployment,
    /// Turn loop running; attacker acts first each turn (§6.12).
    TacticalActive,
    /// 6 turns completed (§8.1); [Sync] button armed, waiting for the player.
    ReadyToSync,
    /// Console injection of the hourly batch in flight (§3.2).
    Synchronizing,
    /// Victory detected (§6.11); [Apply & Exit] armed.
    ReadyToEnd,
    /// Final result injection in flight (§3.2 battle end batch).
    InjectingFinal,
    /// Terminal state (normal end or `tac_abort`, §8.3/§10.4).
    Ended,
}

impl fmt::Display for BattlePhase {
    /// Uppercase names exactly as in the §8.3 diagram.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            BattlePhase::WaitingForTrigger => "WAITING_FOR_TRIGGER",
            BattlePhase::Launching => "LAUNCHING",
            BattlePhase::Deployment => "DEPLOYMENT",
            BattlePhase::TacticalActive => "TACTICAL_ACTIVE",
            BattlePhase::ReadyToSync => "READY_TO_SYNC",
            BattlePhase::Synchronizing => "SYNCHRONIZING",
            BattlePhase::ReadyToEnd => "READY_TO_END",
            BattlePhase::InjectingFinal => "INJECTING_FINAL",
            BattlePhase::Ended => "ENDED",
        };
        f.write_str(name)
    }
}

/// Outcome of [`BattleSession::check_victory`] (§6.11); `Undecided` is
/// non-terminal — the battle continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VictoryOutcome {
    /// Both sides still have unresolved battalions — the battle continues.
    Undecided,
    /// One side is fully resolved; the other wins.
    Winner(Side),
    /// Both sides fully resolved (mutual annihilation) — no winner, but the
    /// battle ends here (a `None` result had no terminal path and stalled
    /// the GUI on an empty board).
    Draw,
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Guard failure on a [`BattleSession`] transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    /// The action is not legal in the current phase (§8.3).
    IllegalTransition {
        from: BattlePhase,
        action: &'static str,
    },
    /// `end_side_turn` was called for the side that is not to act (§6.12).
    OutOfTurn { expected: Side, attempted: Side },
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransitionError::IllegalTransition { from, action } => {
                write!(f, "illegal transition: {action} not allowed from {from}")
            }
            TransitionError::OutOfTurn {
                expected,
                attempted,
            } => {
                write!(
                    f,
                    "out of turn: {expected:?} to act, {attempted:?} tried to end"
                )
            }
        }
    }
}

impl std::error::Error for TransitionError {}

// ── Damage accounting ───────────────────────────────────────────────────────

/// One side's running org/str damage total for the current strategic hour.
///
/// Values are ratios of the affected divisions' max org/str — the
/// injection's `damage_units` lines apply them with `ratio = yes`
/// (§3.2, §10.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DamageRecord {
    pub side: Side,
    pub org: f32,
    pub str: f32,
}

/// Per-side damage totals for one strategic hour; the payload of the hourly
/// sync batch (§8.2, §3.2).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DamageSummary {
    pub attacker_org_lost: f32,
    pub attacker_str_lost: f32,
    pub defender_org_lost: f32,
    pub defender_str_lost: f32,
}

impl DamageSummary {
    /// `(org_lost, str_lost)` for one side.
    pub fn for_side(&self, side: Side) -> (f32, f32) {
        match side {
            Side::Attacker => (self.attacker_org_lost, self.attacker_str_lost),
            Side::Defender => (self.defender_org_lost, self.defender_str_lost),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.attacker_org_lost == 0.0
            && self.attacker_str_lost == 0.0
            && self.defender_org_lost == 0.0
            && self.defender_str_lost == 0.0
    }

    fn add(&mut self, side: Side, org: f32, str_dmg: f32) {
        match side {
            Side::Attacker => {
                self.attacker_org_lost += org;
                self.attacker_str_lost += str_dmg;
            }
            Side::Defender => {
                self.defender_org_lost += org;
                self.defender_str_lost += str_dmg;
            }
        }
    }
}

// ── Injection batches (§3.2) ────────────────────────────────────────────────

/// One console batch file's worth of command lines (§3.2). The injector's
/// `write_batch_file` re-encodes the same trailing-newline contract as
/// [`render_batch`] (the two encodings must stay in sync) and writes the
/// batch into the HOI4 user dir (the console `run` command only resolves
/// relative paths there — `%TEMP%` absolute paths fail with
/// "Couldn't find file"); the default name is `tac_inject.txt`, battle
/// instances use a per-process `tac_inject_<pid>.txt`.
pub type InjectionBatch = Vec<String>;

/// §3.2 injection procedure, step 1: the batch file body is the command
/// lines joined by `'\n'` PLUS a trailing newline — HOI4's `run` parser
/// silently drops a final unterminated line, so the trailing terminator is
/// mandatory here and callers must not re-append one.
pub fn render_batch(batch: &InjectionBatch) -> String {
    let mut body = batch.join("\n");
    body.push('\n');
    body
}

/// Gregorian month length for the absolute battle clock (HOI4's
/// calendar is the real one, leap years included). `month` is 1-based; an
/// out-of-range month degrades to 30 rather than panicking (§11.3).
pub(crate) fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 30,
    }
}

/// §8.4: the clock-advance command, appended as the LAST line
/// of every sync batch and of the end batch's phase 1 (when unsynced
/// turns remain). `pause_in_hours 1` switches to gamespeed 5 by itself,
/// runs exactly one game hour, then auto-pauses. It must
/// be last: the batch executes while the game is paused, so any line
/// after it would run immediately — BEFORE the hour elapses.
pub const CLOCK_ADVANCE_COMMAND: &str = "pause_in_hours 1";

/// The two phases of the battle-end injection (§8.4/§3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndBatches {
    /// Phase 1: collapse + the final partial hour's damage lines, then
    /// [`CLOCK_ADVANCE_COMMAND`] — but only when unsynced full turns
    /// remain; a battle ending exactly on a sync boundary already had its
    /// clock hour, so no extra one is burned. Runs while the battle state
    /// is still frozen: the advancing hour lets the org-zeroed divisions
    /// disintegrate under vanilla rules with the freeze zeroing combat
    /// damage. Can legitimately be EMPTY (battle ended with no unsynced
    /// turns, damage or collapse) — the host then skips to phase 2.
    pub phase1: InjectionBatch,
    /// Phase 2, fired only after phase 1's clock-advance receipt (or its
    /// timeout — a stuck freeze must never linger): the unfreeze sweep,
    /// then the scope-pinned end-battle cleanup (untag both sides + clear
    /// the mode flag + completion popup).
    pub phase2: InjectionBatch,
}

// ── damage_units writeback ───────────────────────────
//
// The damage channel runs on `damage_units` eval_effect one-liners: per-
// division script targeting does not exist, `province=` is the finest filter,
// `ratio = yes` removes percent-of-MAX points, and console-set variables are
// unreadable — so every value rides as a literal in the command line.

/// Player-facing damage writeback mode (Settings key `writeback`, DESIGN
/// §12): how battle losses are injected back into HOI4. Defender losses are
/// always province-exact when written (every division in an attacked
/// province defends it, so the contested province IS the semantic
/// boundary); attacker divisions sit in their source provinces mixed with
/// non-participants, which no script filter can separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritebackMode {
    /// Org + str, province precision: defender line at the contested
    /// province; attacker lines per source province (attacker str diluted
    /// across every attacker-tag division there — province-total exact).
    #[default]
    OrgStr,
    /// No damage writeback at all (sandbox mode; sync event + cleanup
    /// still run).
    Off,
}

impl WritebackMode {
    /// Settings-file token.
    pub fn as_str(&self) -> &'static str {
        match self {
            WritebackMode::OrgStr => "org_str",
            WritebackMode::Off => "off",
        }
    }

    /// Parse the settings-file token (unknown/blank → the default OrgStr;
    /// the retired tokens `dilute`/`native`/`org_only` ride the same
    /// default — the org-only mode was removed after its whole-army
    /// channel zeroed a national army; every damage line now rides
    /// province-scoped `damage_units`).
    pub fn parse(s: &str) -> Self {
        match s {
            "off" => WritebackMode::Off,
            _ => WritebackMode::OrgStr,
        }
    }
}

/// HOI4-side context of one attacker source province: the maxima sums that
/// turn tactical damage points into `damage_units` ratios. Every pool is
/// PER TAG — each country's line books against its own divisions' maxima.
#[derive(Debug, Clone, PartialEq)]
pub struct AttackerProvinceCtx {
    pub province: u32,
    /// Per-tag org pool of the PARTICIPATING attacker battalions there: Σ
    /// assembled battalion max_org (HQ excluded) — the same battalion-scale
    /// currency as the damage numerator (the HOI4 division-level org MEAN
    /// under-counts the base by the battalions-per-division factor).
    pub participants_max_org: std::collections::HashMap<String, f32>,
    /// Per-tag Σ max_strength of the participating attacker divisions there
    /// (a HOI4 division's strength IS its subunit sum — already the
    /// numerator's currency).
    pub participants_max_str: std::collections::HashMap<String, f32>,
    /// Per-tag Σ max_str of ALL of that tag's divisions there (dilution
    /// base — the line hits every matching division in the province).
    pub all_max_str: std::collections::HashMap<String, f32>,
}

/// HOI4 battle geometry + country identity resolved from the save's
/// land_combat record at assembly (None for synthetic/script/demo battles —
/// those never inject).
#[derive(Debug, Clone, PartialEq)]
pub struct BattleContext {
    /// Contested province (land_combat.location).
    pub contested_province: u32,
    /// Country tags of the two battle sides — a battle line can span
    /// several tags when allies fight together. The damage lines emit one
    /// `damage_units` command per tag (`limit = { tag = … }` filters by
    /// owning country); the first tag is the side's primary.
    pub attacker_tags: Vec<String>,
    pub defender_tags: Vec<String>,
    /// Per-tag pools of the participating DEFENDER divisions: (Σ assembled
    /// battalion max_org HQ-excluded, Σ division max_strength) — same
    /// currencies as the attacker pools.
    pub defender_max: std::collections::HashMap<String, (f32, f32)>,
    /// Attacker source provinces (sorted, deduped).
    pub attacker_provinces: Vec<AttackerProvinceCtx>,
}

/// Damage ratio below this is not worth a console line (and a zero line
/// would be a no-op anyway).
const DAMAGE_EPS: f32 = 0.0005;

/// One `damage_units` eval_effect line. Fields with a
/// ratio below [`DAMAGE_EPS`] are omitted; both are percent-of-MAX points
/// (`ratio = yes`).
fn damage_line(province: u32, tag: &str, org_ratio: f32, str_ratio: f32) -> String {
    let org = org_ratio.clamp(0.0, 1.0);
    let str_r = str_ratio.clamp(0.0, 1.0);
    let mut line =
        format!("eval_effect damage_units = {{ province = {province} limit = {{ tag = {tag} }}");
    if org >= DAMAGE_EPS {
        line.push_str(&format!(" org_damage = {org:.3}"));
    }
    if str_r >= DAMAGE_EPS {
        line.push_str(&format!(" str_damage = {str_r:.3}"));
    }
    line.push_str(" ratio = yes army = yes }");
    line
}

// ── Battle session ──────────────────────────────────────────────────────────

/// One tactical battle, from `tac_start` trigger to final injection (§8.3).
///
/// Time model (§8.1): each full turn (both sides acted, §6.12) advances
/// [`BattleSession::clock_minutes`] by 10 and [`BattleSession::turn_number`]
/// by 1; every `turns_per_hour` turns the session stops in
/// [`BattlePhase::ReadyToSync`] until the hourly batch is injected.
/// Clone: checkpoint snapshots (restart/rollback).
#[derive(Clone)]
pub struct BattleSession {
    /// Current lifecycle phase (§8.3).
    pub phase: BattlePhase,
    /// 1-based turn counter; increments only after the *defender* finishes,
    /// i.e. after both sides acted (§6.12).
    pub turn_number: u32,
    /// Completed strategic hours = completed syncs (§8.1/§8.2).
    pub strategic_hour: u32,
    /// Side currently acting; attacker always acts first (§6.12).
    pub current_side: Side,
    /// UI selection, carried across turns (§9.3).
    pub selected_unit_id: Option<usize>,
    /// Running per-side damage totals for the current (not yet synced)
    /// strategic hour; one record per side at most. Cleared by
    /// [`BattleSession::complete_sync`].
    pub accumulated_damage: Vec<DamageRecord>,
    /// Per-battalion ratio rollups (above) drive the UI; the
    /// INJECTION works in tactical damage POINTS attributed by province
    /// AND owner tag — attacker per (source province, tag), defender per
    /// tag at the contested one (its battalions' `hoi4_province`). The tag
    /// split is what lets each `damage_units` line (`limit = { tag = … }`
    /// reaches only the owning country's divisions) carry that country's
    /// OWN ratio instead of a side-wide smear. Cleared by `complete_sync`.
    attacker_damage_by_province:
        std::collections::HashMap<u32, std::collections::HashMap<String, (f32, f32)>>,
    /// Defender damage points (org, str) at the contested province, per tag.
    defender_damage_points: std::collections::HashMap<String, (f32, f32)>,
    /// HOI4 battle context from the save (live assemblies only).
    pub battle_ctx: Option<BattleContext>,
    /// Damage writeback mode (settings, DESIGN §12).
    pub writeback_mode: WritebackMode,
    /// Elapsed battle time in minutes. Turn 1 starts at 00:00, so this is
    /// always `(turn_number - 1) * MINUTES_PER_TURN` (§8.1).
    pub clock_minutes: u32,
    /// The in-game datetime `(year, month, day, hour)` the battle
    /// started at, from the save's `date` header (live assemblies only).
    /// Set → the battle clock displays absolute game time; `None` → the
    /// elapsed-only "HH:MM" display (demo/script battles).
    pub start_datetime: Option<(i32, u32, u32, u32)>,
    /// Authoritative last CONFIRMED clock hour (`yyyy.mm.dd.hh`, §8.4):
    /// initialized from `start_datetime`, sealed forward ONLY by an exact
    /// clock receipt (the desync-guard baseline). Never
    /// refreshed from a pre-batch probe — a drifted clock (player unpaused
    /// mid-battle) would otherwise validate its own drift.
    pub last_receipt_prefix: Option<String>,
    /// Player country tag from the `tac_start` message (§3.1); used as the
    /// scope pin of the sync-ack line in the sync batch (§3.2) and of the
    /// end batch's phase-2 cleanup lines.
    pub country_tag: String,
    /// Side the player commands (§3.1, resolved from the save's land_combat
    /// record). The sync layer is side-agnostic — damage/collapse lines key
    /// on the battle context's attacker/defender tags; the render/UI layer
    /// reads this for perspective and control gating.
    pub player_side: Side,
    /// §12 `turns_per_strategic_hour` (default 6).
    pub turns_per_hour: u32,
    /// Full turns completed within the current strategic hour.
    turns_in_current_hour: u32,
    /// Sealed per-hour summaries, one entry per completed sync (§8.2).
    hourly_history: Vec<DamageSummary>,
    /// §6.11: the battle's flag board — zones + capture progress.
    /// `None` = no flag zones, the annihilation path is the only conclusion.
    pub flags: Option<FlagState>,
    /// §6.11 collapse injection bookkeeping: the collapse fired and its
    /// org-zeroing batch lines have not been sent yet (the batch builders
    /// append them once, at the next sync/end).
    collapse_injected: bool,
    /// The mirrored HOI4 battle ended under the tactical one (the
    /// post-sync save scan found the `land_combat` gone — every division of
    /// the losing side routed under the injected damage, or a manual
    /// retreat). Records the STRATEGIC winner; [`BattleSession::check_victory`]
    /// reports it from here on, and the end flow (Apply&Exit two-phase
    /// batch) rides unchanged.
    external_winner: Option<Side>,
    /// The mid-battle HOI4 division roster (DESIGN §8.2) —
    /// diffed against the `land_combat` unit-id lists at every post-sync
    /// save snapshot; departures march off (`LeftBattle`), reinforcements
    /// enter at the map edge, and the damage-ratio pools re-derive from it.
    /// Empty outside live battles (every roster method is then a no-op).
    pub roster: BattleRoster,
    /// Desync guard (§8.2): set when the player chooses "continue unsynced"
    /// after the guard detected a broken sync pipe (clock receipt mismatch /
    /// played-country mismatch). The hourly sync is dead from here on: no
    /// clock advance, no snapshot check, no roster diff, no damage
    /// writeback, and every exit sends the abort cleanup batch only. The
    /// hour boundary seals locally (see `end_side_turn`) so the battle
    /// keeps turning without the sync stop.
    pub sync_disabled: bool,
}

impl BattleSession {
    /// Fresh session in [`BattlePhase::WaitingForTrigger`] with §12 defaults
    /// (6 turns/hour, tag `GER`, player = attacker).
    pub fn new() -> Self {
        Self {
            phase: BattlePhase::WaitingForTrigger,
            turn_number: 1,
            strategic_hour: 0,
            current_side: Side::Attacker,
            selected_unit_id: None,
            accumulated_damage: Vec::new(),
            attacker_damage_by_province: std::collections::HashMap::new(),
            defender_damage_points: std::collections::HashMap::new(),
            battle_ctx: None,
            writeback_mode: WritebackMode::default(),
            clock_minutes: 0,
            start_datetime: None,
            last_receipt_prefix: None,
            country_tag: "GER".to_string(),
            player_side: Side::Attacker,
            turns_per_hour: DEFAULT_TURNS_PER_STRATEGIC_HOUR,
            turns_in_current_hour: 0,
            hourly_history: Vec::new(),
            flags: None,
            collapse_injected: false,
            external_winner: None,
            roster: BattleRoster::default(),
            sync_disabled: false,
        }
    }

    /// Session configured from shared combat params (§12): uses
    /// `params.turns_per_strategic_hour`.
    pub fn with_params(params: &CombatParams) -> Self {
        Self {
            turns_per_hour: params.turns_per_strategic_hour,
            ..Self::new()
        }
    }

    fn require_phase(
        &self,
        allowed: &[BattlePhase],
        action: &'static str,
    ) -> Result<(), TransitionError> {
        if allowed.contains(&self.phase) {
            Ok(())
        } else {
            Err(TransitionError::IllegalTransition {
                from: self.phase,
                action,
            })
        }
    }

    // ── lifecycle transitions (§8.3) ────────────────────────────────────────

    /// WAITING_FOR_TRIGGER → LAUNCHING, on `tac_start` in game.log (§3.1).
    pub fn start_launching(&mut self) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::WaitingForTrigger], "start_launching")?;
        self.phase = BattlePhase::Launching;
        Ok(())
    }

    /// LAUNCHING → DEPLOYMENT, once save parse/map gen/unit init finished.
    pub fn start_deployment(&mut self) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::Launching], "start_deployment")?;
        self.phase = BattlePhase::Deployment;
        Ok(())
    }

    /// DEPLOYMENT → TACTICAL_ACTIVE ("Begin Battle"). Attacker acts first
    /// (§6.12); turn 1 starts at clock 00:00.
    pub fn start_battle(&mut self) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::Deployment], "start_battle")?;
        self.phase = BattlePhase::TacticalActive;
        self.turn_number = 1;
        self.clock_minutes = 0;
        self.turns_in_current_hour = 0;
        self.current_side = Side::Attacker;
        Ok(())
    }

    /// End the acting side's turn (§6.12).
    ///
    /// Attacker ends → defender to act. Defender ends → both sides acted:
    /// `turn_number` += 1, clock += 10 min (§8.1), attacker to act again.
    /// When `turns_per_hour` full turns complete, the session moves to
    /// [`BattlePhase::ReadyToSync`] and further turns are rejected until the
    /// sync cycle completes (§8.2).
    pub fn end_side_turn(&mut self, side: Side) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::TacticalActive], "end_side_turn")?;
        if side != self.current_side {
            return Err(TransitionError::OutOfTurn {
                expected: self.current_side,
                attempted: side,
            });
        }
        match side {
            Side::Attacker => self.current_side = Side::Defender,
            Side::Defender => {
                self.turn_number += 1;
                self.clock_minutes += MINUTES_PER_TURN;
                self.turns_in_current_hour += 1;
                self.current_side = Side::Attacker;
                if self.turns_in_current_hour >= self.turns_per_hour {
                    if self.sync_disabled {
                        // Desync mode: the hourly sync is dead, so there is
                        // no READY_TO_SYNC stop — the hour seals locally and
                        // the battle keeps turning (guard, §8.2).
                        self.seal_synced_hour();
                    } else {
                        self.phase = BattlePhase::ReadyToSync;
                    }
                }
            }
        }
        Ok(())
    }

    /// READY_TO_SYNC → SYNCHRONIZING (player clicked [Sync], §8.2).
    pub fn start_sync(&mut self) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::ReadyToSync], "start_sync")?;
        self.phase = BattlePhase::Synchronizing;
        Ok(())
    }

    /// SYNCHRONIZING → TACTICAL_ACTIVE after the hourly batch was injected.
    ///
    /// Seals the current hour's damage into [`BattleSession::damage_history`],
    /// increments `strategic_hour`, and resets the per-hour accumulators for
    /// the next hour (§8.2).
    pub fn complete_sync(&mut self) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::Synchronizing], "complete_sync")?;
        self.seal_synced_hour();
        self.phase = BattlePhase::TacticalActive;
        Ok(())
    }

    /// The hour-boundary bookkeeping shared by `complete_sync` and the
    /// desync-mode local seal in `end_side_turn` (§8.2): fold the current
    /// hour into history and reset the per-hour accumulators.
    fn seal_synced_hour(&mut self) {
        let summary = self.hourly_damage_summary();
        self.hourly_history.push(summary);
        self.accumulated_damage.clear();
        self.attacker_damage_by_province.clear();
        self.defender_damage_points.clear();
        self.turns_in_current_hour = 0;
        self.strategic_hour += 1;
        self.current_side = Side::Attacker;
    }

    /// Victory check per §6.11. Exact rule:
    ///
    /// - A battalion is *resolved* (out of the battle for good) in states
    ///   `Eliminated`, `Surrendered` (org 0 while fully encircled, §6.4),
    ///   `Withdrawn` (retreated off a deployment-zone edge, §6.8) and
    ///   `LeftBattle` (lingered out of bounds until it slipped away, §6.14).
    /// - `Active` and `Retreating` battalions are *unresolved*. Retreating
    ///   units have org 0 but have not "successfully retreated" yet (§6.11
    ///   requires org=0 **and** retreated/eliminated) — they can still be
    ///   annihilated by a fresh flanker en route (§6.8).
    /// - A side wins when the opposing side has no unresolved battalion left
    ///   while it still has at least one itself. If both sides are fully
    ///   resolved (mutual annihilation) the battle ends as a draw. A side
    ///   with no battalions at all counts as fully resolved.
    /// - §6.13: HQs are command units, not fighting units — a
    ///   side kept alive only by its HQs counts as fully resolved.
    /// - The unresolved/alive predicate is single-sourced as
    ///   [`BattalionUnit::counts_for_victory`] and shared with the headless
    ///   frontend, which used to mirror it with a slightly different rule.
    /// - Once the HOI4 side resolved the battle externally
    ///   ([`BattleSession::resolve_externally`]), that winner is reported
    ///   without consulting the board.
    pub fn check_victory(&self, units: &[BattalionUnit]) -> VictoryOutcome {
        // An externally-resolved (HOI4-side) ending is
        // authoritative — the tactical board keeps its units, so the
        // battalion scan below would report Undecided forever.
        if let Some(winner) = self.external_winner {
            return VictoryOutcome::Winner(winner);
        }
        let attacker_open = units
            .iter()
            .any(|u| u.side == Side::Attacker && u.counts_for_victory());
        let defender_open = units
            .iter()
            .any(|u| u.side == Side::Defender && u.counts_for_victory());
        match (attacker_open, defender_open) {
            (true, true) => VictoryOutcome::Undecided,
            (false, true) => VictoryOutcome::Winner(Side::Defender),
            (true, false) => VictoryOutcome::Winner(Side::Attacker),
            // Mutual annihilation — no winner (§6.11), but a terminal path:
            // without it the GUI stalls on an empty board.
            (false, false) => VictoryOutcome::Draw,
        }
    }

    /// → READY_TO_END once [`BattleSession::check_victory`] reports a
    /// terminal outcome (Winner or Draw).
    ///
    /// Legal from TACTICAL_ACTIVE and from READY_TO_SYNC — a battle resolved
    /// exactly on a sync boundary skips the pending sync; the unsent damage
    /// goes out in the battle end batch instead (§3.2).
    pub fn ready_to_end(&mut self) -> Result<(), TransitionError> {
        self.require_phase(
            &[BattlePhase::TacticalActive, BattlePhase::ReadyToSync],
            "ready_to_end",
        )?;
        self.phase = BattlePhase::ReadyToEnd;
        Ok(())
    }

    /// The mirrored HOI4 battle ended under the tactical one — the
    /// post-sync save scan (`savecheck`) found the `land_combat` gone.
    /// Records the strategic winner and moves to READY_TO_END, from where
    /// the normal Apply&Exit two-phase end batch rides unchanged (its
    /// remaining damage lines no-op against the routed divisions; no
    /// collapse line is needed — the HOI4 side already resolved itself).
    ///
    /// Legal from the same phases as [`BattleSession::ready_to_end`]; on any
    /// other phase (e.g. the tactical battle already resolved this hour)
    /// the caller logs and drops the detection.
    pub fn resolve_externally(&mut self, winner: Side) -> Result<(), TransitionError> {
        self.require_phase(
            &[BattlePhase::TacticalActive, BattlePhase::ReadyToSync],
            "resolve_externally",
        )?;
        self.external_winner = Some(winner);
        self.phase = BattlePhase::ReadyToEnd;
        Ok(())
    }

    /// READY_TO_END → INJECTING_FINAL (player clicked [Apply & Exit]).
    pub fn complete_end(&mut self) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::ReadyToEnd], "complete_end")?;
        self.phase = BattlePhase::InjectingFinal;
        Ok(())
    }

    /// INJECTING_FINAL → ENDED, after the battle end batch was injected.
    pub fn finish(&mut self) -> Result<(), TransitionError> {
        self.require_phase(&[BattlePhase::InjectingFinal], "finish")?;
        self.phase = BattlePhase::Ended;
        Ok(())
    }

    /// `tac_abort` escape path (§8.3, §10.4): legal from every phase,
    /// including `Ended` (idempotent). Any unsynced damage is lost (§11.3).
    pub fn abort(&mut self) {
        self.phase = BattlePhase::Ended;
    }

    // ── damage accounting ───────────────────────────────────────────────────

    /// Accumulate org/str damage suffered by `side`'s battalions into the
    /// current strategic hour. Called by the combat loop during
    /// TACTICAL_ACTIVE once per combat resolution (per victim and per
    /// counter-attacker).
    ///
    /// * `tag` — the victim battalion's owning country: the point rollups
    ///   key on it so each `damage_units` line (`limit = { tag = … }`)
    ///   carries that country's own ratio. Empty outside live assemblies
    ///   (no battle context → no lines, the points are inert).
    /// * `org_pts`/`str_pts` — tactical damage POINTS (the callers used to
    ///   pre-divide by the battalion maxima; the points form is what the
    ///   `damage_units` batches need).
    /// * `province` — the victim battalion's `hoi4_province` (attacker
    ///   source province / defender contested province); `None` outside
    ///   live assemblies (the point rollups are simply skipped then).
    /// * `max_org`/`max_str` — the victim battalion's maxima, used to keep
    ///   the legacy per-battalion RATIO rollup (`accumulated_damage`, the
    ///   UI/history display) unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn record_damage(
        &mut self,
        side: Side,
        tag: &str,
        province: Option<u32>,
        org_pts: f32,
        str_pts: f32,
        max_org: f32,
        max_str: f32,
    ) {
        let org = org_pts / max_org.max(1.0);
        let str_r = str_pts / max_str.max(1.0);
        match self.accumulated_damage.iter_mut().find(|r| r.side == side) {
            Some(rec) => {
                rec.org += org;
                rec.str += str_r;
            }
            None => self.accumulated_damage.push(DamageRecord {
                side,
                org,
                str: str_r,
            }),
        }
        // Point rollups for the damage_units batches, per (province,) tag.
        match side {
            Side::Attacker => {
                if let Some(p) = province {
                    let e = self
                        .attacker_damage_by_province
                        .entry(p)
                        .or_default()
                        .entry(tag.to_string())
                        .or_insert((0.0, 0.0));
                    e.0 += org_pts;
                    e.1 += str_pts;
                }
            }
            Side::Defender => {
                let e = self
                    .defender_damage_points
                    .entry(tag.to_string())
                    .or_insert((0.0, 0.0));
                e.0 += org_pts;
                e.1 += str_pts;
            }
        }
    }

    /// Fold the current hour's [`DamageRecord`]s into per-side totals — the
    /// payload of the sync batch (§3.2/§8.2).
    pub fn hourly_damage_summary(&self) -> DamageSummary {
        let mut summary = DamageSummary::default();
        for rec in &self.accumulated_damage {
            summary.add(rec.side, rec.org, rec.str);
        }
        summary
    }

    /// Sealed summaries of completed hours, oldest first (§8.2).
    pub fn damage_history(&self) -> &[DamageSummary] {
        &self.hourly_history
    }

    /// Full turns completed within the current strategic hour (0..turns_per_hour).
    pub fn turns_in_current_hour(&self) -> u32 {
        self.turns_in_current_hour
    }

    // ── flag-capture (§6.11) ─────────────────────────────────────

    /// Attach the battle's flag board (derived at assembly from the map /
    /// script / fallback). Called once at battle start.
    pub fn set_flags(&mut self, flags: Option<FlagState>) {
        self.flags = flags;
        self.collapse_injected = false;
    }

    /// The battle's flag board, if it has one.
    pub fn flags(&self) -> Option<&FlagState> {
        self.flags.as_ref()
    }

    pub fn flags_mut(&mut self) -> Option<&mut FlagState> {
        self.flags.as_mut()
    }

    /// §6.11: the defender collapse fired and its org-zeroing injection has
    /// not been sent yet — the next sync/end batch must carry it.
    pub fn flag_collapse_pending(&self) -> bool {
        self.flags.as_ref().map(|f| f.collapsed).unwrap_or(false) && !self.collapse_injected
    }

    /// Mark the collapse as injected (called once the batch went out).
    pub fn mark_collapse_injected(&mut self) {
        self.collapse_injected = true;
    }

    /// §6.11 collapse lines, prepended to the next sync/end batch: org-zero
    /// the collapsing side's divisions at the contested province
    /// (`org_damage = 1.000`, percent-of-MAX), one line per defender tag
    /// (the `limit` filter keys on the owning country). The tactical layer
    /// never flips provinces — HOI4 resolves the rout itself. The collapse
    /// is always the DEFENDER's (§6.11 flag capture), so the lines key on
    /// the battle context's defender tags regardless of the player side.
    /// Province-scoped — the retired `d_tac_collapse_*` mod
    /// effects zeroed EVERY `tac_in_battle`-tagged division of the country
    /// (division tagging can only be country-wide); the
    /// scope-pinned `eval_effect` wrapper is unneeded here because
    /// `damage_units` with `province=`+`limit` is scope-robust.
    fn collapse_lines(&self) -> Vec<String> {
        if !self.flag_collapse_pending() {
            return Vec::new();
        }
        let Some(ctx) = &self.battle_ctx else {
            return Vec::new();
        };
        ctx.defender_tags
            .iter()
            .map(|tag| damage_line(ctx.contested_province, tag, 1.0, 0.0))
            .collect()
    }

    // ── injection batches (§3.2) ────────────────────────────────────────────

    /// The per-hour damage lines shared by the sync and end batches,
    /// shaped by [`WritebackMode`]:
    ///
    /// * `OrgStr` — `damage_units` lines for the defender at the contested
    ///   province (precise — everyone there defends) and per attacker
    ///   source province (org exact; str diluted across every division of
    ///   that tag in the province so the province total stays exact). Each
    ///   line carries ONE tag in its `limit` filter and that tag's OWN
    ///   ratio — damage points and maxima pools are both tracked per tag,
    ///   so an allied tag that took no damage gets no line at all.
    /// * `Off` — no damage lines.
    ///
    /// The ratio bases are pool-matched to the battalion-scale point
    /// rollups: org divides by the assembled battalion org pool
    /// (HQ excluded), str by the division strength sums.
    ///
    /// Ratios below [`DAMAGE_EPS`] are skipped; without a [`BattleContext`]
    /// (non-live battle) no damage lines are produced. (The whole-army
    /// `set_unit_organization` keep lines — and with them the org-only
    /// mode — were retired; every damage line rides the province-scoped
    /// `damage_units` channel.)
    fn damage_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Some(ctx) = &self.battle_ctx else {
            return out;
        };
        if self.writeback_mode == WritebackMode::Off {
            return out;
        }
        // Defender: contested province, per-tag exact (the context's tag
        // Vec order is deterministic — line order must not ride a HashMap).
        for tag in &ctx.defender_tags {
            let (d_org_pts, d_str_pts) = self
                .defender_damage_points
                .get(tag)
                .copied()
                .unwrap_or((0.0, 0.0));
            let (max_org, max_str) = ctx.defender_max.get(tag).copied().unwrap_or((0.0, 0.0));
            let d_org = d_org_pts / max_org.max(1.0);
            let d_str = d_str_pts / max_str.max(1.0);
            if d_org >= DAMAGE_EPS || d_str >= DAMAGE_EPS {
                out.push(damage_line(ctx.contested_province, tag, d_org, d_str));
            }
        }
        // Attacker: per source province × per tag (same ordering rule).
        for ap in &ctx.attacker_provinces {
            for tag in &ctx.attacker_tags {
                let (a_org_pts, a_str_pts) = self
                    .attacker_damage_by_province
                    .get(&ap.province)
                    .and_then(|m| m.get(tag))
                    .copied()
                    .unwrap_or((0.0, 0.0));
                let a_org = a_org_pts
                    / ap.participants_max_org
                        .get(tag)
                        .copied()
                        .unwrap_or(0.0)
                        .max(1.0);
                let a_str = a_str_pts
                    / ap.all_max_str
                        .get(tag)
                        .copied()
                        .unwrap_or(0.0)
                        .max(1.0);
                if a_org >= DAMAGE_EPS || a_str >= DAMAGE_EPS {
                    out.push(damage_line(ap.province, tag, a_org, a_str));
                }
            }
        }
        out
    }

    /// Hourly sync batch (§3.2; clock line per §8.4), e.g.:
    ///
    /// ```text
    /// eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }
    /// eval_effect damage_units = { province = 13251 limit = { tag = ITA } org_damage = 0.080 str_damage = 0.030 ratio = yes army = yes }
    /// eval_effect ITA = { d_tac_sync_hourly = yes }
    /// pause_in_hours 1
    /// ```
    ///
    /// The sync ack rides the scope-pinned scripted effect — a console-fired
    /// `event tac_sync.1 <tag>` ignores the event's `hidden = yes` and pops
    /// an empty window every hour.
    /// [`CLOCK_ADVANCE_COMMAND`] is always the last line (§8.4): anything
    /// after it would execute before the clock starts running.
    pub fn build_sync_batch(&self) -> InjectionBatch {
        let mut batch = self.collapse_lines();
        batch.extend(self.damage_lines());
        batch.push(format!(
            "eval_effect {} = {{ d_tac_sync_hourly = yes }}",
            self.country_tag
        ));
        batch.push(CLOCK_ADVANCE_COMMAND.to_string());
        batch
    }

    /// Battle end batches (§3.2; two-phase per §8.4):
    /// phase 1 carries the final partial hour's damage (plus a pending
    /// collapse) and closes with [`CLOCK_ADVANCE_COMMAND`] when unsynced
    /// full turns remain; phase 2 — sent only after the clock-advance
    /// receipt — lifts the freeze and runs the scope-pinned cleanup
    /// (untag both sides + clear the flag + completion event).
    pub fn build_end_batch(&self) -> EndBatches {
        let mut phase1 = self.collapse_lines();
        phase1.extend(self.damage_lines());
        if self.turns_in_current_hour > 0 {
            phase1.push(CLOCK_ADVANCE_COMMAND.to_string());
        }
        let phase2 = vec![
            format!(
                "eval_effect {} = {{ d_tac_unfreeze_all = yes }}",
                self.country_tag
            ),
            format!(
                "eval_effect {} = {{ d_tac_end_battle = yes }}",
                self.country_tag
            ),
        ];
        EndBatches { phase1, phase2 }
    }

    /// Early-exit batches (§3.2, §8.3): the battle window's End
    /// Tactic / abandon exits must still clean up HOI4-side — the freeze
    /// and the tactical-mode flags outlive the window otherwise. With
    /// `carry` set (End Tactic — "results synced so far stay"), phase 1 is
    /// the end batch's partial-hour payload (damage + clock advance, may
    /// be empty); the abandon exit passes `carry = false` (§11.3: unsynced
    /// damage is lost, no clock advance). Phase 2 is the ABORT situation
    /// event — shared cleanup + popup + the uniform `tac_abort` log token
    /// the listener reads as "tactical mode off" — never
    /// `d_tac_end_battle`: an early exit resolves nothing.
    pub fn build_early_exit_batch(&self, carry: bool) -> EndBatches {
        let mut phase1 = Vec::new();
        if carry {
            phase1.extend(self.collapse_lines());
            phase1.extend(self.damage_lines());
            if self.turns_in_current_hour > 0 {
                phase1.push(CLOCK_ADVANCE_COMMAND.to_string());
            }
        }
        let phase2 = vec![format!(
            "eval_effect {} = {{ country_event = {{ id = tac_abort.1 hours = 0 }} }}",
            self.country_tag
        )];
        EndBatches { phase1, phase2 }
    }

    // ── display helpers ─────────────────────────────────────────────────────

    /// Battle clock display (10 min per full turn, §8.1). Without a start
    /// datetime: elapsed "HH:MM" from [`BattleSession::clock_minutes`]
    /// (hours are total, not wall-clock). With one (live
    /// battles): the absolute in-game time "YYYY-MM-DD HH:MM" — start +
    /// clock_minutes on the Gregorian calendar, leap-aware.
    pub fn battle_clock(&self) -> String {
        let Some((mut year, mut month, mut day, hour)) = self.start_datetime else {
            return format!(
                "{:02}:{:02}",
                self.clock_minutes / 60,
                self.clock_minutes % 60
            );
        };
        let total = hour as u64 * 60 + self.clock_minutes as u64;
        let mut days_left = total / (24 * 60);
        let hh = total % (24 * 60) / 60;
        let mm = total % 60;
        while days_left > 0 {
            if day < days_in_month(year, month) {
                day += 1;
            } else {
                day = 1;
                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
            }
            days_left -= 1;
        }
        format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}")
    }

    /// One-line turn/hour summary for the UI title bar (§9.1).
    pub fn turn_summary(&self) -> String {
        format!(
            "Turn {} | Hour {} | Clock {} | {:?} to act | {}",
            self.turn_number,
            self.strategic_hour,
            self.battle_clock(),
            self.current_side,
            self.phase,
        )
    }
}

impl Default for BattleSession {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_core::{HexCoord, UnitState, UnitType};

    fn unit(id: usize, side: Side, state: UnitState) -> BattalionUnit {
        let mut u = BattalionUnit::new(
            id,
            format!("U{id}"),
            UnitType::Infantry,
            side,
            HexCoord::ZERO,
        );
        u.state = state;
        if state != UnitState::Active {
            u.org = 0.0;
        }
        u
    }

    /// Session driven into TACTICAL_ACTIVE.
    fn active_session() -> BattleSession {
        let mut s = BattleSession::new();
        s.start_launching().unwrap();
        s.start_deployment().unwrap();
        s.start_battle().unwrap();
        s
    }

    fn run_full_turns(s: &mut BattleSession, n: u32) {
        for _ in 0..n {
            s.end_side_turn(Side::Attacker).unwrap();
            s.end_side_turn(Side::Defender).unwrap();
        }
    }

    /// Session driven into any phase reachable without combat.
    fn session_at(phase: BattlePhase) -> BattleSession {
        match phase {
            BattlePhase::WaitingForTrigger => BattleSession::new(),
            BattlePhase::Launching => {
                let mut s = BattleSession::new();
                s.start_launching().unwrap();
                s
            }
            BattlePhase::Deployment => {
                let mut s = BattleSession::new();
                s.start_launching().unwrap();
                s.start_deployment().unwrap();
                s
            }
            BattlePhase::TacticalActive => active_session(),
            BattlePhase::ReadyToSync => {
                let mut s = active_session();
                let n = s.turns_per_hour;
                run_full_turns(&mut s, n);
                s
            }
            BattlePhase::Synchronizing => {
                let mut s = session_at(BattlePhase::ReadyToSync);
                s.start_sync().unwrap();
                s
            }
            BattlePhase::ReadyToEnd => {
                let mut s = active_session();
                s.ready_to_end().unwrap();
                s
            }
            BattlePhase::InjectingFinal => {
                let mut s = session_at(BattlePhase::ReadyToEnd);
                s.complete_end().unwrap();
                s
            }
            BattlePhase::Ended => {
                let mut s = active_session();
                s.abort();
                s
            }
        }
    }

    // ── initial state ───────────────────────────────────────────────────────

    #[test]
    fn new_session_initial_state() {
        let s = BattleSession::new();
        assert_eq!(s.phase, BattlePhase::WaitingForTrigger);
        assert_eq!(s.turn_number, 1);
        assert_eq!(s.strategic_hour, 0);
        assert_eq!(s.current_side, Side::Attacker);
        assert_eq!(s.selected_unit_id, None);
        assert_eq!(s.clock_minutes, 0);
        assert!(s.accumulated_damage.is_empty());
        assert_eq!(s.turns_per_hour, 6);
        assert_eq!(s.turns_in_current_hour(), 0);
        assert!(s.damage_history().is_empty());
    }

    #[test]
    fn phase_display_names_match_design_8_3() {
        assert_eq!(
            BattlePhase::WaitingForTrigger.to_string(),
            "WAITING_FOR_TRIGGER"
        );
        assert_eq!(BattlePhase::Launching.to_string(), "LAUNCHING");
        assert_eq!(BattlePhase::Deployment.to_string(), "DEPLOYMENT");
        assert_eq!(BattlePhase::TacticalActive.to_string(), "TACTICAL_ACTIVE");
        assert_eq!(BattlePhase::ReadyToSync.to_string(), "READY_TO_SYNC");
        assert_eq!(BattlePhase::Synchronizing.to_string(), "SYNCHRONIZING");
        assert_eq!(BattlePhase::ReadyToEnd.to_string(), "READY_TO_END");
        assert_eq!(BattlePhase::InjectingFinal.to_string(), "INJECTING_FINAL");
        assert_eq!(BattlePhase::Ended.to_string(), "ENDED");
    }

    // ── happy path ──────────────────────────────────────────────────────────

    #[test]
    fn happy_path_full_phase_walk() {
        let mut s = BattleSession::new();
        assert_eq!(s.phase, BattlePhase::WaitingForTrigger);

        s.start_launching().unwrap();
        assert_eq!(s.phase, BattlePhase::Launching);

        s.start_deployment().unwrap();
        assert_eq!(s.phase, BattlePhase::Deployment);

        s.start_battle().unwrap();
        assert_eq!(s.phase, BattlePhase::TacticalActive);
        assert_eq!(s.current_side, Side::Attacker); // §6.12 attacker first

        // 5 full turns: still active, no sync.
        run_full_turns(&mut s, 5);
        assert_eq!(s.phase, BattlePhase::TacticalActive);
        assert_eq!(s.turn_number, 6);
        assert_eq!(s.clock_minutes, 50);

        // 6th full turn: sync boundary (§8.1).
        run_full_turns(&mut s, 1);
        assert_eq!(s.phase, BattlePhase::ReadyToSync);
        assert_eq!(s.turn_number, 7);
        assert_eq!(s.clock_minutes, 60);

        s.start_sync().unwrap();
        assert_eq!(s.phase, BattlePhase::Synchronizing);

        s.complete_sync().unwrap();
        assert_eq!(s.phase, BattlePhase::TacticalActive);
        assert_eq!(s.strategic_hour, 1);

        // Battle resolves (defender wiped out).
        let units = vec![
            unit(1, Side::Attacker, UnitState::Active),
            unit(2, Side::Defender, UnitState::Eliminated),
            unit(3, Side::Defender, UnitState::Withdrawn),
        ];
        assert_eq!(
            s.check_victory(&units),
            VictoryOutcome::Winner(Side::Attacker)
        );

        s.ready_to_end().unwrap();
        assert_eq!(s.phase, BattlePhase::ReadyToEnd);

        s.complete_end().unwrap();
        assert_eq!(s.phase, BattlePhase::InjectingFinal);

        s.finish().unwrap();
        assert_eq!(s.phase, BattlePhase::Ended);
    }

    // ── illegal transitions ─────────────────────────────────────────────────

    #[test]
    fn illegal_start_deployment_from_waiting() {
        let mut s = BattleSession::new();
        let err = s.start_deployment().unwrap_err();
        assert_eq!(
            err,
            TransitionError::IllegalTransition {
                from: BattlePhase::WaitingForTrigger,
                action: "start_deployment",
            }
        );
        assert_eq!(s.phase, BattlePhase::WaitingForTrigger);
    }

    #[test]
    fn illegal_start_battle_from_launching() {
        let mut s = session_at(BattlePhase::Launching);
        assert!(s.start_battle().is_err());
        assert_eq!(s.phase, BattlePhase::Launching);
    }

    #[test]
    fn illegal_double_launch() {
        let mut s = session_at(BattlePhase::Launching);
        assert!(s.start_launching().is_err());
        assert_eq!(s.phase, BattlePhase::Launching);
    }

    #[test]
    fn illegal_sync_transitions() {
        // start_sync only from ReadyToSync.
        let mut s = active_session();
        assert!(s.start_sync().is_err());
        assert_eq!(s.phase, BattlePhase::TacticalActive);

        // complete_sync only from Synchronizing.
        let mut s = session_at(BattlePhase::ReadyToSync);
        assert!(s.complete_sync().is_err());
        assert_eq!(s.phase, BattlePhase::ReadyToSync);
        assert_eq!(s.strategic_hour, 0);
    }

    #[test]
    fn illegal_end_transitions() {
        let mut s = BattleSession::new();
        assert!(s.ready_to_end().is_err());
        assert!(s.complete_end().is_err());
        assert!(s.finish().is_err());
        assert_eq!(s.phase, BattlePhase::WaitingForTrigger);

        // complete_end / finish must follow the ReadyToEnd → InjectingFinal chain.
        let mut s = active_session();
        assert!(s.complete_end().is_err());
        assert!(s.finish().is_err());
        let mut s = session_at(BattlePhase::ReadyToEnd);
        assert!(s.finish().is_err());
        assert_eq!(s.phase, BattlePhase::ReadyToEnd);
    }

    #[test]
    fn ended_is_terminal() {
        let mut s = session_at(BattlePhase::Ended);
        assert!(s.start_launching().is_err());
        assert!(s.start_deployment().is_err());
        assert!(s.start_battle().is_err());
        assert!(s.start_sync().is_err());
        assert!(s.complete_sync().is_err());
        assert!(s.ready_to_end().is_err());
        assert!(s.complete_end().is_err());
        assert!(s.finish().is_err());
        assert!(s.end_side_turn(Side::Attacker).is_err());
        s.abort(); // idempotent
        assert_eq!(s.phase, BattlePhase::Ended);
    }

    // ── turn order & clock (§6.12, §8.1) ────────────────────────────────────

    #[test]
    fn end_side_turn_rejects_wrong_phase() {
        let mut s = BattleSession::new();
        assert!(s.end_side_turn(Side::Attacker).is_err());
        let mut s = session_at(BattlePhase::Deployment);
        assert!(s.end_side_turn(Side::Attacker).is_err());
    }

    #[test]
    fn end_side_turn_rejects_out_of_turn() {
        let mut s = active_session();
        let err = s.end_side_turn(Side::Defender).unwrap_err();
        assert_eq!(
            err,
            TransitionError::OutOfTurn {
                expected: Side::Attacker,
                attempted: Side::Defender,
            }
        );
        assert_eq!(s.current_side, Side::Attacker);
        assert_eq!(s.turn_number, 1);
    }

    #[test]
    fn attacker_first_and_sides_alternate() {
        let mut s = active_session();
        assert_eq!(s.current_side, Side::Attacker); // §6.12

        s.end_side_turn(Side::Attacker).unwrap();
        assert_eq!(s.current_side, Side::Defender);
        assert_eq!(s.turn_number, 1); // no increment before both acted

        s.end_side_turn(Side::Defender).unwrap();
        assert_eq!(s.current_side, Side::Attacker);
        assert_eq!(s.turn_number, 2); // incremented after both acted
    }

    #[test]
    fn clock_advances_ten_minutes_per_full_turn() {
        let mut s = active_session();
        assert_eq!(s.clock_minutes, 0);

        s.end_side_turn(Side::Attacker).unwrap();
        assert_eq!(s.clock_minutes, 0); // half-turn: no elapsed time yet

        s.end_side_turn(Side::Defender).unwrap();
        assert_eq!(s.clock_minutes, 10);
        assert_eq!(s.clock_minutes, (s.turn_number - 1) * MINUTES_PER_TURN);
    }

    #[test]
    fn exactly_six_turns_trigger_ready_to_sync() {
        let mut s = active_session();
        run_full_turns(&mut s, 5);
        assert_eq!(s.phase, BattlePhase::TacticalActive);
        assert_eq!(s.turns_in_current_hour(), 5);

        run_full_turns(&mut s, 1);
        assert_eq!(s.phase, BattlePhase::ReadyToSync);
        assert_eq!(s.turns_in_current_hour(), 6);
        assert_eq!(s.turn_number, 7);
        assert_eq!(s.clock_minutes, 60);
    }

    #[test]
    fn with_params_uses_config_turns_per_hour() {
        let params = CombatParams {
            turns_per_strategic_hour: 3,
            ..CombatParams::default()
        };
        let mut s = BattleSession::with_params(&params);
        assert_eq!(s.turns_per_hour, 3);
        s.start_launching().unwrap();
        s.start_deployment().unwrap();
        s.start_battle().unwrap();
        run_full_turns(&mut s, 3);
        assert_eq!(s.phase, BattlePhase::ReadyToSync);

        // Default params keep the §8.1 value of 6.
        assert_eq!(
            BattleSession::with_params(&CombatParams::default()).turns_per_hour,
            6
        );
    }

    #[test]
    fn no_further_turns_until_sync_completes() {
        let mut s = session_at(BattlePhase::ReadyToSync);
        assert!(s.end_side_turn(Side::Attacker).is_err());
        assert_eq!(s.phase, BattlePhase::ReadyToSync);
    }

    #[test]
    fn desync_mode_seals_the_hour_locally_without_the_sync_stop() {
        // The guard's "continue unsynced" mode: the hour boundary must not
        // park the session in READY_TO_SYNC (there is no sync to run) — it
        // seals exactly like a completed sync and play continues.
        let mut s = active_session();
        s.sync_disabled = true;
        s.record_damage(Side::Attacker, "ITA", Some(13251), 6.0, 1.5, 60.0, 30.0);
        run_full_turns(&mut s, 6);
        assert_eq!(s.phase, BattlePhase::TacticalActive);
        assert_eq!(s.strategic_hour, 1);
        assert_eq!(s.turns_in_current_hour(), 0);
        assert_eq!(s.damage_history().len(), 1);
        // And the next hour turns normally.
        run_full_turns(&mut s, 6);
        assert_eq!(s.phase, BattlePhase::TacticalActive);
        assert_eq!(s.strategic_hour, 2);
        assert_eq!(s.damage_history().len(), 2);
    }

    // ── sync cycle & damage sealing (§8.2) ──────────────────────────────────

    #[test]
    fn sync_seals_history_and_resets_accumulators() {
        let mut s = active_session();
        // (side, province, org_pts, str_pts, max_org, max_str): 6/60 = 0.10 etc.
        s.record_damage(Side::Attacker, "ITA", Some(13251), 6.0, 1.5, 60.0, 30.0);
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 2.4, 60.0, 30.0);
        run_full_turns(&mut s, 6);

        // Summary is available for the batch while in ReadyToSync/Synchronizing.
        let pre = s.hourly_damage_summary();
        assert!((pre.attacker_org_lost - 0.10).abs() < 1e-6);
        assert!((pre.defender_str_lost - 0.08).abs() < 1e-6);

        s.start_sync().unwrap();
        s.complete_sync().unwrap();

        assert_eq!(s.phase, BattlePhase::TacticalActive);
        assert_eq!(s.strategic_hour, 1);
        assert_eq!(s.turns_in_current_hour(), 0);
        assert_eq!(s.current_side, Side::Attacker);
        assert!(s.accumulated_damage.is_empty());
        assert!(s.hourly_damage_summary().is_zero());

        let history = s.damage_history();
        assert_eq!(history.len(), 1);
        assert!((history[0].attacker_org_lost - 0.10).abs() < 1e-6);
        assert!((history[0].attacker_str_lost - 0.05).abs() < 1e-6);
        assert!((history[0].defender_org_lost - 0.20).abs() < 1e-6);
        assert!((history[0].defender_str_lost - 0.08).abs() < 1e-6);
    }

    #[test]
    fn multiple_sync_cycles_advance_hour_and_history() {
        let mut s = active_session();
        for hour in 1..=3 {
            s.record_damage(Side::Defender, "ETH", None, 0.6 * hour as f32, 0.0, 60.0, 30.0);
            run_full_turns(&mut s, 6);
            s.start_sync().unwrap();
            s.complete_sync().unwrap();
            assert_eq!(s.strategic_hour, hour);
        }
        assert_eq!(s.damage_history().len(), 3);
        assert_eq!(s.turn_number, 19); // 18 full turns + 1
        assert_eq!(s.clock_minutes, 180);
    }

    #[test]
    fn turn_number_and_clock_continue_across_sync() {
        let mut s = session_at(BattlePhase::ReadyToSync);
        s.start_sync().unwrap();
        s.complete_sync().unwrap();
        assert_eq!(s.turn_number, 7); // not reset by sync
        assert_eq!(s.clock_minutes, 60);
        run_full_turns(&mut s, 1);
        assert_eq!(s.turn_number, 8);
        assert_eq!(s.clock_minutes, 70);
    }

    // ── victory detection (§6.11) ───────────────────────────────────────────

    #[test]
    fn victory_attacker_when_all_defenders_resolved() {
        let s = active_session();
        let units = vec![
            unit(1, Side::Attacker, UnitState::Active),
            unit(2, Side::Defender, UnitState::Eliminated),
            unit(3, Side::Defender, UnitState::Surrendered),
            unit(4, Side::Defender, UnitState::Withdrawn),
        ];
        assert_eq!(
            s.check_victory(&units),
            VictoryOutcome::Winner(Side::Attacker)
        );
    }

    #[test]
    fn victory_counts_left_battle_as_resolved() {
        // §6.14: a unit that slipped away over the boundary is
        // out of the battle for good — a side whose whole force left has
        // lost the field.
        let s = active_session();
        let units = vec![
            unit(1, Side::Attacker, UnitState::LeftBattle),
            unit(2, Side::Defender, UnitState::Active),
        ];
        assert_eq!(
            s.check_victory(&units),
            VictoryOutcome::Winner(Side::Defender)
        );
        // Mirror image: the defender's army dissolved off-map → attacker.
        let units = vec![
            unit(1, Side::Attacker, UnitState::Active),
            unit(2, Side::Defender, UnitState::LeftBattle),
        ];
        assert_eq!(
            s.check_victory(&units),
            VictoryOutcome::Winner(Side::Attacker)
        );
    }

    #[test]
    fn victory_defender_when_all_attackers_resolved() {
        let s = active_session();
        let units = vec![
            unit(1, Side::Attacker, UnitState::Eliminated),
            unit(2, Side::Attacker, UnitState::Withdrawn),
            unit(3, Side::Defender, UnitState::Active),
        ];
        assert_eq!(
            s.check_victory(&units),
            VictoryOutcome::Winner(Side::Defender)
        );
    }

    #[test]
    fn victory_none_while_any_enemy_still_retreating() {
        // §6.11: org=0 is not enough — the unit must finish retreating
        // (Withdrawn) or be annihilated (Eliminated) first.
        let s = active_session();
        let units = vec![
            unit(1, Side::Attacker, UnitState::Active),
            unit(2, Side::Defender, UnitState::Retreating),
        ];
        assert_eq!(s.check_victory(&units), VictoryOutcome::Undecided);
    }

    #[test]
    fn victory_none_while_both_sides_active() {
        let s = active_session();
        let units = vec![
            unit(1, Side::Attacker, UnitState::Active),
            unit(2, Side::Defender, UnitState::Active),
            unit(3, Side::Defender, UnitState::Eliminated),
        ];
        assert_eq!(s.check_victory(&units), VictoryOutcome::Undecided);
    }

    #[test]
    fn victory_none_while_active_with_zero_org() {
        // Survival is judged on STATE, not org/strength —
        // an Active battalion with org 0 (transient/forged; both frontends'
        // upkeep normalizes it to Retreating) still keeps its side alive.
        // This pins the semantic the headless mirror was unified onto.
        let s = active_session();
        let mut broken = unit(2, Side::Defender, UnitState::Active);
        broken.org = 0.0;
        let units = vec![unit(1, Side::Attacker, UnitState::Active), broken];
        assert_eq!(s.check_victory(&units), VictoryOutcome::Undecided);
    }

    #[test]
    fn victory_draw_on_mutual_annihilation() {
        // Mutual annihilation must be a TERMINAL outcome — a None
        // result stalled the GUI on an empty board (final batch never sent).
        let s = active_session();
        let units = vec![
            unit(1, Side::Attacker, UnitState::Eliminated),
            unit(2, Side::Defender, UnitState::Eliminated),
        ];
        assert_eq!(s.check_victory(&units), VictoryOutcome::Draw);
    }

    #[test]
    fn victory_with_empty_side_rosters() {
        let s = active_session();
        let attackers = vec![unit(1, Side::Attacker, UnitState::Active)];
        // No defender battalions at all → vacuously resolved → attacker wins.
        assert_eq!(
            s.check_victory(&attackers),
            VictoryOutcome::Winner(Side::Attacker)
        );
        // No battalions at all → both sides vacuously resolved → draw
        // (the old None result never ended the battle).
        assert_eq!(s.check_victory(&[]), VictoryOutcome::Draw);
    }

    #[test]
    fn victory_ignores_lingering_hqs() {
        // §6.13: an HQ is a command unit, not a fighting unit —
        // a side kept alive only by its HQs has lost.
        let s = active_session();
        let mut hq = unit(2, Side::Defender, UnitState::Active);
        hq.attrs |= tactical_core::Attrs::HQ;
        let units = vec![unit(1, Side::Attacker, UnitState::Active), hq];
        assert_eq!(
            s.check_victory(&units),
            VictoryOutcome::Winner(Side::Attacker)
        );
    }

    #[test]
    fn ready_to_end_allowed_from_ready_to_sync_boundary() {
        // Battle resolved exactly on the 6th turn: skip the pending sync.
        let mut s = session_at(BattlePhase::ReadyToSync);
        s.ready_to_end().unwrap();
        assert_eq!(s.phase, BattlePhase::ReadyToEnd);
    }

    // ── external (HOI4-side) resolution ─────────────────────────────

    #[test]
    fn resolve_externally_records_the_winner_and_ends_the_battle() {
        let mut s = session_at(BattlePhase::TacticalActive);
        s.resolve_externally(Side::Attacker).unwrap();
        assert_eq!(s.phase, BattlePhase::ReadyToEnd);
        // check_victory reports the strategic winner even with both sides
        // still on the board (the empty roster here would read Draw).
        assert_eq!(s.check_victory(&[]), VictoryOutcome::Winner(Side::Attacker));
        // From there the normal end flow rides unchanged.
        s.complete_end().unwrap();
        s.finish().unwrap();
        assert_eq!(s.phase, BattlePhase::Ended);
    }

    #[test]
    fn resolve_externally_rejected_once_the_tactical_battle_already_ended() {
        let mut s = session_at(BattlePhase::ReadyToEnd);
        assert!(s.resolve_externally(Side::Defender).is_err());
        assert_eq!(s.phase, BattlePhase::ReadyToEnd);
        // No external winner recorded — check_victory stays board-driven.
        assert_eq!(s.check_victory(&[]), VictoryOutcome::Draw);
    }

    #[test]
    fn resolve_externally_allowed_from_ready_to_sync_boundary() {
        let mut s = session_at(BattlePhase::ReadyToSync);
        s.resolve_externally(Side::Defender).unwrap();
        assert_eq!(s.check_victory(&[]), VictoryOutcome::Winner(Side::Defender));
    }

    // ── abort (§8.3, §10.4) ─────────────────────────────────────────────────

    #[test]
    fn abort_from_every_phase() {
        let phases = [
            BattlePhase::WaitingForTrigger,
            BattlePhase::Launching,
            BattlePhase::Deployment,
            BattlePhase::TacticalActive,
            BattlePhase::ReadyToSync,
            BattlePhase::Synchronizing,
            BattlePhase::ReadyToEnd,
            BattlePhase::InjectingFinal,
            BattlePhase::Ended,
        ];
        for phase in phases {
            let mut s = session_at(phase);
            s.abort();
            assert_eq!(s.phase, BattlePhase::Ended, "abort from {phase}");
        }
    }

    // ── damage accounting ───────────────────────────────────────────────────

    #[test]
    fn record_damage_merges_per_side() {
        let mut s = active_session();
        s.record_damage(Side::Attacker, "ITA", Some(13251), 6.0, 1.5, 60.0, 30.0);
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 3.0, 60.0, 30.0);
        s.record_damage(Side::Attacker, "ITA", Some(13251), 3.0, 0.9, 60.0, 30.0);

        // One running record per side (UI rollup, per-battalion ratios).
        assert_eq!(s.accumulated_damage.len(), 2);

        let sum = s.hourly_damage_summary();
        assert!((sum.attacker_org_lost - 0.15).abs() < 1e-6);
        assert!((sum.attacker_str_lost - 0.08).abs() < 1e-6);
        assert!((sum.defender_org_lost - 0.20).abs() < 1e-6);
        assert!((sum.defender_str_lost - 0.10).abs() < 1e-6);
    }

    // ── injection batches (§3.2) ─────────────────────────────────

    /// Synthetic live context: ITA attacks ETH at 13237 from two source
    /// provinces (13251 mixed-use, 12723 participants-only).
    fn live_ctx() -> BattleContext {
        let per_tag = |v: f32| [("ITA".to_string(), v)].into_iter().collect();
        BattleContext {
            contested_province: 13237,
            attacker_tags: vec!["ITA".to_string()],
            defender_tags: vec!["ETH".to_string()],
            defender_max: [("ETH".to_string(), (100.0, 300.0))].into_iter().collect(),
            attacker_provinces: vec![
                AttackerProvinceCtx {
                    province: 13251,
                    participants_max_org: per_tag(80.0),
                    participants_max_str: per_tag(200.0),
                    all_max_str: per_tag(400.0),
                },
                AttackerProvinceCtx {
                    province: 12723,
                    participants_max_org: per_tag(40.0),
                    participants_max_str: per_tag(100.0),
                    all_max_str: per_tag(100.0),
                },
            ],
        }
    }

    #[test]
    fn sync_batch_round13_damage_units_lines() {
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        // Defender lost 12 org / 15 str points (of 100 org / 300 str max).
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 15.0, 60.0, 30.0);
        // Attackers at 13251 lost 8 org / 8 str (of 80/200 participants,
        // diluted over 400 all-division str); at 12723, 2 org only.
        s.record_damage(Side::Attacker, "ITA", Some(13251), 8.0, 8.0, 60.0, 30.0);
        s.record_damage(Side::Attacker, "ITA", Some(12723), 2.0, 0.0, 60.0, 30.0);
        let batch = s.build_sync_batch();
        assert_eq!(
            batch,
            vec![
                "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }",
                // dilution: 8 / 400 = 0.020 str (not 8 / 200 = 0.040)
                "eval_effect damage_units = { province = 13251 limit = { tag = ITA } org_damage = 0.100 str_damage = 0.020 ratio = yes army = yes }",
                "eval_effect damage_units = { province = 12723 limit = { tag = ITA } org_damage = 0.050 ratio = yes army = yes }",
                "eval_effect ITA = { d_tac_sync_hourly = yes }",
                // §8.4: the clock advance is always last.
                "pause_in_hours 1",
            ]
        );
    }

    /// A battle line spanning ALLIED tags (GER+ITA attacking, ENG+FRA
    /// defending) books each tag's OWN ratio in its own line — damage
    /// points and maxima pools are tracked per tag, and a tag that took
    /// no damage gets no line at all (no side-wide smear).
    #[test]
    fn sync_batch_allied_side_writes_per_tag_damage_lines() {
        let mut ctx = live_ctx();
        ctx.attacker_tags = vec!["ITA".to_string(), "GER".to_string()];
        ctx.defender_tags = vec!["ETH".to_string(), "FRA".to_string()];
        // FRA co-defends with the same maxima; GER co-attacks from 13251
        // with its own participant/dilution pools.
        ctx.defender_max
            .insert("FRA".to_string(), (100.0, 300.0));
        ctx.attacker_provinces[0]
            .participants_max_org
            .insert("GER".to_string(), 40.0);
        ctx.attacker_provinces[0]
            .participants_max_str
            .insert("GER".to_string(), 100.0);
        ctx.attacker_provinces[0]
            .all_max_str
            .insert("GER".to_string(), 100.0);
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(ctx);
        // ETH defenders lose 12 org / 15 str; FRA defenders only 4 org
        // (its own 0.040 ratio — not ETH's 0.120). At 13251: ITA attackers
        // lose 8/8, GER attackers 2 org only.
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 15.0, 60.0, 30.0);
        s.record_damage(Side::Defender, "FRA", Some(13237), 4.0, 0.0, 60.0, 30.0);
        s.record_damage(Side::Attacker, "ITA", Some(13251), 8.0, 8.0, 60.0, 30.0);
        s.record_damage(Side::Attacker, "GER", Some(13251), 2.0, 0.0, 60.0, 30.0);
        let batch = s.build_sync_batch();
        assert_eq!(
            batch,
            vec![
                "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }",
                "eval_effect damage_units = { province = 13237 limit = { tag = FRA } org_damage = 0.040 ratio = yes army = yes }",
                "eval_effect damage_units = { province = 13251 limit = { tag = ITA } org_damage = 0.100 str_damage = 0.020 ratio = yes army = yes }",
                "eval_effect damage_units = { province = 13251 limit = { tag = GER } org_damage = 0.050 ratio = yes army = yes }",
                "eval_effect ITA = { d_tac_sync_hourly = yes }",
                "pause_in_hours 1",
            ]
        );
        // A tag with zero recorded damage emits no line: 12723's pools
        // exist but nothing landed there, and FRA str stayed 0.
        assert!(!batch.iter().any(|l| l.contains("12723")));
    }

    /// §6.11 collapse with a multi-tag defender side: every defending
    /// tag's divisions at the contested province are org-zeroed.
    #[test]
    fn collapse_lines_cover_every_defender_tag() {
        use tactical_core::FlagState;
        let mut ctx = live_ctx();
        ctx.defender_tags = vec!["ETH".to_string(), "FRA".to_string()];
        let mut s = active_session();
        s.battle_ctx = Some(ctx);
        s.set_flags(Some(FlagState::default()));
        s.flags_mut().unwrap().collapsed = true;
        assert_eq!(
            s.collapse_lines(),
            vec![
                "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 1.000 ratio = yes army = yes }",
                "eval_effect damage_units = { province = 13237 limit = { tag = FRA } org_damage = 1.000 ratio = yes army = yes }",
            ]
        );
    }

    #[test]
    fn sync_batch_off_mode_has_no_damage_lines() {
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        s.writeback_mode = WritebackMode::Off;
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 15.0, 60.0, 30.0);
        s.record_damage(Side::Attacker, "ITA", Some(13251), 8.0, 8.0, 60.0, 30.0);
        let batch = s.build_sync_batch();
        assert_eq!(
            batch,
            vec![
                "eval_effect ITA = { d_tac_sync_hourly = yes }",
                "pause_in_hours 1"
            ]
        );
    }

    #[test]
    fn end_batch_splits_damage_clock_and_cleanup_into_two_phases() {
        // §8.4: phase 1 = damage (+ collapse + clock advance);
        // phase 2 = unfreeze + scope-pinned cleanup, fired only after the
        // clock-advance receipt.
        let mut s = session_at(BattlePhase::ReadyToEnd);
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 15.0, 60.0, 30.0);
        let end = s.build_end_batch();
        // ReadyToEnd with NO unsynced full turns (battle ended exactly on
        // a synced boundary): the damage line rides phase 1, but no extra
        // clock hour is burned.
        assert_eq!(
            end.phase1,
            vec![
                "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }",
            ]
        );
        assert_eq!(
            end.phase2,
            vec![
                "eval_effect ITA = { d_tac_unfreeze_all = yes }",
                "eval_effect ITA = { d_tac_end_battle = yes }",
            ]
        );
    }

    #[test]
    fn end_batch_with_unsynced_turns_advances_the_clock_last() {
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 15.0, 60.0, 30.0);
        run_full_turns(&mut s, 3); // 3 unsynced turns into the hour
        s.ready_to_end().unwrap();
        let end = s.build_end_batch();
        assert_eq!(
            end.phase1,
            vec![
                "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }",
                "pause_in_hours 1",
            ]
        );
        // The phases never mix: no cleanup in phase 1, no clock in phase 2.
        assert!(!end
            .phase1
            .iter()
            .any(|l| l.contains("d_tac_end_battle") || l.contains("unfreeze")));
        assert!(!end.phase2.iter().any(|l| l.contains("pause_in_hours")));
    }

    #[test]
    fn end_batch_on_unsynced_sync_boundary_still_advances_the_clock() {
        // Battle resolved exactly on a sync boundary: the pending sync is
        // skipped (§3.2), so the full unsynced hour rides the end batch —
        // the clock advance MUST fire (that hour never got its sync tick).
        let mut s = session_at(BattlePhase::ReadyToSync);
        s.country_tag = "ITA".to_string();
        s.ready_to_end().unwrap();
        let end = s.build_end_batch();
        assert_eq!(end.phase1, vec!["pause_in_hours 1"]);
        assert_eq!(end.phase2.len(), 2);
    }

    #[test]
    fn end_batch_without_turns_damage_or_collapse_has_empty_phase1() {
        // No unsynced turns, no battle context, no pending collapse: phase
        // 1 is empty (the host skips it) and only phase 2 goes out.
        let s = session_at(BattlePhase::ReadyToEnd);
        let end = s.build_end_batch();
        assert!(end.phase1.is_empty());
        assert_eq!(
            end.phase2,
            vec![
                "eval_effect GER = { d_tac_unfreeze_all = yes }",
                "eval_effect GER = { d_tac_end_battle = yes }",
            ]
        );
    }

    #[test]
    fn early_exit_batch_carries_partial_hour_or_not() {
        // End Tactic (carry) = the end batch's phase-1 payload +
        // the abort situation event; abandon (no carry) = empty phase 1,
        // cleanup only — never d_tac_end_battle (nothing was resolved).
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 15.0, 60.0, 30.0);
        run_full_turns(&mut s, 3); // 3 unsynced turns into the hour
        let abort_line = "eval_effect ITA = { country_event = { id = tac_abort.1 hours = 0 } }";
        let carried = s.build_early_exit_batch(true);
        assert_eq!(
            carried.phase1,
            vec![
                "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }",
                "pause_in_hours 1",
            ]
        );
        assert_eq!(carried.phase2, vec![abort_line]);
        let abandoned = s.build_early_exit_batch(false);
        assert!(abandoned.phase1.is_empty());
        assert_eq!(abandoned.phase2, vec![abort_line]);
    }

    #[test]
    fn batch_without_context_has_no_damage_lines() {
        let s = active_session(); // no battle_ctx (synthetic/script battles)
        let batch = s.build_sync_batch();
        assert_eq!(
            batch,
            vec![
                "eval_effect GER = { d_tac_sync_hourly = yes }",
                "pause_in_hours 1"
            ]
        );
    }

    #[test]
    fn negligible_damage_skips_the_line() {
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        // 0.04 org points on a 100-org base = 0.0004 < DAMAGE_EPS → skipped.
        s.record_damage(Side::Defender, "ETH", Some(13237), 0.04, 0.0, 60.0, 30.0);
        let batch = s.build_sync_batch();
        assert_eq!(
            batch,
            vec![
                "eval_effect ITA = { d_tac_sync_hourly = yes }",
                "pause_in_hours 1"
            ]
        );
    }

    #[test]
    fn damage_ratios_clamp_to_one() {
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        // Way over 100% of the defender base — must clamp to 1.000.
        s.record_damage(Side::Defender, "ETH", Some(13237), 500.0, 900.0, 60.0, 30.0);
        let batch = s.build_sync_batch();
        assert_eq!(
            batch[0],
            "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 1.000 str_damage = 1.000 ratio = yes army = yes }"
        );
    }

    #[test]
    fn collapse_lines_ride_the_next_batch_once() {
        use tactical_core::FlagState;
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        s.set_flags(Some(FlagState::default()));
        assert!(!s.flag_collapse_pending());
        s.flags_mut().unwrap().collapsed = true;
        assert!(s.flag_collapse_pending());
        // The collapse line org-zeroes the DEFENDER at the
        // contested province (§6.11) — province-scoped, not the retired
        // country-wide d_tac_collapse_* mod effect.
        let collapse =
            "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 1.000 ratio = yes army = yes }";
        let sync = s.build_sync_batch();
        assert_eq!(sync[0], collapse);
        // The hourly payload follows unchanged (no damage recorded here).
        assert_eq!(sync[1], "eval_effect ITA = { d_tac_sync_hourly = yes }");
        assert_eq!(sync[2], "pause_in_hours 1");
        // Same ride on the end batch's phase 1 (0 unsynced turns here →
        // no clock line follows the collapse).
        let end = s.build_end_batch();
        assert_eq!(end.phase1, vec![collapse.to_string()]);
        // Once marked injected, later batches are clean again.
        s.mark_collapse_injected();
        assert!(!s.flag_collapse_pending());
        let sync2 = s.build_sync_batch();
        assert_eq!(sync2[0], "eval_effect ITA = { d_tac_sync_hourly = yes }");
        assert!(!sync2.contains(&collapse.to_string()));
    }

    #[test]
    fn collapse_line_targets_the_defender_regardless_of_player_side() {
        use tactical_core::FlagState;
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.player_side = Side::Defender;
        s.battle_ctx = Some(live_ctx());
        s.set_flags(Some(FlagState::default()));
        s.flags_mut().unwrap().collapsed = true;
        let sync = s.build_sync_batch();
        // §6.11: the collapse is always the defender's — the line keys on
        // the battle context's defender tag, not the player side.
        assert_eq!(
            sync[0],
            "eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 1.000 ratio = yes army = yes }"
        );
    }

    #[test]
    fn flags_survive_clone_for_checkpoints() {
        use tactical_core::{FlagKind, FlagState, FlagZone};
        let mut s = active_session();
        let mut fs = FlagState {
            kind: FlagKind::City,
            flags: vec![FlagZone::new(
                tactical_core::HexCoord::new(3, 3),
                vec![tactical_core::HexCoord::new(3, 3)],
            )],
            collapsed: false,
        };
        fs.flags[0].progress = 7;
        s.set_flags(Some(fs));
        let snapshot = s.clone();
        assert_eq!(snapshot.flags.as_ref().unwrap().flags[0].progress, 7);
        assert!(snapshot.flags.as_ref().unwrap().flags[0]
            .zone
            .contains(&tactical_core::HexCoord::new(3, 3)));
    }

    #[test]
    fn render_batch_joins_lines_with_newlines() {
        let mut s = active_session();
        s.country_tag = "ITA".to_string();
        s.battle_ctx = Some(live_ctx());
        s.record_damage(Side::Defender, "ETH", Some(13237), 12.0, 0.0, 60.0, 30.0);
        let batch = s.build_sync_batch();
        let file = render_batch(&batch);
        // The trailing newline is MANDATORY — HOI4's
        // `run` parser drops an unterminated final line.
        assert_eq!(file.lines().count(), 3);
        assert!(file.starts_with("eval_effect damage_units = { province = 13237"));
        assert!(file.ends_with("pause_in_hours 1\n"));
        // A single-line batch must still be terminated.
        assert_eq!(render_batch(&vec!["run one".to_string()]), "run one\n");
    }

    // ── display helpers ─────────────────────────────────────────────────────

    #[test]
    fn battle_clock_formats_hh_mm() {
        let mut s = active_session();
        assert_eq!(s.battle_clock(), "00:00");
        run_full_turns(&mut s, 6); // hits ReadyToSync at 60 min
        assert_eq!(s.battle_clock(), "01:00");
        s.start_sync().unwrap();
        s.complete_sync().unwrap();
        run_full_turns(&mut s, 1);
        assert_eq!(s.clock_minutes, 70);
        assert_eq!(s.battle_clock(), "01:10");
        run_full_turns(&mut s, 5); // hits ReadyToSync again at 120 min
        assert_eq!(s.battle_clock(), "02:00");
    }

    /// With a start datetime the clock shows absolute game time.
    #[test]
    fn battle_clock_absolute_starts_at_save_datetime() {
        let mut s = active_session();
        s.start_datetime = Some((1936, 1, 1, 13));
        assert_eq!(s.battle_clock(), "1936-01-01 13:00");
        run_full_turns(&mut s, 6); // +60 min
        assert_eq!(s.battle_clock(), "1936-01-01 14:00");
    }

    /// Day/month/year rollover, Gregorian leap rules.
    #[test]
    fn battle_clock_absolute_rolls_calendar() {
        let mut s = active_session();
        s.start_datetime = Some((1936, 1, 31, 23));
        run_full_turns(&mut s, 6); // +60 min → next day
        assert_eq!(s.battle_clock(), "1936-02-01 00:00");

        let mut s = active_session();
        s.start_datetime = Some((1936, 2, 28, 23)); // 1936 is a leap year
        run_full_turns(&mut s, 6);
        assert_eq!(s.battle_clock(), "1936-02-29 00:00");

        let mut s = active_session();
        s.start_datetime = Some((1900, 2, 28, 23)); // 1900 is NOT (÷100, ¬÷400)
        run_full_turns(&mut s, 6);
        assert_eq!(s.battle_clock(), "1900-03-01 00:00");

        let mut s = active_session();
        s.start_datetime = Some((1936, 12, 31, 23));
        run_full_turns(&mut s, 6);
        assert_eq!(s.battle_clock(), "1937-01-01 00:00");
    }

    #[test]
    fn turn_summary_contains_turn_hour_clock() {
        let mut s = active_session();
        run_full_turns(&mut s, 6);
        s.start_sync().unwrap();
        s.complete_sync().unwrap();
        let text = s.turn_summary();
        assert!(text.contains("Turn 7"), "{text}");
        assert!(text.contains("Hour 1"), "{text}");
        assert!(text.contains("01:00"), "{text}");
        assert!(text.contains("Attacker"), "{text}");
        assert!(text.contains("TACTICAL_ACTIVE"), "{text}");
    }

    #[test]
    fn selected_unit_id_persists_across_turns_and_sync() {
        let mut s = active_session();
        s.selected_unit_id = Some(42);
        run_full_turns(&mut s, 6);
        s.start_sync().unwrap();
        s.complete_sync().unwrap();
        assert_eq!(s.selected_unit_id, Some(42));
    }
}
