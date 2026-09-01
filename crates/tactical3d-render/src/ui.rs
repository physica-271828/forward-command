//! egui panels (DESIGN §9): right command panel (fixed), info panel, unit
//! list, minimap, tactic card — plus world-space unit labels (org/str bars).

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};
use tactical_ai::CombatTactic;
use tactical_core::damage::{AccuracyClass, HitBreakdown, LinearFactor};
use tactical_core::encirclement::{detect_encirclement, EncirclementLevel};
use tactical_core::unit::{Side, UnitState};
use tactical_core::{compute_command_links, in_command};
use tactical_sync::BattlePhase;

use crate::board::hex_world;
use crate::game::{DivOrderPick, GameController, PendingCommands, PlayerCommand};
use crate::icons::{IconId, IconSet};
use crate::locale::LocaleRes;
use crate::models::SideColors;
use crate::state::{
    BattleTour, CommandMode, DesyncAction, DesyncAlert, DetailWindow, DivPickKind,
    EngagementDetailView, FlashNotice, NoticeBar, RecallKind, SyncStall, SyncStallAction,
    TacticalState, UiWindows,
};

/// Theme color as egui Color32 (SideColors are 0..1 floats).
fn color32(c: [f32; 4], alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        (c[0].clamp(0.0, 1.0) * 255.0) as u8,
        (c[1].clamp(0.0, 1.0) * 255.0) as u8,
        (c[2].clamp(0.0, 1.0) * 255.0) as u8,
        alpha,
    )
}

pub struct TacticalUiPlugin;

/// The minimap's terrain layer is static for the whole battle, but
/// it was re-tessellated as one egui rect PER HEX every frame (15k rects on a
/// Sedan-sized map, 260k at the 512 cap) — the UI's biggest per-frame cost.
/// It is now baked ONCE into an egui texture and drawn as a single image;
/// only the unit blips stay live painter shapes.
#[derive(Resource, Default)]
pub struct MinimapCache {
    texture: Option<egui::TextureHandle>,
}

impl Plugin for TacticalUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiToggles>()
            .init_resource::<UiWindows>()
            .init_resource::<NoticeBar>()
            .init_resource::<MinimapCache>()
            .init_resource::<crate::icons::IconSet>()
            // English default; the binary overrides with the settings.json
            // language via `insert_resource` after plugin build (§15).
            .init_resource::<crate::locale::LocaleRes>()
            .add_systems(
                Update,
                (
                    // One-shot egui setup first: CJK font chain + icon textures.
                    (
                        crate::fonts::install_ui_fonts_once,
                        crate::icons::init_icons_once,
                    ),
                    (
                        ui_pointer_guard,
                        // World-space text (VP/city name + damage floaters)
                        // paints onto the
                        // shared panel Background list and MUST run before
                        // draw_panels so the sidebar / notice bar / report
                        // modal cover it (insertion order within one
                        // PaintList is the z-order). The `.after(camera)`
                        // orders: every painter that projects
                        // world→screen must read the camera GlobalTransform
                        // the controller wrote THIS frame, or the overlays
                        // trail the 3D world by one frame on camera drags.
                        crate::fx::draw_vp_label.after(crate::camera::rts_camera_controller),
                        crate::fx::draw_floaters.after(crate::camera::rts_camera_controller),
                        draw_panels.after(crate::camera::rts_camera_controller),
                        draw_notice_bar,
                        draw_hover_card.after(crate::camera::rts_camera_controller),
                        draw_esc_menu,
                        draw_settings_window,
                        draw_sync_prompt,
                        draw_sync_stall,
                        draw_desync_alert,
                        draw_battle_report,
                        draw_engagement_detail,
                        draw_log_window,
                        draw_oob_window,
                        draw_unit_labels.after(crate::camera::rts_camera_controller),
                        draw_radial_menu,
                    )
                        .chain()
                        // With the render gate closed nothing is
                        // presented — skip the whole painter pass (the egui
                        // immediate-mode repaint is the biggest per-frame CPU
                        // cost, e.g. the minimap). No gate resource = run.
                        .run_if(crate::gate::gate_open),
                ),
            );
    }
}

/// The minimap is the only toggleable panel left — unit info moved
/// to hover cards, the unit list became the OOB window, the tactic card
/// merged into the right panel, the log is a standalone window.
#[derive(Resource)]
pub struct UiToggles {
    pub minimap: bool,
}

impl Default for UiToggles {
    fn default() -> Self {
        UiToggles { minimap: true }
    }
}

/// SidePanel ids whose rects block map input (see ui_pointer_guard). egui 0.29 top-level
/// panels paint on the Background ORDER, so the `layer_id_at` hit-test below
/// (which must exclude Background for the full-screen unit-label layer) is
/// blind to them — hit-test their persisted `PanelState` rects instead.
const BLOCKING_PANELS: &[&str] = &["command_panel"];

/// Track whether egui wants the pointer so the 3D world ignores it.
/// Pub: the render (camera/picking) and game-loop (map clicks) Update sets
/// are explicitly ordered AFTER this system so they
/// never read a one-frame-stale `pointer_over_ui`.
pub fn ui_pointer_guard(
    mut contexts: EguiContexts,
    mut state: ResMut<TacticalState>,
    ui_win: Res<UiWindows>,
    tour: Option<Res<BattleTour>>,
    stall: Option<Res<SyncStall>>,
    desync: Option<Res<DesyncAlert>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
) {
    // On window-close frames the egui context is already torn
    // down — ctx_mut() would panic; skip the frame instead.
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    // Hit-test the LIVE cursor against egui's area layers. Under
    // bevy_egui multipass the ctx input visible from Update lags one frame,
    // so wants_pointer_input()/is_pointer_over_area() still say "over the
    // map" on the frame the cursor first enters a panel — a click there fell
    // through to the map (non-sticky deselect), the panel re-laid-out and
    // egui dropped the click entirely (first End Turn click only deselected).
    // cursor_position() is live winit data; layer rects are last frame's,
    // which is correct for any stationary panel. Background-order layers must
    // be EXCLUDED: the unit-label painter's Background layer spans the whole
    // screen, and counting it pins pointer_over_ui=true and kills ALL map
    // input (is_pointer_over_area filters it the same way internally).
    let cursor = windows
        .get_single()
        .ok()
        .and_then(|w| w.cursor_position())
        .map(|p| egui::pos2(p.x, p.y));
    let over_area = cursor
        .and_then(|p| ctx.layer_id_at(p))
        .map(|layer| layer.order != egui::Order::Background)
        .unwrap_or(false);
    // The right command SidePanel is itself Background-order, so
    // the layer hit-test above never counts it — hovering the panel kept
    // picking the map beneath (hover highlight + terrain hover card popping
    // over the UI, panel clicks falling through). SidePanel persists a
    // PanelState with its rect every frame; test the cursor against those.
    let over_panel = cursor
        .map(|p| {
            BLOCKING_PANELS.iter().any(|id| {
                egui::containers::panel::PanelState::load(ctx, egui::Id::new(*id))
                    .map(|ps| ps.rect.contains(p))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    // Any modal surface (Esc menu, sync prompt, confirms)
    // freezes every map input while open.
    state.pointer_over_ui = over_area
        || over_panel
        || ctx.wants_pointer_input()
        || ctx.is_pointer_over_area()
        || ui_win.modal_open()
        // The sync-stall dialog is modal too — it lives outside
        // UiWindows (the bin-side injector owns its lifecycle), so the
        // guard reads the resource directly. The desync guard's dialog
        // is modal the same way.
        || stall.is_some()
        || desync.is_some()
        // The battle-report tour is modal too — camera pan/zoom and
        // picking stay frozen so the pinned report magnification cannot
        // be disturbed mid-playback.
        || tour.as_deref().map(|t| t.active).unwrap_or(false);
}

/// Pub for cross-plugin ordering: the game loop's `handle_ui_commands`
/// consumes the `PendingCommands` this system fills.
pub fn draw_panels(
    mut contexts: EguiContexts,
    mut pending: ResMut<PendingCommands>,
    game: Option<Res<GameController>>,
    state: Res<TacticalState>,
    colors: Res<SideColors>,
    mut toggles: ResMut<UiToggles>,
    mut ui_win: ResMut<UiWindows>,
    mut reset: ResMut<crate::camera::CameraResetReq>,
    loc: Res<LocaleRes>,
    icons: Res<IconSet>,
    mut minimap: ResMut<MinimapCache>,
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut themed: Local<bool>,
) {
    // On window-close frames the egui context is already torn
    // down — ctx_mut() would panic; skip the frame instead.
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    if !*themed {
        apply_theme(ctx);
        *themed = true;
    }
    let Some(game) = game else { return };

    // The right panel is static; on-demand surfaces (log / OOB /
    // hover cards / Esc menu) are separate systems further down the chain.
    draw_command_panel(
        ctx,
        &game,
        &state,
        &mut pending,
        &mut toggles,
        &mut ui_win,
        &mut reset,
        &loc,
        &icons,
    );
    if toggles.minimap {
        draw_minimap(
            ctx,
            &game,
            &state,
            &colors,
            &mut toggles,
            &mut minimap,
            &loc,
            q_cam.get_single().ok(),
        );
    }
}

/// HOI4-style palette — warm near-black panels, brass borders,
/// gold accents, parchment text (Paradox WWII UI).
const GOLD: egui::Color32 = egui::Color32::from_rgb(216, 178, 92);
const BRASS: egui::Color32 = egui::Color32::from_rgb(138, 122, 82);
const PARCHMENT: egui::Color32 = egui::Color32::from_rgb(232, 224, 204);
/// HOI4 convention: GREEN = organization, YELLOW = strength.
pub(crate) const ORG_GREEN: egui::Color32 = egui::Color32::from_rgb(90, 200, 90);
pub(crate) const STR_YELLOW: egui::Color32 = egui::Color32::from_rgb(240, 200, 60);
/// Warning red for the prominent impassable-terrain hover notice (§6.14).
pub(crate) const WARN_RED: egui::Color32 = egui::Color32::from_rgb(224, 82, 70);
/// Enemy counter name text — the HQ takes the brightest red (the
/// command unit is the highest-value read on the map), line units a slightly
/// muted warm orange-red so the two stay distinguishable within the side
/// (the player's side keeps white + gold HQ).
const ENEMY_RED: egui::Color32 = egui::Color32::from_rgb(255, 166, 92);
const ENEMY_HQ_RED: egui::Color32 = egui::Color32::from_rgb(255, 82, 70);

/// Minimap window geometry (the only floating window left, pinned
/// to the top-LEFT corner — the Enemy tactic card it used to mirror merged
/// into the right panel). fixed_size: default_size is only a hint — egui
/// then auto-sizes to content. 220×170 measured to fit the grid.
const TOP_WIN_W: f32 = 220.0;
const TOP_WIN_H: f32 = 170.0;
const TOP_WIN_MARGIN: f32 = 20.0;
const TOP_WIN_Y: f32 = 30.0;

/// True on egui's very first frame: no real window size yet, screen_rect is
/// a ~6666px placeholder. Windows with screen-computed default_pos must skip
/// this frame or the garbage position is stored and clamped forever.
fn egui_first_frame(ctx: &egui::Context) -> bool {
    ctx.screen_rect().width() > 5000.0
}

/// Panel frame for the fixed side/bottom panels (egui's own side-panel
/// frame draws no border at all, so theme colors alone are invisible).
fn hoi4_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(egui::Color32::from_rgba_unmultiplied(24, 23, 20, 242))
        .stroke(egui::Stroke::new(1.5_f32, BRASS))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .rounding(egui::Rounding::same(3.0))
}

/// Section header: bronze strip with gold text, HOI4 header-bar style.
fn hoi4_heading(ui: &mut egui::Ui, text: &str) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(48, 42, 31))
        .inner_margin(egui::Margin::symmetric(6.0, 3.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).strong().size(15.0).color(GOLD));
        });
}

/// Dark bronze "Paradox" theme (HOI4 pass) — near-black warm
/// panels, brass/gold borders, parchment text, bronze buttons.
fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = egui::Color32::from_rgba_unmultiplied(24, 23, 20, 242);
    v.window_fill = egui::Color32::from_rgba_unmultiplied(26, 25, 21, 246);
    v.window_stroke = egui::Stroke::new(1.5_f32, BRASS);
    v.window_rounding = egui::Rounding::same(3.0);
    v.override_text_color = Some(PARCHMENT);
    v.hyperlink_color = GOLD;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = egui::Color32::from_rgb(34, 31, 26);
    w.noninteractive.weak_bg_fill = egui::Color32::from_rgb(34, 31, 26);
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(70, 63, 48));
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, PARCHMENT);
    w.inactive.bg_fill = egui::Color32::from_rgb(48, 44, 34);
    w.inactive.weak_bg_fill = egui::Color32::from_rgb(48, 44, 34);
    w.inactive.bg_stroke = egui::Stroke::new(1.0_f32, BRASS);
    w.inactive.fg_stroke = egui::Stroke::new(1.0_f32, PARCHMENT);
    w.hovered.bg_fill = egui::Color32::from_rgb(72, 64, 46);
    w.hovered.weak_bg_fill = egui::Color32::from_rgb(72, 64, 46);
    w.hovered.bg_stroke = egui::Stroke::new(1.0_f32, GOLD);
    w.hovered.fg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(255, 250, 235));
    w.active.bg_fill = egui::Color32::from_rgb(96, 82, 52);
    w.active.weak_bg_fill = egui::Color32::from_rgb(96, 82, 52);
    w.active.bg_stroke = egui::Stroke::new(1.0_f32, GOLD);
    w.active.fg_stroke = egui::Stroke::new(1.5_f32, GOLD);
    w.open.bg_fill = egui::Color32::from_rgb(58, 52, 40);
    w.open.weak_bg_fill = egui::Color32::from_rgb(58, 52, 40);
    for w in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        w.rounding = egui::Rounding::same(2.0);
        w.expansion = 1.0;
    }
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(216, 178, 92, 80);
    v.selection.stroke = egui::Stroke::new(1.0_f32, GOLD);

    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);
    ctx.set_style(style);
}

// ---------------------------------------------------------------------------
// Right command panel (§9.2, not closable)
// ---------------------------------------------------------------------------

fn draw_command_panel(
    ctx: &egui::Context,
    game: &GameController,
    state: &TacticalState,
    pending: &mut PendingCommands,
    toggles: &mut UiToggles,
    ui_win: &mut UiWindows,
    reset: &mut crate::camera::CameraResetReq,
    loc: &LocaleRes,
    icons: &IconSet,
) {
    egui::SidePanel::right("command_panel")
        .resizable(false)
        .default_width(190.0)
        .frame(hoi4_frame())
        .show(ctx, |ui| {
            hoi4_heading(ui, loc.tr("app.title").as_ref());
            let turn = game.session.turn_number.to_string();
            let clock = game.session.battle_clock();
            ui.label(loc.trf(
                "battle.panel.turn_line",
                &[("turn", &turn), ("clock", &clock)],
            ));
            let hour = (game.session.strategic_hour + 1).to_string();
            ui.label(loc.trf(
                "battle.panel.hour_line",
                &[
                    ("hour", &hour),
                    ("side", loc.side_name(game.session.current_side).as_ref()),
                ],
            ));
            ui.label(loc.phase_name(game.session.phase).as_ref());
            if !game.location.is_empty() {
                ui.label(egui::RichText::new(&game.location).weak());
            }
            ui.separator();

            // §6.11 battle objective, above the enemy tactic: the
            // side-dependent primary goal (capture vs hold the key points)
            // plus the annihilation path that always ends a battle. A city
            // battle (single urban-cluster flag) names the city itself.
            hoi4_heading(ui, loc.tr("battle.panel.objective").as_ref());
            let is_city = game
                .session
                .flags()
                .map(|f| f.kind == tactical_core::FlagKind::City)
                .unwrap_or(false);
            let city_name = if is_city { game.location.as_str() } else { "" };
            let primary = if state.player_side == Side::Attacker {
                if city_name.is_empty() {
                    loc.tr("battle.panel.objective.capture").into_owned()
                } else {
                    loc.trf(
                        "battle.panel.objective.capture_city",
                        &[("city", city_name)],
                    )
                }
            } else if city_name.is_empty() {
                loc.tr("battle.panel.objective.protect").into_owned()
            } else {
                loc.trf(
                    "battle.panel.objective.protect_city",
                    &[("city", city_name)],
                )
            };
            ui.label(egui::RichText::new(primary).strong());
            let has_flags = game
                .session
                .flags()
                .map(|f| !f.flags.is_empty())
                .unwrap_or(false);
            let alt = if has_flags {
                loc.tr("battle.panel.objective.annihilation")
            } else {
                loc.tr("battle.panel.objective.annihilation_only")
            };
            ui.label(egui::RichText::new(alt.as_ref()).weak().small());
            ui.separator();

            // Enemy tactic, merged into the static panel (it used to be a
            // floating window).
            hoi4_heading(ui, loc.tr("battle.panel.enemy_tactic").as_ref());
            let t: CombatTactic = game.enemy_tactic;
            ui.label(egui::RichText::new(loc.tactic_name(t).as_ref()).strong());
            ui.label(egui::RichText::new(loc.tactic_desc(t).as_ref()).small());
            ui.label(
                egui::RichText::new(loc.trf(
                    "battle.panel.counter_hint",
                    &[("hint", loc.tactic_hint(t).as_ref())],
                ))
                .weak()
                .small(),
            );
            ui.separator();

            // §6.11: the flag-capture board — one bar per flag
            // zone (city = 1 urban flag, field = 3). Gold toward full,
            // red once a flag is falling (≥ 2/3). No flags → no board.
            if let Some(flags) = game.session.flags() {
                let cap = game.combat.params().flag_progress_cap.max(1);
                hoi4_heading(ui, loc.tr("battle.panel.flags").as_ref());
                for f in &flags.flags {
                    let ratio = (f.progress as f32 / cap as f32).clamp(0.0, 1.0);
                    let color = if ratio >= 2.0 / 3.0 {
                        egui::Color32::from_rgb(178, 34, 34) // falling
                    } else if ratio > 0.0 {
                        GOLD
                    } else {
                        egui::Color32::from_gray(110)
                    };
                    let hex_name = format!("({}, {})", f.anchor.q, f.anchor.r);
                    ui.horizontal(|ui| {
                        icons.icon(ui, IconId::Flag, 14.0);
                        ui.add(
                            egui::ProgressBar::new(ratio)
                                .fill(color)
                                .text(format!("{}/{}", f.progress, cap)),
                        )
                        .on_hover_text(loc.trf(
                            "battle.flag.tooltip",
                            &[("hex", &hex_name), ("cap", &cap.to_string())],
                        ));
                    });
                }
                if flags.collapsed {
                    ui.label(
                        egui::RichText::new(loc.tr("battle.flag.collapsed"))
                            .strong()
                            .color(egui::Color32::from_rgb(178, 34, 34)),
                    );
                }
                ui.separator();
            }

            // Fixed main-action slot: CONTENT follows the battle phase, the
            // POSITION never moves (static panel; the Sync state
            // doubles as the "sync needed" prompt).
            // Desync mode (guard, DESIGN.md §3.2/§8.2): a persistent
            // status marker in the main-action area — the Sync
            // affordance is dead for this battle.
            if game.session.sync_disabled {
                ui.label(
                    egui::RichText::new(loc.tr("battle.desync.status"))
                        .strong()
                        .color(egui::Color32::from_rgb(178, 34, 34)),
                );
            }
            let can_act = game.session.phase == BattlePhase::TacticalActive
                && game.session.current_side == state.player_side;
            match game.session.phase {
                BattlePhase::Deployment => {
                    // Auto Deploy hands the still-waiting
                    // battalions to the AI planner; disabled when none are
                    // left (hand-placed layouts stay untouched).
                    // The counts cover only the battalions the player
                    // actually commands — allied contingents deploy through
                    // their own staff, so they never gate these buttons.
                    let undep = state
                        .units
                        .iter()
                        .filter(|u| game.commands(u) && u.undeployed && u.is_combat_effective())
                        .count();
                    let deployed = state
                        .units
                        .iter()
                        .filter(|u| game.commands(u) && !u.undeployed && u.is_combat_effective())
                        .count();
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(undep > 0, |ui| {
                            if tip(
                                ui.add(icons.button(
                                    Some(IconId::Deploy),
                                    loc.tr("battle.button.auto_deploy").as_ref(),
                                    14.0,
                                )),
                                loc.tr("battle.tooltip.auto_deploy"),
                            )
                            .clicked()
                            {
                                pending.0.push(PlayerCommand::AutoDeploy);
                            }
                        });
                        // Recall the whole force to the OOB —
                        // double-confirmed before anything is lost.
                        ui.add_enabled_ui(deployed > 0, |ui| {
                            if tip(
                                ui.add(icons.button(
                                    Some(IconId::Back),
                                    loc.tr("battle.button.recall_all").as_ref(),
                                    14.0,
                                )),
                                loc.tr("battle.tooltip.recall_all"),
                            )
                            .clicked()
                            {
                                ui_win.confirm_recall = Some(crate::state::RecallKind::All);
                            }
                        });
                    });
                    // Grey out Begin Battle until every player-
                    // commanded battalion is deployed — the command-side
                    // rejection (log.deploy.cannot_start) stays as the backstop.
                    // Plain text when disabled so egui's own dimming applies
                    // (an explicit GOLD override would ignore it).
                    ui.add_enabled_ui(undep == 0, |ui| {
                        let label =
                            egui::RichText::new(loc.tr("battle.button.begin_battle").as_ref())
                                .strong();
                        let label = if undep == 0 { label.color(GOLD) } else { label };
                        if tip(
                            ui.add(icons.button(Some(IconId::Attack), label, 14.0)),
                            loc.tr("battle.tooltip.begin_battle"),
                        )
                        .clicked()
                        {
                            pending.0.push(PlayerCommand::BeginBattle);
                        }
                    });
                }
                BattlePhase::ReadyToSync => {
                    // Desync mode never rests in this phase (hours seal
                    // locally) — the disabled branch is the backstop, and
                    // its tooltip carries the reason.
                    let sync_tip = if game.session.sync_disabled {
                        loc.tr("battle.tooltip.sync_disabled")
                    } else {
                        loc.tr("battle.tooltip.sync")
                    };
                    ui.add_enabled_ui(!game.session.sync_disabled, |ui| {
                        if tip(
                            ui.add(
                                icons.button(
                                    Some(IconId::Sync),
                                    egui::RichText::new(loc.tr("battle.button.sync").as_ref())
                                        .strong()
                                        .color(GOLD),
                                    14.0,
                                ),
                            ),
                            sync_tip,
                        )
                        .clicked()
                        {
                            pending.0.push(PlayerCommand::Sync);
                        }
                    });
                }
                BattlePhase::ReadyToEnd => {
                    if tip(
                        ui.add(
                            icons.button(
                                Some(IconId::Check),
                                egui::RichText::new(loc.tr("battle.button.apply_exit").as_ref())
                                    .strong()
                                    .color(GOLD),
                                14.0,
                            ),
                        ),
                        loc.tr("battle.tooltip.apply_exit"),
                    )
                    .clicked()
                    {
                        pending.0.push(PlayerCommand::ApplyAndExit);
                    }
                }
                _ => {
                    ui.add_enabled_ui(can_act, |ui| {
                        if tip(
                            ui.add(
                                icons.button(
                                    Some(IconId::Clock),
                                    egui::RichText::new(loc.tr("battle.button.end_turn").as_ref())
                                        .strong(),
                                    14.0,
                                ),
                            ),
                            loc.tr("battle.tooltip.end_turn"),
                        )
                        .clicked()
                        {
                            pending.0.push(PlayerCommand::EndTurn);
                        }
                    });
                }
            }
            ui.separator();

            // Auxiliary slots (fixed positions as well).
            if tip(
                ui.add(icons.button(
                    Some(IconId::Map),
                    loc.tr("battle.button.minimap").as_ref(),
                    14.0,
                )),
                loc.tr("battle.tooltip.minimap"),
            )
            .clicked()
            {
                toggles.minimap = !toggles.minimap;
            }
            // Restore the opening camera view (target/yaw/pitch/zoom).
            if tip(
                ui.add(icons.button(
                    Some(IconId::Viewfinder),
                    loc.tr("battle.button.reset_view").as_ref(),
                    14.0,
                )),
                loc.tr("battle.tooltip.reset_view"),
            )
            .clicked()
            {
                reset.0 = true;
            }
            // Badge on the button's top-right corner counting
            // battalions still waiting in the OOB (red while any remain).
            // Player-commanded battalions only — allied divisions
            // wait for their own staff AI, not for the player.
            let undep = state
                .units
                .iter()
                .filter(|u| game.commands(u) && u.undeployed && u.is_combat_effective())
                .count();
            let resp = tip(
                ui.add(icons.button(
                    Some(IconId::Oob),
                    loc.tr("battle.button.oob").as_ref(),
                    14.0,
                )),
                loc.tr("battle.tooltip.oob"),
            );
            if resp.clicked() {
                ui_win.oob = true;
            }
            if undep > 0 {
                let painter = ui.painter();
                let center = resp.rect.right_top() + egui::vec2(-7.0, 6.0);
                painter.circle_filled(center, 9.0, egui::Color32::from_rgb(178, 34, 34));
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    undep.to_string(),
                    egui::FontId::proportional(11.0),
                    egui::Color32::WHITE,
                );
            }
            // Battle Log (moved out of the Esc menu, sits below
            // Order of Battle with the other on-demand window buttons).
            if tip(
                ui.add(icons.button(
                    Some(IconId::Scroll),
                    loc.tr("battle.button.game_log").as_ref(),
                    14.0,
                )),
                loc.tr("battle.tooltip.game_log"),
            )
            .clicked()
            {
                ui_win.log = true;
            }
            ui.separator();
            ui.label(
                egui::RichText::new(loc.tr("battle.hint.esc_menu").as_ref())
                    .small()
                    .weak(),
            );
        });
}

/// Localized hover tooltip on a button response — attached to
/// BOTH the enabled and the disabled state (egui splits the two): a
/// greyed-out button is exactly where the player needs the hint most.
fn tip(resp: egui::Response, text: impl Into<egui::WidgetText> + Clone) -> egui::Response {
    resp.on_hover_text(text.clone())
        .on_disabled_hover_text(text)
}

/// Org/Str ratio bar. The bar length already visualizes the ratio, so the
/// text carries the exact current/max values instead of a percentage — the
/// same string doubles as the hover tooltip (HOI4 battle-bubble style
/// precision on demand). Painted manually instead of egui::ProgressBar:
/// the text must be BLACK (egui's default light-gray text is unreadable on
/// the bright ORG_GREEN/STR_YELLOW fills), which in turn needs a LIGHT
/// unfilled track (egui's dark-theme track would swallow black text).
fn bar(ui: &mut egui::Ui, label: &str, ratio: f32, color: egui::Color32, values: String) {
    ui.horizontal(|ui| {
        // Fixed-PIXEL label column, right-aligned — `{label:>4}` padded by
        // CHARS, and CJK labels (组织度 = 3 double-width glyphs) misaligned
        // the bars in zh mode. The column must be a BOUNDED child ui: a bare
        // `with_layout(right_to_left)` spans the whole remaining row width
        // and anchors the label at the row's right edge, pushing the bar
        // past the card boundary.
        let h = ui.text_style_height(&egui::TextStyle::Body);
        ui.allocate_ui_with_layout(
            egui::vec2(60.0, h),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.label(label);
            },
        );
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        let rounding = egui::Rounding::same(2.0);
        painter.rect_filled(rect, rounding, egui::Color32::from_gray(225));
        let fill_w = rect.width() * ratio.clamp(0.0, 1.0);
        if fill_w > 0.0 {
            let mut fill_rect = rect;
            fill_rect.max.x = rect.min.x + fill_w;
            // Clip to the track so a partial fill keeps the track's rounding.
            painter
                .with_clip_rect(rect)
                .rect_filled(fill_rect, rounding, color);
        }
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            &values,
            egui::TextStyle::Body.resolve(ui.style()),
            egui::Color32::BLACK,
        );
        resp.on_hover_text(values);
    });
}

// ---------------------------------------------------------------------------
// Hover info card: replaces the left info panel. The cursor
// resting over the map pops a card — units fast, bare terrain slowly.
// ---------------------------------------------------------------------------

/// Cursor-rest tracker for the hover card.
#[derive(Default)]
struct HoverTrack {
    /// Last cursor position seen (egui logical px).
    cursor: Option<egui::Pos2>,
    /// Timestamp (elapsed secs) when the cursor last moved OUTSIDE the card.
    moved_at: f32,
    /// Card rect from last frame — moving INTO the card to read the exact
    /// bar values must not count as "moved away" (egui tooltip semantics).
    card_rect: Option<egui::Rect>,
}

const HOVER_DELAY_UNIT: f32 = 0.3;
const HOVER_DELAY_HEX: f32 = 0.8;

#[allow(clippy::too_many_arguments)]
fn draw_hover_card(
    mut contexts: EguiContexts,
    game: Option<Res<GameController>>,
    state: Res<TacticalState>,
    ui_win: Res<UiWindows>,
    stall: Option<Res<SyncStall>>,
    loc: Res<LocaleRes>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    time: Res<Time>,
    mut track: Local<HoverTrack>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    if ui_win.modal_open() || stall.is_some() {
        track.card_rect = None;
        return;
    }
    let Some(game) = game else { return };
    let now = time.elapsed_secs();
    let Some(cursor) = windows
        .get_single()
        .ok()
        .and_then(|w| w.cursor_position())
        .map(|p| egui::pos2(p.x, p.y))
    else {
        track.card_rect = None;
        track.cursor = None;
        return;
    };

    // Any movement re-arms the rest timer — unless it stayed inside the
    // shown card (mouse moved there to read exact values).
    let inside_card = track.card_rect.map(|r| r.contains(cursor)).unwrap_or(false);
    let moved = track
        .cursor
        .map(|c| c.distance(cursor) > 4.0)
        .unwrap_or(true);
    if moved && !inside_card {
        track.moved_at = now;
    }
    track.cursor = Some(cursor);

    // What is under the cursor? A VISIBLE unit takes the fast delay; hidden
    // enemies fall through to the plain terrain card (fog rules mirror the
    // 3D scene, and the enemy simply is not placed during Deployment).
    let deploying = game.session.phase == BattlePhase::Deployment;
    let hovered_unit = state.hover_hex.and_then(|h| state.unit_at(h)).filter(|u| {
        u.side == state.player_side
            || (!deploying
                && state.fog_state(u.position) == tactical_core::fog::VisibilityState::Visible)
    });
    let delay = if hovered_unit.is_some() {
        HOVER_DELAY_UNIT
    } else {
        HOVER_DELAY_HEX
    };
    if now - track.moved_at < delay || state.hover_hex.is_none() {
        track.card_rect = None;
        return;
    }

    let screen = ctx.screen_rect();
    let mut pos = cursor + egui::vec2(18.0, 14.0);
    // Clamp against last frame's size so the card never leaves the screen.
    if let Some(r) = track.card_rect {
        pos.x = pos
            .x
            .clamp(6.0, (screen.right() - r.width() - 6.0).max(6.0));
        pos.y = pos
            .y
            .clamp(6.0, (screen.bottom() - r.height() - 6.0).max(6.0));
    }
    let resp = egui::Area::new(egui::Id::new("hover_card"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::LEFT_TOP)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            hoi4_frame().show(ui, |ui| {
                ui.set_max_width(230.0);
                if let Some(u) = hovered_unit {
                    unit_card(ui, &game, &state, u, &loc);
                } else if let Some(h) = state.hover_hex {
                    hex_card(ui, &state, h, &loc, game.combat.params().oob_leaving_turns);
                }
            });
        });
    track.card_rect = Some(resp.response.rect);
}

/// Unit hover card: full stat sheet + exact Org/Str on bar hover.
fn unit_card(
    ui: &mut egui::Ui,
    game: &GameController,
    state: &TacticalState,
    u: &tactical_core::unit::BattalionUnit,
    loc: &LocaleRes,
) {
    ui.label(egui::RichText::new(&u.name).strong());
    if !u.division.is_empty() {
        ui.label(egui::RichText::new(&u.division).small().weak());
    }
    // Attached support companies ride in the host's card.
    if !u.support.is_empty() {
        let names: Vec<String> = u.support.iter().map(|s| s.name.clone()).collect();
        ui.label(
            egui::RichText::new(format!("+ {}", names.join(", ")))
                .small()
                .weak(),
        )
        .on_hover_text(u.support_effects_line());
    }
    let mut state_line = format!(
        "{} | {} | {}",
        loc.unit_type_name(u.unit_type),
        loc.side_name(u.side),
        loc.unit_state_name(u.state)
    );
    if u.is_emplaced {
        state_line.push_str(loc.tr("battle.hover.state_emplaced").as_ref());
    }
    ui.label(state_line);
    // §6.14: out-of-bounds dwell countdown — the recall window
    // before the unit slips away for good must be visible at a glance.
    if u.oob_turns > 0 {
        let max = game.combat.params().oob_leaving_turns;
        ui.label(
            egui::RichText::new(loc.trf(
                "battle.hover.oob_countdown",
                &[("n", &u.oob_turns.to_string()), ("max", &max.to_string())],
            ))
            .strong()
            .color(GOLD),
        );
    }
    let tags = attr_tags(u, loc);
    if !tags.is_empty() {
        ui.label(egui::RichText::new(tags).small().weak());
    }
    ui.separator();
    // HOI4 colors: green = organization, yellow = strength.
    bar(
        ui,
        loc.tr("battle.hover.org").as_ref(),
        u.org_ratio(),
        ORG_GREEN,
        format!("{:.1}/{:.1}", u.org, u.max_org),
    );
    bar(
        ui,
        loc.tr("battle.hover.str").as_ref(),
        u.strength_ratio(),
        STR_YELLOW,
        format!("{:.1}/{:.1}", u.strength, u.max_strength),
    );
    ui.separator();
    ui.label(loc.trf(
        "battle.hover.soft_hard",
        &[
            ("soft", &format!("{:.1}", u.soft_attack)),
            ("hard", &format!("{:.1}", u.hard_attack)),
        ],
    ));
    ui.label(loc.trf(
        "battle.hover.def_brk",
        &[
            ("def", &format!("{:.1}", u.defense)),
            ("brk", &format!("{:.1}", u.breakthrough)),
        ],
    ));
    ui.label(loc.trf(
        "battle.hover.armor_line",
        &[
            ("armor", &format!("{:.1}", u.armor)),
            ("piercing", &format!("{:.1}", u.piercing)),
            ("hard", &format!("{:.0}", u.hardness * 100.0)),
        ],
    ));
    ui.separator();
    // Rockets show their dead zone alongside the max range.
    let speed = format!("{:.0}", u.speed_kmh);
    if u.min_attack_range() > 1 {
        ui.label(loc.trf(
            "battle.hover.range_minmax",
            &[
                ("min", &u.min_attack_range().to_string()),
                ("max", &u.attack_range.to_string()),
                ("sight", &u.sight_range.to_string()),
                ("speed", &speed),
            ],
        ));
    } else {
        ui.label(loc.trf(
            "battle.hover.range",
            &[
                ("range", &u.attack_range.to_string()),
                ("sight", &u.sight_range.to_string()),
                ("speed", &speed),
            ],
        ));
    }
    if u.unit_type.aa_cover_radius() > 0 {
        ui.label(loc.trf(
            "battle.hover.aa_cover",
            &[("n", &u.unit_type.aa_cover_radius().to_string())],
        ));
    }
    if u.entrenchment > 0 {
        ui.label(loc.trf(
            "battle.hover.entrenchment",
            &[("n", &u.entrenchment.to_string())],
        ));
    }
    // ETA lives ONLY here — computed against the player's view so
    // hidden enemies neither block nor project ZOC.
    if let Some(order) = &u.move_order {
        if let Some(dest) = order.path.last() {
            let view: Vec<tactical_core::unit::BattalionUnit> = state
                .units
                .iter()
                .filter(|x| {
                    x.side == state.player_side
                        || state.fog_state(x.position)
                            == tactical_core::fog::VisibilityState::Visible
                })
                .cloned()
                .collect();
            let eta = state.grid.as_ref().and_then(|g| {
                tactical_core::movement::order_eta_hours(g, u, &view, game.combat.params())
            });
            if let Some(hrs) = eta {
                let turns = tactical_core::movement::eta_turns(hrs, game.combat.params());
                ui.label(loc.trf(
                    "battle.hover.eta",
                    &[
                        ("q", &dest.q.to_string()),
                        ("r", &dest.r.to_string()),
                        ("turns", &turns.to_string()),
                    ],
                ));
            }
        }
    }
    let mut flags: Vec<String> = Vec::new();
    if u.is_holding {
        flags.push(loc.tr("battle.hover.flag_holding").into_owned());
    }
    if u.is_emplaced {
        flags.push(loc.tr("battle.hover.flag_emplaced").into_owned());
    }
    if u.acted {
        flags.push(loc.tr("battle.hover.flag_acted").into_owned());
    }
    // Rocket salvo reload countdown.
    if u.fire_cooldown > 0 {
        flags.push(loc.trf(
            "battle.hover.flag_reloading",
            &[("n", &u.fire_cooldown.to_string())],
        ));
    }
    if !flags.is_empty() {
        ui.label(egui::RichText::new(flags.join(" | ")).small());
    }
}

/// Attribute flags as a short tag line ("Infantry · Motorized").
fn attr_tags(u: &tactical_core::unit::BattalionUnit, loc: &LocaleRes) -> String {
    use tactical_core::unit::Attrs;
    let mut tags: Vec<std::borrow::Cow<str>> = Vec::new();
    if u.attrs.has(Attrs::INFANTRY) {
        tags.push(loc.tr("attr.infantry"));
    }
    if u.attrs.has(Attrs::CAVALRY) {
        tags.push(loc.tr("attr.cavalry"));
    }
    if u.attrs.has(Attrs::MOTORIZED) {
        tags.push(loc.tr("attr.motorized"));
    }
    if u.attrs.has(Attrs::MECHANIZED) {
        tags.push(loc.tr("attr.mechanized"));
    }
    if u.attrs.has(Attrs::ARMORED) {
        tags.push(loc.tr("attr.armored"));
    }
    if u.attrs.has(Attrs::ARTILLERY) {
        tags.push(loc.tr("attr.artillery"));
    }
    if u.attrs.has(Attrs::ROCKET) {
        tags.push(loc.tr("attr.rocket"));
    }
    if u.attrs.has(Attrs::AT) {
        tags.push(loc.tr("attr.anti_tank"));
    }
    if u.attrs.has(Attrs::AA) {
        tags.push(loc.tr("attr.anti_air"));
    }
    if u.attrs.has(Attrs::TOWED) {
        tags.push(loc.tr("attr.towed"));
    }
    if u.attrs.has(Attrs::RECON) {
        tags.push(loc.tr("attr.recon"));
    }
    if u.attrs.has(Attrs::SUPPORT) {
        tags.push(loc.tr("attr.support"));
    }
    if u.attrs.has(Attrs::AMPHIBIOUS) {
        tags.push(loc.tr("attr.amphibious"));
    }
    if u.attrs.has(Attrs::FLAME) {
        tags.push(loc.tr("attr.flame"));
    }
    tags.join(" · ")
}

/// Terrain hover card (replaces the info panel's hex block).
fn hex_card(
    ui: &mut egui::Ui,
    state: &TacticalState,
    h: tactical_core::hex::HexCoord,
    loc: &LocaleRes,
    oob_max: u8,
) {
    ui.label(
        egui::RichText::new(loc.trf(
            "battle.hover.hex",
            &[("q", &h.q.to_string()), ("r", &h.r.to_string())],
        ))
        .strong(),
    );
    if let Some(g) = &state.grid {
        if let Some(c) = g.cell(h) {
            ui.label(loc.terrain_name(c.terrain).as_ref());
            // §6.14: impassability and the
            // out-of-bounds leaving rule get a PROMINENT colored line, not
            // just an implicit 99.0 movement cost — water is uncrossable,
            // backdrop land is enterable but bleeds units that linger.
            if !c.is_passable {
                ui.label(
                    egui::RichText::new(loc.tr("battle.hover.impassable"))
                        .strong()
                        .color(WARN_RED),
                );
            } else if c.out_of_bounds {
                ui.label(
                    egui::RichText::new(loc.trf(
                        "battle.hover.out_of_bounds",
                        &[("max", &oob_max.to_string())],
                    ))
                    .strong()
                    .color(GOLD),
                );
            }
            ui.label(loc.trf(
                "battle.hover.terrain_mods",
                &[("move", &format!("{:.1}", c.terrain.movement_cost()))],
            ));
            ui.label(loc.trf(
                "battle.hover.cover",
                &[(
                    "cover",
                    &format!("{:+.0}", c.terrain.cover_percent() * 100.0),
                )],
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Minimap (top-LEFT corner, egui painter dots; the Enemy tactic card
// that used to mirror it moved into the right panel)
// ---------------------------------------------------------------------------

fn draw_minimap(
    ctx: &egui::Context,
    game: &GameController,
    state: &TacticalState,
    colors: &SideColors,
    toggles: &mut UiToggles,
    cache: &mut MinimapCache,
    loc: &LocaleRes,
    cam: Option<(&Camera, &GlobalTransform)>,
) {
    // On the very first frame egui has no real window
    // size yet and reports a garbage ~6666px screen rect. Skip that frame;
    // the window first appears one frame later.
    if egui_first_frame(ctx) {
        return;
    }
    egui::Window::new(loc.tr("battle.button.minimap").as_ref())
        // Top-left corner (the left info panel is gone). Same
        // current_pos discipline as before: re-applied every frame so window
        // resizes / DPI changes / egui's stored state can never strand it.
        .current_pos([
            ctx.available_rect().left() + TOP_WIN_MARGIN,
            ctx.available_rect().top() + TOP_WIN_Y,
        ])
        .fixed_size([TOP_WIN_W, TOP_WIN_H])
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            if ui.button(loc.tr("common.close").as_ref()).clicked() {
                toggles.minimap = false;
            }
            let Some(grid) = &state.grid else { return };
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ui.available_height() - 4.0),
                egui::Sense::hover(),
            );
            let painter = ui.painter_at(rect);
            // Pointy-top projection (angle audit): axial (q,r) is NOT square —
            // world x ∝ q + r/2, y ∝ r·√3/2. A square mapping shears the map.
            let rows = grid.height as f32;
            let cols = grid.width as f32 + rows * 0.5;
            let s = (rect.width() / cols).min(rect.height() / (rows * 0.866));
            let ox = rect.left() + (rect.width() - cols * s) / 2.0;
            let oy = rect.top() + (rect.height() - rows * 0.866 * s) / 2.0;
            let dot = |h: tactical_core::hex::HexCoord| -> egui::Pos2 {
                egui::pos2(
                    ox + (h.q as f32 + h.r as f32 * 0.5) * s + s / 2.0,
                    oy + h.r as f32 * s * 0.866 + s / 2.0,
                )
            };
            // Terrain layer: static for the whole battle — baked
            // ONCE into an egui texture (4 texels per hex) and drawn as a
            // single image, instead of one rect per hex re-tessellated every
            // frame (15k rects on a Sedan-sized map, 260k at the 512 cap).
            if cache.texture.is_none() {
                const K: usize = 4;
                let tw = ((cols * K as f32).ceil().max(1.0)) as usize;
                let th = ((rows * 0.866 * K as f32).ceil().max(1.0)) as usize;
                let mut img = egui::ColorImage::new([tw, th], egui::Color32::from_rgb(20, 20, 24));
                for h in grid.iter_coords() {
                    let Some(c) = grid.cell(h) else { continue };
                    // Board-synced palette: in-bounds cells take
                    // the banded terrain colour (mountain contour bands,
                    // yellow-green hills — same source as the 3D board);
                    // out-of-bounds cells — land AND water alike — fade to
                    // one dim grey, matching the board's sky-haze retreat.
                    let color = if !c.out_of_bounds {
                        let rgb = c.terrain.banded_color(c.elevation);
                        egui::Color32::from_rgb(
                            (rgb[0] * 200.0) as u8,
                            (rgb[1] * 200.0) as u8,
                            (rgb[2] * 200.0) as u8,
                        )
                    } else {
                        egui::Color32::from_rgb(58, 58, 64)
                    };
                    let px = ((h.q as f32 + h.r as f32 * 0.5) * K as f32) as usize;
                    let py = (h.r as f32 * 0.866 * K as f32) as usize;
                    for dy in 0..K {
                        for dx in 0..K {
                            let (x, y) = (px + dx, py + dy);
                            if x < tw && y < th {
                                img.pixels[y * tw + x] = color;
                            }
                        }
                    }
                }
                cache.texture =
                    Some(ctx.load_texture("minimap-terrain", img, egui::TextureOptions::NEAREST));
            }
            if let Some(tex) = &cache.texture {
                painter.image(
                    tex.id(),
                    egui::Rect::from_min_size(
                        egui::pos2(ox, oy),
                        egui::vec2(cols * s, rows * 0.866 * s),
                    ),
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            // units
            for u in &state.units {
                if !u.is_combat_effective() {
                    continue;
                }
                // During Deployment the enemy AI has not placed its
                // units yet — no enemy blips on the minimap.
                if game.session.phase == BattlePhase::Deployment && u.side != state.player_side {
                    continue;
                }
                // Fog-hidden enemies must not blip on the minimap
                // either (same rule as 3D models and unit labels).
                if u.side != state.player_side
                    && state.fog_state(u.position) != tactical_core::fog::VisibilityState::Visible
                {
                    continue;
                }
                // Country theme colors, brightened for readability.
                let c = colors.for_side(u.side);
                let bright = [
                    (c[0] * 1.4 + 0.15).min(1.0),
                    (c[1] * 1.4 + 0.15).min(1.0),
                    (c[2] * 1.4 + 0.15).min(1.0),
                    1.0,
                ];
                painter.circle_filled(dot(u.position), (s * 0.6).max(2.0), color32(bright, 255));
            }
            // White quad = the main camera's visible ground
            // footprint — the four screen corners raycast onto the picking
            // plane, then mapped into the minimap's hex projection
            // (k = px per world unit; world +x → right, +z → down).
            if let Some((camera, cam_tf)) = cam {
                let board_max = hex_world(tactical_core::hex::HexCoord::new(
                    grid.width as i32 - 1,
                    grid.height as i32 - 1,
                ));
                let k = s / (3.0_f32.sqrt() * crate::board::HEX_SIZE);
                let screen = ctx.screen_rect();
                let screen_size = Vec2::new(screen.width(), screen.height());
                let mut pts = Vec::with_capacity(4);
                for corner in [
                    screen.left_top(),
                    screen.right_top(),
                    screen.right_bottom(),
                    screen.left_bottom(),
                ] {
                    if let Some(w) =
                        view_ground_point(camera, cam_tf, corner, board_max, screen_size)
                    {
                        pts.push(egui::pos2(ox + s / 2.0 + k * w.x, oy + s / 2.0 + k * w.z));
                    }
                }
                if pts.len() == 4 {
                    painter.add(egui::Shape::closed_line(
                        pts,
                        egui::Stroke::new(1.0_f32, egui::Color32::WHITE),
                    ));
                }
            }
        });
}

/// Ground-plane point seen at a screen corner: a raycast onto the
/// shared picking plane (`GROUND_PLANE_Y`). Rays that miss it (looking at or
/// above the horizon at shallow pitch) walk far along their ground direction
/// instead, and every result clamps to the board so the quad degrades to a
/// trapezoid rather than vanishing. egui points match the camera's logical
/// viewport (same convention as cursor picking); `screen_size` feeds the
/// render-scale cursor conversion (picking::scaled_cursor).
fn view_ground_point(
    camera: &Camera,
    cam_tf: &GlobalTransform,
    corner: egui::Pos2,
    board_max: Vec3,
    screen_size: Vec2,
) -> Option<Vec3> {
    let corner = crate::picking::scaled_cursor(Vec2::new(corner.x, corner.y), camera, screen_size);
    let ray = camera.viewport_to_world(cam_tf, corner).ok()?;
    const MARGIN: f32 = 2.0;
    let t = (crate::board::GROUND_PLANE_Y - ray.origin.y) / ray.direction.y;
    let hit = if ray.direction.y < -1e-6 && t > 0.0 {
        ray.origin + ray.direction * t
    } else {
        let flat = Vec3::new(ray.direction.x, 0.0, ray.direction.z).normalize_or_zero();
        ray.origin + flat * 1000.0
    };
    Some(hit.clamp(
        Vec3::new(-MARGIN, 0.0, -MARGIN),
        board_max + Vec3::new(MARGIN, 0.0, MARGIN),
    ))
}

// ---------------------------------------------------------------------------
// World-space unit labels (SD2-style counters above units)
// ---------------------------------------------------------------------------

fn draw_unit_labels(
    mut contexts: EguiContexts,
    state: Res<TacticalState>,
    game: Option<Res<GameController>>,
    colors: Res<SideColors>,
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>, // Camera3d filter — a second camera would break get_single
) {
    // On window-close frames the egui context is already torn
    // down — ctx_mut() would panic; skip the frame instead.
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    let Ok((camera, cam_tf)) = q_cam.get_single() else {
        return;
    };
    // During Deployment the enemy AI has not placed its units yet.
    let deploying = game
        .as_deref()
        .map(|g| g.session.phase == BattlePhase::Deployment)
        .unwrap_or(false);
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("unit_labels"),
    ));
    // Labels must hide *behind* side/bottom panels like the 3D
    // models do. `available_rect` is the central region left over by egui
    // panels (draw_panels runs first in this chain, so panels are accounted).
    let avail = ctx.available_rect().shrink(4.0);

    // Morale direction icons on the labels —
    // blue ↓ / ↓↓ for encirclement attrition (Partial / Full), red ↑ for
    // HQ-aura regen. The command links are shared by the whole roster, so
    // compute them once per frame.
    let cmd_links = game
        .as_deref()
        .map(|g| compute_command_links(&state.units, g.combat.params()));

    for (ui, u) in state.units.iter().enumerate() {
        if !u.is_combat_effective() && u.state != UnitState::Retreating {
            continue;
        }
        if deploying && u.side != state.player_side {
            continue; // enemy AI has not deployed yet
        }
        // Skip fog-hidden enemies (matches 3D rendering).
        if u.side != state.player_side
            && state.fog_state(u.position) != tactical_core::fog::VisibilityState::Visible
        {
            continue;
        }
        // The anchor must include the terrain height
        // (`unit_y_on_grid`, same as the model base) — a fixed 0.85 made the
        // label sink into models on high ground and float on low ground.
        let w = hex_world(u.position)
            + Vec3::Y * (crate::picking::unit_y_on_grid(&state, u.position) + 0.85);
        let Some(ndc) = camera.world_to_ndc(cam_tf, w) else {
            continue;
        };
        if ndc.z < 0.0 || ndc.z > 1.0 {
            continue;
        }
        let screen = ctx.screen_rect();
        let x = (ndc.x * 0.5 + 0.5) * screen.width();
        let y = (1.0 - (ndc.y * 0.5 + 0.5)) * screen.height();
        let pos = egui::pos2(x, y);
        if !avail.contains(pos) {
            continue; // label would sit on top of a panel — hide it
        }

        let wdt = 46.0;
        let rect = egui::Rect::from_center_size(pos, egui::vec2(wdt, 15.0));
        // Label bg = darkened side theme color.
        let c = colors.for_side(u.side);
        let bg = color32([c[0] * 0.45, c[1] * 0.45, c[2] * 0.45, 1.0], 220);
        painter.rect_filled(rect, 2.0, bg);
        // org/str mini bars
        let bar_w = wdt - 6.0;
        let org_rect = egui::Rect::from_min_size(
            rect.left_bottom() + egui::vec2(3.0, -5.5),
            egui::vec2(bar_w * u.org_ratio(), 2.0),
        );
        let str_rect = egui::Rect::from_min_size(
            rect.left_bottom() + egui::vec2(3.0, -2.5),
            egui::vec2(bar_w * u.strength_ratio(), 2.0),
        );
        painter.rect_filled(org_rect, 0.0, ORG_GREEN);
        painter.rect_filled(str_rect, 0.0, STR_YELLOW);
        // name (§6.13: own HQ in gold — instant recognition;
        // enemy names in bright red, enemy HQ in a warmer tint so
        // hostile counters read at a glance).
        let name_color = if u.side != state.player_side {
            if u.is_hq() {
                ENEMY_HQ_RED
            } else {
                ENEMY_RED
            }
        } else if u.is_hq() {
            GOLD
        } else {
            egui::Color32::WHITE
        };
        painter.text(
            rect.center() - egui::vec2(0.0, 3.0),
            egui::Align2::CENTER_CENTER,
            short_name(&u.name),
            egui::FontId::proportional(9.5),
            name_color,
        );
        // Stance markers (letter glyphs): H = holding (dug in),
        // E = emplaced guns — left corner; the retreat marker R sits right.
        let mut marks = String::new();
        if u.is_holding {
            marks.push('H');
        }
        if u.is_emplaced {
            marks.push('E');
        }
        // S = shocked (cannot take attack orders this turn).
        if u.shocked {
            marks.push('S');
        }
        // W = waiting on rocket reload (hover shows the count).
        if u.fire_cooldown > 0 {
            marks.push('W');
        }
        if !marks.is_empty() {
            painter.text(
                rect.left_top() + egui::vec2(-2.0, -2.0),
                egui::Align2::RIGHT_TOP,
                marks,
                egui::FontId::proportional(9.0),
                GOLD,
            );
        }
        // Retreating marker — faux-bold double draw + light shadow so the R
        // carries weight next to the thinner stance letters (the letter
        // stays — it belongs to the H/E/S/W family; the morale arrows are
        // a separate icon family at the right edge).
        if u.state == UnitState::Retreating {
            let pos = rect.right_top() + egui::vec2(2.0, -2.0);
            let font = egui::FontId::proportional(11.0);
            let color = egui::Color32::from_rgb(255, 120, 80);
            painter.text(
                pos + egui::vec2(1.0, 1.0),
                egui::Align2::LEFT_TOP,
                "R",
                font.clone(),
                egui::Color32::from_black_alpha(90),
            );
            painter.text(pos, egui::Align2::LEFT_TOP, "R", font.clone(), color);
            painter.text(
                pos + egui::vec2(0.6, 0.0),
                egui::Align2::LEFT_TOP,
                "R",
                font,
                color,
            );
        }
        // Morale direction icon at the
        // label's RIGHT EDGE, vertically centered on the name text — blue
        // ↓ + light shadow = Partial attrition, blue ↓↓ + heavy shadow =
        // Full attrition, red ↑ + highlight = HQ-aura regen (§6.13: in
        // command, out of contact, org < max). Attrition needs an adjacent
        // enemy while regen requires no contact, so the two never co-occur
        // and share one slot. Retreating units are skipped (attrition only
        // bites on combat-effective units anyway) — their R marker owns
        // the right edge.
        let encircled = state
            .grid
            .as_deref()
            .map(|g| detect_encirclement(g, u, &state.units))
            .unwrap_or(EncirclementLevel::None);
        let regen = encircled == EncirclementLevel::None
            && u.is_combat_effective()
            && u.org < u.max_org
            && cmd_links
                .as_deref()
                .map(|links| in_command(links[ui]))
                .unwrap_or(false)
            && !state.units.iter().any(|e| {
                e.side != u.side && e.is_combat_effective() && e.position.distance(u.position) == 1
            });
        let morale = if u.state == UnitState::Retreating {
            None
        } else {
            match encircled {
                EncirclementLevel::Partial => Some(("↓", false, false)),
                EncirclementLevel::Full => Some(("↓↓", true, false)),
                _ if regen => Some(("↑", false, true)),
                _ => None,
            }
        };
        if let Some((glyph, heavy_shadow, highlight)) = morale {
            // The arrows are VECTOR shapes — font arrows render
            // as illegible hairline strokes at this size (verified by
            // screenshot). Glyph box ≈ 6×11 logical px. The name
            // text is centered 3 px above the label center, so the glyph
            // box top goes 5.25 px above that to center on the text.
            let base = egui::pos2(rect.right() + 2.0, rect.center().y - 8.25);
            let color = if highlight {
                egui::Color32::from_rgb(255, 95, 75) // regen red
            } else {
                egui::Color32::from_rgb(100, 150, 255) // attrition blue
            };
            let shadow = egui::Color32::from_black_alpha(if heavy_shadow { 170 } else { 90 });
            paint_morale_icon(&painter, base + egui::vec2(1.0, 1.0), glyph, shadow);
            if heavy_shadow {
                paint_morale_icon(&painter, base + egui::vec2(2.0, 2.0), glyph, shadow);
            }
            if highlight {
                paint_morale_icon(
                    &painter,
                    base + egui::vec2(-0.7, -0.7),
                    glyph,
                    egui::Color32::from_white_alpha(140),
                );
            }
            paint_morale_icon(&painter, base, glyph, color);
        }
    }
}

/// One morale arrow glyph: `text` is "↓", "↓↓" or "↑" — drawn
/// as a shaft + triangular head per arrow, 6 px horizontal step between
/// the two arrows of the Full pair.
fn paint_morale_icon(
    painter: &egui::Painter,
    origin: egui::Pos2,
    text: &str,
    color: egui::Color32,
) {
    let down = text.starts_with('↓');
    let count = if text == "↓↓" { 2 } else { 1 };
    for k in 0..count {
        let ox = origin.x + k as f32 * 6.0 + 2.8;
        let oy = origin.y;
        let (shaft, head) = if down {
            (
                [egui::pos2(ox, oy), egui::pos2(ox, oy + 6.5)],
                [
                    egui::pos2(ox - 2.6, oy + 5.5),
                    egui::pos2(ox + 2.6, oy + 5.5),
                    egui::pos2(ox, oy + 10.5),
                ],
            )
        } else {
            (
                [egui::pos2(ox, oy + 4.0), egui::pos2(ox, oy + 10.5)],
                [
                    egui::pos2(ox - 2.6, oy + 5.0),
                    egui::pos2(ox + 2.6, oy + 5.0),
                    egui::pos2(ox, oy),
                ],
            )
        };
        painter.line_segment(shaft, egui::Stroke::new(1.6_f32, color));
        painter.add(egui::Shape::convex_polygon(
            head.to_vec(),
            color,
            egui::Stroke::NONE,
        ));
    }
}

fn short_name(name: &str) -> String {
    name.chars().take(8).collect()
}

// ---------------------------------------------------------------------------
// Radial command menu: circular letter-buttons over the
// selected unit — placeholder glyphs until proper icons are drawn.
// ---------------------------------------------------------------------------

/// One radial-menu action; executed immediately on click.
#[derive(Clone, Copy)]
enum RadialAct {
    Hold,
    Emplace,
    Fire,
    Retreat,
}

/// Pop-up ring of command buttons around the selected unit. Button set is
/// type-driven: H = Hold stance (leg infantry only,
/// no cavalry), E/L = Emplace/Limber (towed guns, same slot), F = fire
/// mission picking (indirect artillery only), R = retreat (only in contact
/// with the enemy). Unavailable buttons are hidden, not greyed — an acted
/// unit shows no ring at all (its model renders translucent instead), with
/// one exception: an acted unit that is HOLDING keeps a single-button ring
/// (H = stand up), since leaving cover is free.
fn draw_radial_menu(
    mut contexts: EguiContexts,
    game: Option<Res<GameController>>,
    state: Res<TacticalState>,
    ui_win: Res<UiWindows>,
    stall: Option<Res<SyncStall>>,
    loc: Res<LocaleRes>,
    mut pending: ResMut<PendingCommands>,
    q_cam: Query<(&Camera, &GlobalTransform), With<Camera3d>>, // Camera3d filter — a second camera would break get_single
) {
    // Modal surfaces freeze the command ring too.
    if ui_win.modal_open() || stall.is_some() {
        return;
    }
    let Some(game) = game.as_deref() else { return };
    if game.session.phase != BattlePhase::TacticalActive
        || game.session.current_side != state.player_side
    {
        return;
    }
    let Some(u) = state.selected_unit.and_then(|id| state.unit_by_id(id)) else {
        return;
    };
    if u.side != state.player_side || !u.is_combat_effective() {
        return;
    }
    let Ok((camera, cam_tf)) = q_cam.get_single() else {
        return;
    };
    // On window-close frames the egui context is already torn
    // down — ctx_mut() would panic; skip the frame instead.
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };

    // Project the unit to screen space (same ndc math as draw_unit_labels).
    let w = hex_world(u.position) + Vec3::Y * 1.25;
    let Some(ndc) = camera.world_to_ndc(cam_tf, w) else {
        return;
    };
    if ndc.z < 0.0 || ndc.z > 1.0 {
        return;
    }
    let screen = ctx.screen_rect();
    let center = egui::pos2(
        (ndc.x * 0.5 + 0.5) * screen.width(),
        (1.0 - (ndc.y * 0.5 + 0.5)) * screen.height(),
    );
    // Hide under the side/bottom panels, like the unit labels.
    // (draw_panels runs earlier in this chain, so available_rect accounts
    // for them.)
    if !ctx.available_rect().contains(center) {
        return;
    }

    // Unavailable buttons are HIDDEN, not greyed: the ring teaches causality
    // by appearing (E → F after emplacing) and disappears entirely once the
    // unit has acted — pairing with the translucent "spent" model as the
    // done-for-turn signal.
    // EXCEPTION: stand-up (H on a holding unit) stays visible after acting —
    // leaving cover is free and its handler has no acted gate.
    let mut btns: Vec<(&str, String, RadialAct)> = Vec::new();

    // An HQ selection swaps the battalion ring for the DIVISION
    // command bar — square full-name buttons (division orders are given to
    // the whole division at once, so the compact letter-glyph language of
    // the battalion ring does not apply). The bar shows the three order
    // kinds while the division is un-ordered, and [Cancel] (+ the current
    // order's name) while it has a standing order.
    if u.is_hq() {
        draw_division_command_bar(ctx, u, center, screen, game, &loc, &mut pending);
        return;
    }

    // The battalion ring is a command interface — an ALLIED
    // battalion (info-selectable) gets no ring; its orders come from its
    // own staff AI. Allied HQs still reach the division bar above
    // (division-level coordination is allowed, DESIGN §7.5).
    if !game.commands(u) {
        return;
    }

    if u.is_holding || (!u.acted && u.can_hold()) {
        // Cover costs the turn to take, is free to leave, and
        // drops on moving / attacking (no assault restriction). Standing up
        // stays available after acting (see the ring comment above).
        let tip = if u.is_holding {
            loc.tr("battle.radial.stand_up").into_owned()
        } else {
            loc.tr("battle.radial.take_cover").into_owned()
        };
        btns.push(("H", tip, RadialAct::Hold));
    }
    if !u.acted && u.requires_emplacement() {
        let (letter, tip) = if u.is_emplaced {
            ("L", loc.tr("battle.radial.limber").into_owned())
        } else {
            ("E", loc.tr("battle.radial.emplace").into_owned())
        };
        btns.push((letter, tip, RadialAct::Emplace));
    }
    if !u.acted && u.is_indirect_artillery() && u.can_fire_support() {
        btns.push((
            "F",
            loc.tr("battle.radial.fire_mission").into_owned(),
            RadialAct::Fire,
        ));
    }
    // Manual retreat is a disengagement move — only offered in contact with
    // the enemy (otherwise a plain move order is the right tool), and only
    // BEFORE the unit acts: the ring disappears once a unit has spent its
    // turn, and a retreat command onto a spent unit contradicted that
    // contract (the handler gates too).
    let in_contact = !u.acted
        && state.units.iter().any(|e| {
            e.side != u.side && e.is_combat_effective() && e.position.distance(u.position) == 1
        });
    if in_contact {
        btns.push((
            "R",
            loc.tr("battle.radial.retreat").into_owned(),
            RadialAct::Retreat,
        ));
    }

    let n = btns.len();
    for (i, (letter, tip, act)) in btns.iter().enumerate() {
        // Top-half arc above the unit — the old full-ring spread
        // put the bottom button of a 2-button set right ON the model (one
        // misclick away from re-selecting instead of commanding). n=1 top,
        // n=2 top-left/top-right, n=4 fans across the whole upper half.
        let ang = -std::f32::consts::PI + (i as f32 + 0.5) * std::f32::consts::PI / n as f32;
        let pos = center + egui::vec2(ang.cos() * 48.0, ang.sin() * 48.0);
        // Clamp on-screen: near the top edge the arc would otherwise clip.
        let pos = egui::pos2(
            pos.x.clamp(20.0, (screen.width() - 20.0).max(20.0)),
            pos.y.clamp(20.0, (screen.height() - 20.0).max(20.0)),
        );
        egui::Area::new(egui::Id::new(("radial_menu", i)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(egui::Align2::CENTER_CENTER)
            .show(ctx, |ui| {
                let btn =
                    egui::Button::new(egui::RichText::new(*letter).strong().size(13.0).color(GOLD))
                        .min_size(egui::vec2(26.0, 26.0))
                        .rounding(13.0);
                let resp = ui.add(btn).on_hover_text(tip);
                if resp.clicked() {
                    let cmd = match act {
                        RadialAct::Hold => PlayerCommand::ToggleHold,
                        RadialAct::Emplace => PlayerCommand::ToggleEmplace,
                        RadialAct::Retreat => PlayerCommand::RetreatSelected,
                        RadialAct::Fire => PlayerCommand::SetMode(CommandMode::FirePicking),
                    };
                    pending.0.push(cmd);
                }
            });
    }
}

// ---------------------------------------------------------------------------
// Division command bar: horizontal SQUARE buttons with full
// names over the selected HQ — deliberately distinct from the battalion
// letter ring. No order: [推进] [占领] [歼敌]; order active: the current
// order's name badge + [取消]. 占领/歼敌 arm the map target pick.
// ---------------------------------------------------------------------------

fn draw_division_command_bar(
    ctx: &egui::Context,
    hq: &tactical_core::unit::BattalionUnit,
    center: egui::Pos2,
    screen: egui::Rect,
    game: &GameController,
    loc: &LocaleRes,
    pending: &mut ResMut<PendingCommands>,
) {
    let division = hq.division.clone();
    let order = game.div_orders.get(&division);

    // Every button carries a localized hover tooltip explaining
    // what the order does (the badge of the ACTIVE order shows its kind's
    // tooltip too).
    let items: Vec<(String, IconId, egui::Color32, Option<PlayerCommand>, &str)> = match order {
        Some(o) => {
            // The active order's kind badge + [Cancel].
            let (icon, tip_key) = match o.pick {
                DivOrderPick::Advance => (IconId::Advance, "div_order.advance.tooltip"),
                DivOrderPick::Seize(_) => (IconId::Flag, "div_order.seize.tooltip"),
                DivOrderPick::Engage { .. } => (IconId::Target, "div_order.engage.tooltip"),
            };
            vec![
                (
                    loc.tr(o.kind_key()).into_owned(),
                    icon,
                    egui::Color32::from_rgb(255, 220, 130),
                    None,
                    tip_key,
                ),
                (
                    loc.tr("div_order.cancel").into_owned(),
                    IconId::Cross,
                    egui::Color32::from_rgb(255, 150, 110),
                    Some(PlayerCommand::CancelDivOrder(division.clone())),
                    "div_order.cancel.tooltip",
                ),
            ]
        }
        None => vec![
            (
                loc.tr("div_order.advance").into_owned(),
                IconId::Advance,
                GOLD,
                Some(PlayerCommand::DivOrder {
                    division: division.clone(),
                    pick: DivOrderPick::Advance,
                }),
                "div_order.advance.tooltip",
            ),
            (
                loc.tr("div_order.seize").into_owned(),
                IconId::Flag,
                GOLD,
                Some(PlayerCommand::DivPick {
                    division: division.clone(),
                    kind: DivPickKind::Seize,
                }),
                "div_order.seize.tooltip",
            ),
            (
                loc.tr("div_order.engage").into_owned(),
                IconId::Target,
                GOLD,
                Some(PlayerCommand::DivPick {
                    division: division.clone(),
                    kind: DivPickKind::Engage,
                }),
                "div_order.engage.tooltip",
            ),
        ],
    };

    // 占领/歼敌 arm the map pick; 推进 issues immediately.
    let n = items.len();
    let bar_w = (n as f32) * 84.0 + 8.0;
    let bar_center = egui::pos2(
        center.x.clamp(
            bar_w / 2.0 + 10.0,
            (screen.width() - bar_w / 2.0 - 10.0).max(bar_w / 2.0 + 10.0),
        ),
        center.y - 58.0,
    );
    // An ALLIED HQ's bar carries a provenance badge above it —
    // division orders are accepted here, but the battalions otherwise fight
    // under their own staff's control.
    if let Some(ally) = game.allied_division(&division) {
        let pos = egui::pos2(
            bar_center.x.clamp(10.0, (screen.width() - 10.0).max(10.0)),
            (bar_center.y - 30.0).clamp(10.0, (screen.height() - 10.0).max(10.0)),
        );
        egui::Area::new(egui::Id::new("div_cmd_bar_ally"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(egui::Align2::CENTER_CENTER)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(loc.trf("ui.allies.badge", &[("tag", &ally.tag)]))
                        .small()
                        .strong()
                        .color(GOLD),
                )
                .on_hover_text(loc.tr("ui.allies.hq_bar_tip"));
            });
    }
    for (i, (text, icon, color, cmd, tip_key)) in items.iter().enumerate() {
        let x = bar_center.x + (i as f32 - (n as f32 - 1.0) / 2.0) * 84.0;
        let pos = egui::pos2(
            x.clamp(10.0, (screen.width() - 10.0).max(10.0)),
            bar_center.y.clamp(10.0, (screen.height() - 10.0).max(10.0)),
        );
        let id = egui::Id::new(("div_cmd_bar", i));
        egui::Area::new(id)
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(egui::Align2::CENTER_CENTER)
            .show(ctx, |ui| {
                let label = if *icon == IconId::Cross {
                    format!("X {}", text)
                } else {
                    text.clone()
                };
                let btn =
                    egui::Button::new(egui::RichText::new(label).strong().size(13.0).color(*color))
                        .min_size(egui::vec2(78.0, 26.0))
                        .rounding(4.0)
                        .fill(egui::Color32::from_rgb(40, 36, 26))
                        .stroke(egui::Stroke::new(1.5_f32, *color));
                // Center the label inside the square button: egui Button
                // places its text with the CURRENT layout's alignment (the
                // Area default is left-top), so add it under a centered
                // layout of exactly the button size.
                let resp = ui
                    .allocate_ui_with_layout(
                        egui::vec2(78.0, 26.0),
                        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                        |ui| ui.add(btn),
                    )
                    .inner
                    .on_hover_text(loc.tr(tip_key));
                if resp.clicked() {
                    if let Some(cmd) = cmd {
                        pending.0.push(cmd.clone());
                    }
                }
            });
    }
}

// ---------------------------------------------------------------------------
// Top-center notice bar: one-shot event flashes take priority;
// otherwise the phase-driven pinned hint shows. Doubles as the template for
// future tutorial / ops hints — new notices just write `notice.flash`.
// ---------------------------------------------------------------------------

fn draw_notice_bar(
    mut contexts: EguiContexts,
    game: Option<Res<GameController>>,
    notice: Res<NoticeBar>,
    colors: Option<Res<SideColors>>,
    time: Res<Time>,
    state: Option<Res<TacticalState>>,
    tour: Res<BattleTour>,
    loc: Res<LocaleRes>,
    mut anchor: Local<CenterAnchor>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    let Some(game) = game else { return };
    let now = time.elapsed_secs();
    let flash = notice.flash.as_ref().filter(|f| f.until > now);
    // The enemy-turn banner stays pinned for the WHOLE enemy turn: once the
    // 3 s handover flash expires, a phase-driven banner below carries it
    // until the side flips back. Held back while the battle-report tour
    // plays — the same rule as the flash notices.
    let enemy_turn = game.session.phase == BattlePhase::TacticalActive
        && !tour.active
        && state
            .as_ref()
            .is_some_and(|s| game.session.current_side != s.player_side);
    // Flash notices fade out over their final half second — except
    // the pinned enemy-turn banner, which holds full opacity until the side
    // actually flips (no fade-out → pop-back-in at the 3 s mark).
    let fade = flash
        .map(|f| {
            if enemy_turn && f.side == Some(game.session.current_side) {
                1.0
            } else {
                ((f.until - now) / 0.5).clamp(0.0, 1.0)
            }
        })
        .unwrap_or(1.0);
    // Pinned continuation of the enemy-turn banner after the flash expires.
    let pinned;
    let flash = match flash {
        Some(f) => Some(f),
        None if enemy_turn => {
            pinned = FlashNotice::turn(
                loc.tr("notice.turn.enemy").into_owned(),
                game.session.current_side,
                now,
            );
            Some(&pinned)
        }
        None => None,
    };
    let area = egui::Area::new(egui::Id::new("notice_bar")).order(egui::Order::Foreground);
    let resp = anchor
        .area(ctx, area, egui::Align2::CENTER_TOP, egui::vec2(0.0, 6.0))
        .show(ctx, |ui| {
            if let Some((f, side)) = flash.and_then(|f| f.side.map(|s| (f, s))) {
                // Faction-colored turn banner: side-color background
                // (darkened for contrast) + thick side-color border, so turn
                // handovers are unmistakable next to plain parchment notices.
                let c = colors
                    .as_ref()
                    .map(|cc| cc.for_side(side))
                    .unwrap_or([0.5, 0.5, 0.5, 1.0]);
                let a = |v: f32| (v * fade).round() as u8;
                let bg = egui::Color32::from_rgba_unmultiplied(
                    (c[0] * 140.0) as u8,
                    (c[1] * 140.0) as u8,
                    (c[2] * 140.0) as u8,
                    a(240.0),
                );
                let border = egui::Color32::from_rgba_unmultiplied(
                    (c[0] * 255.0) as u8,
                    (c[1] * 255.0) as u8,
                    (c[2] * 255.0) as u8,
                    a(255.0),
                );
                egui::Frame::none()
                    .fill(bg)
                    .stroke(egui::Stroke::new(2.5_f32, border))
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                    .rounding(egui::Rounding::same(3.0))
                    .show(ui, |ui| {
                        // Wide enough that the banner text never wraps.
                        ui.set_min_width(360.0);
                        ui.vertical_centered(|ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&f.text).strong().size(16.0).color(
                                        egui::Color32::from_rgba_unmultiplied(
                                            255,
                                            255,
                                            255,
                                            a(255.0),
                                        ),
                                    ),
                                )
                                .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                    });
                return;
            }
            let text: std::borrow::Cow<str> = if let Some(f) = flash {
                std::borrow::Cow::Borrowed(f.text.as_str())
            } else {
                match game.session.phase {
                    BattlePhase::Deployment => {
                        // Deployment flow: units start in the
                        // OOB — the pinned hint teaches the flow while
                        // any are still waiting. Only the units
                        // the player commands count — allied battalions wait
                        // for their own staff, so they must not pin the
                        // "place your units" hint.
                        let n = state
                            .as_ref()
                            .map(|s| {
                                s.units
                                    .iter()
                                    .filter(|u| {
                                        game.commands(u) && u.undeployed && u.is_combat_effective()
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        if n > 0 {
                            loc.tr("battle.hint.deploy_place")
                        } else {
                            loc.tr("battle.hint.deploy_drag")
                        }
                    }
                    BattlePhase::ReadyToSync => loc.tr("battle.hint.sync_ready"),
                    BattlePhase::ReadyToEnd => loc.tr("battle.hint.battle_resolved"),
                    _ => return,
                }
            };
            hoi4_frame().show(ui, |ui| {
                // Never wrap — a narrow first-frame Area measurement
                // otherwise gets remembered and pins the hint as a vertical
                // one-character-per-line strip.
                ui.add(
                    egui::Label::new(egui::RichText::new(text).strong().color(PARCHMENT))
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
        });
    anchor.update(Some(resp.response.rect), ctx.pixels_per_point());
}

// ---------------------------------------------------------------------------
// Esc global menu: modal — Continue / Settings (in-battle render
// options) / Reset Battle… (merged Restart + To Last Sync, they
// never coexist) / Exit Game. The Game Log entry lives in the right panel
// (below Order of Battle).
// ---------------------------------------------------------------------------

fn draw_esc_menu(
    mut contexts: EguiContexts,
    game: Option<Res<GameController>>,
    state: Option<Res<TacticalState>>,
    checkpoints: Res<crate::game::Checkpoints>,
    loc: Res<LocaleRes>,
    icons: Res<IconSet>,
    mut ui_win: ResMut<UiWindows>,
    mut pending: ResMut<PendingCommands>,
    // CenterAnchor state for this system's four centered windows.
    mut anchors: Local<EscAnchors>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    // The recall confirmation is independent of the Esc menu —
    // it is triggered from the right panel ([Recall All]) or the OOB rows.
    if let Some(kind) = ui_win.confirm_recall.clone() {
        let player = state
            .as_ref()
            .map(|s| s.player_side)
            .unwrap_or(Side::Attacker);
        let n = state
            .as_ref()
            .map(|s| {
                s.units
                    .iter()
                    .filter(|u| {
                        u.side == player
                            && !u.undeployed
                            && u.is_combat_effective()
                            && match &kind {
                                RecallKind::All => true,
                                RecallKind::Division(d) => &u.division == d,
                            }
                    })
                    .count()
            })
            .unwrap_or(0);
        let title = match &kind {
            RecallKind::All => loc.tr("battle.button.recall_all").into_owned(),
            RecallKind::Division(d) => {
                let name = if d.is_empty() {
                    loc.tr("oob.unattached").into_owned()
                } else {
                    d.clone()
                };
                loc.trf("battle.dialog.recall_title", &[("name", &name)])
            }
        };
        let win = egui::Window::new(title).collapsible(false).resizable(false);
        let resp = anchors
            .recall
            .window(
                ctx,
                win,
                egui::Align2::CENTER_CENTER,
                egui::vec2(0.0, -60.0),
            )
            .show(ctx, |ui| {
                ui.label(loc.trf("battle.dialog.recall_body", &[("n", &n.to_string())]));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(loc.tr("common.confirm").as_ref()).clicked() {
                        match &kind {
                            RecallKind::All => {
                                pending.0.push(PlayerCommand::RecallAll);
                            }
                            RecallKind::Division(d) => {
                                pending.0.push(PlayerCommand::RecallDivision(d.clone()));
                            }
                        }
                        ui_win.confirm_recall = None;
                    }
                    if ui.button(loc.tr("common.cancel").as_ref()).clicked() {
                        ui_win.confirm_recall = None;
                    }
                });
            });
        anchors
            .recall
            .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
    }
    if !ui_win.esc_menu {
        ui_win.confirm_reset = false;
        ui_win.confirm_exit = false;
        return;
    }
    let Some(game) = game else { return };

    // Dim the battlefield behind the modal menu.
    egui::Area::new(egui::Id::new("esc_dim"))
        .order(egui::Order::Middle)
        .fixed_pos(egui::pos2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_black_alpha(160));
        });

    // Checkpoint rules, same as the old panel buttons: a full
    // restart is only safe before the first sync; afterwards the newest
    // rollback point is the last completed sync. The two never coexist —
    // hence the single merged "Reset Battle…" entry.
    let can_restart = checkpoints.battle_start.is_some()
        && game.session.strategic_hour == 0
        && matches!(
            game.session.phase,
            BattlePhase::Deployment
                | BattlePhase::TacticalActive
                | BattlePhase::ReadyToSync
                | BattlePhase::ReadyToEnd
        );
    let can_rollback =
        checkpoints.last_sync.is_some() && game.session.phase != BattlePhase::Deployment;

    let win = egui::Window::new(loc.tr("app.title").as_ref())
        .collapsible(false)
        .resizable(false);
    let resp = anchors
        .main
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let btn = |ui: &mut egui::Ui, icon: Option<IconId>, text: &str, hover: &str| {
                tip(
                    ui.add(
                        icons
                            .button(icon, egui::RichText::new(text).size(15.0), 15.0)
                            .min_size(egui::vec2(200.0, 28.0)),
                    ),
                    hover,
                )
            };
            if btn(
                ui,
                None,
                loc.tr("common.continue").as_ref(),
                loc.tr("battle.tooltip.esc_continue").as_ref(),
            )
            .clicked()
            {
                ui_win.esc_menu = false;
            }
            ui.add_space(6.0);
            ui.add_enabled_ui(can_restart || can_rollback, |ui| {
                if btn(
                    ui,
                    Some(IconId::Reset),
                    loc.tr("battle.button.reset_battle").as_ref(),
                    loc.tr("battle.tooltip.reset_battle").as_ref(),
                )
                .clicked()
                {
                    ui_win.confirm_reset = true;
                }
            });
            ui.add_space(6.0);
            // In-battle render/performance settings — a separate
            // surface, so the Esc menu closes behind it (Esc peels the
            // settings window first, the ladder re-opens this menu).
            if btn(
                ui,
                Some(IconId::Gear),
                loc.tr("battle.button.settings").as_ref(),
                loc.tr("battle.tooltip.settings").as_ref(),
            )
            .clicked()
            {
                // Defer one frame (UiWindows::settings_open_delay): opening
                // right here would draw both centered modals on THIS frame —
                // the overlap is the open-transition "jitter".
                ui_win.settings_open_delay = 2;
                ui_win.esc_menu = false;
            }
            ui.add_space(6.0);
            if btn(
                ui,
                Some(IconId::Door),
                loc.tr("menu.main.exit_game").as_ref(),
                loc.tr("battle.tooltip.exit_game").as_ref(),
            )
            .clicked()
            {
                // Live mode (a real province is bound) with an unfinished
                // battle: confirm before abandoning an unsynced engagement.
                // Demo/debug exits straight away. Only the ENDED phase exits
                // plainly — every other live phase routes
                // the abort cleanup (ReadyToEnd included: exiting without
                // applying discards the result but still cleans up HOI4).
                let live_unfinished =
                    game.province != 0 && !matches!(game.session.phase, BattlePhase::Ended);
                if live_unfinished {
                    ui_win.confirm_exit = true;
                } else {
                    pending.0.push(PlayerCommand::ExitGame);
                }
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(loc.tr("battle.hint.esc_continue").as_ref())
                    .small()
                    .weak(),
            );
        });
    anchors.main.update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());

    // Confirmation dialogs (drawn after the menu → on top).
    if ui_win.confirm_reset {
        let win = egui::Window::new(loc.tr("battle.dialog.reset_title").as_ref())
            .collapsible(false)
            .resizable(false);
        let resp = anchors
            .reset
            .window(
                ctx,
                win,
                egui::Align2::CENTER_CENTER,
                egui::vec2(0.0, -60.0),
            )
            .show(ctx, |ui| {
                let text = if can_restart {
                    loc.tr("battle.dialog.reset_start").into_owned()
                } else {
                    loc.trf(
                        "battle.dialog.reset_sync",
                        &[("hour", &game.session.strategic_hour.to_string())],
                    )
                };
                ui.label(text);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(loc.tr("common.confirm").as_ref()).clicked() {
                        if can_restart {
                            pending.0.push(PlayerCommand::RestartBattle);
                        } else {
                            pending.0.push(PlayerCommand::RollbackToSync);
                        }
                        ui_win.confirm_reset = false;
                        ui_win.esc_menu = false;
                    }
                    if ui.button(loc.tr("common.cancel").as_ref()).clicked() {
                        ui_win.confirm_reset = false;
                    }
                });
            });
        anchors.reset.update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
    }
    if ui_win.confirm_exit {
        let win = egui::Window::new(loc.tr("menu.main.exit_game").as_ref())
            .collapsible(false)
            .resizable(false);
        let resp = anchors
            .exit
            .window(
                ctx,
                win,
                egui::Align2::CENTER_CENTER,
                egui::vec2(0.0, -60.0),
            )
            .show(ctx, |ui| {
                icons.label_with_icon(
                    ui,
                    IconId::Warning,
                    loc.tr("battle.dialog.exit_unsynced1").as_ref(),
                    15.0,
                );
                ui.label(loc.tr("battle.dialog.exit_unsynced2").as_ref());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(icons.button(Some(IconId::Door), loc.tr("common.exit").as_ref(), 14.0))
                        .clicked()
                    {
                        // Live battles route the abort cleanup
                        // (non-live exits plainly inside the handler).
                        pending.0.push(PlayerCommand::AbortLiveExit);
                        ui_win.confirm_exit = false;
                        ui_win.esc_menu = false;
                    }
                    if ui.button(loc.tr("common.cancel").as_ref()).clicked() {
                        ui_win.confirm_exit = false;
                    }
                });
            });
        anchors.exit.update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
    }
}

// ---------------------------------------------------------------------------
// In-battle settings window: Esc menu → Settings.
// Render/performance knobs only — MSAA / shadows / render scale / frame cap /
// idle frame-saver — a subset of the menu Settings page (menu-only rows such
// as the menu frame cap are omitted). Writes `BattleSettings`; the render
// crate hot-applies (settings.rs) and the bin crate persists to
// settings.json — both through change detection, no extra wiring here.
// ---------------------------------------------------------------------------

fn draw_settings_window(
    mut contexts: EguiContexts,
    loc: Res<LocaleRes>,
    settings: Option<ResMut<crate::settings::BattleSettings>>,
    mut ui_win: ResMut<UiWindows>,
    // CenterAnchor state against the ±1 px anchor-snapping limit cycle
    // (see CenterAnchor below).
    mut anchor: Local<CenterAnchor>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    // Esc → Settings handoff countdown (see UiWindows::settings_open_delay):
    // the click pass only closes the Esc menu; the settings window opens on
    // the NEXT pass, so the two centered modals never share a frame.
    if ui_win.settings_open_delay > 0 {
        ui_win.settings_open_delay -= 1;
        if ui_win.settings_open_delay == 0 {
            ui_win.settings = true;
        }
    }
    if !ui_win.settings {
        return;
    }
    let Some(mut s) = settings else {
        // No BattleSettings resource (a window mode without in-battle
        // settings) — never strand an empty window.
        ui_win.settings = false;
        return;
    };
    let mut open = true;
    let win = egui::Window::new(loc.tr("menu.main.settings").as_ref())
        .open(&mut open)
        .collapsible(false)
        .resizable(false);
    let resp = anchor
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(loc.tr("menu.settings.msaa").as_ref())
                    .on_hover_text(loc.tr("menu.tooltip.msaa").as_ref());
                for (val, text) in [
                    (4u32, "4×".to_string()),
                    (2, "2×".to_string()),
                    (1, loc.tr("menu.settings.msaa.off").into_owned()),
                ] {
                    if ui.selectable_label(s.msaa == val, text).clicked() && s.msaa != val {
                        s.msaa = val;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(loc.tr("menu.settings.shadow").as_ref())
                    .on_hover_text(loc.tr("menu.tooltip.shadow").as_ref());
                for (val, key) in [
                    (2u32, "menu.settings.shadow.high"),
                    (1, "menu.settings.shadow.low"),
                    (0, "menu.settings.shadow.off"),
                ] {
                    if ui
                        .selectable_label(s.shadow_level == val, loc.tr(key).as_ref())
                        .clicked()
                        && s.shadow_level != val
                    {
                        s.shadow_level = val;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(loc.tr("menu.settings.render_scale").as_ref())
                    .on_hover_text(loc.tr("menu.tooltip.render_scale").as_ref());
                for val in [100u32, 85, 70, 50] {
                    if ui
                        .selectable_label(s.render_scale == val, format!("{val}%"))
                        .clicked()
                        && s.render_scale != val
                    {
                        s.render_scale = val;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(loc.tr("menu.settings.max_fps").as_ref())
                    .on_hover_text(loc.tr("menu.tooltip.max_fps").as_ref());
                for val in [144u32, 120, 90, 60, 30, 0] {
                    let text = if val == 0 {
                        loc.tr("menu.settings.max_fps.uncapped").into_owned()
                    } else {
                        val.to_string()
                    };
                    if ui.selectable_label(s.max_fps == val, text).clicked() && s.max_fps != val {
                        s.max_fps = val;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(loc.tr("menu.settings.low_power").as_ref())
                    .on_hover_text(loc.tr("menu.tooltip.low_power").as_ref());
                let mut lp = s.low_power;
                if ui.checkbox(&mut lp, "").changed() {
                    s.low_power = lp;
                }
            });
        });
    // Store this frame's measured rect (CenterAnchor).
    anchor.update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
    if !open {
        ui_win.settings = false;
    }
}

// ---------------------------------------------------------------------------
// CenterAnchor — anti-jitter anchoring for auto-size egui windows/areas
// (the settings window micro-jitters horizontally without it).
//
// Probe-verified mechanism: egui's `anchor(Align2::CENTER_*)` computes the
// position from the content size EVERY frame and feeds the previous frame's
// pixel-snapped position back into the next frame's rounding. At a
// fractional DPI scale (150% etc.) a centered edge can land exactly on a
// .5-physical-px boundary — the rounding then flip-flops every frame: a
// self-sustaining period-2 limit cycle, the window oscillating ±1 physical
// px forever (width/height/y constant; invisible in static screenshots).
// Whether a given window hits the cycle is a per-machine lottery (screen
// size × DPI × content width parity), so EVERY per-frame center/right
// anchored surface is in the class.
//
// The primitive: keep the LAST measured size, compute the anchored position
// ourselves from (screen, size, align, offset), snap it to physical pixels,
// and place the window with `fixed_pos`. The position is then a pure
// function of honest inputs — no snapped-position feedback, so the cycle
// cannot exist; a constant-size window never moves, while a size change
// (new text, language switch) still re-centers on the next frame, matching
// egui's native behaviour. egui's own anchor is used only until the first
// measurement exists (freshly-opened windows settle there).
//
// Size channel (the stall-dialog jitter): fixing the position does not help
// when the MEASURED size itself flip-flops ±1 physical px at fractional DPI
// (the re-centring follows the size and oscillates with it). `update`
// therefore pixel-aligns the stored size and LATCHES it against ±1
// physical-px changes — the parity flip-flop never reaches the position,
// while real content changes (a whole text line ≫ 1 px) pass through.
// ---------------------------------------------------------------------------

/// Per-surface anchor state (one per window/area, `Local` or a state field).
#[derive(Default, Clone, Copy)]
pub struct CenterAnchor {
    /// Last measured size in egui points (drives the position next frame).
    size: Option<egui::Vec2>,
}

impl CenterAnchor {
    /// Position an egui Window at `align + offset` (see the module comment).
    pub fn window<'open>(
        &mut self,
        ctx: &egui::Context,
        win: egui::Window<'open>,
        align: egui::Align2,
        offset: egui::Vec2,
    ) -> egui::Window<'open> {
        match self.pos(ctx.screen_rect(), ctx.pixels_per_point(), align, offset) {
            Some(p) => win.fixed_pos(p),
            None => win.anchor(align, offset),
        }
    }

    /// Position an egui Area at `align + offset` (same contract).
    pub fn area(
        &mut self,
        ctx: &egui::Context,
        area: egui::Area,
        align: egui::Align2,
        offset: egui::Vec2,
    ) -> egui::Area {
        match self.pos(ctx.screen_rect(), ctx.pixels_per_point(), align, offset) {
            Some(p) => area.fixed_pos(p),
            None => area.anchor(align, offset),
        }
    }

    /// Store the freshly measured rect (call with the shown response rect +
    /// the context's pixels-per-point). Pixel-aligns the size and latches
    /// it against ±1 physical-px parity flip-flops (see the module comment).
    pub fn update(&mut self, rect: Option<egui::Rect>, ppp: f32) {
        let Some(r) = rect else { return };
        let phys = r.size() * ppp;
        if let Some(old) = self.size {
            let old_phys = old * ppp;
            if (phys.x - old_phys.x).abs() <= 1.0 && (phys.y - old_phys.y).abs() <= 1.0 {
                return;
            }
        }
        self.size = Some(phys.round() / ppp);
    }

    /// Pixel-snapped left-top position implied by (screen, size, align,
    /// offset); None before the first measurement.
    fn pos(
        &self,
        screen: egui::Rect,
        ppp: f32,
        align: egui::Align2,
        offset: egui::Vec2,
    ) -> Option<egui::Pos2> {
        let size = self.size?;
        let anchor_pt = align.pos_in_rect(&screen);
        // Align fraction per axis (0.0 / 0.5 / 1.0) from the unit mapping.
        let fx = (anchor_pt.x - screen.min.x) / screen.width().max(1.0);
        let fy = (anchor_pt.y - screen.min.y) / screen.height().max(1.0);
        let raw = egui::pos2(
            anchor_pt.x - fx * size.x + offset.x,
            anchor_pt.y - fy * size.y + offset.y,
        );
        Some(egui::pos2(
            (raw.x * ppp).round() / ppp,
            (raw.y * ppp).round() / ppp,
        ))
    }
}

/// The Esc-menu family's four centered windows each get their own anchor
/// (recall confirm, main menu, reset confirm, exit confirm); the in-battle
/// settings window keeps its own in `draw_settings_window`.
#[derive(Default)]
struct EscAnchors {
    main: CenterAnchor,
    recall: CenterAnchor,
    reset: CenterAnchor,
    exit: CenterAnchor,
}

// ---------------------------------------------------------------------------
// Standalone battle-log window: opened from the right panel's
// Battle Log button (below Order of Battle — moved out of the Esc menu),
// and auto-opened by battle end (sync summaries use their own
// modal instead).
// ---------------------------------------------------------------------------

fn draw_log_window(
    mut contexts: EguiContexts,
    game: Option<Res<GameController>>,
    icons: Res<crate::icons::IconSet>,
    loc: Res<LocaleRes>,
    mut ui_win: ResMut<UiWindows>,
    mut detail: ResMut<DetailWindow>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    if !ui_win.log {
        return;
    }
    let Some(game) = game else { return };
    let mut open = true;
    let mut open_detail: Option<EngagementDetailView> = None;
    egui::Window::new(loc.tr("battle.panel.battle_log").as_ref())
        .open(&mut open)
        .default_size([440.0, 320.0])
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &game.log {
                        if let Some(dv) = &line.detail {
                            // Exchange/counter lines are clickable
                            // — they open the engagement-detail window.
                            let resp = ui
                                .horizontal(|ui| {
                                    if let Some(id) = line.icon {
                                        icons.icon(ui, id, 16.0);
                                    }
                                    ui.add(
                                        egui::Label::new(&line.text)
                                            .sense(egui::Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text(loc.tr("detail.click_hint").as_ref())
                                })
                                .inner;
                            if resp.clicked() {
                                open_detail = Some((**dv).clone());
                            }
                            continue;
                        }
                        // §15: LogLine carries an optional leading icon.
                        match line.icon {
                            Some(id) => {
                                ui.horizontal(|ui| {
                                    icons.icon(ui, id, 16.0);
                                    ui.label(&line.text);
                                });
                            }
                            None => {
                                ui.label(&line.text);
                            }
                        }
                    }
                });
        });
    if let Some(v) = open_detail {
        detail.0 = Some(v);
    }
    if !open {
        ui_win.log = false;
    }
}

// ---------------------------------------------------------------------------
// Order of Battle window: division → battalion tree for the
// player's side; click a battalion to select it AND pan the camera there.
// Destroyed battalions stay listed, greyed and struck through.
// ---------------------------------------------------------------------------

fn draw_oob_window(
    mut contexts: EguiContexts,
    game: Option<Res<GameController>>,
    mut state: ResMut<TacticalState>,
    colors: Res<SideColors>,
    loc: Res<LocaleRes>,
    mut ui_win: ResMut<UiWindows>,
    mut focus: ResMut<crate::camera::CameraFocusReq>,
    mut pending: ResMut<PendingCommands>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    if !ui_win.oob {
        return;
    }
    if game.is_none() {
        return;
    }
    // Attachment re-assignment is a Deployment-phase action.
    let deploying = game
        .as_ref()
        .map(|g| g.session.phase == BattlePhase::Deployment)
        .unwrap_or(false);
    let mut open = true;
    egui::Window::new(loc.tr("battle.button.oob").as_ref())
        .open(&mut open)
        .default_size([300.0, 380.0])
        .show(ctx, |ui| {
            let player = state.player_side;
            // Sector picking in progress — tell the player what
            // to drag (same pattern as the other picking modes).
            if let Some(pick) = &state.deploy_sector {
                let div = if pick.division.is_empty() {
                    loc.tr("oob.unattached").into_owned()
                } else {
                    pick.division.clone()
                };
                ui.label(
                    egui::RichText::new(loc.trf("battle.oob.sector_pick", &[("div", &div)]))
                        .color(GOLD)
                        .small(),
                );
            }
            // OOB placement in progress — tell the player what
            // to click next (same pattern as the attachment reassignment).
            if let Some(id) = state.deploy_placing {
                let label = state
                    .unit_by_id(id)
                    .map(|u| loc.trf("battle.oob.placing", &[("name", &u.name)]))
                    .unwrap_or_default();
                if !label.is_empty() {
                    ui.label(egui::RichText::new(label).color(GOLD).small());
                }
            }
            // Re-assignment in progress: tell the player what to click next.
            if let Some((host, idx)) = state.attach_picking {
                let label = state
                    .unit_by_id(host)
                    .and_then(|u| u.support.get(idx))
                    .map(|a| loc.trf("battle.oob.reassigning", &[("name", &a.name)]))
                    .unwrap_or_default();
                if !label.is_empty() {
                    ui.label(egui::RichText::new(label).color(GOLD).small());
                }
            }
            // §6.13: per-unit command link for the HQ status
            // segment in each division header below (unit id → link).
            let cmd_links: std::collections::HashMap<usize, tactical_core::CommandLink> = game
                .as_ref()
                .map(|g| {
                    let links =
                        tactical_core::compute_command_links(&state.units, g.combat.params());
                    state
                        .units
                        .iter()
                        .enumerate()
                        .map(|(i, u)| (u.id, links[i]))
                        .collect()
                })
                .unwrap_or_default();
            // Group own battalions by division, first-appearance order.
            // The group key doubles as the sector-pick / recall identifier,
            // so the "(unattached)" stand-in is the localized string (same
            // sentinel semantics as before — comparisons stay internal).
            let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
            for u in state.units.iter().filter(|u| u.side == player) {
                let key = if u.division.is_empty() {
                    loc.tr("oob.unattached").into_owned()
                } else {
                    u.division.clone()
                };
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v.push(u.id),
                    None => groups.push((key, vec![u.id])),
                }
            }
            // Player-commanded divisions come first (DESIGN §7.5;
            // first-appearance order, unchanged); each ALLIED contingent
            // then gets its own section in `game.allies` order, introduced
            // by a tag header line. The section tag rides along as Option.
            let mut own: Vec<(String, Vec<usize>)> = Vec::new();
            let mut allied: Vec<(String, Vec<usize>)> = Vec::new();
            for (key, ids) in groups {
                let is_ally = game
                    .as_ref()
                    .map(|g| g.allied_division(&key).is_some())
                    .unwrap_or(false);
                if is_ally {
                    allied.push((key, ids));
                } else {
                    own.push((key, ids));
                }
            }
            let mut ordered: Vec<(Option<String>, String, Vec<usize>)> =
                own.into_iter().map(|(d, ids)| (None, d, ids)).collect();
            let allies_list = game.as_ref().map(|g| g.allies.clone()).unwrap_or_default();
            for ally in &allies_list {
                let mut header = Some(ally.tag.clone());
                let mut i = 0;
                while i < allied.len() {
                    if ally.divisions.contains(&allied[i].0) {
                        let (div, ids) = allied.remove(i);
                        ordered.push((header.take(), div, ids));
                    } else {
                        i += 1;
                    }
                }
            }
            // An allied division no contingent claims (should not happen)
            // trails at the end without a header.
            ordered.extend(allied.into_iter().map(|(d, ids)| (None, d, ids)));
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (section_tag, div, ids) in ordered {
                        if let Some(tag) = &section_tag {
                            ui.label(
                                egui::RichText::new(loc.trf("ui.allies.section", &[("tag", tag)]))
                                    .strong()
                                    .color(GOLD),
                            );
                        }
                        let effective = ids
                            .iter()
                            .filter(|id| {
                                state
                                    .unit_by_id(**id)
                                    .map(|u| u.is_combat_effective())
                                    .unwrap_or(false)
                            })
                            .count();
                        // Undeployed count per division feeds the
                        // "deploy to sector" affordance below.
                        let undep = ids
                            .iter()
                            .filter(|id| {
                                state
                                    .unit_by_id(**id)
                                    .map(|u| u.undeployed)
                                    .unwrap_or(false)
                            })
                            .count();
                        // §6.13: HQ status segment — org% +
                        // command coverage while the HQ commands; red LOST
                        // once it is gone (`cmd_links` maps unit id → link).
                        let hq = ids
                            .iter()
                            .filter_map(|id| state.unit_by_id(*id))
                            .find(|u| u.is_hq());
                        let (hq_seg, hq_lost) = match hq {
                            Some(h) if h.is_combat_effective() => {
                                if h.undeployed {
                                    (loc.tr("battle.oob.hq_undeployed").into_owned(), false)
                                } else {
                                    let (mut n, mut total) = (0usize, 0usize);
                                    for id in &ids {
                                        let Some(u) = state.unit_by_id(*id) else {
                                            continue;
                                        };
                                        if u.is_hq() || !u.is_combat_effective() || u.undeployed {
                                            continue;
                                        }
                                        total += 1;
                                        if cmd_links
                                            .get(id)
                                            .is_some_and(|l| tactical_core::in_command(*l))
                                        {
                                            n += 1;
                                        }
                                    }
                                    // Aura radius reflects a signal company
                                    // riding the HQ (§6.13: 3 → 6 km).
                                    let r = game
                                        .as_ref()
                                        .map(|g| {
                                            tactical_core::aura_radius_of(h, g.combat.params())
                                        })
                                        .unwrap_or(0);
                                    (
                                        loc.trf(
                                            "battle.oob.hq_status",
                                            &[
                                                ("org", &format!("{:.1}/{:.1}", h.org, h.max_org)),
                                                ("n", &n.to_string()),
                                                ("total", &total.to_string()),
                                                ("r", &r.to_string()),
                                            ],
                                        ),
                                        false,
                                    )
                                }
                            }
                            Some(_) => (loc.tr("battle.oob.hq_lost").into_owned(), true),
                            None => (String::new(), false),
                        };
                        let mut title = loc.trf(
                            "battle.oob.division_header",
                            &[
                                ("div", &div),
                                ("effective", &effective.to_string()),
                                ("total", &ids.len().to_string()),
                            ],
                        );
                        if !hq_seg.is_empty() {
                            title.push_str("  ");
                            title.push_str(&hq_seg);
                        }
                        // The standing division-order badge — its
                        // kind name in brackets — plus the cancel affordance
                        // in the header row (works even after the HQ fell:
                        // the OOB header is the guaranteed cancel path).
                        let div_order = game.as_ref().and_then(|g| g.div_orders.get(&div));
                        if let Some(o) = div_order {
                            title.push_str("  [");
                            title.push_str(&loc.tr(o.kind_key()));
                            title.push(']');
                        }
                        let title = if hq_lost {
                            egui::RichText::new(title).color(egui::Color32::from_rgb(255, 90, 70))
                        } else {
                            egui::RichText::new(title)
                        };
                        let mut order_cancel = false;
                        ui.horizontal(|ui| {
                            egui::CollapsingHeader::new(title)
                                .default_open(true)
                                .show(ui, |ui| {
                                    // Sector deployment: hand ONE
                                    // division's waiting battalions to a
                                    // player-drawn rectangle on the map. The
                                    // Recall neighbour returns the division's
                                    // placed battalions to the OOB (confirmed).
                                    if deploying {
                                        let placed = ids.len() - undep;
                                        let picking = state
                                            .deploy_sector
                                            .as_ref()
                                            .map(|s| s.division == div)
                                            .unwrap_or(false);
                                        // For an ALLIED division the
                                        // deploy drag stores a sector SUGGESTION
                                        // (its own AI deploys at BeginBattle);
                                        // recall then clears the suggestion.
                                        let allied = game
                                            .as_ref()
                                            .map(|g| g.allied_division(&div).is_some())
                                            .unwrap_or(false);
                                        let suggestion = allied
                                            && game
                                                .as_ref()
                                                .map(|g| g.allied_sectors.contains_key(&div))
                                                .unwrap_or(false);
                                        let mut deploy_clicked = false;
                                        let mut recall_clicked = false;
                                        ui.horizontal(|ui| {
                                            ui.add_enabled_ui(undep > 0, |ui| {
                                                let text = if picking {
                                                    loc.tr("battle.oob.deploying").into_owned()
                                                } else {
                                                    loc.trf(
                                                        "battle.oob.deploy_n",
                                                        &[("n", &undep.to_string())],
                                                    )
                                                };
                                                deploy_clicked = tip(
                                                    ui.add(
                                                        egui::Button::new(
                                                            egui::RichText::new(text).small(),
                                                        )
                                                        .fill(if picking {
                                                            egui::Color32::from_rgb(72, 90, 110)
                                                        } else {
                                                            egui::Color32::from_rgb(48, 44, 34)
                                                        })
                                                        .stroke(egui::Stroke::new(
                                                            1.0_f32,
                                                            if picking { GOLD } else { BRASS },
                                                        )),
                                                    ),
                                                    // For an allied
                                                    // division the rectangle is
                                                    // only a SUGGESTION for its
                                                    // own staff's deploy AI.
                                                    if allied {
                                                        loc.tr("ui.allies.sector_tip")
                                                    } else {
                                                        loc.tr("battle.tooltip.deploy_sector")
                                                    },
                                                )
                                                .clicked();
                                            });
                                            ui.add_enabled_ui(placed > 0 || suggestion, |ui| {
                                                recall_clicked = tip(
                                                    ui.add(
                                                        egui::Button::new(
                                                            egui::RichText::new(loc.trf(
                                                                "battle.oob.recall_n",
                                                                &[("n", &placed.to_string())],
                                                            ))
                                                            .small(),
                                                        )
                                                        .fill(egui::Color32::from_rgb(48, 44, 34))
                                                        .stroke(egui::Stroke::new(1.0_f32, BRASS)),
                                                    ),
                                                    loc.tr("battle.tooltip.recall_div"),
                                                )
                                                .clicked();
                                            });
                                        });
                                        if deploy_clicked {
                                            if picking {
                                                state.deploy_sector = None;
                                                state.clear_highlights();
                                            } else {
                                                state.deploy_sector =
                                                    Some(crate::state::SectorPick {
                                                        division: div.clone(),
                                                        anchor: None,
                                                    });
                                                // Sector picking owns the mouse —
                                                // drop any pending OOB placement.
                                                state.deploy_placing = None;
                                            }
                                        }
                                        if recall_clicked {
                                            state.deploy_sector = None;
                                            state.clear_highlights();
                                            if allied {
                                                // Clearing a hint destroys
                                                // nothing — no confirmation.
                                                pending.0.push(PlayerCommand::RecallDivision(
                                                    div.clone(),
                                                ));
                                            } else {
                                                ui_win.confirm_recall = Some(
                                                    crate::state::RecallKind::Division(div.clone()),
                                                );
                                            }
                                        }
                                    }
                                    for id in ids {
                                        let Some(u) = state.unit_by_id(id) else {
                                            continue;
                                        };
                                        // Copy everything the row needs up front
                                        // so the click handler may mutate state.
                                        let side_c = colors.for_side(u.side);
                                        let effective = u.is_combat_effective();
                                        let retreating = u.state == UnitState::Retreating;
                                        let selected = state.selected_unit == Some(id);
                                        let undeployed = u.undeployed;
                                        let placing = state.deploy_placing == Some(id);
                                        // An ALLIED battalion's row is
                                        // information-only — click selects and
                                        // pans, but never arms OOB placement
                                        // (its own staff deploys it).
                                        let commanded =
                                            game.as_ref().map(|g| g.commands(u)).unwrap_or(true);
                                        let org = u.org_ratio();
                                        let strr = u.strength_ratio();
                                        let pos = u.position;
                                        let name = if undeployed {
                                            format!(
                                                "{} {}",
                                                u.name,
                                                loc.tr("battle.oob.suffix_undeployed")
                                            )
                                        } else if effective {
                                            u.name.clone()
                                        } else if retreating {
                                            format!(
                                                "{} {}",
                                                u.name,
                                                loc.tr("battle.oob.suffix_routed")
                                            )
                                        } else {
                                            format!(
                                                "{} {}",
                                                u.name,
                                                loc.tr("battle.oob.suffix_destroyed")
                                            )
                                        };
                                        let supports: Vec<String> =
                                            u.support.iter().map(|s| s.name.clone()).collect();
                                        let resp = ui
                                            .horizontal(|ui| {
                                                let (dot, _) = ui.allocate_exact_size(
                                                    egui::vec2(10.0, 10.0),
                                                    egui::Sense::hover(),
                                                );
                                                ui.painter().circle_filled(
                                                    dot.center(),
                                                    4.0,
                                                    color32(
                                                        [side_c[0], side_c[1], side_c[2], 1.0],
                                                        255,
                                                    ),
                                                );
                                                let text = if !effective
                                                    && !retreating
                                                    && !undeployed
                                                {
                                                    egui::RichText::new(&name)
                                                        .weak()
                                                        .strikethrough()
                                                } else if undeployed {
                                                    egui::RichText::new(&name).color(
                                                        egui::Color32::from_rgb(255, 190, 80),
                                                    )
                                                } else if retreating {
                                                    egui::RichText::new(&name).color(
                                                        egui::Color32::from_rgb(255, 160, 90),
                                                    )
                                                } else if selected {
                                                    egui::RichText::new(&name).strong().underline()
                                                } else {
                                                    egui::RichText::new(&name)
                                                };
                                                let resp =
                                                    ui.selectable_label(selected || placing, text);
                                                // Row tooltip: during
                                                // deployment a waiting battalion
                                                // click starts placement instead
                                                // of selecting.
                                                let resp = if effective
                                                    && deploying
                                                    && undeployed
                                                    && commanded
                                                {
                                                    resp.on_hover_text(
                                                        loc.tr("battle.tooltip.oob_row_deploy"),
                                                    )
                                                } else if effective {
                                                    resp.on_hover_text(
                                                        loc.tr("battle.tooltip.oob_row"),
                                                    )
                                                } else {
                                                    resp
                                                };
                                                // Mini Org/Str bars (HOI4 colors).
                                                let (br, _) = ui.allocate_exact_size(
                                                    egui::vec2(44.0, 10.0),
                                                    egui::Sense::hover(),
                                                );
                                                let p = ui.painter();
                                                let bg = egui::Color32::from_white_alpha(18);
                                                for (dy, ratio, col) in
                                                    [(0.0, org, ORG_GREEN), (6.0, strr, STR_YELLOW)]
                                                {
                                                    let min = br.left_top() + egui::vec2(0.0, dy);
                                                    p.rect_filled(
                                                        egui::Rect::from_min_size(
                                                            min,
                                                            egui::vec2(44.0, 4.0),
                                                        ),
                                                        0.0,
                                                        bg,
                                                    );
                                                    p.rect_filled(
                                                        egui::Rect::from_min_size(
                                                            min,
                                                            egui::vec2(44.0 * ratio, 4.0),
                                                        ),
                                                        0.0,
                                                        col,
                                                    );
                                                }
                                                resp
                                            })
                                            .inner;
                                        // While re-assigning, a battalion click
                                        // completes the swap. An
                                        // UNDEPLOYED battalion click enters OOB
                                        // placement mode instead of selecting.
                                        // Otherwise it selects + focuses as before.
                                        // An ALLIED row never completes
                                        // an attachment swap nor arms placement —
                                        // it only selects + focuses.
                                        if resp.clicked() && effective {
                                            if !commanded {
                                                state.selected_unit = Some(id);
                                                state.units_dirty = true;
                                                focus.0 = Some(hex_world(pos) + Vec3::Y * 0.4);
                                            } else if let Some((src_host, att_idx)) =
                                                state.attach_picking
                                            {
                                                if src_host != id {
                                                    let att = state
                                                        .units
                                                        .iter_mut()
                                                        .find(|x| x.id == src_host)
                                                        .and_then(|u2| u2.detach(att_idx));
                                                    if let Some(att) = att {
                                                        if let Some(host) = state
                                                            .units
                                                            .iter_mut()
                                                            .find(|x| x.id == id)
                                                        {
                                                            host.attach(att);
                                                        }
                                                    }
                                                }
                                                state.attach_picking = None;
                                                state.units_dirty = true;
                                            } else if deploying && undeployed && commanded {
                                                state.deploy_placing = Some(id);
                                                state.selected_unit = None;
                                                state.units_dirty = true;
                                            } else {
                                                state.selected_unit = Some(id);
                                                state.units_dirty = true;
                                                focus.0 = Some(hex_world(pos) + Vec3::Y * 0.4);
                                            }
                                        }
                                        // Attachment rows, indented under the
                                        // host; click during Deployment to reassign.
                                        if effective {
                                            for (ai, att_name) in supports.iter().enumerate() {
                                                let picking =
                                                    state.attach_picking == Some((id, ai));
                                                let row = ui
                                                    .horizontal(|ui| {
                                                        ui.add_space(14.0);
                                                        let text = if picking {
                                                            egui::RichText::new(loc.trf(
                                                                "battle.oob.attach_pick",
                                                                &[("name", att_name)],
                                                            ))
                                                            .color(GOLD)
                                                        } else {
                                                            egui::RichText::new(format!(
                                                                "+ {att_name}"
                                                            ))
                                                            .weak()
                                                            .small()
                                                        };
                                                        let r = ui.selectable_label(picking, text);
                                                        // Reassignment
                                                        // only works during
                                                        // Deployment — only then
                                                        // does the tooltip apply.
                                                        if deploying {
                                                            r.on_hover_text(
                                                                loc.tr("battle.tooltip.attach_row"),
                                                            )
                                                        } else {
                                                            r
                                                        }
                                                    })
                                                    .inner;
                                                // Attachment
                                                // reassignment is a command —
                                                // allied rows cannot arm it.
                                                if row.clicked() && deploying && commanded {
                                                    state.attach_picking =
                                                        if picking { None } else { Some((id, ai)) };
                                                }
                                            }
                                        }
                                    }
                                });
                            if div_order.is_some() {
                                let resp = ui
                                    .add(
                                        egui::Button::new(egui::RichText::new("X").small())
                                            .fill(egui::Color32::from_rgb(48, 36, 30))
                                            .stroke(egui::Stroke::new(
                                                1.0_f32,
                                                egui::Color32::from_rgb(255, 150, 110),
                                            )),
                                    )
                                    .on_hover_text(loc.tr("div_order.cancel.tooltip"));
                                order_cancel = resp.clicked();
                            }
                        });
                        if order_cancel {
                            pending.0.push(PlayerCommand::CancelDivOrder(div.clone()));
                        }
                    }
                });
        });
    if !open {
        ui_win.oob = false;
    }
}

// ---------------------------------------------------------------------------
// Battle-report modal: after each fire phase the engagements are
// walked one by one — the camera focuses the hex (tick_battle_tour) while
// this window lists every attacker / damage / counter-fire. [Continue]
// advances; Esc skips the rest (handle_map_clicks). Map input is frozen.
// ---------------------------------------------------------------------------

fn draw_battle_report(
    mut contexts: EguiContexts,
    mut tour: ResMut<BattleTour>,
    state: Option<Res<TacticalState>>,
    loc: Res<LocaleRes>,
    time: Res<Time>,
    mut detail: ResMut<DetailWindow>,
    mut anchor: Local<CenterAnchor>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    // The window opens when the camera glide LANDS (the arrival
    // pulse sets `focused` to the index) — not while the camera is moving.
    if !tour.active || tour.index >= tour.queue.len() || tour.focused != Some(tour.index) {
        return;
    }
    let total = tour.queue.len();
    let n = tour.index + 1;
    let (hex, defender, acting, lanes, outcome) = {
        let r = &tour.queue[tour.index];
        (
            r.hex,
            r.defender.clone(),
            r.acting,
            r.lanes.clone(),
            r.outcome.clone(),
        )
    };
    let yours = state.as_ref().map(|s| s.player_side) == Some(acting);

    // Dim behind the modal (same treatment as the sync prompt).
    egui::Area::new(egui::Id::new("report_dim"))
        .order(egui::Order::Middle)
        .fixed_pos(egui::pos2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_black_alpha(120));
        });

    let mut advance = false;
    let mut open_detail: Option<EngagementDetailView> = None;
    let win = egui::Window::new(loc.trf(
        "battle.report.title",
        &[
            ("n", &n.to_string()),
            ("total", &total.to_string()),
            ("defender", &defender),
            ("q", &hex.q.to_string()),
            ("r", &hex.r.to_string()),
        ],
    ))
    .collapsible(false)
    .resizable(false)
    .default_width(430.0);
    let resp = anchor
        .window(ctx, win, egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(if yours {
                    loc.tr("battle.report.your_attack").into_owned()
                } else {
                    loc.tr("battle.report.enemy_attack").into_owned()
                })
                .strong()
                .color(if yours { PARCHMENT } else { GOLD }),
            );
            ui.add_space(2.0);
            for lane in &lanes {
                // lane.kind arrives pre-localized from game.rs (attack_kind.*).
                let shocked = if lane.shocked_defender {
                    loc.tr("battle.report.shocked").into_owned()
                } else {
                    String::new()
                };
                let friendly = if lane.friendly {
                    loc.tr("battle.report.friendly_fire").into_owned()
                } else {
                    String::new()
                };
                let line = loc.trf(
                    "battle.report.lane",
                    &[
                        ("attacker", &lane.attacker),
                        ("kind", &lane.kind),
                        ("org", &format!("{:.1}", lane.org)),
                        ("str", &format!("{:.1}", lane.str_)),
                        ("shocked", &shocked),
                        ("friendly", &friendly),
                    ],
                );
                ui.horizontal(|ui| {
                    // Rocket friendly-fire lanes stand out in orange.
                    if lane.friendly {
                        ui.label(
                            egui::RichText::new(&line)
                                .color(egui::Color32::from_rgb(230, 130, 40)),
                        );
                    } else {
                        ui.label(&line);
                    }
                    // The lane's formula chain opens in the
                    // engagement-detail window (same content as clicking
                    // the battle-log line).
                    if let Some(dv) = &lane.detail {
                        if ui
                            .small_button(loc.tr("battle.report.detail").as_ref())
                            .clicked()
                        {
                            open_detail = Some((**dv).clone());
                        }
                    }
                });
                if lane.counter_org > 0.0 || lane.counter_str > 0.0 {
                    ui.label(
                        egui::RichText::new(loc.trf(
                            "log.combat.counter",
                            &[
                                ("name", &lane.attacker),
                                ("org", &format!("{:.1}", lane.counter_org)),
                                ("str", &format!("{:.1}", lane.counter_str)),
                            ],
                        ))
                        .weak(),
                    );
                }
            }
            if !outcome.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(loc.trf(
                        "battle.report.outcome",
                        &[("defender", &defender), ("outcome", &outcome)],
                    ))
                    .strong()
                    .color(GOLD),
                );
            }
            ui.add_space(8.0);
            if ui
                .button(egui::RichText::new(loc.tr("common.continue").as_ref()).strong())
                .clicked()
            {
                advance = true;
            }
        });
    anchor.update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
    if let Some(v) = open_detail {
        detail.0 = Some(v);
    }
    if advance {
        // When this report carries a deferred consequence
        // (repel slide / annihilation vanish), let the camera dwell on the
        // old hex for a beat after the release so the player actually sees
        // the unit move / disappear before the focus jumps on.
        let had_pending = !tour.queue[tour.index].pending.is_empty();
        tour.index += 1;
        tour.focused = None;
        if had_pending && tour.index < tour.queue.len() {
            tour.next_focus_at =
                Some(time.elapsed_secs() + crate::state::REPORT_RELEASE_LINGER_SECS);
        }
    }
}

// ---------------------------------------------------------------------------
// Engagement-detail window: the full formula chain of one
// exchange (q → P → D → hit → linear stack → cap → delivered org/str),
// opened from the report modal's「细节」button or a clickable battle-log
// line. Every number comes from the resolution-time HitBreakdown capture —
// the panel RE-SHOWS the computation, it never recomputes it.
// ---------------------------------------------------------------------------

fn draw_engagement_detail(
    mut contexts: EguiContexts,
    mut detail: ResMut<DetailWindow>,
    loc: Res<LocaleRes>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    let Some(view) = &detail.0 else { return };
    let mut open = true;
    // Title carries only the hex: the direction lines
    // inside are the first-level headers.
    egui::Window::new(loc.trf(
        "detail.title",
        &[
            ("q", &view.hex.q.to_string()),
            ("r", &view.hex.r.to_string()),
        ],
    ))
    .open(&mut open)
    .default_size([520.0, 560.0])
    .show(ctx, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // First-level header: kind + direction, side-coloured (same
                // semantics as the report modal's your/enemy attack line).
                ui.label(
                    egui::RichText::new(loc.trf(
                        "detail.sec.outgoing",
                        &[
                            ("kind", &view.kind),
                            ("attacker", &view.attacker),
                            ("defender", &view.defender),
                        ],
                    ))
                    .strong()
                    .size(18.0)
                    .color(if view.yours { PARCHMENT } else { GOLD }),
                );
                hit_block(
                    ui,
                    &loc,
                    &view.hit,
                    if view.shocked_defender {
                        Some("detail.row.shocked_def")
                    } else {
                        None
                    },
                    view.friendly,
                );
                if let Some(c) = &view.counter {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);
                    // The counter direction: the defender fires back at the
                    // attacker caught mid-attack (breakthrough side); the
                    // colour inverts — the counter-firer is the other side.
                    ui.label(
                        egui::RichText::new(loc.trf(
                            "detail.sec.counter",
                            &[
                                ("attacker", &view.defender),
                                ("defender", &view.attacker),
                            ],
                        ))
                        .strong()
                        .size(18.0)
                        .color(if view.yours { GOLD } else { PARCHMENT }),
                    );
                    hit_block(
                        ui,
                        &loc,
                        c,
                        if view.shocked_attacker {
                            Some("detail.row.shocked_atk")
                        } else {
                            None
                        },
                        false,
                    );
                }
            });
    });
    if !open {
        detail.0 = None;
    }
}

fn d_f0(x: f32) -> String {
    format!("{x:.0}")
}
fn d_f1(x: f32) -> String {
    format!("{x:.1}")
}
fn d_f2(x: f32) -> String {
    format!("{x:.2}")
}
fn d_pct(x: f32) -> String {
    format!("{:.0}%", x * 100.0)
}
fn d_spct(x: f32) -> String {
    format!("{:+.0}%", x * 100.0)
}

/// Second-level section header (▪ …): strong, a notch below the
/// first-level direction header, with grouping space above.
fn detail_header(ui: &mut egui::Ui, text: String) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(text).strong().size(15.0));
}

/// One formula row — indented; neutral rows render weak so the real ones
/// pop. Chain-row convention: a segment inside a
/// chain is "name value" (× is ONLY the operator between segments); a
/// standalone modifier row keeps "name ×value".
fn detail_row(ui: &mut egui::Ui, text: String, neutral: bool) {
    let rt = egui::RichText::new(format!("  {text}"));
    if neutral {
        ui.label(rt.weak());
    } else {
        ui.label(rt);
    }
}

/// One chain segment " × name value" (a separate key so segments compose,
/// §15). Neutral segments are OMITTED from chains.
fn chain_seg(loc: &LocaleRes, name: &str, value: f32) -> String {
    loc.trf(
        "detail.row.chain_seg",
        &[("name", name), ("value", &d_f2(value))],
    )
}

fn acc_class_key(c: AccuracyClass) -> &'static str {
    match c {
        AccuracyClass::Standard => "detail.acc.standard",
        AccuracyClass::Artillery => "detail.acc.artillery",
        AccuracyClass::AntiTank => "detail.acc.anti_tank",
        AccuracyClass::AntiAir => "detail.acc.anti_air",
        AccuracyClass::Armored => "detail.acc.armored",
    }
}

fn factor_label(loc: &LocaleRes, kind: LinearFactor, bd: &HitBreakdown) -> String {
    let tname = loc.terrain_name(bd.target_terrain);
    match kind {
        LinearFactor::CommandAura => loc.tr("detail.factor.command_aura").into_owned(),
        LinearFactor::TargetCommand => loc.tr("detail.factor.target_command").into_owned(),
        LinearFactor::TerrainAttack => {
            loc.trf("detail.factor.terrain_attack", &[("terrain", tname.as_ref())])
        }
        LinearFactor::DirectFireFalloff => {
            loc.tr("detail.factor.direct_fire_falloff").into_owned()
        }
        LinearFactor::MeleeElevation => loc.tr("detail.factor.melee_elevation").into_owned(),
        LinearFactor::IndirectCrest => loc.tr("detail.factor.indirect_crest").into_owned(),
        LinearFactor::Cover => loc.trf(
            "detail.factor.cover",
            &[
                ("terrain", tname.as_ref()),
                ("cover", &d_spct(bd.target_terrain.cover_percent())),
            ],
        ),
        LinearFactor::AreaWeight => loc.tr("detail.factor.area_weight").into_owned(),
    }
}

/// One direction of the exchange (outgoing strike or counter-fire) as a
/// formula tree with the captured numbers plugged in.
fn hit_block(
    ui: &mut egui::Ui,
    loc: &LocaleRes,
    bd: &HitBreakdown,
    shocked_key: Option<&'static str>,
    friendly: bool,
) {
    // ── attack quality q ──
    detail_header(ui, loc.trf("detail.sec.quality", &[("q", &d_f1(bd.q))]));
    detail_row(
        ui,
        loc.trf(
            "detail.row.q_soft",
            &[
                ("soft", &d_f1(bd.soft_attack)),
                ("h", &d_pct(bd.target_hardness)),
                ("val", &d_f1(bd.soft_attack * (1.0 - bd.target_hardness))),
            ],
        ),
        false,
    );
    detail_row(
        ui,
        loc.trf(
            "detail.row.q_hard",
            &[
                ("hard", &d_f1(bd.hard_attack)),
                ("h", &d_pct(bd.target_hardness)),
                ("pierce", &d_f2(bd.piercing_mult)),
                (
                    "val",
                    &d_f1(bd.hard_attack * bd.target_hardness * bd.piercing_mult),
                ),
            ],
        ),
        false,
    );
    detail_row(
        ui,
        loc.trf(
            "detail.row.q_acc",
            &[
                ("class", &loc.tr(acc_class_key(bd.accuracy_class))),
                ("acc", &d_f2(bd.accuracy)),
            ],
        ),
        (bd.accuracy - 1.0).abs() < 0.005,
    );

    // ── firepower P ──
    detail_header(ui, loc.trf("detail.sec.firepower", &[("p", &d_f1(bd.p))]));
    if let Some(pool) = &bd.pool {
        detail_row(
            ui,
            loc.trf(
                "detail.row.p_pool",
                &[
                    ("sum_qg", &d_f1(pool.sum_qg)),
                    ("sum_g", &d_f1(pool.sum_g)),
                    ("p", &d_f1(bd.p)),
                    ("members", &pool.members.to_string()),
                ],
            ),
            false,
        );
    } else if bd.counter_split > 1.0 {
        detail_row(
            ui,
            loc.trf(
                "detail.row.p_counter",
                &[
                    ("full", &d_f1(bd.p * bd.counter_split)),
                    ("n", &d_f0(bd.counter_split)),
                    ("p", &d_f1(bd.p)),
                ],
            ),
            false,
        );
    } else {
        // Fire missions show the linear area-fire form.
        let key = if bd.area_fire {
            "detail.row.p_lone_area"
        } else {
            "detail.row.p_lone"
        };
        detail_row(
            ui,
            loc.trf(
                key,
                &[
                    ("q", &d_f1(bd.q)),
                    ("g", &d_pct(bd.strength_ratio)),
                    ("p", &d_f1(bd.p)),
                ],
            ),
            false,
        );
    }

    // ── defense D: only the ACTIVE segments chain (neutral ones omitted);
    // an all-neutral chain collapses to the grey base row. The
    // breakthrough side never has Hold/entrenchment channels at all. ──
    let dkind = if bd.uses_breakthrough {
        loc.tr("detail.def.breakthrough")
    } else {
        loc.tr("detail.def.defense")
    };
    detail_header(
        ui,
        loc.trf("detail.sec.defense", &[("d", &d_f1(bd.d)), ("kind", &dkind)]),
    );
    let mut segs = String::new();
    if !bd.uses_breakthrough {
        if (bd.d_hold_mult - 1.0).abs() > 0.005 {
            segs.push_str(&chain_seg(loc, &loc.tr("detail.seg.hold"), bd.d_hold_mult));
        }
        if (bd.d_entrench_mult - 1.0).abs() > 0.005 {
            segs.push_str(&chain_seg(
                loc,
                &loc.tr("detail.seg.entrench"),
                bd.d_entrench_mult,
            ));
        }
    }
    if (bd.d_terrain_mult - 1.0).abs() > 0.005 {
        segs.push_str(&chain_seg(
            loc,
            &loc.tr("detail.seg.terrain_apt"),
            bd.d_terrain_mult,
        ));
    }
    if segs.is_empty() {
        detail_row(
            ui,
            loc.trf("detail.row.d_base", &[("base", &d_f1(bd.d_base))]),
            true,
        );
    } else {
        detail_row(
            ui,
            loc.trf(
                "detail.row.d_compose",
                &[("base", &d_f1(bd.d_base)), ("segs", &segs), ("d", &d_f1(bd.d))],
            ),
            false,
        );
    }

    // ── hit step ──
    detail_header(ui, loc.trf("detail.sec.hit", &[("hit", &d_pct(bd.hit))]));
    detail_row(
        ui,
        loc.trf(
            "detail.row.hit",
            &[
                ("base", &d_pct(bd.hit_base)),
                ("sat", &d_pct(bd.hit_saturated)),
                ("hit", &d_pct(bd.hit)),
            ],
        ),
        false,
    );

    // ── linear modifier stack ──
    detail_header(
        ui,
        loc.trf("detail.sec.linear", &[("total", &d_f2(bd.linear_total))]),
    );
    for (kind, value) in &bd.linear[..bd.linear_len as usize] {
        let neutral = (*value - 1.0).abs() < 0.005;
        let label = factor_label(loc, *kind, bd);
        detail_row(
            ui,
            loc.trf(
                "detail.row.factor",
                &[("label", &label), ("value", &d_f2(*value))],
            ),
            neutral,
        );
    }

    // ── org damage ──
    detail_header(ui, loc.trf("detail.sec.org", &[("org", &d_f1(bd.org_final))]));
    let linear_seg = if (bd.linear_total - 1.0).abs() > 0.005 {
        chain_seg(loc, &loc.tr("detail.seg.linear"), bd.linear_total)
    } else {
        String::new()
    };
    detail_row(
        ui,
        loc.trf(
            "detail.row.org_compose",
            &[
                ("scale", &d_f1(bd.damage_scale)),
                ("p", &d_f1(bd.p)),
                ("hit", &d_pct(bd.hit)),
                ("linear_seg", &linear_seg),
                ("raw", &d_f2(bd.org_raw)),
            ],
        ),
        false,
    );
    if (bd.jitter_mult - 1.0).abs() > 1e-6 {
        detail_row(
            ui,
            loc.trf("detail.row.org_jitter", &[("mult", &d_f2(bd.jitter_mult))]),
            false,
        );
    }
    if bd.org_capped {
        detail_row(
            ui,
            loc.trf("detail.row.org_cap_hit", &[("cap", &d_f1(bd.org_cap))]),
            false,
        );
    } else {
        detail_row(
            ui,
            loc.trf("detail.row.org_cap", &[("cap", &d_f1(bd.org_cap))]),
            true,
        );
    }
    if bd.pool.is_some() {
        detail_row(
            ui,
            loc.trf(
                "detail.row.org_share",
                &[
                    ("pool", &d_f1(bd.org_pool_final)),
                    ("share", &d_pct(bd.pool_share)),
                    ("org", &d_f1(bd.org_final)),
                ],
            ),
            false,
        );
    }
    if (bd.area_weight - 1.0).abs() > 1e-6 {
        detail_row(
            ui,
            loc.trf(
                "detail.row.org_area",
                &[("w", &d_f2(bd.area_weight)), ("org", &d_f1(bd.org_final))],
            ),
            false,
        );
    }
    if friendly {
        ui.label(
            egui::RichText::new(loc.tr("detail.row.friendly").into_owned())
                .strong()
                .color(egui::Color32::from_rgb(230, 130, 40)),
        );
    }
    if let Some(key) = shocked_key {
        ui.label(
            egui::RichText::new(loc.tr(key).into_owned())
                .strong()
                .color(GOLD),
        );
    }

    // ── strength damage ──
    detail_header(ui, loc.trf("detail.sec.str", &[("str", &d_f1(bd.str_final))]));
    let rate_label = if bd.str_rate_broken {
        loc.tr("detail.str_rate.broken")
    } else {
        loc.tr("detail.str_rate.normal")
    };
    detail_row(
        ui,
        loc.trf(
            "detail.row.str_compose",
            &[
                ("org", &d_f1(bd.org_final)),
                ("rate", &d_f2(bd.str_rate)),
                ("label", &rate_label),
                ("max_str", &d_f1(bd.max_strength)),
                ("max_org", &d_f1(bd.max_org)),
                ("str", &d_f1(bd.str_final)),
            ],
        ),
        false,
    );
}

// ---------------------------------------------------------------------------
// Sync-completion prompt: after each successful Sync the battle
// pauses behind a top-anchored modal — damage summary + [Continue] /
// [End Tactic]. End Tactic opens a CENTER confirmation; confirming exits the
// battle cleanly (results so far are already in HOI4).
// ---------------------------------------------------------------------------

fn draw_sync_prompt(
    mut contexts: EguiContexts,
    game: Option<Res<GameController>>,
    loc: Res<LocaleRes>,
    icons: Res<IconSet>,
    mut ui_win: ResMut<UiWindows>,
    mut pending: ResMut<PendingCommands>,
    // CenterAnchor state for the prompt window + the End-Tactic confirmation.
    mut anchors: Local<SyncAnchors>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    if !ui_win.sync_prompt {
        ui_win.confirm_end = false;
        return;
    }
    let Some(game) = game else { return };

    // Dim behind the modal (same treatment as the Esc menu).
    egui::Area::new(egui::Id::new("sync_dim"))
        .order(egui::Order::Middle)
        .fixed_pos(egui::pos2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_black_alpha(140));
        });

    let win = egui::Window::new(loc.trf(
        "battle.sync.title",
        &[("hour", &game.session.strategic_hour.to_string())],
    ))
    .collapsible(false)
    .resizable(false)
    // Without a width floor egui shrinks the window to the
    // title and wraps the summary into stacked lines.
    .default_width(440.0);
    let resp = anchors
        .prompt
        .window(ctx, win, egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
        .show(ctx, |ui| {
            if let Some(s) = &game.last_sync_summary {
                // Sync summary arrives pre-localized from game.rs; the
                // leading hourglass marks this as the hourly report.
                ui.horizontal(|ui| {
                    icons.icon(ui, IconId::Sync, 15.0);
                    ui.label(s.as_str());
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .button(egui::RichText::new(loc.tr("common.continue").as_ref()).strong())
                    .clicked()
                {
                    ui_win.sync_prompt = false;
                }
                if ui
                    .button(
                        egui::RichText::new(loc.tr("battle.sync.end_tactic").as_ref())
                            .strong()
                            .color(GOLD),
                    )
                    .on_hover_text(loc.tr("battle.tooltip.end_tactic"))
                    .clicked()
                {
                    ui_win.confirm_end = true;
                }
            });
        });
    anchors
        .prompt
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());

    // Center confirmation (drawn after → on top).
    if ui_win.confirm_end {
        let win = egui::Window::new(loc.tr("battle.sync.end_tactic").as_ref())
            .collapsible(false)
            .resizable(false);
        let resp = anchors
            .confirm
            .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                icons.label_with_icon(
                    ui,
                    IconId::Warning,
                    loc.tr("battle.sync.confirm_q").as_ref(),
                    15.0,
                );
                ui.label(loc.trf(
                    "battle.sync.confirm_synced",
                    &[("hour", &game.session.strategic_hour.to_string())],
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(
                            egui::RichText::new(loc.tr("battle.sync.end_battle").as_ref()).strong(),
                        )
                        .clicked()
                    {
                        // Live battles route the end-batch cleanup
                        // (non-live exits plainly inside the handler).
                        pending.0.push(PlayerCommand::EndTacticEarly);
                        ui_win.confirm_end = false;
                        ui_win.sync_prompt = false;
                    }
                    if ui.button(loc.tr("common.cancel").as_ref()).clicked() {
                        ui_win.confirm_end = false;
                    }
                });
            });
        anchors
            .confirm
            .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
    }
}

/// draw_sync_prompt's two centered windows (the hourly prompt + the
/// End-Tactic confirmation).
#[derive(Default)]
struct SyncAnchors {
    prompt: CenterAnchor,
    confirm: CenterAnchor,
}

/// §8.4: the hourly sync's clock receipt timed out — HOI4 did
/// not prove the strategic hour advanced (menu/pause stall or a dead game).
/// Modal dialog: the player checks HOI4, then picks Retry (clock-only
/// re-issue — the damage lines already landed) or Cancel (the original
/// log-and-continue behaviour). The bin-side injector owns the `SyncStall`
/// lifecycle; this dialog only writes `action`. Esc deliberately does NOT
/// peel it (handle_map_clicks freezes early) — the choice is mandatory.
fn draw_sync_stall(
    mut contexts: EguiContexts,
    loc: Res<LocaleRes>,
    icons: Res<IconSet>,
    mut ui_win: ResMut<UiWindows>,
    stall: Option<ResMut<SyncStall>>,
    mut anchor: Local<CenterAnchor>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    let Some(mut stall) = stall else { return };
    // The hourly "sync complete" prompt would be a lie while the receipt is
    // unconfirmed — this dialog replaces it (and its confirmation child).
    ui_win.sync_prompt = false;
    ui_win.confirm_end = false;

    // Dim behind the modal (same treatment as the sync prompt).
    egui::Area::new(egui::Id::new("sync_stall_dim"))
        .order(egui::Order::Middle)
        .fixed_pos(egui::pos2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_black_alpha(140));
        });

    let win = egui::Window::new(loc.tr("battle.sync.stall.title").as_ref())
        .collapsible(false)
        .resizable(false)
        .default_width(440.0);
    let resp = anchor
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            icons.label_with_icon(
                ui,
                IconId::Warning,
                loc.tr("battle.sync.stall.body").as_ref(),
                15.0,
            );
            if stall.attempts > 1 {
                ui.label(loc.trf(
                    "battle.sync.stall.attempts",
                    &[("n", &(stall.attempts - 1).to_string())],
                ));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(
                        egui::RichText::new(loc.tr("battle.sync.stall.retry").as_ref())
                            .strong()
                            .color(GOLD),
                    )
                    .clicked()
                {
                    stall.action = SyncStallAction::Retry;
                }
                if ui
                    .button(loc.tr("battle.sync.stall.cancel").as_ref())
                    .clicked()
                {
                    stall.action = SyncStallAction::Cancel;
                }
            });
        });
    anchor.update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

/// The desync guard's modal (DESIGN.md §3.2/§8.2): the sync probes proved
/// the strategic world no longer matches this battle — the clock receipt
/// reads other than the expected next hour, or the save's played country
/// changed. End Battle queues the abort cleanup batch (unfreeze + flags +
/// popup, NO results); Continue Unsynced turns the sync pipeline off for
/// the rest of the battle (every later exit sends the same cleanup only).
/// The bin-side injector owns the `DesyncAlert` lifecycle; this dialog
/// only writes `action`. Esc deliberately does NOT peel it
/// (handle_map_clicks freezes early) — the choice is mandatory.
fn draw_desync_alert(
    mut contexts: EguiContexts,
    loc: Res<LocaleRes>,
    icons: Res<IconSet>,
    mut ui_win: ResMut<UiWindows>,
    alert: Option<ResMut<DesyncAlert>>,
    mut anchor: Local<CenterAnchor>,
) {
    // Window-close frame guard (see ui_pointer_guard).
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    let Some(mut alert) = alert else { return };
    // A sync-completion prompt would be a lie while the guard is up —
    // this dialog replaces it (and its confirmation child).
    ui_win.sync_prompt = false;
    ui_win.confirm_end = false;

    // Dim behind the modal (same treatment as the sync stall).
    egui::Area::new(egui::Id::new("desync_dim"))
        .order(egui::Order::Middle)
        .fixed_pos(egui::pos2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_black_alpha(140));
        });

    let win = egui::Window::new(loc.tr("battle.desync.title").as_ref())
        .collapsible(false)
        .resizable(false)
        .default_width(460.0);
    let resp = anchor
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            icons.label_with_icon(
                ui,
                IconId::Warning,
                loc.tr("battle.desync.lead").as_ref(),
                15.0,
            );
            match &alert.verdict {
                tactical_sync::desync::DesyncVerdict::HourMismatch { expected, found } => {
                    ui.label(loc.trf(
                        "battle.desync.body.hour",
                        &[("expected", expected), ("found", found)],
                    ));
                }
                tactical_sync::desync::DesyncVerdict::TagMismatch { expected, found } => {
                    ui.label(loc.trf(
                        "battle.desync.body.tag",
                        &[("expected", expected), ("found", found)],
                    ));
                }
                tactical_sync::desync::DesyncVerdict::Ok => {}
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(
                        egui::RichText::new(loc.tr("battle.desync.end_battle").as_ref())
                            .strong()
                            .color(GOLD),
                    )
                    .clicked()
                {
                    alert.action = DesyncAction::EndBattle;
                }
                if ui
                    .button(loc.tr("battle.desync.continue").as_ref())
                    .clicked()
                {
                    alert.action = DesyncAction::ContinueUnsynced;
                }
            });
        });
    anchor.update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

#[cfg(test)]
mod tests {
    use super::CenterAnchor;
    use bevy_egui::egui;

    fn rect(w: f32, h: f32) -> Option<egui::Rect> {
        Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h)))
    }

    /// Fractional-DPI parity: the measured size flip-flops by exactly one
    /// physical px every frame — the latch must swallow it (the position
    /// follows the stored size, so a flip-flopping size IS the jitter).
    #[test]
    fn center_anchor_latches_one_physical_px_size_wobble() {
        let mut a = CenterAnchor::default();
        let ppp = 1.5;
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1920.0, 1080.0));
        a.update(rect(440.0, 200.0), ppp); // 660×300 phys px, exact
        a.update(rect(440.0 + 1.0 / ppp, 200.0), ppp); // +1 phys px wobble
        a.update(rect(440.0, 200.0 + 1.0 / ppp), ppp);
        // Still the first measurement — the ±1 px frames never landed.
        let p1 = a.pos(screen, ppp, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0));
        a.update(rect(440.0 + 1.0 / ppp, 200.0 + 1.0 / ppp), ppp);
        let p2 = a.pos(screen, ppp, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0));
        assert_eq!(p1, p2, "latched size must keep the position constant");
        // Window center at screen center (± half a phys px).
        let p = p1.unwrap();
        assert!((p.x - (1920.0 - 440.0) / 2.0).abs() < 1.0 / ppp);
        assert!((p.y - (1080.0 - 200.0) / 2.0).abs() < 1.0 / ppp);
    }

    /// A real content change (a whole text row ≫ 1 phys px) passes the
    /// latch and re-centers on the next frame, matching native behaviour.
    #[test]
    fn center_anchor_real_resize_still_passes() {
        let mut a = CenterAnchor::default();
        let ppp = 1.5;
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1920.0, 1080.0));
        a.update(rect(440.0, 200.0), ppp);
        a.update(rect(440.0, 224.0), ppp); // +24 pt = +36 phys px: real change
        let p = a
            .pos(screen, ppp, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .unwrap();
        assert!((p.y - (1080.0 - 224.0) / 2.0).abs() < 1.0 / ppp);
    }
}

