//! `--autoplay` battle driver (agent automation): plays the standalone
//! battle hands-free through the PRODUCTION command path — auto-deploy,
//! issue a Seize order per division on the first flag (Advance when the
//! battle has no flags), End Turn, click through battle reports / sync
//! prompts, Sync at the hour boundary, Apply & Exit at the end. Used to
//! reproduce in-window hangs (e.g. the ~turn-12 freeze) without GUI input.
//!
//!   forward-command --battle file=1939_warsaw --autoplay
//!
//! Pair with FC_PERF=1: the once-a-second FPS line stopping IS the hang
//! signal (the last game-log lines before it name the phase it died in).

use bevy::prelude::*;
use tactical3d_render::camera::RtsCamera;
use tactical3d_render::game::{DivOrderPick, GameController, PendingCommands, PlayerCommand};
use tactical3d_render::state::{BattleTour, TacticalState, UiWindows};
use tactical_sync::BattlePhase;

/// Autoplay bookkeeping: what still has to be done this battle.
#[derive(Resource, Default)]
pub struct Autoplay {
    /// AutoDeploy already pushed (deployment runs once).
    deployed: bool,
    /// BeginBattle already pushed.
    begun: bool,
    /// Pace commands at ~5 Hz so the world can advance between them.
    timer: f32,
    /// FC_DRAGSYNC=1: camera-drag simulation around the sync boundary —
    /// 0 = idle, 1 = pre-Sync orbit drag in progress, 2 = post-Sync drag.
    /// (Freeze hunt: the battle window was observed to die at 6-multiple
    /// turns while dragging the map at the sync boundary.)
    drag_phase: u8,
    /// Frame counter inside the current drag phase.
    drag_frame: u32,
}

pub struct AutoplayPlugin;

impl Plugin for AutoplayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Autoplay>().add_systems(Update, drive);
        // FC_AUTOSHOT=1: register the art-direction layer (see AutoShot).
        if std::env::var_os("FC_AUTOSHOT").is_some() {
            app.insert_resource(AutoShot::default());
            app.add_systems(Update, drive_shots.after(drive));
        }
        // FC_TABLEAU=1: register the staged-assault director (see Tableau).
        if std::env::var_os("FC_TABLEAU").is_some() {
            app.insert_resource(Tableau::default());
            app.add_systems(Update, drive_tableau.after(drive));
        }
    }
}

/// FC_DRAGSYNC=1: simulate a camera drag (orbit) through the sync boundary
/// — drive the RtsCamera component directly for ~40 frames before and after
/// clicking Sync (the same downstream load as a manual map drag: transform
/// recompute, picking rays, board render). Returns true while a simulated
/// drag is in progress (the normal driver waits).
fn drag_sim(auto: &mut Autoplay, phase: BattlePhase, q_cam: &mut Query<&mut RtsCamera>) -> bool {
    if std::env::var_os("FC_DRAGSYNC").is_none() {
        return false;
    }
    // Arm at the sync boundary: start the pre-Sync drag.
    if auto.drag_phase == 0 && phase == BattlePhase::ReadyToSync {
        auto.drag_phase = 1;
        auto.drag_frame = 0;
    }
    // After the sync completed (phase flipped back to TacticalActive), run
    // the post-Sync drag once.
    if auto.drag_phase == 2 && phase == BattlePhase::TacticalActive {
        auto.drag_phase = 3;
        auto.drag_frame = 0;
    }
    let active = matches!(auto.drag_phase, 1 | 3);
    if !active {
        return false;
    }
    auto.drag_frame += 1;
    if let Ok(mut cam) = q_cam.get_single_mut() {
        cam.yaw += 0.03;
        cam.pitch = (cam.pitch - 0.004).clamp(-1.45, -0.25);
    }
    if auto.drag_frame >= 40 {
        // Phase 1 → 2 (drag done, Sync may now be clicked); 3 → 4 (done).
        auto.drag_phase += 1;
    }
    true
}

/// One paced step per tick: reports → sync prompt → deployment → orders /
/// End Turn → sync / battle end. Pushes at most one command per tick so a
/// rejected command cannot spin (the next tick re-evaluates fresh state).
fn drive(
    mut auto: ResMut<Autoplay>,
    game: Option<Res<GameController>>,
    state: Option<Res<TacticalState>>,
    mut tour: ResMut<BattleTour>,
    mut ui_win: ResMut<UiWindows>,
    mut pending: ResMut<PendingCommands>,
    mut q_cam: Query<&mut RtsCamera>,
    time: Res<Time>,
    shot: Option<Res<AutoShot>>,
    tab: Option<Res<Tableau>>,
) {
    let Some(game) = game else { return };
    let Some(state) = state else { return };
    // The sync-boundary drag simulation runs unthrottled (per-frame cursor
    // motion); while a drag is in flight the normal driver waits.
    if drag_sim(&mut auto, game.session.phase, &mut q_cam) {
        return;
    }
    // FC_DRAGSYNC: hold the Sync click until the pre-Sync drag finished.
    if auto.drag_phase == 1 {
        return;
    }
    auto.timer -= time.delta_secs();
    if auto.timer > 0.0 {
        return;
    }
    auto.timer = 0.2;

    // Battle-report tour: click [Continue] — only once the report is
    // actually showing (the camera glide must land first).
    if tour.active && tour.index < tour.queue.len() && tour.focused == Some(tour.index) {
        tour.index += 1;
        tour.focused = None;
        return;
    }
    // Sync-completion prompt: click [Continue] (never End Tactic — the
    // autoplay rides the battle to its natural end).
    if ui_win.sync_prompt {
        ui_win.sync_prompt = false;
        return;
    }
    if ui_win.confirm_end {
        ui_win.confirm_end = false;
        return;
    }
    if !pending.0.is_empty() {
        return; // one command in flight — let it land first
    }

    match game.session.phase {
        BattlePhase::Deployment => {
            if !auto.deployed {
                auto.deployed = true;
                pending.0.push(PlayerCommand::AutoDeploy);
            } else if !auto.begun {
                auto.begun = true;
                pending.0.push(PlayerCommand::BeginBattle);
            }
        }
        BattlePhase::TacticalActive => {
            if game.session.current_side != state.player_side {
                return; // enemy AI turn — watch it play out
            }
            // FC_TABLEAU: the staged-assault director owns the board while
            // staging (div orders cancelled, End Turn held).
            if tab.as_ref().is_some_and(|t| t.staging) {
                return;
            }
            // Keep every division under a standing order (re-issuing as
            // orders complete / targets appear — mirrors the manual
            // 占领/推进/歼敌 mix of a human playthrough).
            // FC_AUTOPLAY=seize (default): all divisions Seize the first
            // flag anchor (Advance when the battle has no flags).
            // FC_AUTOPLAY=mix: first division Seizes, second Advances, the
            // rest Engage the nearest VISIBLE enemy (retried each turn as
            // fog lifts / targets die).
            let mix = std::env::var("FC_AUTOPLAY").as_deref() == Ok("mix");
            let flag = game
                .session
                .flags()
                .and_then(|f| f.flags.first())
                .map(|f| f.anchor);
            let mut divisions: Vec<String> = state
                .units
                .iter()
                // Autoplay plays the PLAYER's divisions (DESIGN.md §7.5) —
                // allied contingents plan autonomously (their autonomy is
                // precisely what an allied-AI smoke run exercises).
                .filter(|u| game.commands(u) && u.is_combat_effective())
                .map(|u| u.division.clone())
                .collect();
            divisions.sort();
            divisions.dedup();
            let mut issued = false;
            for (i, division) in divisions.into_iter().enumerate() {
                if game.div_orders.contains_key(&division) {
                    continue;
                }
                let pick = if mix && i >= 2 {
                    state
                        .units
                        .iter()
                        .filter(|e| {
                            e.side != state.player_side
                                && e.is_combat_effective()
                                && state.fog_state(e.position)
                                    == tactical_core::fog::VisibilityState::Visible
                        })
                        .min_by_key(|e| e.position.q + e.position.r)
                        .map(|e| DivOrderPick::Engage {
                            unit: e.id,
                            hex: e.position,
                        })
                } else if mix && i == 1 {
                    Some(DivOrderPick::Advance)
                } else {
                    flag.map(DivOrderPick::Seize)
                        .or(Some(DivOrderPick::Advance))
                };
                if let Some(pick) = pick {
                    pending.0.push(PlayerCommand::DivOrder { division, pick });
                    issued = true;
                }
            }
            if issued {
                return;
            }
            // Dev aid for art verification: FC_HOLD_BEFORE_ENDTURN=1
            // parks the driver once any attack order is registered, holding
            // the player turn so the pre-order lanes stay on screen for a
            // capture. FC_HOLD_AREA=1 narrows the hold to turns with an
            // AREA (non-precise) fire mission. Never set in normal runs.
            let hold = match std::env::var("FC_HOLD_AREA") {
                Ok(_) => state.attack_orders.iter().any(|o| {
                    matches!(
                        o.target,
                        tactical_combat::AttackTarget::FireMission { precise: false, .. }
                    )
                }),
                Err(_) => {
                    std::env::var_os("FC_HOLD_BEFORE_ENDTURN").is_some()
                        && !state.attack_orders.is_empty()
                }
            };
            if hold {
                return;
            }
            // FC_AUTOSHOT: hold End Turn while the art driver still wants a
            // still of this turn's pre-order lanes (released by drive_shots).
            if let Some(s) = shot.as_ref() {
                if s.holds_player_turn(game.session.turn_number) {
                    return;
                }
            }
            pending.0.push(PlayerCommand::EndTurn);
        }
        BattlePhase::ReadyToSync => pending.0.push(PlayerCommand::Sync),
        BattlePhase::ReadyToEnd => pending.0.push(PlayerCommand::ApplyAndExit),
        // The result screen sits forever without a human to click Exit
        // (exit_when_ended is live-mode only) — close the app ourselves.
        BattlePhase::Ended => pending.0.push(PlayerCommand::ExitGame),
        _ => {}
    }
}

// ---------------------------------------------------------------- FC_AUTOSHOT
/// FC_AUTOSHOT=1: art-direction layer on top of the autoplay driver. Once
/// per player turn (while the pre-order lanes are up — End Turn is held via
/// `holds_player_turn`) and once per enemy turn, the camera glides to the
/// hottest engagement cluster at medium range and a screenshot lands in
/// previews/thumbnails/autorun/. Exists to capture REAL combat stills for
/// store-page art. Never set in normal runs.
const MAX_SHOTS: u32 = 14;

#[derive(Resource)]
pub struct AutoShot {
    /// Turn of the last player-turn still (u32::MAX = none yet).
    last_player_turn: u32,
    /// Turn of the last enemy-turn still.
    last_enemy_turn: u32,
    /// Camera aimed, settle countdown running before the capture.
    pending: bool,
    settle: f32,
    /// Seconds spent in the current player turn without lanes appearing.
    hold_time: f32,
    shots: u32,
}

impl Default for AutoShot {
    fn default() -> Self {
        Self {
            last_player_turn: u32::MAX,
            last_enemy_turn: u32::MAX,
            pending: false,
            settle: 0.0,
            hold_time: 0.0,
            shots: 0,
        }
    }
}

impl AutoShot {
    /// Autoplay holds End Turn until this turn's order-lane still is taken.
    fn holds_player_turn(&self, turn: u32) -> bool {
        self.shots < MAX_SHOTS && (self.pending || self.last_player_turn != turn)
    }
}

/// The engagement hotspot: the enemy hex with the most opposing weight
/// within 5 hexes (visible enemies count double — hidden ones don't
/// render). Falls back to the first player unit.
fn hotspot(state: &TacticalState) -> Vec3 {
    let player = state.player_side;
    let mut best: Option<(i32, tactical_core::hex::HexCoord)> = None;
    for e in state
        .units
        .iter()
        .filter(|u| u.side != player && u.is_combat_effective())
    {
        let mut score = 0i32;
        for u in state.units.iter().filter(|u| u.is_combat_effective()) {
            let d = u.position.distance(e.position);
            if u.side == player {
                if d <= 5 {
                    score += (6 - d) as i32 * 20;
                }
            } else if d <= 4 {
                score += 4; // friendly cluster around the candidate
            }
        }
        if state.fog_state(e.position) == tactical_core::fog::VisibilityState::Visible {
            score *= 2;
        }
        if score > 0 && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, e.position));
        }
    }
    let h = best.map(|(_, h)| h).or_else(|| {
        state
            .units
            .iter()
            .find(|u| u.side == player && u.is_combat_effective())
            .map(|u| u.position)
    });
    match h {
        Some(h) => {
            let (x, z) = h.to_world(1.0);
            Vec3::new(x, 0.0, z)
        }
        None => Vec3::ZERO,
    }
}

/// Medium framing: engaged pieces + order lanes readable, not zoomed out.
fn aim(state: &TacticalState, q_cam: &mut Query<&mut RtsCamera>) {
    let p = hotspot(state);
    for mut cam in q_cam.iter_mut() {
        cam.target = p;
        cam.distance = 34.0;
        cam.pitch = -0.85;
    }
}

fn drive_shots(
    mut shot: ResMut<AutoShot>,
    game: Option<Res<GameController>>,
    state: Option<Res<TacticalState>>,
    mut commands: Commands,
    mut q_cam: Query<&mut RtsCamera>,
    time: Res<Time>,
) {
    use bevy::render::view::window::screenshot::{save_to_disk, Screenshot};
    let (Some(game), Some(state)) = (game, state) else {
        return;
    };
    if shot.shots >= MAX_SHOTS || game.session.phase != BattlePhase::TacticalActive {
        return;
    }
    let turn = game.session.turn_number;
    let player_turn = game.session.current_side == state.player_side;
    if shot.pending {
        shot.settle -= time.delta_secs();
        if shot.settle <= 0.0 {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("previews")
                .join("thumbnails")
                .join("autorun");
            let _ = std::fs::create_dir_all(&dir);
            let tag = if player_turn { "orders" } else { "enemy" };
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(dir.join(format!("auto_t{turn:02}_{tag}.png"))));
            shot.shots += 1;
            shot.pending = false;
        }
        return;
    }
    if player_turn {
        shot.hold_time += time.delta_secs();
        let lanes_up = !state.attack_orders.is_empty()
            || state
                .units
                .iter()
                .any(|u| u.side == state.player_side && u.move_order.is_some());
        if shot.last_player_turn != turn && (lanes_up || shot.hold_time > 6.0) {
            aim(&state, &mut q_cam);
            shot.last_player_turn = turn;
            shot.pending = true;
            shot.settle = 0.9;
            shot.hold_time = 0.0;
        }
    } else {
        shot.hold_time = 0.0;
        if shot.last_enemy_turn != turn {
            aim(&state, &mut q_cam);
            shot.last_enemy_turn = turn;
            shot.pending = true;
            shot.settle = 1.6; // let the AI action unfold before the still
        }
    }
}



// ---------------------------------------------------------------- FC_TABLEAU
/// FC_TABLEAU=1: staged-assault director layered on the autoplay. Real
/// battles contact too slowly for art shoots (28 marching turns, zero
/// assaults), so the director MANUFACTURES the textbook tableau at turn 2:
/// teleports an armor + a line battalion next to one isolated enemy, parks
/// a supporting gun in range behind them, lifts the fog (render-only F8
/// flag), wipes every division order (no lanes, no Engage rings), registers
/// the two assault orders + a precise fire mission on the target, frames
/// gun → assault pair → target in one medium shot, then releases the turn
/// for an impact still. Disposable photo-shoot battle; store-page art only.
#[derive(Resource)]
pub struct Tableau {
    /// Owns the board: autoplay holds div orders + End Turn (drive() gate).
    staging: bool,
    /// 0 arrange, 1 formation settling, 2 lanes shot (waiting out the
    /// screenshot frame), 3 End Turn pushed (fire phase), 4 impact shot,
    /// 5 done.
    phase: u8,
    settle: f32,
    picked: Option<Picked>,
}

/// The staged cast: target id + its hex, assaulter (id, teleport dest)
/// armor-first, and the supporting gun (id, teleport dest).
struct Picked {
    enemy: usize,
    enemy_hex: tactical_core::hex::HexCoord,
    assaulters: [(usize, tactical_core::hex::HexCoord); 2],
    gun: (usize, tactical_core::hex::HexCoord),
}

impl Default for Tableau {
    fn default() -> Self {
        Self {
            staging: false,
            phase: 0,
            settle: 0.0,
            picked: None,
        }
    }
}

/// Cast the tableau: the isolated enemy nearest to a player armor, the
/// nearest armor + line battalion as the assault pair, the nearest tube
/// gun (rockets as fallback) as the support. Teleport destinations are
/// passable in-province unoccupied hexes.
fn find_stage(state: &TacticalState) -> Option<Picked> {
    use tactical_core::unit::UnitType;
    let player = state.player_side;
    let grid = state.grid.as_ref()?;
    let normal_cell = |h: tactical_core::hex::HexCoord| {
        grid.cell(h)
            .is_some_and(|c| c.is_passable && !c.out_of_bounds)
    };
    let usable = |h: tactical_core::hex::HexCoord| normal_cell(h) && state.unit_at(h).is_none();

    let mut enemy: Option<(i32, usize)> = None;
    for e in state
        .units
        .iter()
        .filter(|u| u.side != player && u.is_combat_effective())
    {
        if !normal_cell(e.position) {
            continue;
        }
        let near_armor = state
            .units
            .iter()
            .filter(|u| u.side == player && u.unit_type.is_armor() && u.is_combat_effective())
            .map(|u| u.position.distance(e.position))
            .min()
            .unwrap_or(99);
        // Fewer pieces around the target reads cleaner in the frame.
        let clutter = state
            .units
            .iter()
            .filter(|u| u.id != e.id && u.position.distance(e.position) <= 2)
            .count() as i32;
        let score = near_armor * 10 + clutter;
        if enemy.is_none_or(|(s, _)| score < s) {
            enemy = Some((score, e.id));
        }
    }
    let (_, enemy) = enemy?;
    let enemy_hex = state.units.iter().find(|u| u.id == enemy)?.position;

    let free_adj: Vec<_> = enemy_hex
        .neighbors()
        .into_iter()
        .filter(|&h| usable(h))
        .collect();
    if free_adj.len() < 2 {
        return None;
    }

    let armor = state
        .units
        .iter()
        .filter(|u| u.side == player && u.is_combat_effective() && u.unit_type.is_armor())
        .min_by_key(|u| u.position.distance(enemy_hex))?;
    let line = state
        .units
        .iter()
        .filter(|u| {
            u.side == player
                && u.is_combat_effective()
                && u.id != armor.id
                && !u.unit_type.is_armor()
                && !u.unit_type.is_support_company()
                && !matches!(
                    u.unit_type,
                    UnitType::Headquarters
                        | UnitType::ArtilleryBrigade
                        | UnitType::RocketArtillery
                        | UnitType::MotRocketArtillery
                        | UnitType::AntiTankBrigade
                        | UnitType::AntiAirBrigade
                )
        })
        .min_by_key(|u| u.position.distance(enemy_hex))?;
    let gun = state
        .units
        .iter()
        .filter(|u| {
            u.side == player
                && u.is_combat_effective()
                && u.id != armor.id
                && u.id != line.id
                && matches!(
                    u.unit_type,
                    UnitType::ArtilleryBrigade
                        | UnitType::RocketArtillery
                        | UnitType::MotRocketArtillery
                )
        })
        // Tube artillery draws the clean precise arc; nearest gun teleports
        // the shortest distance.
        .min_by_key(|u| {
            (
                !matches!(u.unit_type, UnitType::ArtilleryBrigade),
                u.position.distance(enemy_hex),
            )
        })?;
    let gun_range = gun.attack_range;
    let mut gun_dest: Option<(i32, tactical_core::hex::HexCoord)> = None;
    for h in grid.iter_coords() {
        let d = h.distance(enemy_hex);
        if d < 3 || d > gun_range || !usable(h) || free_adj.iter().take(2).any(|&a| a == h) {
            continue;
        }
        // Closest to the target keeps the frame compact.
        if gun_dest.is_none_or(|(s, _)| d < s) {
            gun_dest = Some((d, h));
        }
    }
    let (_, gun_hex) = gun_dest?;
    Some(Picked {
        enemy,
        enemy_hex,
        assaulters: [(armor.id, free_adj[0]), (line.id, free_adj[1])],
        gun: (gun.id, gun_hex),
    })
}

fn world_of(h: tactical_core::hex::HexCoord) -> Vec3 {
    let (x, z) = h.to_world(1.0);
    Vec3::new(x, 0.0, z)
}

fn tableau_shot(commands: &mut Commands, name: &str) {
    use bevy::render::view::window::screenshot::{save_to_disk, Screenshot};
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("previews")
        .join("thumbnails")
        .join("autorun");
    let _ = std::fs::create_dir_all(&dir);
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(dir.join(name)));
}

fn drive_tableau(
    mut tab: ResMut<Tableau>,
    game: Option<Res<GameController>>,
    state: Option<ResMut<TacticalState>>,
    mut pending: ResMut<PendingCommands>,
    mut commands: Commands,
    mut q_cam: Query<&mut RtsCamera>,
    time: Res<Time>,
) {
    let (Some(game), Some(mut state)) = (game, state) else {
        return;
    };
    if tab.phase >= 5 || game.session.phase != BattlePhase::TacticalActive {
        return;
    }
    let player_turn = game.session.current_side == state.player_side;
    match tab.phase {
        0 => {
            if !player_turn || game.session.turn_number < 2 {
                return;
            }
            let Some(p) = find_stage(&state) else {
                return;
            };
            info!(
                "TABLEAU staged: target=#{} pair=[#{}, #{}] gun=#{} at turn {}",
                p.enemy, p.assaulters[0].0, p.assaulters[1].0, p.gun.0, game.session.turn_number
            );
            // Teleport the cast into formation; reveal the board (render-only
            // F8 flag) so the target piece shows in the still.
            for (id, dest) in p.assaulters.iter().chain(std::iter::once(&p.gun)) {
                if let Some(u) = state.units.iter_mut().find(|u| u.id == *id) {
                    u.position = *dest;
                    u.undeployed = false;
                }
            }
            // Towed guns fire from the emplaced model only — the support
            // piece must look stood-to in the still.
            if let Some(u) = state.units.iter_mut().find(|u| u.id == p.gun.0) {
                u.is_emplaced = true;
            }
            state.units_dirty = true;
            state.debug_no_fog = true;
            tab.staging = true;
            // Wipe every division order: their lanes and Engage rings
            // disappear from the render before the still.
            let divisions: Vec<String> = game.div_orders.keys().cloned().collect();
            for d in divisions {
                pending.0.push(PlayerCommand::CancelDivOrder(d));
            }
            tab.picked = Some(p);
            tab.phase = 1;
            tab.settle = 2.2; // let the unit visuals land on their new hexes
        }
        1 => {
            tab.settle -= time.delta_secs();
            if tab.settle > 0.0 {
                return;
            }
            let Some(p) = tab.picked.clone().map(|p| (p.enemy, p.enemy_hex, p.assaulters, p.gun))
            else {
                tab.staging = false;
                tab.phase = 5;
                return;
            };
            let (enemy, enemy_hex, assaulters, gun) = p;
            // Unit-level orders straight into the standing-order list — the
            // same data the UI's register_order produces (assault lanes +
            // the ballistic fire arc render from attack_orders directly).
            use tactical_combat::{AttackOrder, AttackTarget};
            if !state.units.iter().any(|u| u.id == enemy) {
                tab.staging = false; // target died while staging — abandon
                tab.phase = 5;
                return;
            }
            // Clear EVERY standing attack order (other battalions' fire
            // missions paint their own target markers) — the still shows
            // exactly the staged three.
            state.attack_orders.clear();
            for (attacker, target) in assaulters
                .iter()
                .map(|&(id, _)| (id, AttackTarget::Assault(enemy)))
                .chain(std::iter::once((
                    gun.0,
                    AttackTarget::FireMission {
                        hex: enemy_hex,
                        precise: true,
                    },
                )))
            {
                state.attack_orders.retain(|o| o.attacker != attacker);
                state.attack_orders.push(AttackOrder { attacker, target });
                if let Some(u) = state.units.iter_mut().find(|u| u.id == attacker) {
                    u.move_order = None;
                    u.manual_override = true;
                }
            }
            state.orders_dirty = true;
            // Frame gun → target with the assault pair in between.
            let g = world_of(gun.1);
            let tgt = world_of(enemy_hex);
            let span = g.distance(tgt);
            for mut cam in q_cam.iter_mut() {
                cam.target = g.lerp(tgt, 0.55);
                cam.distance = (span * 0.9 + 9.0).clamp(13.0, 26.0);
                cam.pitch = -0.85;
            }
            tab.phase = 2;
            tab.settle = 0.9;
        }
        2 => {
            tab.settle -= time.delta_secs();
            if tab.settle > 0.0 {
                return;
            }
            tableau_shot(&mut commands, "tableau_orders.png");
            // The screenshot lands ~0.3s later on a rendered frame — End
            // Turn must clear that window by a wide margin, or the still
            // catches the post-resolution board (lanes consumed, pieces
            // mid-retreat). Diag the cast's positions for the log diff.
            if let Some(p) = tab.picked.as_ref() {
                let pos = |id: usize| state.units.iter().find(|u| u.id == id).map(|u| u.position);
                info!(
                    "TABLEAU orders-shot: pair at {:?} gun at {:?} target {:?}",
                    p.assaulters.map(|(id, _)| pos(id)),
                    pos(p.gun.0),
                    pos(p.enemy)
                );
            }
            tab.phase = 3;
            tab.settle = 1.0;
        }
        3 => {
            tab.settle -= time.delta_secs();
            if tab.settle > 0.0 {
                return;
            }
            // End the turn OURSELVES and keep staging: the fire phase runs
            // on the game systems while the autoplay gate stays shut, so no
            // division order can re-arm inside the impact window.
            pending.0.push(PlayerCommand::EndTurn);
            tab.phase = 4;
            tab.settle = 1.25;
        }
        4 => {
            tab.settle -= time.delta_secs();
            if tab.settle > 0.0 {
                return;
            }
            tableau_shot(&mut commands, "tableau_impact.png");
            tab.staging = false; // hand the battle back to the autoplay
            tab.phase = 5;
        }
        _ => {}
    }
}

impl Clone for Picked {
    fn clone(&self) -> Self {
        Self {
            enemy: self.enemy,
            enemy_hex: self.enemy_hex,
            assaulters: self.assaulters,
            gun: self.gun,
        }
    }
}
