//! Game-loop wiring: deployment, player commands, AI turns, fog of war,
//! encirclement attrition, battle clock and victory — driven by
//! tactical-sync's BattleSession state machine (DESIGN §6/§8).

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::prelude::*;
use tactical_ai::{AiAction, CombatTactic, DivOrderTarget, TacticalAi};
use tactical_combat::{
    apply_oob_leaving, retreat_step_zoned, AttackOrder, AttackTarget, CombatEngine,
};
use tactical_core::fog::{FogOfWar, VisibilityState};
use tactical_core::grid::HexGrid;
use tactical_core::hex::HexCoord;
use tactical_core::movement::{
    advance_move_orders, eta_turns, order_eta_hours, refresh_move_order, MovementEvent,
};
use tactical_core::params::CombatParams;
use tactical_core::pathfinding::find_path;
use tactical_core::unit::{BattalionUnit, MoveOrder, Side, UnitState};
use tactical_locale::Locale;
use tactical_sync::{BattlePhase, BattleSession, VictoryOutcome};

use crate::board::hex_world;
use crate::camera::MapRightClick;
use crate::fx::{FloaterKind, FxEvent, FxQueue};
use crate::icons::IconId;
use crate::locale::LocaleRes;
use crate::picking::{cursor_hex, unit_y_on_grid};
use crate::state::{
    BattleReport, BattleTour, CommandMode, DesyncAlert, DivPickKind, EngagementDetailView,
    FlashNotice, HighlightKind, NoticeBar, PendingUnitVisual, ReportLane, SyncStall, TacticalState,
    UiWindows,
};
use crate::units::MoveAnims;

/// Spawn tracers + damage floaters for a resolved strike.
fn push_combat_fx(
    fx: &mut FxQueue,
    from: HexCoord,
    to: HexCoord,
    org: f32,
    str_d: f32,
    artillery: bool,
    loc: &Locale,
) {
    let from_w = hex_world(from) + Vec3::Y * 0.55;
    let to_w = hex_world(to) + Vec3::Y * 0.45;
    fx.push(FxEvent::Tracer {
        from: from_w,
        to: to_w,
        artillery,
    });
    if org > 0.005 {
        let n = format!("{org:.1}");
        fx.push(FxEvent::Floater {
            pos: to_w,
            text: loc.trf("floater.org_damage", &[("n", &n)]),
            kind: FloaterKind::OrgDamage,
        });
    }
    if str_d > 0.005 {
        let n = format!("{str_d:.1}");
        fx.push(FxEvent::Floater {
            pos: to_w + Vec3::X * 0.2,
            text: loc.trf("floater.str_damage", &[("n", &n)]),
            kind: FloaterKind::StrDamage,
        });
    }
}

/// Commands pushed by UI buttons, drained by game systems.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCommand {
    SetMode(CommandMode),
    BeginBattle,
    /// Hand the still-undeployed battalions to the AI planner
    /// (player-side "Auto Deploy"); already-placed units are untouched.
    AutoDeploy,
    /// Recall EVERY player battalion back to the OOB queue
    /// (deployment phase; confirmation lives in the UI layer).
    RecallAll,
    /// Recall one division's battalions to the OOB queue.
    RecallDivision(String),
    EndTurn,
    Sync,
    ApplyAndExit,
    ToggleHold,
    ToggleEmplace,
    RetreatSelected,
    /// Restore the battle-start checkpoint (pre-first-sync only).
    RestartBattle,
    /// Restore the post-last-sync checkpoint.
    RollbackToSync,
    /// Clean process exit from the Esc menu (AppExit::Success).
    /// Any live-mode "unsynced battle" confirmation happens in the UI layer
    /// before this is queued.
    ExitGame,
    /// End Tactic on a LIVE battle — graceful early end: the
    /// partial hour rides phase 1 and the abort cleanup rides phase 2
    /// (`build_early_exit_batch(true)`; desync mode carries nothing —
    /// `carry=false`, cleanup only). Non-live: plain exit.
    EndTacticEarly,
    /// Abandon a LIVE battle (Esc exit) — unsynced damage is lost
    /// (§11.3, no phase 1), but the HOI4-side cleanup still rides phase 2
    /// (`build_early_exit_batch(false)` — the same cleanup-only batch every
    /// desync-mode exit sends). Non-live: plain exit.
    AbortLiveExit,
    /// Issue (or replace) a division order for `division` —
    /// issued from the HQ radial bar (Advance directly; Seize/Engage after
    /// the player picked the target on the map).
    DivOrder {
        division: String,
        pick: DivOrderPick,
    },
    /// Arm the map target pick for a division order (Seize hex /
    /// Engage unit) from the HQ command bar.
    DivPick {
        division: String,
        kind: DivPickKind,
    },
    /// Cancel `division`'s standing order (OOB header button or
    /// the HQ radial bar).
    CancelDivOrder(String),
}

/// A division-order target pick — the player's intent, resolved
/// into [`DivOrderTarget`] by the planner at plan time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivOrderPick {
    /// 推进: no target — the division advances per its tactic card (blind
    /// march on the pre-battle intel / flags when nothing is visible).
    Advance,
    /// 占领: march on the hex, then hold it (defense card) once seized.
    Seize(HexCoord),
    /// 歼敌: pursue the picked enemy battalion to destruction/rout.
    Engage { unit: usize, hex: HexCoord },
}

/// One division's standing order (DESIGN §7.4). Lives in
/// [`GameController`] so battle checkpoints (restart/rollback)
/// restore the orders too. At the start of every player turn the game loop
/// re-plans each ordered division's battalions that the player has not
/// manually overridden; the order persists until its goal is reached /
/// lost, the division runs out of combat-effective units, or the player
/// cancels it.
#[derive(Debug, Clone)]
pub struct DivisionOrder {
    pub pick: DivOrderPick,
    /// Seize hold-back phase: the point was occupied and the
    /// division now defends it with the defense card. An enemy re-taking
    /// the hex flips the flag back to the maneuver phase (目标被夺回).
    pub seized: bool,
    /// Engage pursuit: the target's last known position (updated only
    /// while the target is visible — fog hides the rest).
    pub engage_last_pos: Option<HexCoord>,
}

impl DivisionOrder {
    /// The label key of the order kind (localised via `div_order.*`).
    pub fn kind_key(&self) -> &'static str {
        match self.pick {
            DivOrderPick::Advance => "div_order.advance",
            DivOrderPick::Seize(_) => "div_order.seize",
            DivOrderPick::Engage { .. } => "div_order.engage",
        }
    }

    /// The tactic card for the current phase: the
    /// assault card while maneuvering / pursuing, the elastic-defense card
    /// once a seized point is held (occupy terrain, emplace guns, do not
    /// advance).
    pub fn phase_tactic(&self) -> CombatTactic {
        if self.seized {
            CombatTactic::ElasticDefense
        } else {
            CombatTactic::Assault
        }
    }

    /// The planner's target injection for the current phase.
    pub fn planner_target(&self) -> Option<DivOrderTarget> {
        match self.pick {
            DivOrderPick::Advance => None,
            DivOrderPick::Seize(hex) => Some(DivOrderTarget::Seize { hex }),
            DivOrderPick::Engage { unit, .. } => Some(DivOrderTarget::Engage {
                unit,
                last_pos: self.engage_last_pos.unwrap_or(HexCoord::ZERO),
            }),
        }
    }
}

/// One allied nation fighting on the player's side (DESIGN §7.5) — its
/// divisions are planned by their own TacticalAi each player
/// turn; the player may still issue division orders to its HQs (which
/// suspends the division from the allied slice while the order stands).
#[derive(Debug, Clone)]
pub struct AllyContingent {
    pub tag: String,
    pub tactic: CombatTactic,
    pub divisions: Vec<String>,
}

#[derive(Resource, Default)]
pub struct PendingCommands(pub Vec<PlayerCommand>);

/// A request for the host program to inject a batch into HOI4 (§3.2).
/// The render crate fills this on Sync / Apply&Exit; the bin crate's live
/// mode drains it and performs the actual console injection. In demo mode it
/// is only logged.
#[derive(Debug, Clone)]
pub struct InjectionRequest {
    pub batch: Vec<String>,
    pub is_final: bool,
    pub result: Option<String>,
    /// The second phase of a FINAL batch (§8.4) — unfreeze +
    /// cleanup lines the host injects only after `batch`'s clock-advance
    /// receipt (or its timeout). Always `None` for hourly syncs.
    pub phase2: Option<Vec<String>>,
}

#[derive(Resource, Default)]
pub struct PendingInjection(pub Option<InjectionRequest>);

/// AI turn execution queue (actions played out with small delays).
#[derive(Resource, Default)]
pub struct AiTurn {
    pub actions: VecDeque<AiAction>,
    pub timer: f32,
    pub active: bool,
}

/// Full battle-state snapshot for restart/rollback.
#[derive(Clone)]
pub struct BattleSnapshot {
    game: GameController,
    units: Vec<BattalionUnit>,
    fog: Option<FogOfWar>,
    deployment_zones: Option<(Vec<HexCoord>, Vec<HexCoord>)>,
}

impl BattleSnapshot {
    pub fn take(game: &GameController, state: &TacticalState) -> Self {
        BattleSnapshot {
            game: game.clone(),
            units: state.units.clone(),
            fog: state.fog.clone(),
            deployment_zones: state.deployment_zones.clone(),
        }
    }

    /// Write the snapshot back over the live resources and force a full
    /// re-render; transient UI state (selection/hover/drag) is reset.
    fn restore(self, game: &mut GameController, state: &mut TacticalState) {
        *game = self.game;
        state.units = self.units;
        state.fog = self.fog;
        state.deployment_zones = self.deployment_zones;
        state.turn = game.session.turn_number;
        state.selected_unit = None;
        state.hover_hex = None;
        state.command_mode = CommandMode::Select;
        state.deploy_drag = None;
        state.deploy_placing = None;
        state.deploy_sector = None;
        state.sector_preview.clear();
        state.attack_orders.clear(); // orders don't survive a rewind
        state.clear_highlights();
        state.board_mesh_dirty = true;
        state.board_colors_dirty = true;
        state.units_dirty = true;
        state.orders_dirty = true;
        // The snapshot's game carries its own allied_sectors —
        // the suggestion overlay must repaint to match.
        state.ally_sectors_dirty = true;
    }
}

/// Checkpoint slots. `battle_start` is captured when the battle
/// scenario is created (deployment phase); `last_sync` right after each
/// completed sync — never earlier, since already-injected damage cannot be
/// un-sent to HOI4.
#[derive(Resource, Default)]
pub struct Checkpoints {
    pub battle_start: Option<BattleSnapshot>,
    pub last_sync: Option<BattleSnapshot>,
}

/// One battle-log line: localized text + optional leading icon (§15).
#[derive(Debug, Clone)]
pub struct LogLine {
    pub icon: Option<crate::icons::IconId>,
    pub text: String,
    /// Combat exchange lines carry their formula chain — the
    /// line becomes clickable and opens the engagement-detail window.
    pub detail: Option<Box<EngagementDetailView>>,
}

/// Top-level game controller resource.
/// Clone: checkpoint snapshots (restart/rollback).
#[derive(Resource, Clone)]
pub struct GameController {
    pub session: BattleSession,
    pub combat: CombatEngine,
    pub ai: TacticalAi,
    pub enemy_tactic: CombatTactic,
    /// Battle seed: the division-order planners derive their own
    /// per-division, per-turn seeds from it, so replays are deterministic.
    pub seed: u64,
    /// Standing division orders, keyed by division name. Also
    /// checkpointed — a rollback restores the orders as of that moment.
    pub div_orders: HashMap<String, DivisionOrder>,
    /// The turn number the division orders were last planned for
    /// (turn-start planning runs once per turn; order issue / cancel /
    /// completion re-arms or marks it).
    pub div_planned_turn: u32,
    /// The player side's AI-controlled allied contingents (§7.5,
    /// script battles). Empty = the player commands her whole side.
    pub allies: Vec<AllyContingent>,
    /// Every division name → its country tag (both sides; the
    /// side tag for undeclared divisions). Empty for non-script battles.
    pub division_tags: HashMap<String, String>,
    /// Player-drawn deployment suggestion rectangles for allied
    /// divisions (division name → anchor/release hex), consumed at BeginBattle.
    pub allied_sectors: HashMap<String, (HexCoord, HexCoord)>,
    /// Injection batch context (live mode): HOI4 state/province + template.
    pub province: u32,
    pub template: String,
    /// Battle location label for the right panel: demo/debug set a
    /// place name ("Sedan"), live mode shows "Province #id" until a province
    /// name table is extracted.
    pub location: String,
    /// Damage summary of the most recent sync — displayed in the
    /// sync-completion prompt (Continue / End Tactic).
    pub last_sync_summary: Option<String>,
    /// Most recent combat results (for FX + UI log).
    pub last_results: Vec<tactical_combat::CombatResult>,
    /// Battle event log lines (newest last).
    pub log: Vec<LogLine>,
}

impl GameController {
    pub fn new(player_side: Side, enemy_tactic: CombatTactic, seed: u64) -> Self {
        let mut session = BattleSession::with_params(&CombatParams::default());
        session.player_side = player_side;
        GameController {
            session,
            combat: CombatEngine::new(CombatParams::default(), seed),
            ai: TacticalAi::new(player_side.opponent(), enemy_tactic, seed ^ 0x5EED),
            enemy_tactic,
            seed,
            div_orders: HashMap::new(),
            div_planned_turn: 0,
            allies: Vec::new(),
            division_tags: HashMap::new(),
            allied_sectors: HashMap::new(),
            province: 0,
            template: "Infanterie-Division".to_string(),
            location: String::new(),
            last_sync_summary: None,
            last_results: Vec::new(),
            log: Vec::new(),
        }
    }

    /// The allied contingent owning `division`, if any.
    pub fn allied_division(&self, division: &str) -> Option<&AllyContingent> {
        self.allies
            .iter()
            .find(|a| a.divisions.iter().any(|d| d == division))
    }

    /// Same-side unit commanded by an allied nation's AI.
    pub fn is_allied(&self, u: &BattalionUnit) -> bool {
        u.side == self.session.player_side && self.allied_division(&u.division).is_some()
    }

    /// THE commandability predicate — same side and NOT
    /// allied-AI-controlled.
    pub fn commands(&self, u: &BattalionUnit) -> bool {
        u.side == self.session.player_side && !self.is_allied(u)
    }

    /// Append an icon-less log line (convenience wrapper).
    fn log_line(&mut self, msg: impl Into<String>) {
        self.log_line_icon(None, msg);
    }

    /// Append one localized battle-log line; the tracing mirror stays plain
    /// text (icons are a UI-layer decoration, §15). Pub for the bin crate's
    /// live systems (the external-resolution notice).
    pub fn log_line_icon(&mut self, icon: Option<IconId>, msg: impl Into<String>) {
        let text = msg.into();
        info!("{text}");
        self.log.push(LogLine {
            icon,
            text,
            detail: None,
        });
        // The battle log is a standalone scrollable window
        // (Esc menu) instead of a 6-line panel corner — keep real history.
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }
}

/// Systems plugin section for the game loop.
pub struct GameLoopPlugin;

impl Plugin for GameLoopPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingCommands>()
            .init_resource::<PendingInjection>()
            .init_resource::<AiTurn>()
            .init_resource::<Checkpoints>()
            .init_resource::<BattleTour>()
            // Render gate: PreUpdate so the egui painters / picking
            // read this frame's gate.open decision. No-ops (beyond the
            // minimize guard) when the low_power setting didn't insert the
            // RenderGate resource.
            .add_systems(PreUpdate, crate::gate::render_gate)
            .add_systems(
                Update,
                (
                    debug_fog_toggle,
                    handle_ui_commands,
                    tick_sector_preview,
                    handle_deploy_drag,
                    handle_map_clicks,
                    run_ai_turn,
                    tick_division_orders,
                    tick_battle_tour,
                    tick_battle,
                )
                    .chain()
                    // Consume this frame's inputs, never stale ones —
                    // map clicks read hover/MapRightClick (written by the
                    // camera+picking systems) and PendingCommands (filled by
                    // the egui panels).
                    .after(crate::picking::update_hover)
                    .after(crate::camera::rts_camera_controller)
                    .after(crate::ui::draw_panels),
            );
    }
}

/// F8: debug fog toggle — testing aid for inspecting AI deployment and fog
/// leaks (render-only, never touches game logic). Dirty flags force tiles
/// and unit models to re-evaluate visibility.
fn debug_fog_toggle(keys: Res<ButtonInput<KeyCode>>, mut state: ResMut<TacticalState>) {
    if keys.just_pressed(KeyCode::F8) {
        state.debug_no_fog = !state.debug_no_fog;
        state.board_colors_dirty = true;
        state.units_dirty = true;
    }
}

/// Convenience: is it the player's phase & turn to act?
fn player_can_act(game: &GameController, state: &TacticalState) -> bool {
    game.session.phase == BattlePhase::TacticalActive
        && game.session.current_side == state.player_side
}

// ---------------------------------------------------------------------------
// UI button intents
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn handle_ui_commands(
    mut pending: ResMut<PendingCommands>,
    mut injection: ResMut<PendingInjection>,
    mut game: Option<ResMut<GameController>>,
    mut state: ResMut<TacticalState>,
    mut checkpoints: ResMut<Checkpoints>,
    mut ai_turn: ResMut<AiTurn>,
    mut fx: ResMut<FxQueue>,
    mut anims: ResMut<MoveAnims>,
    mut tour: ResMut<BattleTour>,
    mut notice: ResMut<NoticeBar>,
    time: Res<Time>,
    loc: Res<LocaleRes>,
    mut exit: EventWriter<AppExit>,
) {
    // ExitGame is handled even without a battle (menu must always work).
    if pending
        .0
        .iter()
        .any(|c| matches!(c, PlayerCommand::ExitGame))
    {
        exit.send(AppExit::Success);
        return;
    }
    let Some(game) = game.as_deref_mut() else {
        pending.0.clear();
        return;
    };
    for cmd in pending.0.drain(..) {
        crate::stage!("handle_ui_commands: {cmd:?}");
        match cmd {
            // Intercepted above the drain so it also fires with no battle
            // loaded; nothing to do inside the loop.
            PlayerCommand::ExitGame => {}
            PlayerCommand::SetMode(mode) => {
                state.command_mode = mode;
                refresh_command_highlights(&mut state);
            }
            PlayerCommand::RestartBattle => {
                // Only before the first sync: afterwards the damage already
                // injected into HOI4 makes a full restart a desync.
                if game.session.strategic_hour == 0 {
                    if let Some(snap) = checkpoints.battle_start.clone() {
                        snap.restore(game, &mut state);
                        *ai_turn = AiTurn::default();
                        injection.0 = None;
                        game.log_line(loc.tr("log.checkpoint.restarted"));
                    }
                }
            }
            PlayerCommand::RollbackToSync => {
                if let Some(snap) = checkpoints.last_sync.clone() {
                    snap.restore(game, &mut state);
                    *ai_turn = AiTurn::default();
                    injection.0 = None;
                    game.log_line(loc.tr("log.checkpoint.rollback"));
                }
            }
            // Issue (or replace) a division order — only during
            // the player's own tactical turn; the immediate plan puts the
            // division in motion this very turn.
            PlayerCommand::DivOrder { division, pick } => {
                issue_division_order(
                    game,
                    &mut state,
                    &loc,
                    &mut notice,
                    time.elapsed_secs(),
                    &division,
                    pick,
                );
            }
            // Arm the division-order target pick on the map
            // (Seize hex / Engage unit) from the HQ command bar.
            PlayerCommand::DivPick { division, kind } => {
                if game.session.phase != BattlePhase::TacticalActive
                    || game.session.current_side != state.player_side
                {
                    continue;
                }
                state.div_pick = Some(crate::state::DivPick { division, kind });
                refresh_command_highlights(&mut state);
                let hint = match kind {
                    crate::state::DivPickKind::Seize => {
                        loc.tr("battle.hint.div_pick_seize").into_owned()
                    }
                    crate::state::DivPickKind::Engage => {
                        loc.tr("battle.hint.div_pick_engage").into_owned()
                    }
                };
                notice.flash = Some(FlashNotice::plain(hint, time.elapsed_secs() + 4.0));
            }
            PlayerCommand::CancelDivOrder(division) => {
                if game.div_orders.remove(&division).is_some() {
                    game.log_line(loc.trf("log.div.cancelled", &[("div", &division)]));
                }
                // The remaining orders were already planned this turn — the
                // turn-start tick must not re-plan after a cancel.
                game.div_planned_turn = game.session.turn_number;
                // The HQ's division-filter ribbon overlay must
                // rebuild (an HQ selection with no order falls back to the
                // single-unit ribbon).
                state.orders_dirty = true;
            }
            PlayerCommand::BeginBattle => {
                if game.session.phase == BattlePhase::Deployment {
                    // Units start OFF the
                    // board; the battle may not start while any are still
                    // waiting in the OOB (reject + prompt).
                    // Only units the PLAYER commands block the
                    // start — allied divisions wait OFFBOARD for their own
                    // AI (deploy_allied_nations below) by design.
                    let undep = state
                        .units
                        .iter()
                        .filter(|u| game.commands(u) && u.undeployed && u.is_combat_effective())
                        .count();
                    if undep > 0 {
                        let n = undep.to_string();
                        game.log_line(loc.trf("log.deploy.cannot_start", &[("n", &n)]));
                        notice.flash = Some(FlashNotice::plain(
                            loc.trf("notice.deploy.undeployed", &[("n", &n)]),
                            time.elapsed_secs() + 4.0,
                        ));
                        continue;
                    }
                    if let Err(e) = game.session.start_battle() {
                        let err = e.to_string();
                        game.log_line(loc.trf("log.deploy.cannot_start_error", &[("err", &err)]));
                        continue;
                    }
                    // Enemy AI deploys only now, inside its own zone, out of
                    // the player's sight (§11.1.5).
                    // `false` — the enemy arrives NOT flagged
                    // undeployed (the assembly spread is a placeholder); a
                    // true here would plan zero units and leave the whole
                    // side piled on the zone's first hexes.
                    let enemy = state.player_side.opponent();
                    deploy_side(
                        &mut state,
                        enemy,
                        game.enemy_tactic,
                        &HashSet::new(),
                        &HashSet::new(),
                        false,
                    );
                    // The allied contingents deploy through their own AI
                    // (§7.5) — into the player's sector suggestion
                    // where one was drawn, else the full player zone.
                    deploy_allied_nations(game, &mut state, &loc);
                    // Zones stay in state (border mesh re-shows them under
                    // the F8 debug view); zone-gated logic is phase-driven.
                    state.clear_highlights();
                    update_fog(&mut state, game);
                    game.log_line(loc.tr("log.deploy.battle_started"));
                }
            }
            PlayerCommand::AutoDeploy => {
                if game.session.phase == BattlePhase::Deployment {
                    // Hand-placed units must survive: the planner
                    // skips non-undeployed units outright, and their hexes are
                    // also marked pre-used so nobody re-takes them.
                    let pre_used: HashSet<(i32, i32)> = state
                        .units
                        .iter()
                        .filter(|u| {
                            u.side == state.player_side && !u.undeployed && u.is_combat_effective()
                        })
                        .map(|u| (u.position.q, u.position.r))
                        .collect();
                    let player = state.player_side;
                    // Allied divisions stay OFFBOARD for their own
                    // AI (BeginBattle) — Auto Deploy fills only the player's
                    // own battalions.
                    let exclude: HashSet<String> = game
                        .allies
                        .iter()
                        .flat_map(|a| a.divisions.iter().cloned())
                        .collect();
                    let placed = deploy_side(
                        &mut state,
                        player,
                        CombatTactic::Default,
                        &pre_used,
                        &exclude,
                        true, // OOB waiters only — hand-placed stay put
                    );
                    if placed > 0 {
                        let n = placed.to_string();
                        game.log_line(loc.trf("log.deploy.auto_deploy", &[("n", &n)]));
                        notice.flash = Some(FlashNotice::plain(
                            loc.trf("notice.deploy.auto_deployed", &[("n", &n)]),
                            time.elapsed_secs() + 3.0,
                        ));
                    } else {
                        game.log_line(loc.tr("log.deploy.auto_deploy_empty"));
                    }
                }
            }
            PlayerCommand::RecallAll | PlayerCommand::RecallDivision(_) => {
                if game.session.phase == BattlePhase::Deployment {
                    let player = state.player_side;
                    let division = match &cmd {
                        PlayerCommand::RecallDivision(d) => Some(d.clone()),
                        _ => None,
                    };
                    // For an ALLIED division "recall" clears the
                    // stored sector suggestion — allied battalions deploy
                    // through their own AI at BeginBattle, so there is
                    // nothing to pull back (player divisions unchanged).
                    if let Some(d) = &division {
                        if game.allied_division(d).is_some() {
                            if game.allied_sectors.remove(d).is_some() {
                                state.ally_sectors_dirty = true;
                            }
                            continue;
                        }
                    }
                    let mut recalled = 0usize;
                    for u in &mut state.units {
                        let in_scope = u.side == player
                            && !u.undeployed
                            && division.as_deref().map(|d| u.division == d).unwrap_or(true);
                        if in_scope {
                            u.undeployed = true;
                            u.position = BattalionUnit::OFFBOARD;
                            u.move_order = None;
                            u.is_holding = false;
                            recalled += 1;
                        }
                    }
                    state.units_dirty = true;
                    state.selected_unit = None;
                    let label = match &division {
                        Some(d) if d.is_empty() => loc.tr("oob.unattached").into_owned(),
                        Some(d) => d.clone(),
                        None => loc.tr("oob.all").into_owned(),
                    };
                    let n = recalled.to_string();
                    game.log_line(loc.trf("log.deploy.recall", &[("n", &n), ("label", &label)]));
                    notice.flash = Some(FlashNotice::plain(
                        loc.trf("notice.deploy.recalled", &[("n", &n)]),
                        time.elapsed_secs() + 3.0,
                    ));
                }
            }
            PlayerCommand::EndTurn => {
                // The battle-report modal must be read through
                // before the next turn can start.
                if tour.active || !player_can_act(game, &state) {
                    continue;
                }
                end_player_turn(
                    &mut *game,
                    &mut state,
                    &mut fx,
                    &mut anims,
                    &mut tour,
                    &loc,
                    &mut notice,
                    time.elapsed_secs(),
                );
            }
            PlayerCommand::Sync => {
                // Desync mode: the sync pipeline is dead — the session
                // never rests in ReadyToSync (hours seal locally), and
                // this command is a backstop only.
                if game.session.phase == BattlePhase::ReadyToSync && !game.session.sync_disabled {
                    let summary = game.session.hourly_damage_summary();
                    // Side-aware: the batch reports the PLAYER side's
                    // losses — show both, labeled, not the raw attacker
                    // fields mislabeled "dmg dealt".
                    let (p_org, p_str) = summary.for_side(state.player_side);
                    let (e_org, e_str) = summary.for_side(state.player_side.opponent());
                    let hour = game.session.strategic_hour + 1;
                    let (hour_s, p_org_s, p_str_s, e_org_s, e_str_s) = (
                        hour.to_string(),
                        format!("{p_org:.2}"),
                        format!("{p_str:.2}"),
                        format!("{e_org:.2}"),
                        format!("{e_str:.2}"),
                    );
                    game.log_line_icon(
                        Some(IconId::Sync),
                        loc.trf(
                            "log.sync.losses",
                            &[
                                ("hour", &hour_s),
                                ("p_org", &p_org_s),
                                ("p_str", &p_str_s),
                                ("e_org", &e_org_s),
                                ("e_str", &e_str_s),
                            ],
                        ),
                    );
                    // The sync-completion prompt shows this summary.
                    game.last_sync_summary = Some(loc.trf(
                        "log.sync.summary",
                        &[
                            ("p_org", &p_org_s),
                            ("p_str", &p_str_s),
                            ("e_org", &e_org_s),
                            ("e_str", &e_str_s),
                        ],
                    ));
                    // Transitions first: queue the injection only if
                    // the session actually advances — otherwise the batch
                    // would fire while the session stays put. (The batch is
                    // a pure read, so building it ahead is safe.)
                    let batch = game.session.build_sync_batch();
                    match game
                        .session
                        .start_sync()
                        .and_then(|_| game.session.complete_sync())
                    {
                        Ok(()) => {
                            injection.0 = Some(InjectionRequest {
                                batch,
                                is_final: false,
                                result: None,
                                phase2: None,
                            });
                            state.board_colors_dirty = true;
                            // §6.11: the collapse lines just went
                            // into the batch — they must not ride again.
                            game.session.mark_collapse_injected();
                            // This is the newest state HOI4 has seen —
                            // rollback target until the next sync completes.
                            checkpoints.last_sync = Some(BattleSnapshot::take(game, &state));
                        }
                        Err(e) => {
                            let err = format!("{e:?}");
                            game.log_line(loc.trf("log.sync.rejected", &[("err", &err)]));
                        }
                    }
                }
            }
            PlayerCommand::ApplyAndExit => {
                if game.session.phase == BattlePhase::ReadyToEnd {
                    let winner = match game.session.check_victory(&state.units) {
                        VictoryOutcome::Winner(Side::Attacker) => "attacker_victory",
                        VictoryOutcome::Winner(Side::Defender) => "defender_victory",
                        // Mutual annihilation and any non-winner case.
                        _ => "draw",
                    };
                    // Same transition-first ordering as Sync; the
                    // batch itself is a pure read. The end
                    // injection splits in two (§8.4) — phase 1 (damage + collapse
                    // + the clock advance) goes out first; phase 2
                    // (unfreeze + cleanup) follows the clock receipt.
                    // Desync mode: the results must NOT be written into a
                    // world this battle no longer matches — the exit sends
                    // the abort cleanup batch only (unfreeze + flags +
                    // popup, no damage, no clock).
                    let end = if game.session.sync_disabled {
                        game.session.build_early_exit_batch(false)
                    } else {
                        game.session.build_end_batch()
                    };
                    match game
                        .session
                        .complete_end()
                        .and_then(|_| game.session.finish())
                    {
                        Ok(()) => {
                            injection.0 = Some(InjectionRequest {
                                batch: end.phase1,
                                is_final: true,
                                result: Some(winner.to_string()),
                                phase2: Some(end.phase2),
                            });
                            // §6.11: same as the sync path — the
                            // collapse lines must not ride a second batch.
                            game.session.mark_collapse_injected();
                            if game.session.sync_disabled {
                                game.log_line_icon(
                                    Some(IconId::Door),
                                    loc.tr("log.desync.cleanup_exit"),
                                );
                            } else {
                                game.log_line_icon(Some(IconId::Check), loc.tr("log.sync.applied"));
                            }
                        }
                        Err(e) => {
                            let err = format!("{e:?}");
                            game.log_line(loc.trf("log.end.rejected", &[("err", &err)]));
                        }
                    }
                }
            }
            PlayerCommand::EndTacticEarly => {
                // End Tactic on a LIVE battle must run the same
                // cleanup protocol as a resolved battle — otherwise the
                // freeze / tactical-mode flags would outlive the window.
                // Non-live: nothing to clean up.
                if game.session.battle_ctx.is_none() {
                    exit.send(AppExit::Success);
                    continue;
                }
                if matches!(
                    game.session.phase,
                    BattlePhase::TacticalActive | BattlePhase::ReadyToSync
                ) {
                    game.session.ready_to_end().ok();
                }
                if game.session.phase == BattlePhase::ReadyToEnd {
                    // Desync mode carries nothing: no partial-hour damage,
                    // no clock — the exit is the abort cleanup batch only.
                    let batches = game
                        .session
                        .build_early_exit_batch(!game.session.sync_disabled);
                    match game
                        .session
                        .complete_end()
                        .and_then(|_| game.session.finish())
                    {
                        Ok(()) => {
                            injection.0 = Some(InjectionRequest {
                                batch: batches.phase1,
                                is_final: true,
                                result: None,
                                phase2: Some(batches.phase2),
                            });
                            game.session.mark_collapse_injected();
                            if game.session.sync_disabled {
                                game.log_line_icon(
                                    Some(IconId::Door),
                                    loc.tr("log.desync.cleanup_exit"),
                                );
                            } else {
                                game.log_line_icon(Some(IconId::Check), loc.tr("log.sync.applied"));
                            }
                        }
                        Err(e) => {
                            let err = format!("{e:?}");
                            game.log_line(loc.trf("log.end.rejected", &[("err", &err)]));
                        }
                    }
                }
            }
            PlayerCommand::AbortLiveExit => {
                // Abandoning a live battle (Esc exit) — unsynced
                // damage is lost (§11.3, empty phase 1), but the HOI4-side
                // cleanup still rides phase 2 (unfreeze + flags + the abort
                // popup); the `tac_abort` log token is the listener's
                // uniform "tactical mode off" signal (and our own abort
                // watcher reads it too — abort() on Ended is idempotent).
                if game.session.battle_ctx.is_none() {
                    exit.send(AppExit::Success);
                    continue;
                }
                if game.session.phase != BattlePhase::Ended {
                    let batches = game.session.build_early_exit_batch(false);
                    injection.0 = Some(InjectionRequest {
                        batch: batches.phase1,
                        is_final: true,
                        result: None,
                        phase2: Some(batches.phase2),
                    });
                    // Ended now; exit_when_ended's countdown only ticks on
                    // frames where drain_injections is not blocking, so the
                    // batch goes out before the window closes.
                    game.session.abort();
                }
            }
            PlayerCommand::ToggleHold => {
                if !player_can_act(game, &state) {
                    continue;
                }
                if let Some(id) = state.selected_unit {
                    let mut took_cover = false;
                    if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
                        // Hold is a command — allied battalions
                        // take their stance from their own staff AI.
                        if !game.commands(u) {
                            continue;
                        }
                        // Infantry-attribute only
                        // (incl. motorized/mechanized; cavalry and vehicle
                        // crews cannot take cover). Taking cover
                        // COSTS the turn's action and cancels any march;
                        // standing back up is free. The stance drops on
                        // movement (movement.rs) / attacking (combat).
                        if !u.can_hold() {
                            continue;
                        }
                        if u.is_holding {
                            u.is_holding = false;
                            // Standing up = the Hold command is
                            // done — the division AI may resume command.
                            u.manual_override = false;
                            game.log_line(loc.trf("log.order.stand_up", &[("name", &u.name)]));
                        } else {
                            if u.acted {
                                continue;
                            }
                            u.is_holding = true;
                            u.acted = true; // §6.8: taking cover takes the turn
                            u.move_order = None;
                            // Hold is an open-ended player command
                            // (the division AI must not march the unit away).
                            u.manual_override = true;
                            took_cover = true;
                            game.log_line_icon(
                                Some(IconId::Defense),
                                loc.trf("log.order.take_cover", &[("name", &u.name)]),
                            );
                        }
                    }
                    // §6.2 命令顶替: taking cover also cancels any
                    // attack order registered this turn (done outside the
                    // unit borrow — ResMut field access borrows all of it).
                    if took_cover {
                        state.attack_orders.retain(|o| o.attacker != id);
                    }
                }
            }
            PlayerCommand::ToggleEmplace => {
                if !player_can_act(game, &state) {
                    continue;
                }
                if let Some(id) = state.selected_unit {
                    if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
                        // Emplacement is a command — `commands`
                        // covers the old side check and excludes allies.
                        if !game.commands(u) || !u.requires_emplacement() || u.acted {
                            continue;
                        }
                        u.is_emplaced = !u.is_emplaced;
                        u.acted = true; // §6.3: (un-)limbering takes the turn
                                        // Emplacement is an open-ended player
                                        // command (refresh_turn keeps the override while the
                                        // gun stays emplaced; limbering keeps it too — the
                                        // player is handling this gun until she stands by).
                        u.manual_override = true;
                        if u.is_emplaced {
                            u.move_order = None;
                        }
                        let name = u.name.clone();
                        let key = if u.is_emplaced {
                            "log.order.emplace"
                        } else {
                            "log.order.limber"
                        };
                        game.log_line(loc.trf(key, &[("name", &name)]));
                        state.units_dirty = true;
                        state.orders_dirty = true;
                    }
                }
            }
            PlayerCommand::RetreatSelected => {
                if !player_can_act(game, &state) {
                    continue;
                }
                if let Some(id) = state.selected_unit {
                    let player = state.player_side;
                    // Manual retreat is disengagement — only
                    // legal in contact with the enemy.
                    let in_contact = state.unit_by_id(id).is_some_and(|u| {
                        state.units.iter().any(|e| {
                            e.side != player
                                && e.is_combat_effective()
                                && e.position.distance(u.position) == 1
                        })
                    });
                    if !in_contact {
                        continue;
                    }
                    if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
                        // Manual retreat is a command — `commands`
                        // covers the old side check and excludes allies.
                        // An acted unit's ring hides R (ui.rs contract) — the
                        // handler enforces the same gate.
                        if game.commands(u) && u.is_combat_effective() && !u.acted {
                            u.state = UnitState::Retreating;
                            u.org *= 0.8; // manual retreat org penalty (§6.8)
                            u.entrenchment = 0;
                            u.manual_override = true; // player-ordered
                            let name = u.name.clone();
                            game.log_line(loc.trf("log.order.retreat", &[("name", &name)]));
                            state.units_dirty = true;
                            state.board_colors_dirty = true;
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Map clicks: selection + command targeting
// ---------------------------------------------------------------------------

/// Drag-and-drop deployment. Left-press on an own unit starts a
/// drag (and selects it); while held, `sync_deploy_ghost` previews the drop
/// hex; releasing over a valid zone hex moves the unit there, anywhere else
/// cancels. Plain click-to-deploy (select, then click a zone hex) still
/// works through `handle_map_clicks`.
fn handle_deploy_drag(
    mut game: Option<ResMut<GameController>>,
    mut state: ResMut<TacticalState>,
    mouse: Res<ButtonInput<MouseButton>>,
    loc: Res<LocaleRes>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>, // the 3D scene camera
) {
    let Some(game) = game.as_deref_mut() else {
        return;
    };
    if game.session.phase != BattlePhase::Deployment {
        state.deploy_drag = None;
        return;
    }
    // OOB placement AND sector picking own the mouse — no
    // drag-away while either is active.
    if state.deploy_placing.is_some() || state.deploy_sector.is_some() {
        state.deploy_drag = None;
        return;
    }

    if mouse.just_pressed(MouseButton::Left) && !state.pointer_over_ui {
        if let Some(h) = cursor_hex(&windows, &q_cam) {
            if let Some((uid, uside, commanded)) =
                state.unit_at(h).map(|u| (u.id, u.side, game.commands(u)))
            {
                if uside == state.player_side {
                    // An ALLIED battalion stays info-selectable
                    // but deploys through its own staff — no drag.
                    if commanded {
                        state.deploy_drag = Some(uid);
                    }
                    state.selected_unit = Some(uid);
                    state.units_dirty = true;
                }
            }
        }
    }

    if mouse.just_released(MouseButton::Left) {
        let Some(id) = state.deploy_drag.take() else {
            return;
        };
        if state.pointer_over_ui {
            return; // dropped over a panel — cancel, unit stays put
        }
        let Some(h) = cursor_hex(&windows, &q_cam) else {
            return;
        };
        let Some(grid) = state.grid.clone() else {
            return;
        };
        if !grid.in_bounds(h) || state.unit_at(h).is_some() {
            return; // off-board or occupied (dropping on itself is a no-op)
        }
        let zone_ok = state
            .deployment_zones
            .as_ref()
            .map(|(a, d)| {
                let z = if state.player_side == Side::Attacker {
                    a
                } else {
                    d
                };
                z.contains(&h)
            })
            .unwrap_or(true);
        let deployable = grid
            .cell(h)
            .map(|c| c.is_passable && c.terrain.is_deployable())
            .unwrap_or(false);
        if zone_ok && deployable {
            if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
                u.position = h;
                state.units_dirty = true;
            }
        } else if zone_ok && !deployable {
            game.log_line(loc.tr("log.deploy.impassable"));
        }
    }
}

/// Mouse scheme. Left button = non-sticky selection (click a
/// friendly unit to select; click anything else to release). Right button =
/// context command for the selected unit: empty/own hex → standing move
/// order; visible enemy → attack (indirect artillery delivers a fire
/// mission, direct-fire units assault adjacent targets or direct-fire at
/// range); the selected unit itself → stand by (cancels both the standing
/// move order and any registered attack order). Right-DRAG orbits the
/// camera — the click/drag split lives in camera.rs (`MapRightClick`).
#[allow(clippy::too_many_arguments)]
fn handle_map_clicks(
    mut game: Option<ResMut<GameController>>,
    mut state: ResMut<TacticalState>,
    mut fx: ResMut<FxQueue>,
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut rclick: ResMut<MapRightClick>,
    mut ui_win: ResMut<UiWindows>,
    stall: Option<Res<SyncStall>>,
    desync: Option<Res<DesyncAlert>>,
    mut tour: ResMut<BattleTour>,
    mut notice: ResMut<NoticeBar>,
    time: Res<Time>,
    loc: Res<LocaleRes>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>, // the 3D scene camera
) {
    // Right-click arrives via the camera's click/orbit splitter; take() up
    // front so a stale click can never linger and fire twice.
    let right_clicked = rclick.0.take().is_some();
    // The sync-stall dialog demands an explicit choice — freeze
    // ALL map input AND the Esc ladder (no peeling, no menu) while open.
    // The desync guard's dialog is modal the same way.
    if stall.is_some() || desync.is_some() {
        return;
    }
    let Some(game) = game.as_deref_mut() else {
        return;
    };

    // Esc priority ladder: close the topmost modal → cancel
    // fire picking → drop the selection → open the menu. While ANY modal is
    // open (Esc menu / sync prompt / confirmations), ALL map input freezes
    // (pointer guard covers the mouse; this covers same-frame click races).
    if ui_win.modal_open() {
        if keys.just_pressed(KeyCode::Escape) {
            ui_win.close_modal();
        }
        return;
    }
    // Battle reports open: ALL map input is frozen while the
    // modal walks the engagements; Esc skips the remaining reports.
    if tour.active {
        if keys.just_pressed(KeyCode::Escape) {
            tour.index = tour.queue.len();
        }
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        if state.deploy_sector.is_some() {
            // Esc cancels sector deployment picking.
            state.deploy_sector = None;
            state.clear_highlights();
        } else if state.deploy_placing.is_some() {
            // Esc cancels OOB placement mode.
            state.deploy_placing = None;
        } else if state.attach_picking.is_some() {
            // Cancel an attachment re-assignment first.
            state.attach_picking = None;
        } else if state.command_mode == CommandMode::FirePicking {
            // Esc cancels fire-mission picking.
            state.command_mode = CommandMode::Select;
            refresh_command_highlights(&mut state);
        } else if state.div_pick.is_some() {
            // Esc cancels division-order target picking.
            state.div_pick = None;
            refresh_command_highlights(&mut state);
        } else if state.selected_unit.is_some() {
            state.selected_unit = None;
            state.units_dirty = true;
            state.clear_highlights();
        } else {
            ui_win.esc_menu = true;
        }
        return;
    }

    let left = mouse.just_pressed(MouseButton::Left) && !state.pointer_over_ui;
    let left_released = mouse.just_released(MouseButton::Left) && !state.pointer_over_ui;
    let right = right_clicked && !state.pointer_over_ui;
    if !left && !left_released && !right {
        return;
    }
    let Some(h) = cursor_hex(&windows, &q_cam) else {
        return;
    };
    let Some(grid) = state.grid.clone() else {
        return;
    };
    if !grid.in_bounds(h) {
        return;
    }

    match game.session.phase {
        BattlePhase::Deployment => {
            // Sector deployment: OOB "deploy to sector" — left
            // press anchors the rectangle, the per-frame preview system
            // highlights it while dragging, release commits the division's
            // battalions into the sector; right / Esc cancels.
            if state.deploy_sector.is_some() {
                let division = state.deploy_sector.as_ref().unwrap().division.clone();
                let anchored = state.deploy_sector.as_ref().unwrap().anchor;
                if right {
                    state.deploy_sector = None;
                    state.clear_highlights();
                    return;
                }
                if anchored.is_none() {
                    if left {
                        // Anchor the rectangle (clicking the same spot again
                        // before dragging still works — release commits).
                        state.deploy_sector.as_mut().unwrap().anchor = Some(h);
                    }
                    return;
                }
                if left {
                    // Left-click while already anchored: cancel and re-anchor
                    // (misdrag recovery); the preview system keeps running.
                    state.deploy_sector.as_mut().unwrap().anchor = Some(h);
                    return;
                }
                if left_released {
                    // Commit the sector: the rectangle from the anchor to the
                    // release hex, intersected with the player's zone and
                    // deployable terrain, fed to the AI planner for this
                    // division's undeployed battalions. For an
                    // ALLIED division it stores a deployment suggestion
                    // instead (its AI deploys the division at BeginBattle).
                    commit_sector_deploy(
                        &mut state,
                        game,
                        division,
                        anchored.unwrap(),
                        h,
                        &loc,
                        &mut notice,
                        time.elapsed_secs(),
                    );
                    state.deploy_sector = None;
                    state.clear_highlights();
                    return;
                }
                return;
            }
            // OOB placement mode owns the
            // mouse — left drops the picked unit on a legal hex, right
            // cancels the placement (selection untouched).
            if let Some(id) = state.deploy_placing {
                if right {
                    state.deploy_placing = None;
                    return;
                }
                if !left {
                    return;
                }
                let zone_ok = state
                    .deployment_zones
                    .as_ref()
                    .map(|(a, d)| {
                        let z = if state.player_side == Side::Attacker {
                            a
                        } else {
                            d
                        };
                        z.contains(&h)
                    })
                    .unwrap_or(true);
                let deployable = grid
                    .cell(h)
                    .map(|c| c.is_passable && c.terrain.is_deployable())
                    .unwrap_or(false);
                if zone_ok && deployable && state.unit_at(h).is_none() {
                    if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
                        u.position = h;
                        u.undeployed = false;
                        state.deploy_placing = None;
                        state.selected_unit = Some(id);
                        state.units_dirty = true;
                    }
                } else if zone_ok && !deployable {
                    game.log_line(loc.tr("log.deploy.impassable"));
                }
                return;
            }
            if right {
                // Right-click only releases the selection during deployment.
                state.selected_unit = None;
                state.units_dirty = true;
                return;
            }
            // Click own unit → select; click empty zone hex → reposition (§11.1.5).
            let clicked = state.unit_at(h).map(|u| (u.id, u.side));
            if let Some((uid, uside)) = clicked {
                if uside == state.player_side {
                    state.selected_unit = Some(uid);
                    state.units_dirty = true;
                }
                return;
            }
            if let Some(id) = state.selected_unit {
                // An ALLIED selection is information-only — clicking
                // a zone hex must not reposition another staff's battalion.
                let Some(sel) = state.unit_by_id(id) else {
                    return;
                };
                if !game.commands(sel) {
                    return;
                }
                let zone_ok = state
                    .deployment_zones
                    .as_ref()
                    .map(|(a, d)| {
                        let z = if state.player_side == Side::Attacker {
                            a
                        } else {
                            d
                        };
                        z.contains(&h)
                    })
                    .unwrap_or(true);
                // Impassable holes and full-hex water are not
                // deployable, even if a stale zone list still contains them.
                let deployable = grid
                    .cell(h)
                    .map(|c| c.is_passable && c.terrain.is_deployable())
                    .unwrap_or(false);
                if zone_ok && deployable && state.unit_at(h).is_none() {
                    if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
                        u.position = h;
                        state.units_dirty = true;
                    }
                } else if zone_ok && !deployable {
                    game.log_line(loc.tr("log.deploy.impassable"));
                }
            }
        }
        BattlePhase::TacticalActive => {
            if !player_can_act(game, &state) {
                return;
            }
            // A pure left-RELEASE frame means "the click ended" —
            // the press frame already did the selecting (and deployment drags
            // are a Deployment-phase feature). Falling through to the
            // right-click section below would treat the release as a command:
            // releasing over the selected unit's own hex hits the stand-by
            // branch and CANCELS its standing move order, and releasing over
            // empty ground silently issues a move order.
            if left_released {
                return;
            }
            let player = state.player_side;
            // A fog-hidden enemy must not be discoverable by
            // clicking its hex — treat the click as "empty ground".
            let clicked = state
                .unit_at(h)
                .filter(|u| {
                    u.side == player || state.fog_state(u.position) == VisibilityState::Visible
                })
                .map(|u| (u.id, u.side));

            // Division-order target picking owns the buttons
            // while armed — left-click commits (Seize: a passable hex;
            // Engage: a visible enemy), right-click cancels.
            if let Some(pick) = state.div_pick.clone() {
                if right {
                    state.div_pick = None;
                    refresh_command_highlights(&mut state);
                } else if left {
                    match pick.kind {
                        DivPickKind::Seize => {
                            // Only impassable ground is refused —
                            // an enemy-held hex is a legitimate seizure goal.
                            let passable = grid.cell(h).map(|c| c.is_passable).unwrap_or(false);
                            if !passable {
                                game.log_line(
                                    loc.trf("log.div.reject_hex", &[("div", &pick.division)]),
                                );
                                fx.push(FxEvent::Floater {
                                    pos: hex_world(h),
                                    text: loc.tr("reject.impassable").into_owned(),
                                    kind: FloaterKind::Congested,
                                });
                                return;
                            }
                            state.div_pick = None;
                            refresh_command_highlights(&mut state);
                            issue_division_order(
                                game,
                                &mut state,
                                &loc,
                                &mut notice,
                                time.elapsed_secs(),
                                &pick.division,
                                DivOrderPick::Seize(h),
                            );
                        }
                        DivPickKind::Engage => {
                            if let Some((tid, uside)) = clicked {
                                if uside != player {
                                    state.div_pick = None;
                                    refresh_command_highlights(&mut state);
                                    issue_division_order(
                                        game,
                                        &mut state,
                                        &loc,
                                        &mut notice,
                                        time.elapsed_secs(),
                                        &pick.division,
                                        DivOrderPick::Engage { unit: tid, hex: h },
                                    );
                                }
                                // A friendly/empty click keeps picking.
                            }
                        }
                    }
                }
                return;
            }

            // Fire-mission picking owns the buttons while active:
            // left registers the barrage at the picked hex, right cancels.
            if state.command_mode == CommandMode::FirePicking {
                if right {
                    state.command_mode = CommandMode::Select;
                    refresh_command_highlights(&mut state);
                } else {
                    // Register the mission (one order per turn,
                    // replacing any old one); the unified fire phase does
                    // the rest. A vanished selection (unit eliminated while
                    // the mode was armed) exits the mode — otherwise
                    // FirePicking would stick forever with no escape except
                    // right-click.
                    let Some(sel) = state
                        .selected_unit
                        .and_then(|id| state.unit_by_id(id).cloned())
                    else {
                        state.command_mode = CommandMode::Select;
                        refresh_command_highlights(&mut state);
                        return;
                    };
                    let d = sel.position.distance(h);
                    let min_r = sel.min_attack_range();
                    let reason = if sel.shocked {
                        Some(loc.tr("reject.shocked").into_owned())
                    } else if sel.acted {
                        Some(loc.tr("reject.already_acted").into_owned())
                    } else if !sel.can_fire_support() {
                        Some(loc.tr("reject.emplace_first").into_owned())
                    } else if d < min_r {
                        Some(loc.tr("reject.too_close").into_owned())
                    } else if d > sel.attack_range {
                        Some(loc.tr("reject.out_of_range").into_owned())
                    } else {
                        // The F-key barrage is ALWAYS area fire
                        // (÷7 zone dispersion), regardless of vision.
                        register_fire_mission(game, &mut state, h, false, &loc);
                        None
                    };
                    if let Some(reason) = reason {
                        game.log_line(loc.trf(
                            "log.order.rejected",
                            &[("name", &sel.name), ("reason", &reason)],
                        ));
                        fx.push(FxEvent::Floater {
                            pos: hex_world(h),
                            text: reason,
                            kind: FloaterKind::Congested,
                        });
                    }
                    state.command_mode = CommandMode::Select;
                    refresh_command_highlights(&mut state);
                }
                return;
            }

            if left {
                // Non-sticky selection: friendly unit → select;
                // anything else (ground, visible enemy) → release.
                match clicked {
                    Some((uid, uside)) if uside == player => {
                        state.selected_unit = Some(uid);
                        state.units_dirty = true;
                    }
                    _ => {
                        if state.selected_unit.is_some() {
                            state.selected_unit = None;
                            state.units_dirty = true;
                            state.clear_highlights();
                        }
                    }
                }
                return;
            }

            // Right-click: context command for the selected unit.
            let Some(sel_id) = state.selected_unit else {
                return;
            };
            let Some(sel) = state.unit_by_id(sel_id).cloned() else {
                return;
            };
            if sel.side != player || !sel.is_combat_effective() {
                state.selected_unit = None;
                return;
            }
            // Allied battalions answer to their own staff — a
            // right-click with an allied selection is silently ignored (the
            // selection itself is kept, it is information-only).
            if !game.commands(&sel) {
                return;
            }

            // Right-click the selected unit itself → stand by: cancel the
            // standing order — move OR attack. Stance,
            // emplacement and the acted flag are untouched.
            if h == sel.position {
                let has_order = sel.move_order.is_some()
                    || state.attack_orders.iter().any(|o| o.attacker == sel_id);
                if has_order {
                    if let Some(u) = state.units.iter_mut().find(|u| u.id == sel_id) {
                        u.move_order = None;
                        // Standing by releases the unit back to
                        // its division order (the player's command is done).
                        u.manual_override = false;
                    }
                    state.attack_orders.retain(|o| o.attacker != sel_id);
                    state.orders_dirty = true;
                    game.log_line(loc.trf("log.order.stand_by", &[("name", &sel.name)]));
                }
                return;
            }

            match clicked {
                // Visible enemy → attack split.
                Some((tid, uside)) if uside != player => {
                    attack_target(game, &mut state, &sel, h, tid, &mut fx, &loc);
                }
                // Empty ground or a friendly hex → standing move order
                // (occupancy rules live in issue_move_order).
                _ => {
                    // unit_at skips non-effective units, but a Retreating
                    // enemy is still a valid target (§6.8 pursuit) — check
                    // for one before falling through to a move order.
                    if let Some(t) = state.attack_target_at(h, player) {
                        let tid = t.id;
                        attack_target(game, &mut state, &sel, h, tid, &mut fx, &loc);
                    } else {
                        issue_move_order(game, &mut state, h, &loc);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Right-click attack split: registers a
/// standing attack order — resolved in the end-of-turn unified fire phase,
/// never immediately. Failures pop an orange floater on the target hex.
fn attack_target(
    game: &mut GameController,
    state: &mut TacticalState,
    sel: &BattalionUnit,
    h: HexCoord,
    target_id: usize,
    fx: &mut FxQueue,
    loc: &Locale,
) {
    let d = sel.position.distance(h);
    let min_r = sel.min_attack_range();
    let in_envelope = d >= min_r && d <= sel.attack_range;
    let r = |key: &str| loc.tr(key).into_owned();
    let failed: Option<String> = if sel.acted {
        Some(r("reject.already_acted"))
    } else if sel.shocked {
        Some(r("reject.shocked"))
    } else if sel.is_indirect_artillery() {
        if !in_envelope {
            Some(r(if d < min_r {
                "reject.too_close"
            } else {
                "reject.out_of_range"
            }))
        } else if !sel.can_fire_support() {
            Some(r("reject.emplace_first"))
        } else {
            // Gesture rule: right-clicking a VISIBLE enemy unit =
            // precise fire (full damage on the hex — picking never reveals
            // hidden ones). Rocket launchers can never be precise — their
            // right-click resolves as an area salvo (§6.3).
            register_fire_mission(game, state, h, !sel.is_rocket(), loc);
            None
        }
    } else if sel.is_direct_gun() {
        // Direct-fire guns (AT/AA): support fire ONLY — a right-click
        // anywhere in the envelope registers a precise direct strike,
        // point-blank included; never an assault (the d==1 branch below),
        // never a zone mission. These pieces have no area-fire button.
        if !in_envelope {
            Some(r(if d < min_r {
                "reject.too_close"
            } else {
                "reject.out_of_range"
            }))
        } else if !sel.can_fire_support() {
            Some(r("reject.emplace_first"))
        } else {
            register_order(game, state, AttackTarget::DirectFire(target_id), loc);
            None
        }
    } else if d == 1 {
        if sel.can_assault() {
            register_order(game, state, AttackTarget::Assault(target_id), loc);
            None
        } else if sel.is_holding {
            Some(r("reject.holding"))
        } else if sel.is_emplaced {
            Some(r("reject.emplaced"))
        } else {
            Some(r("reject.cannot_assault"))
        }
    } else if sel.attack_range > 1 && in_envelope {
        // Armor / AT direct fire (§6.3).
        if sel.can_fire_support() {
            register_order(game, state, AttackTarget::DirectFire(target_id), loc);
            None
        } else {
            Some(r("reject.emplace_first"))
        }
    } else {
        Some(r("reject.out_of_range"))
    };
    if let Some(reason) = failed {
        game.log_line(loc.trf(
            "log.order.rejected",
            &[("name", &sel.name), ("reason", &reason)],
        ));
        fx.push(FxEvent::Floater {
            pos: hex_world(h),
            text: reason,
            kind: FloaterKind::Congested,
        });
    }
}

/// Compute highlights for the current command mode + selection.
/// Plain Select shows nothing (the standing route ribbon is drawn by the
/// orders system, the max-range ring keys off the selection).
/// FirePicking shows NO enemy markers either —
/// the F-key barrage is area fire (÷7 zone), so highlighting visible
/// enemies would mislead (they are precision-strike targets for the
/// right-click path instead, which needs no picking mode).
pub fn refresh_command_highlights(state: &mut TacticalState) {
    let mut out: Vec<(HexCoord, HighlightKind)> = Vec::new();
    let Some(grid) = state.grid.clone() else {
        return;
    };
    // Division-order target picking — Seize marks the hover hex
    // (Move-style), Engage marks every visible enemy (Support-style).
    if let Some(pick) = state.div_pick.as_ref() {
        match pick.kind {
            DivPickKind::Seize => {
                if let Some(h) = state.hover_hex {
                    if grid.in_bounds(h) {
                        out.push((h, HighlightKind::Move));
                    }
                }
            }
            DivPickKind::Engage => {
                for u in state.units.iter() {
                    if u.side != state.player_side
                        && u.is_targetable()
                        && state.fog_state(u.position) == VisibilityState::Visible
                    {
                        out.push((u.position, HighlightKind::Support));
                    }
                }
            }
        }
        state.set_highlights(out);
        return;
    }
    state.set_highlights(out);
}

/// §6.2: issue a standing movement order — the unit marches at its own
/// speed at the end of each of its turns, with a route arrow and an ETA in
/// the command panel. Replaces the old AP-limited instant move.
fn issue_move_order(
    game: &mut GameController,
    state: &mut TacticalState,
    h: HexCoord,
    loc: &Locale,
) -> bool {
    let Some(grid) = state.grid.clone() else {
        return false;
    };
    let Some(id) = state.selected_unit else {
        return false;
    };
    let Some(u) = state.unit_by_id(id).cloned() else {
        return false;
    };
    // March orders go only to units the player actually commands
    // (allied battalions are planned by their own staff AI).
    if !game.commands(&u) || !u.is_combat_effective() {
        return false;
    }
    if u.is_emplaced {
        game.log_line(loc.trf("log.order.emplaced_move", &[("name", &u.name)]));
        return false;
    }
    // Right-click must honour the same gate as the radial menu — a
    // unit that already acted (fired/assaulted) cannot take a march order.
    if u.acted {
        game.log_line(loc.trf("log.order.already_acted", &[("name", &u.name)]));
        return false;
    }
    // Target occupancy: a friendly-occupied target hex is
    // legal — the occupant may march away before we arrive; blocking is only
    // judged at execution. A fog-hidden enemy hex is legal too (it looks
    // empty; interception handles the encounter). Only a VISIBLE enemy hex
    // is rejected — that is an assault target, not a move target.
    if let Some(occ) = state.unit_at(h) {
        let visible_enemy =
            occ.side != state.player_side && state.fog_state(h) == VisibilityState::Visible;
        if visible_enemy {
            return false;
        }
    }
    let params = game.combat.params().clone();
    // The player's pathing & ETA see only fog-visible enemies —
    // hidden ones neither block the route nor project ZOC (no leak).
    let view: Vec<BattalionUnit> = state
        .units
        .iter()
        .filter(|x| {
            x.side == state.player_side || state.fog_state(x.position) == VisibilityState::Visible
        })
        .cloned()
        .collect();
    let Some((path, _)) = find_path(&grid, &u, &view, h, &params) else {
        return false;
    };
    if path.is_empty() {
        return false;
    }
    let eta = {
        let mut probe = u.clone();
        probe.move_order = Some(MoveOrder {
            path: path.clone(),
            hours: 0.0,
        });
        order_eta_hours(&grid, &probe, &view, &params)
            .map(|hrs| eta_turns(hrs, &params))
            .unwrap_or(0)
    };
    if let Some(mu) = state.units.iter_mut().find(|x| x.id == id) {
        mu.move_order = Some(MoveOrder { path, hours: 0.0 });
        // A personal player command outranks the division order
        // — the division AI leaves this unit alone until the march ends.
        mu.manual_override = true;
    }
    // §6.2 命令顶替: a fresh move order supersedes any attack
    // order the unit registered earlier this turn, just as an attack order
    // supersedes the march (register_order).
    if state.attack_orders.iter().any(|o| o.attacker == id) {
        state.attack_orders.retain(|o| o.attacker != id);
        game.log_line(loc.trf("log.order.attack_superseded", &[("name", &u.name)]));
    }
    let (q, r, n) = (h.q.to_string(), h.r.to_string(), eta.to_string());
    game.log_line(loc.trf(
        "log.move.eta",
        &[("name", &u.name), ("q", &q), ("r", &r), ("n", &n)],
    ));
    state.orders_dirty = true;
    state.arrows_grow = true; // grow the ribbon from the unit
    true
}

/// Turn a unit to face a hex (combat facing, §6.3): persists in
/// `unit_facing` so the next respawn keeps it. No-op for same-hex.
fn face_toward_hex(state: &mut TacticalState, unit_id: usize, hex: HexCoord) {
    let Some(u) = state.units.iter().find(|u| u.id == unit_id) else {
        return;
    };
    let (ax, az) = u.position.to_world(crate::board::HEX_SIZE);
    let (bx, bz) = hex.to_world(crate::board::HEX_SIZE);
    let (dx, dz) = (bx - ax, bz - az);
    if dx * dx + dz * dz < 1e-4 {
        return;
    }
    state.unit_facing.insert(unit_id, -dz.atan2(dx));
    state.units_dirty = true;
}

/// Display name of a unit (or "#id" when gone).
fn unit_name(state: &TacticalState, id: usize) -> String {
    state
        .unit_by_id(id)
        .map(|u| u.name.clone())
        .unwrap_or_else(|| format!("#{id}"))
}

/// Register (or replace) the selected unit's standing attack order
/// for this turn. One order per unit — re-registering swaps the target; the
/// order resolves in the end-of-turn unified fire phase, never immediately.
fn register_order(
    game: &mut GameController,
    state: &mut TacticalState,
    target: AttackTarget,
    loc: &Locale,
) {
    let Some(id) = state.selected_unit else {
        return;
    };
    let name = unit_name(state, id);
    state.attack_orders.retain(|o| o.attacker != id);
    state.attack_orders.push(AttackOrder {
        attacker: id,
        target,
    });
    // An attack order supersedes the standing move order (§6.2).
    if let Some(u) = state.units.iter_mut().find(|u| u.id == id) {
        u.move_order = None;
        // Manual attack orders protect the unit for exactly one
        // turn (they resolve in the fire phase; refresh_turn then releases
        // the unit back to the division AI unless a march/Hold/emplacement
        // keeps the override alive).
        u.manual_override = true;
    }
    state.orders_dirty = true;
    // Combat facing: the attacker aims at its target; an assault target
    // turns to face its attacker back.
    match target {
        AttackTarget::Assault(t) | AttackTarget::DirectFire(t) => {
            if let Some(hex) = state.unit_by_id(t).map(|u| u.position) {
                face_toward_hex(state, id, hex);
                if matches!(target, AttackTarget::Assault(_)) {
                    if let Some(back) = state.unit_by_id(id).map(|u| u.position) {
                        face_toward_hex(state, t, back);
                    }
                }
            }
        }
        AttackTarget::FireMission { hex, .. } => face_toward_hex(state, id, hex),
    }
    let (desc, icon) = match target {
        AttackTarget::Assault(t) => (
            loc.trf(
                "log.order.desc_assault",
                &[("target", &unit_name(state, t))],
            ),
            Some(IconId::Attack),
        ),
        AttackTarget::DirectFire(t) => (
            loc.trf(
                "log.order.desc_direct_fire",
                &[("target", &unit_name(state, t))],
            ),
            Some(IconId::Target),
        ),
        AttackTarget::FireMission { hex, .. } => {
            let (q, r) = (hex.q.to_string(), hex.r.to_string());
            (
                loc.trf("log.order.desc_fire_mission", &[("q", &q), ("r", &r)]),
                Some(IconId::Fire),
            )
        }
    };
    game.log_line_icon(
        icon,
        loc.trf("log.order.registered", &[("name", &name), ("desc", &desc)]),
    );
}

/// Register a fire mission on hex `h` (gesture rule): `precise` is decided
/// by HOW the order was issued —
/// right-clicking a VISIBLE enemy unit = precise (full damage on the
/// hex); the F-key barrage on a hex = always area fire (÷7 zone),
/// regardless of vision. Rockets are always area (callers pass false).
fn register_fire_mission(
    game: &mut GameController,
    state: &mut TacticalState,
    h: HexCoord,
    precise: bool,
    loc: &Locale,
) {
    register_order(
        game,
        state,
        AttackTarget::FireMission { hex: h, precise },
        loc,
    );
}

/// The `attack_kind.*` locale key for one resolved result's order
/// (shared by the log-line detail attach and the report lane).
fn attack_kind_key(orders: &[AttackOrder], attacker_id: usize, rocket: bool) -> &'static str {
    orders
        .iter()
        .find(|o| o.attacker == attacker_id)
        .map(|o| match &o.target {
            AttackTarget::Assault(_) => "attack_kind.assault",
            AttackTarget::DirectFire(_) => "attack_kind.direct_fire",
            AttackTarget::FireMission { precise, .. } => {
                if rocket {
                    "attack_kind.rocket_barrage"
                } else if *precise {
                    "attack_kind.fire_precise"
                } else {
                    "attack_kind.fire_area"
                }
            }
        })
        .unwrap_or("attack_kind.attack")
}

/// Bundle one result's formula chains with the display context
/// the engagement-detail window needs. None for fizzled orders (no chain
/// was computed → no「细节」button, no clickable log line).
fn detail_view(
    state: &TacticalState,
    r: &tactical_combat::CombatResult,
    kind: String,
    acting: Side,
) -> Option<Box<EngagementDetailView>> {
    let hit = r.breakdown?;
    Some(Box::new(EngagementDetailView {
        attacker: unit_name(state, r.attacker_id),
        defender: unit_name(state, r.defender_id),
        kind,
        hex: r.hex,
        friendly: state.unit_by_id(r.defender_id).map(|u| u.side) == Some(acting),
        yours: state.player_side == acting,
        hit,
        counter: r.counter_breakdown,
        shocked_defender: r.shocked_defender,
        shocked_attacker: r.shocked_attacker,
    }))
}

/// Unified fire phase: resolve the acting side's registered attack
/// orders plus counter-fire simultaneously, then narrate (log lines, damage
/// floaters, shock markers) and queue the SD2-style battle tour.
fn execute_fire_phase(
    game: &mut GameController,
    state: &mut TacticalState,
    side: Side,
    fx: &mut FxQueue,
    tour: &mut BattleTour,
    loc: &Locale,
) {
    if state.attack_orders.is_empty() {
        return;
    }
    crate::stage!(
        "execute_fire_phase: enter ({} orders)",
        state.attack_orders.len()
    );
    let orders = std::mem::take(&mut state.attack_orders);
    let Some(grid) = state.grid.clone() else {
        return;
    };
    // Pre-combat positions — the report ghost deferral renders
    // repelled / annihilated units at their pre-combat hex until the player
    // confirms the engagement report.
    let pre_pos: HashMap<usize, HexCoord> =
        state.units.iter().map(|u| (u.id, u.position)).collect();
    let results = game
        .combat
        .resolve_fire_phase(&grid, &mut state.units, &orders);
    crate::stage!("execute_fire_phase: resolved ({} results)", results.len());
    // Per-result combat animations, attached to the battle report
    // and played when the modal shows the engagement (not one burst here).
    let mut fx_by_result: Vec<Vec<FxEvent>> = Vec::with_capacity(results.len());
    // Per-result attack-kind label + formula-chain view, attached
    // to the log lines here and to the report lanes in the second loop.
    let mut kind_by_result: Vec<String> = Vec::with_capacity(results.len());
    let mut detail_by_result: Vec<Option<Box<EngagementDetailView>>> =
        Vec::with_capacity(results.len());
    for r in &results {
        let aname = unit_name(state, r.attacker_id);
        let dname = unit_name(state, r.defender_id);
        if r.target_lost {
            game.log_line(loc.trf("log.combat.target_lost", &[("name", &aname)]));
            fx.push(FxEvent::Floater {
                pos: hex_world(r.hex),
                text: loc.tr("floater.target_lost").into_owned(),
                kind: FloaterKind::Congested,
            });
            fx_by_result.push(Vec::new());
            kind_by_result.push(String::new());
            detail_by_result.push(None);
            continue;
        }
        // Damage accounting: the VICTIM's side absorbs `dealt` (with
        // rocket friendly fire that can be the acting side itself) and
        // the ATTACKER's side absorbs `taken`. Sync carries RATIOS of the
        // victim's own max org/str (§8.2) — with per-type baselines
        // (tanks 10/2, infantry 60/25) the conversion must happen per
        // victim, here at the source.
        let friendly_fire = state.unit_by_id(r.defender_id).map(|u| u.side) == Some(side);
        let (dm_org, dm_str, am_org, am_str) = {
            let d = state.unit_by_id(r.defender_id);
            let a = state.unit_by_id(r.attacker_id);
            (
                d.map(|u| u.max_org).unwrap_or(60.0).max(1.0),
                d.map(|u| u.max_strength).unwrap_or(25.0).max(1.0),
                a.map(|u| u.max_org).unwrap_or(60.0).max(1.0),
                a.map(|u| u.max_strength).unwrap_or(25.0).max(1.0),
            )
        };
        let victim_side = state
            .unit_by_id(r.defender_id)
            .map(|u| u.side)
            .unwrap_or(side.opponent());
        let attacker_side = state
            .unit_by_id(r.attacker_id)
            .map(|u| u.side)
            .unwrap_or(side);
        // §6.13: HQ units are synthesized — no HOI4 division maps
        // to them, so their own casualties are never injected (the
        // division-wide command collapse below IS injected).
        let victim_hq = state
            .unit_by_id(r.defender_id)
            .map(|u| u.is_hq())
            .unwrap_or(false);
        let attacker_hq = state
            .unit_by_id(r.attacker_id)
            .map(|u| u.is_hq())
            .unwrap_or(false);
        if !victim_hq {
            let (vtag, vprov) = state
                .unit_by_id(r.defender_id)
                .map(|u| (u.tag.clone(), u.hoi4_province))
                .unwrap_or_default();
            game.session.record_damage(
                victim_side,
                &vtag,
                vprov,
                r.org_damage_dealt,
                r.str_damage_dealt,
                dm_org,
                dm_str,
            );
        }
        if !attacker_hq {
            let (atag, aprov) = state
                .unit_by_id(r.attacker_id)
                .map(|u| (u.tag.clone(), u.hoi4_province))
                .unwrap_or_default();
            game.session.record_damage(
                attacker_side,
                &atag,
                aprov,
                r.org_damage_taken,
                r.str_damage_taken,
                am_org,
                am_str,
            );
        }
        // Suffix fragments are separate keys so they compose (§15).
        let mut suffixes = String::new();
        if friendly_fire {
            suffixes.push_str(&loc.tr("log.combat.friendly_fire"));
        }
        if r.defender_broken {
            suffixes.push_str(&loc.tr("log.combat.broken"));
        }
        if r.surrendered {
            suffixes.push_str(&loc.tr("log.combat.surrenders"));
        }
        if r.eliminated {
            suffixes.push_str(&loc.tr("log.combat.annihilated"));
        }
        let icon = if r.eliminated {
            Some(IconId::Explosion)
        } else if r.surrendered {
            Some(IconId::Surrender)
        } else {
            None
        };
        let (org_s, str_s) = (
            format!("{:.1}", r.org_damage_dealt),
            format!("{:.1}", r.str_damage_dealt),
        );
        // The attack-kind label + formula chain ride BOTH the log
        // lines (clickable) and the report lane built in the second loop.
        let rocket = state
            .unit_by_id(r.attacker_id)
            .map(|u| u.is_rocket())
            .unwrap_or(false);
        let kind = loc
            .tr(attack_kind_key(&orders, r.attacker_id, rocket))
            .into_owned();
        let detail = detail_view(state, r, kind.clone(), side);
        game.log_line_icon(
            icon,
            loc.trf(
                "log.combat.exchange",
                &[
                    ("attacker", &aname),
                    ("defender", &dname),
                    ("org", &org_s),
                    ("str", &str_s),
                    ("suffixes", &suffixes),
                ],
            ),
        );
        if let Some(last) = game.log.last_mut() {
            last.detail = detail.clone();
        }
        if r.org_damage_taken > 0.0 || r.str_damage_taken > 0.0 {
            let (org_t, str_t) = (
                format!("{:.1}", r.org_damage_taken),
                format!("{:.1}", r.str_damage_taken),
            );
            game.log_line(loc.trf(
                "log.combat.counter",
                &[("name", &aname), ("org", &org_t), ("str", &str_t)],
            ));
            if let Some(last) = game.log.last_mut() {
                last.detail = detail.clone();
            }
        }
        // Tracer + damage numbers on the exchange hex — COLLECTED for the
        // report modal, not pushed live.
        let mut evs = Vec::new();
        if r.org_damage_dealt > 0.0 || r.str_damage_dealt > 0.0 {
            let apos = state
                .unit_by_id(r.attacker_id)
                .map(|u| u.position)
                .unwrap_or(r.hex);
            let arty = state
                .unit_by_id(r.attacker_id)
                .map(|u| u.is_indirect_artillery())
                .unwrap_or(false);
            let mut q = FxQueue::default();
            push_combat_fx(
                &mut q,
                apos,
                r.hex,
                r.org_damage_dealt,
                r.str_damage_dealt,
                arty,
                loc,
            );
            evs.extend(q.0);
        }
        if r.shocked_defender {
            evs.push(FxEvent::Floater {
                pos: hex_world(r.hex),
                text: loc.tr("floater.shocked").into_owned(),
                kind: FloaterKind::Congested,
            });
        }
        if r.shocked_attacker {
            if let Some(p) = state.unit_by_id(r.attacker_id).map(|u| u.position) {
                evs.push(FxEvent::Floater {
                    pos: hex_world(p),
                    text: loc.tr("floater.shocked").into_owned(),
                    kind: FloaterKind::Congested,
                });
            }
        }
        fx_by_result.push(evs);
        kind_by_result.push(kind);
        detail_by_result.push(detail);
    }
    // §6.13: HQ annihilations — narrate the command collapse,
    // float the per-battalion org hit, and inject it (unlike the HQ's own
    // casualties above, the collapse maps to real HOI4 divisions).
    let hq_events = game.combat.take_hq_events();
    for ev in &hq_events {
        game.log_line_icon(
            Some(IconId::Skull),
            loc.trf("log.combat.hq_destroyed", &[("name", &ev.division)]),
        );
        for (uid, org_lost) in &ev.losses {
            let Some(u) = state.unit_by_id(*uid) else {
                continue;
            };
            let side = u.side;
            let pos = u.position;
            let prov = u.hoi4_province;
            let tag = u.tag.clone();
            let (mx_org, mx_str) = (u.max_org, u.max_strength);
            let n = format!("{org_lost:.0}");
            fx.push(FxEvent::Floater {
                pos: hex_world(pos),
                text: loc.trf("floater.command_lost", &[("n", &n)]),
                kind: FloaterKind::Congested,
            });
            game.session
                .record_damage(side, &tag, prov, *org_lost, 0.0, mx_org, mx_str);
        }
    }
    // Battle reports: group every lane landing on one defender
    // into a single click-through engagement report (modal, [Continue]).
    // Each report also carries its combat animation, played when
    // the modal focuses the engagement.
    let mut reports: Vec<BattleReport> = Vec::new();
    for (i, r) in results.iter().enumerate() {
        if r.target_lost {
            continue;
        }
        // Attack-kind label + formula chain were computed in the first loop
        // — the log lines and this lane share the same view.
        let kind = std::mem::take(&mut kind_by_result[i]);
        let lane = ReportLane {
            attacker: unit_name(state, r.attacker_id),
            kind,
            org: r.org_damage_dealt,
            str_: r.str_damage_dealt,
            counter_org: r.org_damage_taken,
            counter_str: r.str_damage_taken,
            shocked_defender: r.shocked_defender,
            // A victim on the acting side = friendly fire
            // (rocket salvos and tube area-fire barrages both splash
            // friends in the zone).
            friendly: state.unit_by_id(r.defender_id).map(|u| u.side) == Some(side),
            detail: std::mem::take(&mut detail_by_result[i]),
        };
        let outcome = if r.eliminated {
            loc.tr("outcome.annihilated").into_owned()
        } else if r.surrendered {
            loc.tr("outcome.surrenders").into_owned()
        } else if r.defender_broken {
            loc.tr("outcome.broken").into_owned()
        } else {
            String::new()
        };
        let dname = unit_name(state, r.defender_id);
        // Defer the visual consequence of this engagement — a
        // repelled (moved) or annihilated / surrendered defender, plus an
        // assault attacker that occupied the vacated hex, keep rendering at
        // their pre-combat hex until the player confirms THIS report; only
        // then does the unit slide away / disappear (no early animation).
        let mut pendings: Vec<PendingUnitVisual> = Vec::new();
        let def_gone = r.eliminated || r.surrendered;
        let def_moved = state
            .unit_by_id(r.defender_id)
            .map(|u| {
                pre_pos
                    .get(&r.defender_id)
                    .is_some_and(|p| *p != u.position)
            })
            .unwrap_or(false);
        if def_gone || def_moved {
            if let Some(from) = pre_pos.get(&r.defender_id) {
                pendings.push(PendingUnitVisual {
                    unit_id: r.defender_id,
                    from: *from,
                });
            }
        }
        if r.advanced {
            let atk_moved = state
                .unit_by_id(r.attacker_id)
                .map(|u| {
                    pre_pos
                        .get(&r.attacker_id)
                        .is_some_and(|p| *p != u.position)
                })
                .unwrap_or(false);
            if atk_moved {
                if let Some(from) = pre_pos.get(&r.attacker_id) {
                    pendings.push(PendingUnitVisual {
                        unit_id: r.attacker_id,
                        from: *from,
                    });
                }
            }
        }
        match reports
            .iter_mut()
            .find(|rep| rep.hex == r.hex && rep.defender == dname)
        {
            Some(rep) => {
                rep.lanes.push(lane);
                rep.fx.append(&mut fx_by_result[i]);
                for p in pendings {
                    if !rep.pending.iter().any(|q| q.unit_id == p.unit_id) {
                        rep.pending.push(p);
                    }
                }
                if !outcome.is_empty() {
                    rep.outcome = outcome;
                }
            }
            None => reports.push(BattleReport {
                hex: r.hex,
                defender: dname,
                acting: side,
                lanes: vec![lane],
                outcome,
                fx: std::mem::take(&mut fx_by_result[i]),
                pending: pendings,
            }),
        }
    }
    // Arm the ghosts: the affected units keep their pre-combat look through
    // the respawn below, until tick_battle_tour releases them on confirm.
    for rep in &reports {
        for p in &rep.pending {
            state.report_ghosts.insert(p.unit_id, p.from);
        }
    }
    if !reports.is_empty() {
        // Normally the queue is read through before the next fire phase
        // (the AI turn is gated on it); append defensively if not.
        if tour.active {
            tour.queue.extend(reports);
        } else {
            tour.queue = reports;
            tour.active = true;
            tour.index = 0;
            tour.focused = None;
        }
    }
    state.units_dirty = true;
    // The fire phase just emptied attack_orders (mem::take) —
    // rebuild the attack arrows so the resolved side's pending attack lines
    // vanish right after resolution instead of lingering into the next
    // side's turn (with division-order automation the player's lines
    // would stay visible while the enemy moves).
    state.orders_dirty = true;
    update_fog(state, game);
    check_victory_and_transition(game, state, loc);
    crate::stage!("execute_fire_phase: exit");
}

// ---------------------------------------------------------------------------
// Turn sequencing
// ---------------------------------------------------------------------------

/// §6.2: advance one side's standing move orders by one turn's budget and
/// log the interesting events (contact / arrival / blockage), with FX popups
/// (red = intercepted/contact, orange = friendly congestion) and a
/// detour retry for congested units.
fn execute_movement(
    game: &mut GameController,
    state: &mut TacticalState,
    side: Side,
    fx: &mut FxQueue,
    anims: &mut MoveAnims,
    loc: &Locale,
) {
    let Some(grid) = state.grid.clone() else {
        return;
    };
    let params = game.combat.params().clone();

    // Refresh every standing route against the current
    // situation BEFORE marching (detours heal once blockers leave; guards
    // against oscillation and lost progress live in refresh_move_order).
    // Each side paths against its own fog view (player fog / AI fog).
    let is_player = side == state.player_side;
    let view: Vec<BattalionUnit> = state
        .units
        .iter()
        .filter(|x| {
            x.side == side
                || (if is_player {
                    state.fog_state(x.position)
                } else {
                    state.ai_fog_state(x.position)
                }) == VisibilityState::Visible
        })
        .cloned()
        .collect();
    let mut refreshed = false;
    for u in state
        .units
        .iter_mut()
        .filter(|u| u.side == side && u.is_combat_effective() && u.move_order.is_some())
    {
        refreshed |= refresh_move_order(&grid, u, &view, &params);
    }
    if refreshed {
        state.orders_dirty = true;
    }

    let events = advance_move_orders(&grid, &mut state.units, side, &params);
    let mut moved = false;
    let mut had_events = false;
    for ev in &events {
        let name = |id: usize| {
            state
                .unit_by_id(id)
                .map(|u| u.name.clone())
                .unwrap_or_else(|| format!("#{id}"))
        };
        let pos_of = |id: usize| state.unit_by_id(id).map(|u| u.position);
        match *ev {
            MovementEvent::Advanced { unit_id, from, to } => {
                moved = true;
                // Queue slide waypoints for the model. Enemies
                // only animate if their origin hex is currently visible —
                // otherwise the slide would leak where they came from.
                let animatable =
                    side == state.player_side || state.fog_state(from) == VisibilityState::Visible;
                if animatable {
                    let wp = |h: HexCoord| {
                        let (x, z) = h.to_world(crate::board::HEX_SIZE);
                        Vec3::new(x, unit_y_on_grid(state, h), z)
                    };
                    anims
                        .0
                        .entry(unit_id)
                        .or_insert_with(|| vec![wp(from)])
                        .push(wp(to));
                }
            }
            MovementEvent::MadeContact { unit_id } => {
                game.log_line_icon(
                    Some(IconId::Warning),
                    loc.trf("log.move.contact", &[("name", &name(unit_id))]),
                );
                if let Some(p) = pos_of(unit_id) {
                    fx.push(FxEvent::Floater {
                        pos: hex_world(p),
                        text: loc.tr("floater.contact").into_owned(),
                        kind: FloaterKind::Intercepted,
                    });
                }
                had_events = true;
            }
            MovementEvent::Intercepted { unit_id, enemy_id } => {
                game.log_line_icon(
                    Some(IconId::Warning),
                    loc.trf(
                        "log.move.intercepted",
                        &[("name", &name(unit_id)), ("enemy", &name(enemy_id))],
                    ),
                );
                for id in [unit_id, enemy_id] {
                    if let Some(p) = pos_of(id) {
                        fx.push(FxEvent::Floater {
                            pos: hex_world(p),
                            text: loc.tr("floater.intercepted").into_owned(),
                            kind: FloaterKind::Intercepted,
                        });
                    }
                }
                had_events = true;
            }
            MovementEvent::Arrived { unit_id } => {
                // An enemy arrival is enemy-internal info — the
                // player's battle log stays silent about it.
                if side == state.player_side {
                    game.log_line_icon(
                        Some(IconId::Flag),
                        loc.trf("log.move.arrived", &[("name", &name(unit_id))]),
                    );
                }
                had_events = true;
            }
            MovementEvent::Blocked {
                unit_id,
                blocker_id,
            } => {
                // Following a MOVING friend is a normal convoy —
                // quiet. Only a PARKED blocker is real congestion (popup +
                // log); the parked-escape refresh usually heals it next turn.
                // Congestion is side-internal — log + popup only
                // for the player's own columns, never the enemy's.
                if side == state.player_side
                    && state
                        .unit_by_id(blocker_id)
                        .is_some_and(|u| u.move_order.is_none())
                {
                    game.log_line(loc.trf("log.move.blocked", &[("name", &name(unit_id))]));
                    if let Some(p) = pos_of(unit_id) {
                        fx.push(FxEvent::Floater {
                            pos: hex_world(p),
                            text: loc.tr("floater.congested").into_owned(),
                            kind: FloaterKind::Congested,
                        });
                    }
                }
                had_events = true;
            }
            MovementEvent::Progress { .. } => {}
        }
    }
    if moved || had_events {
        state.units_dirty = true;
        state.orders_dirty = true;
        update_fog(state, game);
    }
}

fn end_player_turn(
    game: &mut GameController,
    state: &mut TacticalState,
    fx: &mut FxQueue,
    anims: &mut MoveAnims,
    tour: &mut BattleTour,
    loc: &Locale,
    notice: &mut NoticeBar,
    now: f32,
) {
    let side = state.player_side;
    crate::stage!("end_player_turn: enter (turn {})", game.session.turn_number);
    // §6.2: the player's standing orders march out at the end of her turn.
    execute_movement(game, state, side, fx, anims, loc);
    crate::stage!("end_player_turn: movement done");
    // Then her registered attack orders resolve together in the
    // unified fire phase (with counter-fire).
    execute_fire_phase(game, state, side, fx, tour, loc);
    crate::stage!("end_player_turn: fire phase done");
    // Shocks set during THIS turn-end's fire
    // phase (incl. counter-fire on the acting side) persist until the end
    // of the next turn-end; everything older wears off now.
    game.combat.expire_shocks(&mut state.units);
    let turn_before = game.session.turn_number;
    if game.session.end_side_turn(side).is_err() {
        return;
    }
    // §6.12 attacker→defender: when the player defends she ends SECOND —
    // both sides have now acted, so the full-turn closeout runs here.
    // (When she attacks, the closeout runs after the AI's turn instead.)
    if game.session.turn_number != turn_before {
        finish_full_turn(game, state, loc, notice, now);
    }
    crate::stage!("end_player_turn: exit (turn {})", game.session.turn_number);
    game.log_line(loc.trf(
        "log.turn.end",
        &[("n", &game.session.turn_number.to_string())],
    ));
    state.command_mode = CommandMode::Select;
    state.clear_highlights();
    state.selected_unit = None;
    state.units_dirty = true;
    // AI turn is queued in tick_battle when current_side flips.
}

fn run_ai_turn(
    mut game: Option<ResMut<GameController>>,
    mut state: ResMut<TacticalState>,
    mut ai_turn: ResMut<AiTurn>,
    mut fx: ResMut<FxQueue>,
    mut anims: ResMut<MoveAnims>,
    mut tour: ResMut<BattleTour>,
    mut notice: ResMut<NoticeBar>,
    time: Res<Time>,
    loc: Res<LocaleRes>,
) {
    let Some(game) = game.as_deref_mut() else {
        return;
    };
    if game.session.phase != BattlePhase::TacticalActive {
        ai_turn.active = false;
        ai_turn.actions.clear();
        return;
    }
    let enemy_side = state.player_side.opponent();
    if game.session.current_side != enemy_side {
        return;
    }
    // The battle-report modal pauses the world — the AI turn
    // waits until the player has clicked through every engagement.
    if tour.active {
        return;
    }

    // Queue the AI plan once.
    if !ai_turn.active {
        let Some(grid) = state.grid.clone() else {
            return;
        };
        let own: Vec<BattalionUnit> = state
            .units
            .iter()
            .filter(|u| u.side == enemy_side)
            .cloned()
            .collect();
        // The AI plans against its own fog view — hidden enemies
        // do not exist for planning (execution stays omniscient).
        let foe: Vec<BattalionUnit> = state
            .units
            .iter()
            .filter(|u| u.side != enemy_side)
            .filter(|u| state.ai_fog_state(u.position) == VisibilityState::Visible)
            .cloned()
            .collect();
        // Pre-battle intel: an attacking AI marches on the player's muster
        // zone; a defending AI holds its ground. The whole zone
        // is the intel — each unit aims at its NEAREST zone hex, so a big
        // front advances along its full width instead of collapsing on one
        // centroid point.
        let intel: Option<Vec<HexCoord>> = if enemy_side == Side::Attacker {
            state.deployment_zones.as_ref().map(|(a, d)| {
                (if state.player_side == Side::Attacker {
                    a
                } else {
                    d
                })
                .clone()
            })
        } else {
            None
        };
        ai_turn.actions = game
            .ai
            // No passive friendlies for the enemy's whole-side AI.
            // The physical foe list feeds the fog-wall
            // blind-assault probe (a dark-fog wall that halts the march is
            // stormed when beaten/overwhelmed).
            .plan_turn_full(
                &grid,
                &own,
                &foe,
                intel.as_deref(),
                game.session.flags(),
                None,
                Some(&state.units),
            )
            .into();
        // Make the AI inspectable — one plan summary per turn.
        let count = |f: fn(&tactical_ai::AiAction) -> bool| {
            ai_turn.actions.iter().filter(|a| f(a)).count().to_string()
        };
        game.log_line(loc.trf(
            "log.ai.plan",
            &[
                (
                    "move",
                    &count(|a| matches!(a, tactical_ai::AiAction::MoveUnit { .. })),
                ),
                (
                    "assault",
                    &count(|a| matches!(a, tactical_ai::AiAction::Assault { .. })),
                ),
                (
                    "fire",
                    &count(|a| matches!(a, tactical_ai::AiAction::FireSupport { .. })),
                ),
                (
                    "emplim",
                    &count(|a| {
                        matches!(
                            a,
                            tactical_ai::AiAction::Emplace { .. }
                                | tactical_ai::AiAction::Limber { .. }
                        )
                    }),
                ),
                (
                    "hold",
                    &count(|a| matches!(a, tactical_ai::AiAction::Hold { .. })),
                ),
            ],
        ));
        ai_turn.active = true;
        // No pacing budget: the enemy's registered pre-orders are
        // invisible to the player (no ribbons, no attack lanes — only
        // log lines), so spending one tick per command bought nothing
        // but wait time. The drain runs on the very next frame; the
        // visible parts of the enemy turn (handover banner, marching
        // slides, click-through battle reports) pace themselves.
        ai_turn.timer = 0.0;
        crate::stage!("run_ai_turn: plan done ({} actions)", ai_turn.actions.len());
        return;
    }

    ai_turn.timer -= time.delta_secs();
    if ai_turn.timer > 0.0 {
        return;
    }

    if state.grid.is_none() {
        return;
    }

    loop {
        let Some(action) = ai_turn.actions.pop_front() else {
            // AI finished: its orders march out, then close its turn (§6.2).
            crate::stage!("run_ai_turn: actions drained — movement");
            execute_movement(game, &mut state, enemy_side, &mut fx, &mut anims, &loc);
            crate::stage!("run_ai_turn: movement done — fire phase");
            // Then its registered attack orders resolve together.
            execute_fire_phase(game, &mut state, enemy_side, &mut fx, &mut tour, &loc);
            crate::stage!("run_ai_turn: fire phase done — closeout");
            // Shock expiry — same rule as the player turn end:
            // this phase's fresh shocks persist, older ones wear off.
            game.combat.expire_shocks(&mut state.units);
            let turn_before = game.session.turn_number;
            let _ = game.session.end_side_turn(enemy_side);
            ai_turn.active = false;
            // Close out the full turn only when both sides have acted (the
            // turn counter just advanced). With a defending player the AI
            // moves first — firing this mid-turn would attrition/refresh
            // before her half.
            if game.session.turn_number != turn_before {
                finish_full_turn(game, &mut state, &loc, &mut notice, time.elapsed_secs());
            }
            crate::stage!("run_ai_turn: exit (turn {})", game.session.turn_number);
            return;
        };
        if matches!(action, AiAction::EndTurn) {
            continue; // popped — the empty queue above closes the turn
        }
        // Drain the whole queue in this frame — see the pacing note above.
        apply_ai_action(&action, enemy_side, game, &mut state, &loc, true);
    }
}

/// Apply one proposed AI action for `acting_side` (the side that
/// owns the units). Shared by the enemy AI turn ([`run_ai_turn`]) and the
/// player's division automation ([`plan_division_orders`]) — both register
/// standing orders through this single path, so the order-persistence rule
/// (re-affirming a destination keeps the invested travel hours)
/// and the §6.2 gates stay identical.
///
/// Returns true when the action registered a command (move / assault /
/// fire mission) — once used to pace the enemy turn one command per
/// tick; the enemy turn now drains in a single frame, so the flag is
/// informational (tests read it). Emplace / limber / Hold / Retreat
/// never set it.
///
/// `log = false` for the player's division orders: per-unit lines would
/// flood the battle log — the per-division plan summary and the map arrows
/// carry the information instead.
fn apply_ai_action(
    action: &AiAction,
    acting_side: Side,
    game: &mut GameController,
    state: &mut TacticalState,
    loc: &Locale,
    log: bool,
) -> bool {
    match action {
        AiAction::MoveUnit { unit_id, path } => {
            if path.is_empty() {
                return false;
            }
            // §6.2: a standing order — the unit marches it at its speed.
            if let Some(mu) = state.units.iter_mut().find(|x| x.id == *unit_id) {
                if !mu.is_emplaced {
                    let dest = *path.last().unwrap();
                    // Re-affirming the same destination keeps
                    // the standing order and its invested hours — a fresh
                    // order every turn would reset progress and pin any
                    // unit slower than 1 hex/turn (towed guns, armor in
                    // forest, river crossings).
                    match &mut mu.move_order {
                        Some(o) if o.path.last() == Some(&dest) => {
                            // A same-destination re-affirm keeps the
                            // WHOLE standing order (path + invested hours).
                            // Adopting the planner's freshly recomputed path
                            // here would reset hours whenever the first step
                            // flapped, pinning any unit slower than
                            // 1 hex/turn. refresh_move_order already heals
                            // the standing path each side-turn under
                            // anti-oscillation rules.
                        }
                        _ => {
                            if log {
                                let name = mu.name.clone();
                                let (q, r) = (dest.q.to_string(), dest.r.to_string());
                                game.log_line(
                                    loc.trf(
                                        "log.ai.move",
                                        &[("name", &name), ("q", &q), ("r", &r)],
                                    ),
                                );
                            }
                            mu.move_order = Some(MoveOrder {
                                path: path.clone(),
                                hours: 0.0,
                            });
                        }
                    }
                }
            }
            state.orders_dirty = true;
            true
        }
        AiAction::Emplace { unit_id } => {
            if let Some(u) = state.units.iter_mut().find(|u| u.id == *unit_id) {
                if !u.acted && u.requires_emplacement() && !u.is_emplaced {
                    u.is_emplaced = true;
                    u.acted = true;
                    u.move_order = None;
                    if log {
                        let name = u.name.clone();
                        game.log_line(loc.trf("log.ai.emplace", &[("name", &name)]));
                    }
                    state.units_dirty = true;
                }
            }
            false
        }
        AiAction::Limber { unit_id } => {
            if let Some(u) = state.units.iter_mut().find(|u| u.id == *unit_id) {
                if !u.acted && u.is_emplaced {
                    u.is_emplaced = false;
                    u.acted = true;
                    if log {
                        let name = u.name.clone();
                        game.log_line(loc.trf("log.ai.limber", &[("name", &name)]));
                    }
                    state.units_dirty = true;
                }
            }
            false
        }
        AiAction::Assault {
            attacker_id,
            target_id,
        } => {
            // Shocked units cannot take attack orders — the
            // planner picks a different action instead of wasting this one.
            if state
                .unit_by_id(*attacker_id)
                .map(|u| u.shocked)
                .unwrap_or(true)
            {
                if log {
                    let an = state
                        .unit_by_id(*attacker_id)
                        .map(|u| u.name.clone())
                        .unwrap_or_default();
                    game.log_line(loc.trf("log.ai.shocked_hold", &[("name", &an)]));
                }
                return false;
            }
            let an = state
                .unit_by_id(*attacker_id)
                .map(|u| u.name.clone())
                .unwrap_or_default();
            let tn = state
                .unit_by_id(*target_id)
                .map(|u| u.name.clone())
                .unwrap_or_default();
            // Register a standing attack order — resolved
            // together in the fire phase at the end of the acting turn.
            state.attack_orders.retain(|o| o.attacker != *attacker_id);
            state.attack_orders.push(AttackOrder {
                attacker: *attacker_id,
                target: AttackTarget::Assault(*target_id),
            });
            // Combat facing: attacker aims at the target, the target turns
            // to face its attacker back.
            if let Some(hex) = state.unit_by_id(*target_id).map(|u| u.position) {
                face_toward_hex(state, *attacker_id, hex);
            }
            if let Some(back) = state.unit_by_id(*attacker_id).map(|u| u.position) {
                face_toward_hex(state, *target_id, back);
            }
            if let Some(u) = state.units.iter_mut().find(|u| u.id == *attacker_id) {
                u.move_order = None;
                // Attacking breaks the hunker — the Hold stance clears.
                if u.is_holding {
                    u.is_holding = false;
                    state.units_dirty = true;
                }
            }
            state.orders_dirty = true;
            if log {
                game.log_line_icon(
                    Some(IconId::Attack),
                    loc.trf("log.ai.assault", &[("name", &an), ("target", &tn)]),
                );
            }
            true
        }
        AiAction::FireSupport {
            attacker_id,
            target_hex,
        } => {
            // Shocked units cannot take attack orders.
            if state
                .unit_by_id(*attacker_id)
                .map(|u| u.shocked)
                .unwrap_or(true)
            {
                if log {
                    let an = state
                        .unit_by_id(*attacker_id)
                        .map(|u| u.name.clone())
                        .unwrap_or_default();
                    game.log_line(loc.trf("log.ai.shocked_hold", &[("name", &an)]));
                }
                return false;
            }
            // The gesture rule mirrored for the AI: a mission is
            // PRECISE only when a combat-effective enemy stands at the aim
            // hex AND the acting side currently sees that hex (its own fog
            // view) — i.e. the AI equivalent of right-clicking a visible
            // enemy. Everything else (intel goals, empty ground) is area
            // fire (÷7 zone); rockets are always area.
            // Direct-fire guns (AT/AA) are the exception — by design their
            // missions are ALWAYS precision: no zone saturation, no
            // self-splash (a point-blank area mission used to hit the
            // gun's own hex). With no combat-effective enemy exactly on
            // the aim hex there is no mission at all.
            let enemy_at_hex = state.units.iter().any(|e| {
                e.side != acting_side && e.is_combat_effective() && e.position == *target_hex
            });
            let direct_gun = state
                .unit_by_id(*attacker_id)
                .map(|u| u.is_direct_gun())
                .unwrap_or(false);
            if direct_gun && !enemy_at_hex {
                return false;
            }
            let hex_visible = if acting_side == state.player_side {
                state.fog_state(*target_hex)
            } else {
                state.ai_fog_state(*target_hex)
            } == VisibilityState::Visible;
            let is_rocket = state
                .unit_by_id(*attacker_id)
                .map(|u| u.is_rocket())
                .unwrap_or(false);
            let precise = if direct_gun {
                true
            } else {
                !is_rocket && hex_visible && enemy_at_hex
            };
            let an = state
                .unit_by_id(*attacker_id)
                .map(|u| u.name.clone())
                .unwrap_or_default();
            // Register — resolved in the fire phase.
            state.attack_orders.retain(|o| o.attacker != *attacker_id);
            state.attack_orders.push(AttackOrder {
                attacker: *attacker_id,
                target: AttackTarget::FireMission {
                    hex: *target_hex,
                    precise,
                },
            });
            // Combat facing: the battery aims downrange.
            face_toward_hex(state, *attacker_id, *target_hex);
            if let Some(u) = state.units.iter_mut().find(|u| u.id == *attacker_id) {
                u.move_order = None;
            }
            state.orders_dirty = true;
            if log {
                let (q, r) = (target_hex.q.to_string(), target_hex.r.to_string());
                game.log_line_icon(
                    Some(IconId::Fire),
                    loc.trf(
                        "log.ai.fire_mission",
                        &[("name", &an), ("q", &q), ("r", &r)],
                    ),
                );
            }
            true
        }
        AiAction::Hold { unit_id } => {
            // "No action this turn" doubles as the Hold stance for AI
            // units: an idle unit hunkers (+hold_defense_bonus). The old
            // player-only rationale is gone — can_assault no longer reads
            // the stance and movement drops it on the first step, so the
            // AI cannot trap itself. Assault/retreat clear it below.
            if let Some(u) = state.units.iter_mut().find(|u| u.id == *unit_id) {
                if u.is_combat_effective() && !u.is_holding {
                    u.is_holding = true;
                    state.units_dirty = true;
                }
            }
            false
        }
        AiAction::Retreat { unit_id } => {
            if let Some(u) = state.units.iter_mut().find(|u| u.id == *unit_id) {
                u.state = UnitState::Retreating;
                if u.is_holding {
                    u.is_holding = false;
                    state.units_dirty = true;
                }
            }
            false
        }
        AiAction::EndTurn => false,
    }
}

/// Issue (or replace) a division order — validated, logged, and
/// planned immediately so the division moves this very turn. Shared by the
/// HQ radial bar (via `PlayerCommand::DivOrder`) and the map target pick.
fn issue_division_order(
    game: &mut GameController,
    state: &mut TacticalState,
    loc: &Locale,
    notice: &mut NoticeBar,
    now: f32,
    division: &str,
    pick: DivOrderPick,
) -> bool {
    if game.session.phase != BattlePhase::TacticalActive
        || game.session.current_side != state.player_side
    {
        return false;
    }
    // The division must exist on the player's side.
    let div_ok = state
        .units
        .iter()
        .any(|u| u.side == state.player_side && u.division == division);
    if !div_ok {
        return false;
    }
    // An Engage target must be a currently VISIBLE enemy (the player picks
    // what she sees; fog hides the rest).
    if let DivOrderPick::Engage { unit, .. } = pick {
        let visible = state.units.iter().any(|u| {
            u.id == unit
                && u.side != state.player_side
                && state.fog_state(u.position) == VisibilityState::Visible
        });
        if !visible {
            return false;
        }
    }
    let last_pos = match pick {
        DivOrderPick::Engage { hex, .. } => Some(hex),
        _ => None,
    };
    game.div_orders.insert(
        division.to_string(),
        DivisionOrder {
            pick,
            seized: false,
            engage_last_pos: last_pos,
        },
    );
    let kind = game
        .div_orders
        .get(division)
        .map(|o| loc.tr(o.kind_key()).into_owned())
        .unwrap_or_default();
    let msg = loc.trf("log.div.ordered", &[("div", division), ("kind", &kind)]);
    game.log_line_icon(
        Some(match pick {
            DivOrderPick::Advance => IconId::Advance,
            DivOrderPick::Seize(_) => IconId::Flag,
            DivOrderPick::Engage { .. } => IconId::Target,
        }),
        msg.clone(),
    );
    notice.flash = Some(FlashNotice::plain(msg, now + 5.0));
    plan_division_orders(game, state, loc, notice, now);
    true
}

/// Plan and register every standing division order for the
/// player's side (DESIGN §7.4). Runs once at the start of each player turn
/// (gated by [`GameController::div_planned_turn`]) and immediately after an
/// order is issued / replaced.
///
/// Per ordered division:
/// - completion checks — no combat-effective units left (无兵力), a Seize
///   goal reached (→ hold-back phase), a Seize goal lost (→ back to the
///   maneuver phase), an Engage target gone (→ order complete);
/// - one `TacticalAi` plan for the division's own slice against the
///   player's fog view, registered via [`apply_ai_action`] (the player's
///   manual commands supersede any of these by the 命令顶替 rule).
///
/// Returns true when any command was registered (dirty-flag signal).
#[allow(clippy::too_many_arguments)]
fn plan_division_orders(
    game: &mut GameController,
    state: &mut TacticalState,
    loc: &Locale,
    notice: &mut NoticeBar,
    now: f32,
) -> bool {
    if game.div_orders.is_empty() {
        game.div_planned_turn = game.session.turn_number;
        return false;
    }
    let Some(grid) = state.grid.clone() else {
        return false;
    };
    let player = state.player_side;
    // The division AI plans against the PLAYER's fog view — the
    // same information limit the player has (hidden enemies neither block
    // routes nor draw fire; execution stays omniscient).
    let foe: Vec<BattalionUnit> = state
        .units
        .iter()
        .filter(|u| u.side != player)
        .filter(|u| state.fog_state(u.position) == VisibilityState::Visible)
        .cloned()
        .collect();
    // Pre-battle intel: the opponent's deployment zone — the blind-march
    // goal for a commanded advance with nothing visible.
    let intel: Option<Vec<HexCoord>> = state
        .deployment_zones
        .as_ref()
        .map(|(a, d)| (if player == Side::Attacker { d } else { a }).clone());
    let flags = game.session.flags().cloned();

    let mut planned = false;
    let orders = std::mem::take(&mut game.div_orders);
    for (division, mut order) in orders {
        let div_units: Vec<usize> = state
            .units
            .iter()
            .filter(|u| u.side == player && u.division == division)
            .map(|u| u.id)
            .collect();
        let effective = div_units
            .iter()
            .filter(|id| {
                state
                    .unit_by_id(**id)
                    .map(|u| u.is_combat_effective())
                    .unwrap_or(false)
            })
            .count();
        let div_owned = |h: HexCoord| {
            state.units.iter().any(|u| {
                u.side == player
                    && u.division == division
                    && u.position == h
                    && u.is_combat_effective()
            })
        };
        let enemy_on = |h: HexCoord| {
            state
                .units
                .iter()
                .any(|u| u.side != player && u.is_combat_effective() && u.position == h)
        };

        // ── Completion / phase transitions (DESIGN §7.4).
        let mut done: Option<&'static str> = None;
        if effective == 0 {
            done = Some("log.div.no_units");
        } else {
            match order.pick {
                DivOrderPick::Advance => {}
                DivOrderPick::Seize(hex) => {
                    if order.seized {
                        // 目标被夺回: an enemy on the hex while no division
                        // unit holds it — flip back to the maneuver phase.
                        if enemy_on(hex) && !div_owned(hex) {
                            order.seized = false;
                            let msg = loc.trf("log.div.lost", &[("div", &division)]);
                            game.log_line_icon(Some(IconId::Warning), msg.clone());
                            notice.flash = Some(FlashNotice::plain(msg.clone(), now + 5.0));
                        }
                    } else if div_owned(hex) {
                        // 已占领 → hold-back phase (report and
                        // keep defending; the order ends only on cancel).
                        order.seized = true;
                        let msg = loc.trf("log.div.seized", &[("div", &division)]);
                        game.log_line_icon(Some(IconId::Flag), msg.clone());
                        notice.flash = Some(FlashNotice::plain(msg.clone(), now + 6.0));
                    }
                }
                DivOrderPick::Engage { unit, .. } => {
                    let gone = state
                        .unit_by_id(unit)
                        .is_none_or(|t| t.side == player || !t.is_combat_effective());
                    if gone {
                        done = Some("log.div.engage_done");
                    } else if let Some(t) = state.unit_by_id(unit) {
                        // Fog-honest pursuit: update the last known position
                        // only while the target is visible.
                        if state.fog_state(t.position) == VisibilityState::Visible {
                            order.engage_last_pos = Some(t.position);
                        }
                    }
                }
            }
        }
        if let Some(key) = done {
            let msg = loc.trf(key, &[("div", &division)]);
            game.log_line_icon(Some(IconId::Surrender), msg.clone());
            notice.flash = Some(FlashNotice::plain(msg.clone(), now + 5.0));
            continue; // order finished — dropped
        }

        // ── Plan the division (one TacticalAi per division per turn; the
        // seed mixes in division + turn so replays stay deterministic and
        // tie-breaks vary across turns).
        let mut h = 0xcbf29ce484222325u64;
        for b in division.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        let seed = game.seed.wrapping_mul(0x9E3779B97F4A7C15)
            ^ h
            ^ (game.session.turn_number as u64).wrapping_mul(0x517CC1B727220A95);
        let own: Vec<BattalionUnit> = state
            .units
            .iter()
            .filter(|u| u.side == player && u.division == division)
            .cloned()
            .collect();
        let mut ai = TacticalAi::new(player, order.phase_tactic(), seed);
        crate::stage!("plan_division_orders: planning '{division}'");
        let actions = ai.plan_turn_div_order(
            &grid,
            &own,
            &foe,
            intel.as_deref(),
            flags.as_ref(),
            order.planner_target(),
        );
        crate::stage!(
            "plan_division_orders: '{division}' -> {} actions",
            actions.len()
        );

        // ── Register the proposals as standing orders (silent — the map
        // arrows show them; one summary line per division covers the log).
        let count = |f: fn(&AiAction) -> bool| actions.iter().filter(|a| f(a)).count();
        let moves = count(|a| matches!(a, AiAction::MoveUnit { .. }));
        let assaults = count(|a| matches!(a, AiAction::Assault { .. }));
        let fires = count(|a| matches!(a, AiAction::FireSupport { .. }));
        let emplims = count(|a| matches!(a, AiAction::Emplace { .. } | AiAction::Limber { .. }));
        for a in &actions {
            apply_ai_action(a, player, game, state, loc, false);
        }
        game.log_line(loc.trf(
            "log.div.plan",
            &[
                ("div", &division),
                ("move", &moves.to_string()),
                ("assault", &assaults.to_string()),
                ("fire", &fires.to_string()),
                ("emplim", &emplims.to_string()),
            ],
        ));
        planned = true;
        game.div_orders.insert(division, order);
    }
    game.div_planned_turn = game.session.turn_number;
    if planned {
        state.orders_dirty = true;
    }
    planned
}

/// Turn-start planning for the AI-controlled allied
/// contingents on the player's side (§7.5) — one TacticalAi per nation, each
/// planning only its own divisions against the player's fog view (the same
/// information limit as the division orders). Every other
/// ON-BOARD player-side unit goes in as a passive friendly: it blocks
/// pathing and counts in the odds, but is never commanded. A division under
/// a player-issued standing order is suspended from the slice (§7.4 —
/// plan_division_orders already planned it in the same tick), and the
/// trailing `AiAction::EndTurn` is dropped: the turn belongs to the player.
///
/// Runs right after [`plan_division_orders`] in the same once-per-turn tick
/// (shared `div_planned_turn` latch). Returns true when any contingent
/// planned (dirty-flag signal for the caller).
fn plan_allied_nations(game: &mut GameController, state: &mut TacticalState, loc: &Locale) -> bool {
    if game.allies.is_empty() {
        return false;
    }
    let Some(grid) = state.grid.clone() else {
        return false;
    };
    let player = state.player_side;
    let foe: Vec<BattalionUnit> = state
        .units
        .iter()
        .filter(|u| u.side != player)
        .filter(|u| state.fog_state(u.position) == VisibilityState::Visible)
        .cloned()
        .collect();
    // Pre-battle intel: the opponent's deployment zone — the blind-march
    // goal with nothing visible (same as the division orders).
    let intel: Option<Vec<HexCoord>> = state
        .deployment_zones
        .as_ref()
        .map(|(a, d)| (if player == Side::Attacker { d } else { a }).clone());
    let flags = game.session.flags().cloned();

    let mut planned = false;
    let allies = game.allies.clone();
    for contingent in &allies {
        let slice: Vec<String> = contingent
            .divisions
            .iter()
            .filter(|d| !game.div_orders.contains_key(*d))
            .cloned()
            .collect();
        if slice.is_empty() {
            continue;
        }
        let own: Vec<BattalionUnit> = state
            .units
            .iter()
            .filter(|u| u.side == player && slice.contains(&u.division))
            .cloned()
            .collect();
        if !own.iter().any(|u| u.is_combat_effective()) {
            continue;
        }
        let own_ids: HashSet<usize> = own.iter().map(|u| u.id).collect();
        // Any state counts — a shattered battalion still occupies its hex.
        let passive: Vec<BattalionUnit> = state
            .units
            .iter()
            .filter(|u| {
                u.side == player
                    && u.position != BattalionUnit::OFFBOARD
                    && !own_ids.contains(&u.id)
            })
            .cloned()
            .collect();
        // Per-nation, per-turn deterministic seed — the division orders'
        // FNV-1a mix, keyed on the country tag.
        let mut h = 0xcbf29ce484222325u64;
        for b in contingent.tag.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        let seed = game.seed.wrapping_mul(0x9E3779B97F4A7C15)
            ^ h
            ^ (game.session.turn_number as u64).wrapping_mul(0x517CC1B727220A95);
        let mut ai = TacticalAi::new(player, contingent.tactic, seed);
        crate::stage!("plan_allied_nations: planning '{}'", contingent.tag);
        let actions = ai.plan_turn_full(
            &grid,
            &own,
            &foe,
            intel.as_deref(),
            flags.as_ref(),
            Some(&passive),
            Some(&state.units),
        );
        let actions: Vec<AiAction> = actions
            .into_iter()
            .filter(|a| !matches!(a, AiAction::EndTurn))
            .collect();
        crate::stage!(
            "plan_allied_nations: '{}' -> {} actions",
            contingent.tag,
            actions.len()
        );
        // Register the proposals as standing orders (silent — one summary
        // line per nation covers the log, mirroring log.div.plan).
        let count = |f: fn(&AiAction) -> bool| actions.iter().filter(|a| f(a)).count();
        let moves = count(|a| matches!(a, AiAction::MoveUnit { .. }));
        let assaults = count(|a| matches!(a, AiAction::Assault { .. }));
        let fires = count(|a| matches!(a, AiAction::FireSupport { .. }));
        let emplims = count(|a| matches!(a, AiAction::Emplace { .. } | AiAction::Limber { .. }));
        for a in &actions {
            apply_ai_action(a, player, game, state, loc, false);
        }
        game.log_line(loc.trf(
            "log.ally.plan",
            &[
                ("tag", &contingent.tag),
                ("move", &moves.to_string()),
                ("assault", &assaults.to_string()),
                ("fire", &fires.to_string()),
                ("emplim", &emplims.to_string()),
            ],
        ));
        planned = true;
    }
    planned
}

/// Turn-start planning for the player's division orders — once
/// per player turn (the same lazy gate the enemy AI turn uses), AFTER the
/// report modal drains. The enemy fire phase queues its battle reports in
/// the SAME frame the turn hands back to the player (run_ai_turn's
/// closeout); without the tour gate the plan would register the player's
/// attack orders (and draw her red attack lines) while the enemy's
/// resolution is still on screen — the player must see the enemy results
/// BEFORE her own orders appear.
fn tick_division_orders(
    mut game: Option<ResMut<GameController>>,
    mut state: ResMut<TacticalState>,
    tour: Res<BattleTour>,
    mut notice: ResMut<NoticeBar>,
    time: Res<Time>,
    loc: Res<LocaleRes>,
) {
    let Some(game) = game.as_deref_mut() else {
        return;
    };
    if game.session.phase != BattlePhase::TacticalActive {
        return;
    }
    if game.session.current_side != state.player_side {
        return;
    }
    if game.div_planned_turn == game.session.turn_number {
        return;
    }
    // The battle-report modal pauses the world — same rule as run_ai_turn:
    // the plan (and the player's attack lines it registers) waits until the
    // player has clicked through every engagement of the previous side.
    if tour.active {
        return;
    }
    crate::stage!(
        "tick_division_orders: planning (turn {})",
        game.session.turn_number
    );
    plan_division_orders(game, &mut state, &loc, &mut notice, time.elapsed_secs());
    // The allied nations plan right after — same once-per-turn
    // latch (plan_division_orders sets it even with no orders pending).
    if plan_allied_nations(game, &mut state, &loc) {
        state.orders_dirty = true;
    }
    crate::stage!("tick_division_orders: done");
}

/// Both sides have acted: retreat steps, §6.14 out-of-bounds leaving,
/// attrition, per-turn resets, fog, clock, flag tick + collapse
/// trigger (§6.11), sync/victory checks (§6.12/§8).
fn finish_full_turn(
    game: &mut GameController,
    state: &mut TacticalState,
    loc: &Locale,
    notice: &mut NoticeBar,
    now: f32,
) {
    let Some(grid) = state.grid.clone() else {
        return;
    };

    crate::stage!(
        "finish_full_turn: enter (turn {})",
        game.session.turn_number
    );
    // Normalization fallback first — an Active unit with
    // org/strength ≤ 0 (transient or forged) is untargetable yet counts
    // for victory; normalize before anything else reads the roster.
    for u in state.units.iter_mut() {
        u.normalize_broken_state();
    }
    // Maintenance attachments regenerate a little strength each
    // full turn.
    for u in state.units.iter_mut() {
        let regen = u.support_str_regen();
        if regen > 0.0 && u.is_combat_effective() {
            u.strength = (u.strength + regen).min(u.max_strength);
        }
    }

    // §6.13: in-command battalions regenerate org near their HQ.
    game.combat.apply_command_regen(&mut state.units);

    // Retreating units stumble toward their own edge (§6.8). The defender
    // scores against its deployment zone's eastern rim (on stitched maps
    // the map edge can lie outside the battle province — a rout that
    // dead-ends at the province rim never leaves).
    let retreating: Vec<usize> = state
        .units
        .iter()
        .filter(|u| u.state == UnitState::Retreating)
        .map(|u| u.id)
        .collect();
    let retreat_zones = state.deployment_zones.clone();
    for id in retreating {
        let zones = retreat_zones
            .as_ref()
            .map(|(a, d)| (a.as_slice(), d.as_slice()));
        retreat_step_zoned(&grid, &mut state.units, id, true, zones);
    }

    // §6.14: out-of-bounds dwell — a unit ending the turn in the
    // shoreline ring accrues dwell; at oob_leaving_turns it leaves the
    // battle (org wiped, strength frozen, OFFBOARD). The wiped org ratio
    // rides the sync damage channel like any combat loss (strength 0.0 —
    // the unit slipped away intact).
    let departures = apply_oob_leaving(&grid, &mut state.units, game.combat.params());
    for d in &departures {
        // The point channel wants wiped POINTS (= org_frac × max_org).
        let (tag, prov, mx_org, mx_str) = state
            .unit_by_id(d.unit_id)
            .map(|u| (u.tag.clone(), u.hoi4_province, u.max_org, u.max_strength))
            .unwrap_or_else(|| (String::new(), None, 1.0, 1.0));
        game.session
            .record_damage(d.side, &tag, prov, d.org_frac * mx_org, 0.0, mx_org, mx_str);
        game.log_line_icon(
            Some(IconId::Door),
            loc.trf("log.oob.left", &[("name", &d.name)]),
        );
    }
    crate::stage!("finish_full_turn: regen+retreat done");

    // Encirclement attrition (§6.4).
    game.combat
        .apply_encirclement_attrition(&grid, &mut state.units);
    crate::stage!("finish_full_turn: attrition done");

    // §6.11: end-of-turn flag tick — the control ratio inside
    // each flag zone nudges its progress; full capture zeroes the
    // defender's org (strength UNTOUCHED) and the ATTACKER WINS at once
    // (no mop-up flow — the strategic layer resolves the
    // retreat). The player gets a progress bar and a "flag falling" warning,
    // never a dialog.
    let mut warn_falling = false;
    let mut collapse_count: Option<usize> = None;
    if let Some(flags) = game.session.flags_mut() {
        let params = game.combat.params();
        let tick = flags.tick(&grid, &mut state.units, params);
        for f in &mut flags.flags {
            if f.progress * 3 >= params.flag_progress_cap * 2 && !f.warned_high {
                f.warned_high = true;
                warn_falling = true;
            }
        }
        if tick.collapse_fired {
            collapse_count = Some(
                state
                    .units
                    .iter()
                    .filter(|u| u.side == Side::Defender && u.org <= 0.0)
                    .count(),
            );
        }
    }
    if warn_falling {
        notice.flash = Some(FlashNotice::plain(
            loc.tr("notice.flag.falling").into_owned(),
            now + 6.0,
        ));
        game.log_line_icon(Some(IconId::Warning), loc.tr("log.flag.falling"));
    }
    if let Some(n) = collapse_count {
        notice.flash = Some(FlashNotice::plain(
            loc.tr("notice.flag.collapse").into_owned(),
            now + 8.0,
        ));
        game.log_line_icon(
            Some(IconId::Surrender),
            loc.trf("log.flag.collapse", &[("n", &n.to_string())]),
        );
    }

    // Per-turn reset for everyone (§6.2): action flags + defense pools.
    for u in state.units.iter_mut() {
        u.refresh_turn();
    }

    crate::stage!("finish_full_turn: flag tick done");
    state.turn = game.session.turn_number;
    update_fog(state, game);
    state.units_dirty = true;
    check_victory_and_transition(game, state, loc);
    crate::stage!("finish_full_turn: exit");
}

fn check_victory_and_transition(
    game: &mut GameController,
    state: &mut TacticalState,
    loc: &Locale,
) {
    if game.session.phase != BattlePhase::TacticalActive
        && game.session.phase != BattlePhase::ReadyToSync
    {
        return;
    }
    // §6.11: flag capture ends the battle
    // IMMEDIATELY — the ATTACKER wins, the defender's org is already zeroed
    // (strength untouched) by the tick; no rout flow, no mop-up. The
    // strategic layer resolves the retreat / province outcome itself.
    if let Some(flags) = game.session.flags() {
        if flags.captured(game.combat.params()) {
            if Side::Attacker == state.player_side {
                game.log_line_icon(Some(IconId::Trophy), loc.tr("log.victory"));
            } else {
                game.log_line_icon(Some(IconId::Dove), loc.tr("log.defeat"));
            }
            game.session.ready_to_end().ok();
            return;
        }
    }
    match game.session.check_victory(&state.units) {
        VictoryOutcome::Winner(winner) => {
            if winner == state.player_side {
                game.log_line_icon(Some(IconId::Trophy), loc.tr("log.victory"));
            } else {
                game.log_line_icon(Some(IconId::Dove), loc.tr("log.defeat"));
            }
            game.session.ready_to_end().ok();
        }
        // Mutual annihilation is a terminal outcome too — otherwise the
        // battle would stall on an empty board (the end batch never leaves).
        VictoryOutcome::Draw => {
            game.log_line_icon(Some(IconId::Dove), loc.tr("log.draw"));
            game.session.ready_to_end().ok();
        }
        VictoryOutcome::Undecided => {
            if game.session.phase == BattlePhase::ReadyToSync {
                game.log_line_icon(
                    Some(IconId::Hourglass),
                    loc.trf(
                        "log.hour.complete",
                        &[("n", &(game.session.strategic_hour + 1).to_string())],
                    ),
                );
            }
        }
    }
}

/// A drawn rectangle's deployable area — `anchor.rect_between`
/// (release) ∩ the side's zone ∩ passable, deployable terrain (the guard
/// shared by sector deploy and the allied suggestion path), minus
/// already-occupied hexes.
fn sector_area(
    grid: &HexGrid,
    zone: &[HexCoord],
    anchor: HexCoord,
    release: HexCoord,
    used: &HashSet<(i32, i32)>,
) -> Vec<HexCoord> {
    anchor
        .rect_between(release)
        .into_iter()
        .filter(|h| {
            zone.contains(h)
                && grid
                    .cell(*h)
                    .map(|c| c.is_passable && c.terrain.is_deployable())
                    .unwrap_or(false)
                && !used.contains(&(h.q, h.r))
        })
        .collect()
}

/// AI deployment (§11.1.5, terrain-aware scoring): arranges `side`'s
/// combat-effective units inside its own zone.
/// Runs for the ENEMY at Begin Battle, out of the player's sight; also used
/// by the player's "Auto Deploy" button for the units still
/// waiting in the OOB — `pre_used` then protects the hexes the player
/// placed by hand. Zones already hold only deployable hexes.
/// The tactic card shapes the deployment (urban garrisons /
/// cover lurking / river hugging); the player's card is Default.
/// `exclude` names divisions that must stay OFFBOARD (allied AI
/// slices on the player's side) — ai_deploy's `only_division` is an
/// INCLUSION filter, so an exclusion deploys division-by-division.
/// `only_undeployed`: the player's Auto Deploy passes
/// true (hand-placed units stay put); the BeginBattle ENEMY deploy must pass
/// false — enemy battalions arrive NOT marked undeployed (scenario.rs
/// `mark_player_undeployed` flags the player side only) on a naive
/// zone-order spread, and a true here filters every one of them out of the
/// planner, leaving the whole side piled on the zone's first hexes.
#[allow(clippy::too_many_arguments)]
fn deploy_side(
    state: &mut TacticalState,
    side: Side,
    tactic: CombatTactic,
    pre_used: &HashSet<(i32, i32)>,
    exclude: &HashSet<String>,
    only_undeployed: bool,
) -> usize {
    let Some((a_zone, d_zone)) = state.deployment_zones.clone() else {
        return 0;
    };
    let Some(grid) = state.grid.clone() else {
        return 0;
    };
    let (self_zone, foe_zone) = if side == Side::Attacker {
        (a_zone, d_zone)
    } else {
        (d_zone, a_zone)
    };
    let mut placed = 0usize;
    for u in state.units.iter().filter(|u| {
        u.side == side && (!only_undeployed || u.undeployed) && !exclude.contains(&u.division)
    }) {
        if u.is_combat_effective() {
            placed += 1;
        }
    }
    if exclude.is_empty() {
        tactical_ai::ai_deploy_impl(
            &grid,
            &mut state.units,
            &self_zone,
            &foe_zone,
            side,
            tactic,
            pre_used,
            None,
            None,
            only_undeployed,
        );
    } else {
        // Division-by-division so the excluded slices stay untouched; each
        // division's fresh hexes join `used` to keep the single call's
        // accumulation semantics (later divisions spread, never stack).
        // The ordered division list rides along so the sector
        // partition gives each division its own slice of the band instead
        // of every division spanning the whole front.
        let mut used = pre_used.clone();
        let mut divisions: Vec<String> = Vec::new();
        for u in &state.units {
            if u.side == side
                && (!only_undeployed || u.undeployed)
                && !exclude.contains(&u.division)
                && !divisions.contains(&u.division)
            {
                divisions.push(u.division.clone());
            }
        }
        for division in &divisions {
            tactical_ai::ai_deploy_impl(
                &grid,
                &mut state.units,
                &self_zone,
                &foe_zone,
                side,
                tactic,
                &used,
                Some(division),
                Some(&divisions),
                only_undeployed,
            );
            for u in &state.units {
                if u.side == side
                    && u.division == *division
                    && u.position != BattalionUnit::OFFBOARD
                {
                    used.insert((u.position.q, u.position.r));
                }
            }
        }
    }
    // ai_deploy only writes positions; the flag is ours. Units it could not
    // fit (zone smaller than the force) keep the OFFBOARD sentinel and stay
    // in the OOB queue. Excluded (allied) divisions keep waiting OFFBOARD.
    for u in &mut state.units {
        if u.side == side
            && u.undeployed
            && u.position != BattalionUnit::OFFBOARD
            && !exclude.contains(&u.division)
        {
            u.undeployed = false;
        }
    }
    state.units_dirty = true;
    placed
}

/// Sector deployment: commit a player-drawn rectangle (anchor →
/// release hex, axial bounding box) as the deployment area for ONE division.
/// The rectangle is intersected with the player's zone + deployable terrain
/// and the planner fills it with the division's undeployed battalions
/// (hand-placed hexes protected via `pre_used`, exactly like Auto Deploy).
/// For an ALLIED division (§7.5) the rectangle is a deployment
/// SUGGESTION instead — stored into `game.allied_sectors` and consumed by
/// the division's own AI at BeginBattle (re-drag overwrites, Recall clears).
#[allow(clippy::too_many_arguments)]
fn commit_sector_deploy(
    state: &mut TacticalState,
    game: &mut GameController,
    division: String,
    anchor: HexCoord,
    release: HexCoord,
    loc: &Locale,
    notice: &mut NoticeBar,
    now: f32,
) {
    let Some(grid) = state.grid.clone() else {
        return;
    };
    let Some((a_zone, d_zone)) = state.deployment_zones.clone() else {
        return;
    };
    let player = state.player_side;
    let (self_zone, foe_zone) = if player == Side::Attacker {
        (a_zone, d_zone)
    } else {
        (d_zone, a_zone)
    };
    let used: HashSet<(i32, i32)> = state
        .units
        .iter()
        .filter(|u| u.side == player && !u.undeployed && u.is_combat_effective())
        .map(|u| (u.position.q, u.position.r))
        .collect();
    // The same guard as everywhere else: only passable, deployable zone
    // hexes may host units, even inside a player-drawn sector.
    let sector = sector_area(&grid, &self_zone, anchor, release, &used);
    if sector.is_empty() {
        game.log_line(loc.tr("log.deploy.sector_empty"));
        return;
    }
    // A drag across the whole board would hand the planner a sector with
    // hundreds of thousands of hexes (O(sector × foe-zone) front math) —
    // reject absurd rectangles instead of freezing.
    if sector.len() > 4096 {
        game.log_line(loc.trf(
            "log.deploy.sector_too_large",
            &[("n", &sector.len().to_string())],
        ));
        return;
    }
    // The allied branch stores the hint and stops — the division
    // waits OFFBOARD for its own AI (deploy_allied_nations at BeginBattle).
    if game.allied_division(&division).is_some() {
        game.allied_sectors
            .insert(division.clone(), (anchor, release));
        state.ally_sectors_dirty = true;
        notice.flash = Some(FlashNotice::plain(
            loc.trf("notice.deploy.ally_sector_set", &[("div", &division)]),
            now + 4.0,
        ));
        return;
    }
    let waiting: usize = state
        .units
        .iter()
        .filter(|u| {
            u.side == player && u.undeployed && u.division == division && u.is_combat_effective()
        })
        .count();
    tactical_ai::ai_deploy(
        &grid,
        &mut state.units,
        &sector,
        &foe_zone,
        player,
        CombatTactic::Default,
        &used,
        Some(&division),
    );
    let mut placed = 0usize;
    for u in &mut state.units {
        if u.side == player && u.undeployed && u.position != BattalionUnit::OFFBOARD {
            u.undeployed = false;
            placed += 1;
        }
    }
    state.units_dirty = true;
    let label = if division.is_empty() {
        loc.tr("oob.unattached").into_owned()
    } else {
        division
    };
    if placed > 0 {
        game.log_line(loc.trf(
            "log.deploy.sector_placed",
            &[("label", &label), ("n", &placed.to_string())],
        ));
    } else if waiting > 0 {
        game.log_line(loc.trf(
            "log.deploy.sector_no_room",
            &[("label", &label), ("waiting", &waiting.to_string())],
        ));
    }
}

/// Deploy every allied contingent at BeginBattle (§7.5) — each of
/// its divisions through its own `ai_deploy` call with the contingent's
/// tactic card, into the player's stored sector suggestion when one exists
/// (rect ∩ zone ∩ deployable, the sector_area guard; empty → the full
/// player zone). A deployed division's hexes join `pre_used` so later
/// divisions and nations spread instead of stacking.
fn deploy_allied_nations(game: &mut GameController, state: &mut TacticalState, loc: &Locale) {
    if game.allies.is_empty() {
        return;
    }
    let Some(grid) = state.grid.clone() else {
        return;
    };
    let Some((a_zone, d_zone)) = state.deployment_zones.clone() else {
        return;
    };
    let player = state.player_side;
    let (self_zone, foe_zone) = if player == Side::Attacker {
        (a_zone, d_zone)
    } else {
        (d_zone, a_zone)
    };
    // Hand-placed hexes (any already-placed player-side unit) are sacred —
    // the same pre_used semantics as Auto Deploy.
    let mut pre_used: HashSet<(i32, i32)> = state
        .units
        .iter()
        .filter(|u| u.side == player && !u.undeployed && u.is_combat_effective())
        .map(|u| (u.position.q, u.position.r))
        .collect();
    let allies = game.allies.clone();
    let sectors = game.allied_sectors.clone();
    // The full player-side division order — the sector partition
    // slices the band per division instead of letting every division span
    // the whole front.
    let division_order: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for c in &allies {
            for d in &c.divisions {
                if seen.insert(d.clone()) {
                    out.push(d.clone());
                }
            }
        }
        out
    };
    for contingent in &allies {
        for division in &contingent.divisions {
            let area: Vec<HexCoord> = sectors
                .get(division)
                .map(|(anchor, release)| {
                    sector_area(&grid, &self_zone, *anchor, *release, &pre_used)
                })
                .filter(|area| !area.is_empty())
                .unwrap_or_else(|| self_zone.clone());
            tactical_ai::ai_deploy_impl(
                &grid,
                &mut state.units,
                &area,
                &foe_zone,
                player,
                contingent.tactic,
                &pre_used,
                Some(division),
                Some(&division_order),
                false, // BeginBattle allied deploy: arrange the contingent whole
            );
            // Division-scoped flip (deploy_side's whole-side flip is the
            // wrong granularity here): only this division's placed units,
            // and their hexes crowd out the next division.
            for u in &mut state.units {
                if u.side == player
                    && u.division == *division
                    && u.undeployed
                    && u.position != BattalionUnit::OFFBOARD
                {
                    u.undeployed = false;
                    pre_used.insert((u.position.q, u.position.r));
                }
            }
        }
        game.log_line(loc.trf("log.ally.deployed", &[("tag", &contingent.tag)]));
    }
    state.units_dirty = true;
}

/// Per-frame sector-deployment preview: while the player drags (anchor set),
/// compute where the division would land by running a throwaway ai_deploy
/// on a cloned roster — the result renders as translucent ghosts
/// (state.sector_preview, units.rs). No area highlighting — the ghost
/// preview alone suffices. The ghost math is throttled to 5 Hz (a cloned
/// 58-battalion roster + front-distance table every frame is too heavy while dragging).
fn tick_sector_preview(mut state: ResMut<TacticalState>, time: Res<Time>, mut last: Local<f32>) {
    let Some(pick) = state.deploy_sector.clone() else {
        state.sector_preview.clear();
        return;
    };
    let Some(anchor) = pick.anchor else {
        state.sector_preview.clear();
        return;
    };
    let Some(h) = state.hover_hex else { return };
    let Some(grid) = state.grid.clone() else {
        return;
    };
    let Some((a_zone, d_zone)) = state.deployment_zones.clone() else {
        return;
    };
    let player = state.player_side;
    let (self_zone, foe_zone) = if player == Side::Attacker {
        (a_zone, d_zone)
    } else {
        (d_zone, a_zone)
    };
    let used: HashSet<(i32, i32)> = state
        .units
        .iter()
        .filter(|u| u.side == player && !u.undeployed && u.is_combat_effective())
        .map(|u| (u.position.q, u.position.r))
        .collect();
    // Same filtering as the commit path.
    let sector = sector_area(&grid, &self_zone, anchor, h, &used);
    if sector.is_empty() || sector.len() > 4096 {
        state.sector_preview.clear();
        return;
    }
    // Throttle: recompute the ghost positions at most every 0.2 s while
    // dragging (the commit on release always uses the live roster).
    let now = time.elapsed_secs();
    if now - *last < 0.2 {
        return;
    }
    *last = now;
    // Throwaway preview: clone the roster, let the planner place the
    // division, read back where it would land. The real units are never
    // touched — only the id → hex map is kept for the ghost rendering.
    let mut probe = state.units.clone();
    tactical_ai::ai_deploy(
        &grid,
        &mut probe,
        &sector,
        &foe_zone,
        player,
        CombatTactic::Default,
        &used,
        Some(&pick.division),
    );
    state.sector_preview = probe
        .iter()
        .filter(|u| {
            u.side == player && u.division == pick.division && u.position != BattalionUnit::OFFBOARD
        })
        .map(|u| (u.id, u.position))
        .collect();
}

fn update_fog(state: &mut TacticalState, game: &GameController) {
    let Some(grid) = state.grid.clone() else {
        return;
    };
    state
        .fog
        .get_or_insert_with(|| {
            FogOfWar::new(
                grid.width,
                grid.height,
                game.combat.params().fog_reveal_duration_turns,
            )
        })
        .update(
            &grid,
            &state.units,
            state.player_side,
            game.session.turn_number,
        );
    // The opposing side's fog, for fog-limited AI planning.
    state
        .ai_fog
        .get_or_insert_with(|| {
            FogOfWar::new(
                grid.width,
                grid.height,
                game.combat.params().fog_reveal_duration_turns,
            )
        })
        .update(
            &grid,
            &state.units,
            state.player_side.opponent(),
            game.session.turn_number,
        );
    // A selected enemy that slips out of sight must not keep
    // leaking through the selection ring / stat panel / command highlights.
    if let Some(id) = state.selected_unit {
        let leaks = state.unit_by_id(id).is_some_and(|u| {
            u.side != state.player_side
                && state
                    .fog
                    .as_ref()
                    .is_some_and(|f| !f.is_visible(u.position))
        });
        if leaks {
            state.selected_unit = None;
        }
    }
    state.board_colors_dirty = true;
}

/// The battle-report modal drives pacing ([Continue] advances
/// `tour.index`); this system only keeps the camera focused on the current
/// engagement hex and cleans up once the queue is read through. Esc skips
/// the rest (handled in handle_map_clicks). The current report's
/// hex also gets a bright-red board highlight, replaced per report and
/// cleared when the tour drains. The report's own combat
/// animation (tracers + floaters) is released here, per engagement.
/// The zoom is pinned to REPORT_CAM_DISTANCE for the playback
/// (saved + restored), and each report's deferred unit visuals (repel /
/// annihilation ghosts) are released only once the player confirms it.
/// All camera moves are smooth GLIDES (CameraGlide) — from the
/// pre-tour view to the first report, between reports, and back to the
/// pre-tour view when the queue drains. The combat FX and the report
/// window wait for the glide's arrival pulse; the tour's teardown waits
/// for the return glide to land.
fn tick_battle_tour(
    mut tour: ResMut<BattleTour>,
    mut glide: ResMut<crate::camera::CameraGlide>,
    mut state: ResMut<TacticalState>,
    mut fx: ResMut<FxQueue>,
    mut anims: ResMut<MoveAnims>,
    q_cam: Query<&crate::camera::RtsCamera>,
    time: Res<Time>,
) {
    if !tour.active {
        return;
    }
    // The glide's one-frame arrival pulse (consumed exactly once).
    let arrived = glide.take_arrived();

    // Gliding BACK to the pre-tour view: teardown on landing.
    if tour.returning {
        if !arrived {
            return;
        }
        if !state.report_ghosts.is_empty() {
            state.report_ghosts.clear();
            state.units_dirty = true;
        }
        tour.active = false;
        tour.returning = false;
        tour.queue.clear();
        tour.focused = None;
        tour.next_focus_at = None;
        tour.saved_zoom = None;
        tour.saved_target = None;
        state.clear_highlights(); // the report-hex highlight
        return;
    }

    // Esc may skip the queue mid-glide: drop the pending beat entirely.
    if tour.awaiting_arrival && tour.index >= tour.queue.len() {
        tour.awaiting_arrival = false;
    }
    // Gliding TO the current report: on arrival its combat animation plays
    // and the report window opens (draw_battle_report gates on `focused`).
    if tour.awaiting_arrival {
        if !arrived {
            return;
        }
        tour.awaiting_arrival = false;
        let i = tour.index;
        let evs = std::mem::take(&mut tour.queue[i].fx);
        for ev in evs {
            fx.push(ev);
        }
        tour.focused = Some(i);
        return;
    }

    // Release the deferred unit visuals of every report the player has
    // confirmed (index moved past it) — a repelled defender slides to its
    // new hex now, an annihilated one vanishes now, never before the click.
    let confirmed = tour.index.min(tour.queue.len());
    for i in 0..confirmed {
        release_report_pending(&mut tour.queue[i], &mut state, &mut anims);
    }
    if tour.index >= tour.queue.len() {
        // Esc-skipped reports never got a click: release their visuals too,
        // then clear any ghost that somehow outlived its report.
        for i in confirmed..tour.queue.len() {
            release_report_pending(&mut tour.queue[i], &mut state, &mut anims);
        }
        if !state.report_ghosts.is_empty() {
            state.report_ghosts.clear();
            state.units_dirty = true;
        }
        // Glide back to the pre-tour view (teardown on landing,
        // see the `returning` branch) — no camera → tear down instantly.
        let Ok(cam) = q_cam.get_single() else {
            tour.active = false;
            tour.queue.clear();
            tour.focused = None;
            tour.next_focus_at = None;
            tour.saved_zoom = None;
            tour.saved_target = None;
            state.clear_highlights();
            return;
        };
        let to_target = tour.saved_target.unwrap_or(cam.target);
        let to_distance = tour.saved_zoom.unwrap_or(cam.distance);
        glide.start(cam, to_target, to_distance);
        tour.returning = true;
        return;
    }
    if tour.focused != Some(tour.index) {
        // A just-confirmed report's consequence (repel slide /
        // vanish) is animating on the OLD hex — dwell there for the beat
        // draw_battle_report asked for before gliding to the next engagement.
        if let Some(at) = tour.next_focus_at {
            if time.elapsed_secs() < at {
                return;
            }
            tour.next_focus_at = None;
        }
        let i = tour.index;
        let hex = tour.queue[i].hex;
        // The pre-tour view is saved ONCE — the return glide's destination
        // (the player's own distance + where she was looking).
        if tour.saved_zoom.is_none() {
            if let Ok(cam) = q_cam.get_single() {
                tour.saved_zoom = Some(cam.distance);
                tour.saved_target = Some(cam.target);
            }
        }
        // Glide to the engagement at the pinned report
        // magnification — the combat FX and the report window wait for the
        // arrival pulse. No camera (defensive) → the old instant beat.
        if let Ok(cam) = q_cam.get_single() {
            glide.start(
                cam,
                hex_world(hex) + Vec3::Y * 0.4,
                crate::camera::REPORT_CAM_DISTANCE,
            );
            tour.awaiting_arrival = true;
        } else {
            let evs = std::mem::take(&mut tour.queue[i].fx);
            for ev in evs {
                fx.push(ev);
            }
            tour.focused = Some(i);
        }
        state.set_highlights(vec![(hex, HighlightKind::Report)]);
    }
}

/// Release one report's deferred unit visuals — drop the ghost
/// (pre-combat stand-in) and animate the consequence NOW: a unit that moved
/// (repelled defender / assault advance) slides from its ghost hex to
/// wherever it stands; an eliminated / surrendered one simply disappears on
/// the rebuild. Idempotent — the report's pending list is taken, not read.
fn release_report_pending(
    report: &mut BattleReport,
    state: &mut TacticalState,
    anims: &mut MoveAnims,
) {
    let pending = std::mem::take(&mut report.pending);
    if pending.is_empty() {
        return;
    }
    for p in pending {
        state.report_ghosts.remove(&p.unit_id);
        let Some(u) = state.unit_by_id(p.unit_id) else {
            continue;
        };
        let rendered = u.is_combat_effective() || u.state == UnitState::Retreating;
        let to = u.position;
        if rendered && to != p.from {
            let wp = |h: HexCoord| {
                let (x, z) = h.to_world(crate::board::HEX_SIZE);
                Vec3::new(x, unit_y_on_grid(state, h), z)
            };
            anims
                .0
                .entry(p.unit_id)
                .or_insert_with(|| vec![wp(p.from)])
                .push(wp(to));
        }
    }
    state.units_dirty = true;
}

/// Per-frame tick: deployment zone highlights, hover highlight, and the
/// transition listener — phase / side / hour transitions drive notice-bar
/// flashes and auto-open the battle log on turn-level summaries
/// (damage-scale events stay on model floaters; only turn-level events and
/// the sync summary may pop a window).
fn tick_battle(
    game: Option<ResMut<GameController>>,
    mut state: ResMut<TacticalState>,
    mut ui_win: ResMut<UiWindows>,
    mut notice: ResMut<NoticeBar>,
    tour: Res<BattleTour>,
    time: Res<Time>,
    loc: Res<LocaleRes>,
    mut prev: Local<Option<(BattlePhase, Side, u32, u32)>>,
) {
    let Some(game) = game else { return };

    let now = time.elapsed_secs();
    // A turn banner held back for the battle-report tour fires
    // once the reports are read through — with a fresh 3 s expiry.
    if !tour.active {
        if let Some(mut b) = notice.pending.take() {
            b.until = now + 3.0;
            notice.flash = Some(b);
        }
    }
    let cur = (
        game.session.phase,
        game.session.current_side,
        game.session.turn_number,
        game.session.strategic_hour,
    );
    if let Some((p_phase, p_side, p_turn, p_hour)) = prev.replace(cur) {
        let (phase, side, turn, hour) = cur;
        if cur != (p_phase, p_side, p_turn, p_hour) {
            // Side-colored turn banner on EVERY handover — the
            // AI's turn gets one too (its faction color), so the player
            // never clicks into the AI's action window by mistake. While
            // the battle-report tour is still playing the banner is held
            // back (notice.pending) until the reports are read through.
            if (side != p_side || turn != p_turn) && phase == BattlePhase::TacticalActive {
                let banner = if side == state.player_side {
                    FlashNotice::turn(
                        loc.trf("notice.turn.yours", &[("n", &turn.to_string())]),
                        side,
                        now,
                    )
                } else {
                    FlashNotice::turn(loc.tr("notice.turn.enemy").into_owned(), side, now)
                };
                if tour.active {
                    notice.pending = Some(banner);
                } else {
                    notice.flash = Some(banner);
                }
            }
            // Battle starts (deployment finished).
            if p_phase == BattlePhase::Deployment && phase == BattlePhase::TacticalActive {
                notice.flash = Some(FlashNotice::plain(
                    loc.tr("notice.battle_started").into_owned(),
                    now + 4.0,
                ));
            }
            // Sync completed (hour advanced outside deployment): a
            // top-anchored modal prompt (summary + Continue / End Tactic)
            // replaces the auto-opened log window. Desync mode seals hours
            // locally without any sync — the prompt would be a lie there.
            if hour != p_hour && phase != BattlePhase::Deployment && !game.session.sync_disabled {
                ui_win.sync_prompt = true;
            }
            // Ready to sync: point at the fixed button slot (right panel).
            if phase == BattlePhase::ReadyToSync && p_phase != BattlePhase::ReadyToSync {
                notice.flash = Some(FlashNotice::plain(
                    loc.trf("notice.hour_complete", &[("n", &(hour + 1).to_string())]),
                    now + 6.0,
                ));
            }
            // Battle resolved: auto-open the log for the victory/defeat line.
            if phase == BattlePhase::ReadyToEnd && p_phase != BattlePhase::ReadyToEnd {
                ui_win.log = true;
                notice.flash = Some(FlashNotice::plain(
                    loc.tr("notice.battle_resolved").into_owned(),
                    now + 6.0,
                ));
            }
        }
    }

    // There is intentionally NO fog during
    // deployment — the player surveys the whole map, and enemy units simply
    // are not placed yet (the AI deploys at BeginBattle). Fog starts with
    // the first update_fog call at battle start.
    // No Deployment zone WASH highlight — the
    // thick border mesh (sync_zone_border) carries zone readability.

    // Hover highlight (lowest priority layer).
    let hover = state.hover_hex;
    let hover_marked = hover
        .map(|h| state.highlight_at(h) == Some(HighlightKind::Hover))
        .unwrap_or(false);
    let has_hover = state
        .highlights
        .iter()
        .any(|(_, k)| *k == HighlightKind::Hover);
    if hover_marked != has_hover {
        state.highlights.retain(|(_, k)| *k != HighlightKind::Hover);
        if let Some(h) = hover {
            state.highlights.push((h, HighlightKind::Hover));
        }
        state.board_colors_dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tactical_core::unit::UnitType;
    use tactical_core::{HexGrid, Terrain};

    fn report_with_pending(hex: HexCoord, pending: Vec<PendingUnitVisual>) -> BattleReport {
        BattleReport {
            hex,
            defender: "D".to_string(),
            acting: Side::Attacker,
            lanes: Vec::new(),
            outcome: String::new(),
            fx: Vec::new(),
            pending,
        }
    }

    /// Confirming a repelled defender's report releases its
    /// ghost — the pre-combat stand-in is dropped and the slide from the
    /// pre-combat hex to the retreated hex is queued only now (never
    /// before the click).
    #[test]
    fn release_report_pending_slides_repelled_unit() {
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(8, 8, Terrain::Plains)));
        let old = HexCoord::new(2, 2);
        let new = HexCoord::new(3, 2);
        state.units = vec![BattalionUnit::new(
            1,
            "D",
            UnitType::Infantry,
            Side::Defender,
            new,
        )];
        state.units[0].state = UnitState::Retreating;
        state.report_ghosts.insert(1, old);
        let mut report = report_with_pending(
            old,
            vec![PendingUnitVisual {
                unit_id: 1,
                from: old,
            }],
        );
        let mut anims = MoveAnims::default();

        release_report_pending(&mut report, &mut state, &mut anims);

        assert!(state.report_ghosts.is_empty(), "ghost released");
        let wp = anims.0.get(&1).expect("slide queued for the repelled unit");
        assert_eq!(wp.len(), 2, "ghost hex → current hex");
        let (fx, fz) = old.to_world(crate::board::HEX_SIZE);
        assert!((wp[0].x - fx).abs() < 1e-4 && (wp[0].z - fz).abs() < 1e-4);
        let (tx, tz) = new.to_world(crate::board::HEX_SIZE);
        assert!((wp[1].x - tx).abs() < 1e-4 && (wp[1].z - tz).abs() < 1e-4);
        assert!(state.units_dirty);

        // Idempotent: the pending list was taken — a second release is a no-op.
        state.units_dirty = false;
        release_report_pending(&mut report, &mut state, &mut anims);
        assert!(!state.units_dirty && anims.0.len() == 1);
    }

    /// An annihilated defender has nothing to slide — the ghost
    /// is dropped and the unit simply disappears on the rebuild (no
    /// waypoints queued).
    #[test]
    fn release_report_pending_vanishes_eliminated_unit() {
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(8, 8, Terrain::Plains)));
        let old = HexCoord::new(2, 2);
        state.units = vec![BattalionUnit::new(
            1,
            "D",
            UnitType::Infantry,
            Side::Defender,
            old,
        )];
        state.units[0].state = UnitState::Eliminated;
        state.report_ghosts.insert(1, old);
        let mut report = report_with_pending(
            old,
            vec![PendingUnitVisual {
                unit_id: 1,
                from: old,
            }],
        );
        let mut anims = MoveAnims::default();

        release_report_pending(&mut report, &mut state, &mut anims);

        assert!(state.report_ghosts.is_empty());
        assert!(
            anims.0.is_empty(),
            "eliminated units vanish without a slide"
        );
        assert!(state.units_dirty);
    }

    /// A broken defender who never moved (fire break, no assault
    /// occupation) renders identically before and after — the release drops
    /// the ghost without queuing a zero-length slide.
    #[test]
    fn release_report_pending_stationary_unit_no_slide() {
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(8, 8, Terrain::Plains)));
        let hex = HexCoord::new(2, 2);
        state.units = vec![BattalionUnit::new(
            1,
            "D",
            UnitType::Infantry,
            Side::Defender,
            hex,
        )];
        state.report_ghosts.insert(1, hex);
        let mut report = report_with_pending(
            hex,
            vec![PendingUnitVisual {
                unit_id: 1,
                from: hex,
            }],
        );
        let mut anims = MoveAnims::default();

        release_report_pending(&mut report, &mut state, &mut anims);

        assert!(state.report_ghosts.is_empty());
        assert!(anims.0.is_empty(), "no slide when the unit never moved");
        assert!(state.units_dirty);
    }

    /// The suggestion-rect → area filter — rect ∩ zone ∩
    /// passable+deployable terrain, minus occupied hexes (shared by sector
    /// deploy, the ghost preview, and the allied BeginBattle deploy).
    #[test]
    fn sector_area_filters_rect_zone_terrain_and_used() {
        let mut grid = HexGrid::new(8, 8, Terrain::Plains);
        // (2,2) impassable; (3,2) a river — crossable, never deployable.
        grid.cell_mut(HexCoord::new(2, 2)).unwrap().is_passable = false;
        grid.cell_mut(HexCoord::new(3, 2)).unwrap().terrain = Terrain::River;
        // Zone covers the rect except column q=1.
        let zone: Vec<HexCoord> = (2..=3)
            .flat_map(|q| (1..=3).map(move |r| HexCoord::new(q, r)))
            .collect();
        let mut used = HashSet::new();
        used.insert((2, 3));
        let area = sector_area(
            &grid,
            &zone,
            HexCoord::new(1, 1),
            HexCoord::new(3, 3),
            &used,
        );
        let set: HashSet<(i32, i32)> = area.iter().map(|h| (h.q, h.r)).collect();
        // Rect (1..=3 × 1..=3) ∩ zone (q≥2) = 6 hexes; minus impassable
        // (2,2), undeployable river (3,2), occupied (2,3) → 3 left.
        assert_eq!(set.len(), 3, "got {set:?}");
        assert!(set.contains(&(2, 1)));
        assert!(set.contains(&(3, 1)));
        assert!(set.contains(&(3, 3)));
        assert!(!set.contains(&(1, 1)), "outside the zone");
        assert!(!set.contains(&(2, 2)), "impassable");
        assert!(!set.contains(&(3, 2)), "river is not deployable");
        assert!(!set.contains(&(2, 3)), "occupied");
    }

    /// An empty-zone / empty-rect suggestion yields no area — the
    /// BeginBattle allied deploy falls back to the full zone on this.
    #[test]
    fn sector_area_empty_when_rect_misses_zone() {
        let grid = HexGrid::new(8, 8, Terrain::Plains);
        let zone: Vec<HexCoord> = (5..=6)
            .flat_map(|q| (5..=6).map(move |r| HexCoord::new(q, r)))
            .collect();
        let area = sector_area(
            &grid,
            &zone,
            HexCoord::new(1, 1),
            HexCoord::new(2, 2),
            &HashSet::new(),
        );
        assert!(area.is_empty(), "rect wholly outside the zone: {area:?}");
    }

    /// The BeginBattle ENEMY deploy must re-plan the
    /// whole side — enemy battalions arrive NOT flagged `undeployed`
    /// (scenario.rs marks the player side only) sitting on the assembly's
    /// naive `zone[i % len]` spread, so an `only_undeployed = true` here
    /// filters every enemy unit out of the planner and the side stays piled
    /// on the zone's first hexes. The flag belongs
    /// to the player's Auto Deploy only.
    #[test]
    fn begin_battle_enemy_deploy_replans_assembly_spread() {
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(16, 8, Terrain::Plains)));
        let a_zone: Vec<HexCoord> = (0..8).map(|r| HexCoord::new(0, r)).collect();
        let d_zone: Vec<HexCoord> = (10..14)
            .flat_map(|q| (0..8).map(move |r| HexCoord::new(q, r)))
            .collect();
        state.deployment_zones = Some((a_zone, d_zone.clone()));
        // Assembly placeholder: NOT undeployed, all piled on the zone's
        // first hex (the state the assembly spread leaves at BeginBattle).
        state.units = (0..3)
            .map(|i| {
                let mut u =
                    BattalionUnit::new(i + 1, "D", UnitType::Infantry, Side::Defender, d_zone[0]);
                u.division = "Div".to_string();
                u
            })
            .collect();

        let placed = deploy_side(
            &mut state,
            Side::Defender,
            CombatTactic::Default,
            &HashSet::new(),
            &HashSet::new(),
            false,
        );

        assert_eq!(placed, 3, "the whole side is in scope, flag or no flag");
        let spots: HashSet<(i32, i32)> = state
            .units
            .iter()
            .map(|u| (u.position.q, u.position.r))
            .collect();
        assert_eq!(
            spots.len(),
            3,
            "every unit re-planned off the assembly pile: {:?}",
            state.units.iter().map(|u| u.position).collect::<Vec<_>>()
        );
        assert!(
            state.units.iter().all(|u| d_zone.contains(&u.position)),
            "re-planned inside the own zone"
        );
    }

    /// Direct-fire guns (AT/AA) fire precision missions only: an enemy
    /// exactly on the aim hex → precise registration; an empty aim hex →
    /// NO registration at all (no zone saturation, no self-splash).
    #[test]
    fn direct_gun_fire_mission_is_precise_or_absent() {
        let mut game = GameController::new(Side::Attacker, CombatTactic::Default, 7);
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(8, 8, Terrain::Plains)));
        state.units = vec![
            BattalionUnit::new(1, "AT", UnitType::AntiTankBrigade, Side::Defender, HexCoord::new(3, 3)),
            BattalionUnit::new(2, "T", UnitType::Infantry, Side::Attacker, HexCoord::new(4, 3)),
        ];
        let loc = Locale::from_text(tactical_locale::Language::English, "", "");

        apply_ai_action(
            &AiAction::FireSupport {
                attacker_id: 1,
                target_hex: HexCoord::new(4, 3),
            },
            Side::Defender,
            &mut game,
            &mut state,
            &loc,
            false,
        );
        match state.attack_orders.iter().find(|o| o.attacker == 1).map(|o| &o.target) {
            Some(AttackTarget::FireMission { precise, .. }) => {
                assert!(precise, "an AT mission at an occupied hex is precise")
            }
            other => panic!("AT mission must register precise: {other:?}"),
        }

        state.attack_orders.clear();
        apply_ai_action(
            &AiAction::FireSupport {
                attacker_id: 1,
                target_hex: HexCoord::new(5, 5),
            },
            Side::Defender,
            &mut game,
            &mut state,
            &loc,
            false,
        );
        assert!(
            state.attack_orders.iter().all(|o| o.attacker != 1),
            "AT guns never register a zone mission: {:?}",
            state.attack_orders
        );
    }

    /// Direct guns (AT/AA) answer a right-click with a precise direct
    /// strike anywhere in the envelope — point-blank included, never an
    /// assault, never a zone mission (the area-fire button never exists
    /// for them in the first place).
    #[test]
    fn direct_gun_right_click_is_direct_fire_at_any_envelope_range() {
        let mut game = GameController::new(Side::Attacker, CombatTactic::Default, 7);
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(8, 8, Terrain::Plains)));
        let mut at =
            BattalionUnit::new(1, "AT", UnitType::AntiTankBrigade, Side::Attacker, HexCoord::new(3, 3));
        at.is_emplaced = true; // towed gun ready to fire
        state.units = vec![
            at,
            BattalionUnit::new(2, "E1", UnitType::Infantry, Side::Defender, HexCoord::new(4, 3)),
            BattalionUnit::new(3, "E2", UnitType::Infantry, Side::Defender, HexCoord::new(3, 5)),
        ];
        let loc = Locale::from_text(tactical_locale::Language::English, "", "");
        let mut fx = crate::fx::FxQueue::default();
        state.selected_unit = Some(1);

        // Point-blank (dist 1): a direct strike, NOT an assault.
        let sel = state.unit_by_id(1).unwrap().clone();
        attack_target(
            &mut game, &mut state, &sel, HexCoord::new(4, 3), 2, &mut fx, &loc,
        );
        match state.attack_orders.iter().find(|o| o.attacker == 1).map(|o| &o.target) {
            Some(AttackTarget::DirectFire(t)) => assert_eq!(*t, 2, "point-blank direct fire"),
            other => panic!("point-blank must be a direct strike: {other:?}"),
        }

        // Envelope edge (dist 2): same direct strike.
        state.attack_orders.clear();
        let sel = state.unit_by_id(1).unwrap().clone();
        attack_target(
            &mut game, &mut state, &sel, HexCoord::new(3, 5), 3, &mut fx, &loc,
        );
        match state.attack_orders.iter().find(|o| o.attacker == 1).map(|o| &o.target) {
            Some(AttackTarget::DirectFire(t)) => assert_eq!(*t, 3, "envelope-edge direct fire"),
            other => panic!("in-envelope must be a direct strike: {other:?}"),
        }

        // Out of envelope: nothing registered.
        state.attack_orders.clear();
        let sel = state.unit_by_id(1).unwrap().clone();
        attack_target(
            &mut game, &mut state, &sel, HexCoord::new(3, 6), 3, &mut fx, &loc,
        );
        assert!(
            state.attack_orders.iter().all(|o| o.attacker != 1),
            "out of envelope registers nothing"
        );
    }

    /// An AI "no action" turn doubles as the Hold stance (an idle unit
    /// hunkers for the defense bonus); assaulting clears it again.
    #[test]
    fn ai_hold_sets_and_assault_clears_the_stance() {
        let mut game = GameController::new(Side::Attacker, CombatTactic::Default, 7);
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(8, 8, Terrain::Plains)));
        state.units = vec![
            BattalionUnit::new(1, "E1", UnitType::Infantry, Side::Defender, HexCoord::new(3, 3)),
            BattalionUnit::new(2, "E2", UnitType::Infantry, Side::Defender, HexCoord::new(4, 4)),
        ];
        let loc = Locale::from_text(tactical_locale::Language::English, "", "");

        apply_ai_action(
            &AiAction::Hold { unit_id: 1 },
            Side::Defender,
            &mut game,
            &mut state,
            &loc,
            false,
        );
        assert!(state.units[0].is_holding, "idle AI unit hunkers");

        apply_ai_action(
            &AiAction::Assault {
                attacker_id: 1,
                target_id: 2,
            },
            Side::Defender,
            &mut game,
            &mut state,
            &loc,
            false,
        );
        assert!(!state.units[0].is_holding, "attacking drops the stance");
    }

    /// Combat facing: registering an assault turns the attacker toward the
    /// target and the target back toward its attacker.
    #[test]
    fn assault_faces_attacker_and_target() {
        let mut game = GameController::new(Side::Attacker, CombatTactic::Default, 7);
        let mut state = TacticalState::default();
        state.grid = Some(Arc::new(HexGrid::new(8, 8, Terrain::Plains)));
        state.units = vec![
            BattalionUnit::new(1, "E1", UnitType::Infantry, Side::Defender, HexCoord::new(3, 3)),
            BattalionUnit::new(2, "E2", UnitType::Infantry, Side::Defender, HexCoord::new(4, 4)),
        ];
        let loc = Locale::from_text(tactical_locale::Language::English, "", "");

        apply_ai_action(
            &AiAction::Assault {
                attacker_id: 1,
                target_id: 2,
            },
            Side::Defender,
            &mut game,
            &mut state,
            &loc,
            false,
        );
        let yaw = |from: HexCoord, to: HexCoord| {
            let (ax, az) = from.to_world(crate::board::HEX_SIZE);
            let (bx, bz) = to.to_world(crate::board::HEX_SIZE);
            -(bz - az).atan2(bx - ax)
        };
        let want12 = yaw(HexCoord::new(3, 3), HexCoord::new(4, 4));
        let want21 = yaw(HexCoord::new(4, 4), HexCoord::new(3, 3));
        assert_eq!(state.unit_facing.get(&1), Some(&want12), "attacker faces target");
        assert_eq!(state.unit_facing.get(&2), Some(&want21), "target faces back");
    }
}
