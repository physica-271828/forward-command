//! Shared tactical state resource consumed by the render/UI systems.

use bevy::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use tactical_combat::AttackOrder;
use tactical_core::fog::{FogOfWar, VisibilityState};
use tactical_core::grid::HexGrid;
use tactical_core::hex::HexCoord;
use tactical_core::unit::{BattalionUnit, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightKind {
    Move,
    Assault,
    Support,
    Hover,
    Deployment,
    /// The battle-report modal's current engagement hex — bright
    /// red, held until [Continue] advances the tour.
    Report,
}

/// Sector deployment (OOB "deploy to sector"): the division
/// being placed and the drag anchor of the rectangle. While `anchor` is
/// Some the player is dragging; the preview rectangle is highlighted and
/// committed on mouse release.
#[derive(Debug, Clone, PartialEq)]
pub struct SectorPick {
    pub division: String,
    pub anchor: Option<HexCoord>,
}

/// Which recall the confirmation dialog is asking about —
/// the whole player force or one division (OOB "recall" affordance).
#[derive(Debug, Clone, PartialEq)]
pub enum RecallKind {
    All,
    Division(String),
}

/// UI/command interaction mode. The mouse itself is the command
/// interface (left = non-sticky select, right = context order); the only
/// modal state left is artillery fire-mission picking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    /// Default: left-click (de)selects, right-click issues context orders.
    Select,
    /// An artillery unit is picking a fire-mission target hex (radial F
    /// button): left-click fires one barrage, right-click / Esc cancels.
    FirePicking,
}

/// Division-order target picking — set while the HQ radial bar's
/// Seize / Engage buttons await a map pick (left-click commits, right-click
/// / Esc cancels). Mutually exclusive with selection-based commands.
#[derive(Debug, Clone)]
pub struct DivPick {
    pub division: String,
    pub kind: DivPickKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivPickKind {
    /// Pick a passable hex as the seizure target.
    Seize,
    /// Pick a visible enemy battalion as the engage target.
    Engage,
}

#[derive(Resource)]
pub struct TacticalState {
    // Arc-shared: the grid is immutable for the whole battle, and the many
    // `state.grid.clone()` call sites (per-click, per-AI-tick, per-turn)
    // become refcount bumps instead of copying up to 512×512 hexes.
    pub grid: Option<Arc<HexGrid>>,
    pub units: Vec<BattalionUnit>,
    pub fog: Option<FogOfWar>,
    /// The AI side's fog: AI planning and route refresh share
    /// the player's information limits.
    pub ai_fog: Option<FogOfWar>,
    pub turn: u32,

    pub selected_unit: Option<usize>,
    pub hover_hex: Option<HexCoord>,
    pub command_mode: CommandMode,
    pub highlights: Vec<(HexCoord, HighlightKind)>,

    /// Deployment zones (attacker/defender) shown during deployment phase.
    pub deployment_zones: Option<(Vec<HexCoord>, Vec<HexCoord>)>,

    /// Unit being drag-deployed: set on left-press over an own
    /// unit during Deployment, consumed on release. While set, a ghost
    /// preview follows the cursor over valid zone hexes.
    pub deploy_drag: Option<usize>,

    /// OOB deployment: the unit picked in the OOB window, waiting
    /// for a left-click on a valid zone hex to enter the map. Right-click /
    /// Esc cancels. Mutually exclusive with `deploy_drag`.
    pub deploy_placing: Option<usize>,

    /// Sector deployment: the division being placed into a
    /// player-drawn rectangle ("deploy to sector"). Mutually exclusive with
    /// `deploy_placing` / `deploy_drag`.
    pub deploy_sector: Option<SectorPick>,

    /// Sector-deployment preview: while the player drags the
    /// rectangle, a throwaway ai_deploy run on a cloned roster computes
    /// where the division WOULD land — unit id → hex, rendered as
    /// translucent ghosts. Cleared when picking ends.
    pub sector_preview: Vec<(usize, HexCoord)>,

    /// Dirty flags driving re-render.
    pub board_mesh_dirty: bool,
    pub board_colors_dirty: bool,
    pub units_dirty: bool,
    /// Last rendered facing (the `from_rotation_y` value) per unit id —
    /// persists across `units_dirty` respawns so a rebuilt model keeps its
    /// facing instead of snapping back to the spawn default every turn.
    pub unit_facing: std::collections::HashMap<usize, f32>,
    /// Move-order route arrows need a rebuild (§6.2 order issued / advanced /
    /// consumed).
    pub orders_dirty: bool,
    /// The allied sector-suggestion overlay rebuilds (§7.5) —
    /// set when a suggestion rect is stored / cleared / restored.
    pub ally_sectors_dirty: bool,
    /// Set when a NEW order is issued: the route ribbon plays its grow-from-
    /// unit animation. Plain path advances just rebuild.
    pub arrows_grow: bool,

    /// The side the local player commands (fog is computed for this side).
    pub player_side: tactical_core::unit::Side,

    /// Set by egui systems: true when the cursor is over a UI panel
    /// (camera/picking must ignore input then).
    pub pointer_over_ui: bool,

    /// Testing aid (F8): fog reports everything Visible so AI deployment /
    /// fog leaks can be inspected. Not used by game logic, render-only.
    pub debug_no_fog: bool,

    /// Standing attack orders of the CURRENT acting side (pre-order
    /// mode): registered by right-clicks (player) or the AI planner, resolved
    /// together in the unified fire phase at end of turn. Cleared after each
    /// fire phase. One order per unit per turn (re-registering replaces).
    pub attack_orders: Vec<AttackOrder>,

    /// Deployment-phase attachment re-assignment: Some((host id,
    /// support index)) after the player clicks an attachment row in the OOB
    /// window — the next battalion clicked there becomes the new host.
    pub attach_picking: Option<(usize, usize)>,

    /// Division-order target picking (Seize hex / Engage unit) —
    /// the HQ radial bar's square buttons arm it; the next map left-click
    /// commits, right-click / Esc cancels.
    pub div_pick: Option<DivPick>,

    /// Battle-report deferral: units whose resolved state already
    /// changed (broken / annihilated / surrendered / assault-advanced) but
    /// which must KEEP rendering at their pre-combat hex until the player
    /// confirms that engagement's report — unit id → the hex to render at.
    /// Entries are released (slide / disappear) by `tick_battle_tour`.
    pub report_ghosts: HashMap<usize, HexCoord>,
}

/// One attack lane inside an engagement report.
#[derive(Debug, Clone)]
pub struct ReportLane {
    pub attacker: String,
    /// Localized attack label (`attack_kind.*` from 30-names_l_*.yml, §15).
    pub kind: String,
    pub org: f32,
    pub str_: f32,
    pub counter_org: f32,
    pub counter_str: f32,
    pub shocked_defender: bool,
    /// The victim is on the ACTING side (rocket friendly fire).
    pub friendly: bool,
    /// The full formula chain behind this lane (None when the
    /// order fizzled) — the「细节」button opens it.
    pub detail: Option<Box<EngagementDetailView>>,
}

/// One engagement's complete formula chain, attacker side + counter side —
/// shared by the battle-report modal's per-lane「细节」button
/// and the battle log's clickable exchange lines — both entry points open
/// the same engagement-detail window with the same content.
#[derive(Debug, Clone)]
pub struct EngagementDetailView {
    pub attacker: String,
    pub defender: String,
    /// Localized attack label (same source as `ReportLane::kind`).
    pub kind: String,
    pub hex: HexCoord,
    /// The victim is on the acting side (rocket/area splash).
    pub friendly: bool,
    /// The ACTING side is the player's — the first-level header colours by
    /// side (PARCHMENT yours / GOLD enemy, same semantics as the report
    /// modal's your/enemy attack line); the counter block inverts it.
    pub yours: bool,
    /// The outgoing strike's chain (attacker → defender).
    pub hit: tactical_core::damage::HitBreakdown,
    /// The counter-fire chain (defender → attacker), when it fired back.
    pub counter: Option<tactical_core::damage::HitBreakdown>,
    pub shocked_defender: bool,
    pub shocked_attacker: bool,
}

/// The open engagement-detail window — a plain non-modal
/// egui window; the X clears this resource.
#[derive(Resource, Default)]
pub struct DetailWindow(pub Option<EngagementDetailView>);

/// One unit whose on-board visual must lag behind the resolved game state
/// until its engagement report is confirmed: the unit keeps rendering at
/// `from` (its pre-combat hex) while the report modal shows the engagement;
/// [Continue] releases it
/// — a broken defender slides to its retreated hex, an annihilated /
/// surrendered one disappears, an assault attacker slides onto the vacated
/// hex. Never play the consequence before the player clicks it away.
#[derive(Debug, Clone, Copy)]
pub struct PendingUnitVisual {
    pub unit_id: usize,
    pub from: HexCoord,
}

/// One engagement = every lane landing on one defender.
#[derive(Debug, Clone)]
pub struct BattleReport {
    pub hex: HexCoord,
    pub defender: String,
    /// The side whose fire phase produced this report.
    pub acting: tactical_core::unit::Side,
    pub lanes: Vec<ReportLane>,
    /// Empty, or the localized outcome word (`outcome.*` from
    /// 30-names_l_*.yml, §15).
    pub outcome: String,
    /// This engagement's combat animation (tracers + damage
    /// floaters), played when the report modal SHOWS the engagement — not
    /// in one burst at end of turn.
    pub fx: Vec<crate::fx::FxEvent>,
    /// Deferred unit visuals (repel / annihilation / assault
    /// advance) released when the player confirms THIS report.
    pub pending: Vec<PendingUnitVisual>,
}

/// How long the camera keeps dwelling on a just-confirmed
/// report's hex when its consequence (repel slide / annihilation vanish) is
/// animating, before jumping to the next engagement — a one-hex slide takes
/// ~0.4 s, so 0.8 s lets it read without stalling the tour.
pub const REPORT_RELEASE_LINGER_SECS: f32 = 0.8;

/// End-of-turn battle reports: after the fire phase a modal walks
/// every engagement — the camera focuses the hex while the report window
/// lists each attacker / damage / counter-fire; [Continue] advances, Esc
/// skips the rest. Replaces the timed notice-bar tour.
#[derive(Resource, Default)]
pub struct BattleTour {
    pub queue: Vec<BattleReport>,
    pub index: usize,
    pub active: bool,
    /// Camera focus already issued for this index (tick system).
    pub focused: Option<usize>,
    /// The player's camera distance from before the tour — the
    /// report playback pins the zoom to REPORT_CAM_DISTANCE and restores
    /// this when the queue drains.
    pub saved_zoom: Option<f32>,
    /// After confirming a report whose consequence is animating,
    /// the camera dwells on the old hex until this timestamp (elapsed secs)
    /// so the player actually SEES the repel slide / disappearance before
    /// the focus jumps to the next engagement.
    pub next_focus_at: Option<f32>,
    /// The camera is gliding to the current report's hex — the
    /// combat FX and the report window wait for the arrival pulse.
    pub awaiting_arrival: bool,
    /// The queue drained and the camera is gliding back to the
    /// pre-tour view; the tour tears down when it lands.
    pub returning: bool,
    /// The pre-tour camera target — the return glide's
    /// destination together with `saved_zoom`.
    pub saved_target: Option<bevy::prelude::Vec3>,
}

/// Which floating UI windows are open. The right panel is static;
/// every on-demand surface lives here so any system can open/close them.
#[derive(Resource, Default)]
pub struct UiWindows {
    /// Esc global menu — modal: while open, map input is frozen entirely
    /// (ui_pointer_guard forces pointer_over_ui).
    pub esc_menu: bool,
    /// Standalone battle-log window (Esc menu → "Game Log"; auto-opened only
    /// by battle end — sync summaries use their own modal instead).
    pub log: bool,
    /// Order of Battle window (right-panel "Order of Battle" button).
    pub oob: bool,
    /// "Reset Battle…" confirmation dialog (inside the Esc menu).
    pub confirm_reset: bool,
    /// "Exit Game" confirmation (live mode with an unsynced battle).
    pub confirm_exit: bool,
    /// Sync-completion prompt — top-anchored modal with the damage
    /// summary and Continue / End Tactic (map input frozen while open).
    pub sync_prompt: bool,
    /// Center confirmation behind the sync prompt's End Tactic.
    pub confirm_end: bool,
    /// "Recall deployed units to the OOB" confirmation —
    /// Some(All) recalls the whole player force, Some(Division(d)) one.
    pub confirm_recall: Option<RecallKind>,
    /// In-battle settings window (Esc menu → Settings): render/
    /// performance knobs (MSAA / shadows / render scale / frame cap / idle
    /// saver), hot-applied via `BattleSettings` (settings.rs). Modal like
    /// the Esc menu itself — map input freezes, Esc peels it first.
    pub settings: bool,
    /// One-frame deferral for the Esc → Settings handoff (without it the
    /// swap "jitters"). The Settings click lands INSIDE the Esc menu's draw
    /// pass, so opening the settings window immediately draws BOTH centered
    /// modals on that frame (settings on top) — one overlapped frame, which
    /// at low fps lingers ~100 ms and reads as a shake. Set to 2 by the
    /// button; draw_settings_window counts down and opens on the NEXT pass,
    /// making the swap atomic (Esc frame → settings frame, never both).
    pub settings_open_delay: u8,
}

impl UiWindows {
    /// Any modal surface open → freeze ALL map input (pointer guard forces
    /// pointer_over_ui; click handlers early-return). The settings-open
    /// countdown frame counts too — no one-frame input gap in the handoff.
    pub fn modal_open(&self) -> bool {
        self.esc_menu
            || self.settings
            || self.settings_open_delay > 0
            || self.sync_prompt
            || self.confirm_end
    }

    /// Esc peels off the topmost modal layer.
    pub fn close_modal(&mut self) {
        if self.settings {
            self.settings = false;
        } else if self.settings_open_delay > 0 {
            // Esc inside the handoff frame cancels the pending open.
            self.settings_open_delay = 0;
        } else if self.confirm_end {
            self.confirm_end = false;
        } else if self.confirm_reset {
            self.confirm_reset = false;
        } else if self.confirm_exit {
            self.confirm_exit = false;
        } else if self.sync_prompt {
            self.sync_prompt = false;
        } else if self.esc_menu {
            self.esc_menu = false;
            self.confirm_reset = false;
            self.confirm_exit = false;
        }
    }
}

/// An hourly sync's clock receipt timed out (§8.4) — HOI4 did
/// not prove it advanced the strategic hour (menu/pause stall or a dead
/// game). A modal asks the player to check HOI4 and choose; the bin-side
/// injector holds ALL further injection work until `action` leaves
/// `Pending`. Present only while unresolved (inserted on the stall,
/// removed on resolution).
#[derive(Resource)]
pub struct SyncStall {
    /// Game-hour prefix captured before the stalled batch — a retry
    /// re-waits for a move away from it (and counts an already-moved
    /// prefix as success without re-sending the clock command: a stale
    /// `pause_in_hours` timer may have caught the hour up on its own).
    pub prev_prefix: String,
    /// Receipt waits failed so far (1 = the initial attempt; the dialog
    /// shows the count from the first failed retry on).
    pub attempts: u32,
    /// The player's dialog choice, consumed by the bin-side injector.
    pub action: SyncStallAction,
}

/// The sync-stall dialog's three states.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SyncStallAction {
    /// Dialog open, no choice yet — the injector waits.
    #[default]
    Pending,
    /// "Retry Sync": re-issue the clock advance ONLY (the damage lines
    /// already landed — re-sending them would double-apply).
    Retry,
    /// "Cancel Sync": accept the unconfirmed hour and continue (the
    /// original log-and-continue path; the clock lags one hour).
    Cancel,
}

/// The desync guard fired (DESIGN.md §3.2/§8.2): the sync probes proved
/// the strategic world no longer matches this battle (clock receipt off
/// by other than one hour, or the save's played country changed). A modal
/// asks the player to end the battle (abort cleanup batch) or fight on
/// unsynced (the session's sync pipeline turns off). Present only while
/// unresolved (inserted on the probe, removed on resolution); while
/// pending, the bin-side injector holds ALL injection work.
#[derive(Resource)]
pub struct DesyncAlert {
    /// Which probe fired — the dialog body explains this verdict.
    pub verdict: tactical_sync::desync::DesyncVerdict,
    /// The player's dialog choice, consumed by the bin-side injector.
    pub action: DesyncAction,
}

/// The desync dialog's two choices.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DesyncAction {
    /// Dialog open, no choice yet — the injector waits.
    #[default]
    Pending,
    /// "End Tactical Battle": queue the abort cleanup batch
    /// (`build_early_exit_batch(false)`) and end the session.
    EndBattle,
    /// "Continue Unsynced": disable the sync pipeline for the rest of the
    /// battle — no clock advance, no savecheck/roster, no damage writeback;
    /// every later exit sends the same cleanup-only batch.
    ContinueUnsynced,
}

/// Top-center notice bar. Phase-driven pinned hints are computed
/// on the fly by the draw system; `flash` carries one-shot event notices.
/// Doubles as the template for future tutorial / ops hints.
#[derive(Resource, Default)]
pub struct NoticeBar {
    pub flash: Option<FlashNotice>,
    /// A turn banner held back while the battle-report tour is
    /// still playing — emitted (with a fresh expiry) once the tour drains,
    /// so the next side's banner never overlaps the previous side's
    /// resolution reports.
    pub pending: Option<FlashNotice>,
}

/// One-shot event notice in the top bar.
pub struct FlashNotice {
    pub text: String,
    /// Expiry in elapsed seconds; the final 0.5 s is the fade-out.
    pub until: f32,
    /// `Some(side)` → faction-colored turn banner (side-color
    /// background + thick side-color border); `None` → default parchment.
    pub side: Option<Side>,
}

impl FlashNotice {
    pub fn plain(text: String, until: f32) -> Self {
        FlashNotice {
            text,
            until,
            side: None,
        }
    }

    /// Turn handover banner: 2.5 s full visibility, then a 0.5 s
    /// fade (3 s total).
    pub fn turn(text: String, side: Side, now: f32) -> Self {
        FlashNotice {
            text,
            until: now + 3.0,
            side: Some(side),
        }
    }
}

impl Default for TacticalState {
    fn default() -> Self {
        TacticalState {
            grid: None,
            units: Vec::new(),
            fog: None,
            ai_fog: None,
            turn: 0,
            selected_unit: None,
            hover_hex: None,
            command_mode: CommandMode::Select,
            highlights: Vec::new(),
            deployment_zones: None,
            deploy_drag: None,
            deploy_placing: None,
            deploy_sector: None,
            sector_preview: Vec::new(),
            board_mesh_dirty: false,
            board_colors_dirty: false,
            units_dirty: false,
            unit_facing: std::collections::HashMap::new(),
            orders_dirty: false,
            ally_sectors_dirty: false,
            arrows_grow: false,
            player_side: tactical_core::unit::Side::Attacker,
            pointer_over_ui: false,
            debug_no_fog: false,
            attack_orders: Vec::new(),
            attach_picking: None,
            div_pick: None,
            report_ghosts: HashMap::new(),
        }
    }
}

impl TacticalState {
    pub fn highlight_at(&self, h: HexCoord) -> Option<HighlightKind> {
        self.highlights
            .iter()
            .find(|(x, _)| *x == h)
            .map(|(_, k)| *k)
    }

    pub fn set_highlights(&mut self, list: Vec<(HexCoord, HighlightKind)>) {
        // Equality short-circuit: several click paths re-set the IDENTICAL
        // highlight set — dirtying the board palette for those was a free
        // full recolor per click.
        if self.highlights == list {
            return;
        }
        self.highlights = list;
        self.board_colors_dirty = true;
    }

    pub fn clear_highlights(&mut self) {
        if !self.highlights.is_empty() {
            self.highlights.clear();
            self.board_colors_dirty = true;
        }
    }

    pub fn fog_state(&self, h: HexCoord) -> VisibilityState {
        if self.debug_no_fog {
            return VisibilityState::Visible;
        }
        match &self.fog {
            Some(f) => f.state(h, self.turn),
            None => VisibilityState::Visible,
        }
    }

    /// The AI side's fog view: mirrors `fog_state` for the
    /// opposing side so AI planning / route refresh share the player's
    /// information limits. `debug_no_fog` is a player-side aid only.
    pub fn ai_fog_state(&self, h: HexCoord) -> VisibilityState {
        match &self.ai_fog {
            Some(f) => f.state(h, self.turn),
            None => VisibilityState::Visible,
        }
    }

    pub fn unit_at(&self, h: HexCoord) -> Option<&BattalionUnit> {
        self.units
            .iter()
            .find(|u| u.position == h && u.is_combat_effective())
    }

    /// Enemy unit at `h` that may be attacked (§6.8): combat-effective OR
    /// Retreating — broken units stay valid targets (pursuit). Fog-hidden
    /// enemies are never returned — targeting must not reveal them.
    pub fn attack_target_at(&self, h: HexCoord, player: Side) -> Option<&BattalionUnit> {
        self.units.iter().find(|u| {
            u.position == h
                && u.side != player
                && u.is_targetable()
                && self.fog_state(u.position) == VisibilityState::Visible
        })
    }

    pub fn unit_by_id(&self, id: usize) -> Option<&BattalionUnit> {
        self.units.iter().find(|u| u.id == id)
    }
}

/// Render-quality knobs sourced from settings.json. Inserted by the bin
/// crate at startup — these are the STARTUP values only, read by
/// `spawn_camera` / `setup_board`; after the window is up the live source of
/// truth is `BattleSettings` (settings.rs), written by the in-battle
/// Settings window and hot-applied by `apply_battle_settings`. The menu
/// hot-applies its own edits directly onto the camera / light (menu.rs
/// apply_menu_quality).
#[derive(Resource)]
pub struct RenderQuality {
    /// MSAA sample count on the 3D camera (settings: 4 / 2 / off).
    pub msaa: Msaa,
    /// Directional-light shadows on at all (settings shadow level > 0).
    /// Read by `setup_board` when spawning the sun; the menu
    /// hot-applies by rewriting the light's `shadows_enabled` directly.
    pub shadows: bool,
    /// Shadow quality level (settings `shadow`: 0 = off, 1 = low, 2 = high).
    /// The level drives REAL cost tiers, not just the map size
    /// — low = 1024 map + ONE cascade + Hardware2x2 filtering; high = 2048
    /// map + two cascades + Gaussian (9-tap) filtering. Read by
    /// `spawn_camera` (filtering) and `setup_board` (cascades).
    pub shadow_level: u32,
}

impl Default for RenderQuality {
    fn default() -> Self {
        // Placeholder until apply_render_quality installs the settings.json
        // values; mirrors the current defaults (MSAA 2×, shadow Low).
        RenderQuality {
            msaa: Msaa::Sample2,
            shadows: true,
            shadow_level: 1,
        }
    }
}
