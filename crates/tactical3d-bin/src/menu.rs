//! Main menu: the default entry point. A full Bevy app whose
//! backdrop is a slowly orbiting 3D province map (a random land
//! province from the real HOI4 map per launch, synthetic arena without
//! HOI4) with a frosted-glass egui menu on top: a Live listen
//! toggle, Debug Battle (forged-tac_start form), Settings, Exit.
//!
//! Frosted glass note: egui has no real backdrop blur (that would need a
//! render-to-texture + shader pass), so the panels fake it with a dark
//! 80%-alpha fill + brass border + shadow over the bright 3D map.
//!
//! Architecture: battles run as CHILD PROCESSES of this
//! menu — winit allows only one EventLoop per process (`RecreationAttempt`
//! panic), so an in-process menu↔battle loop is impossible on Windows. The
//! menu stays resident while a battle child runs; instead of sitting
//! minimized in the taskbar it hides to the SYSTEM TRAY
//! (tray.rs — double-click the icon to restore, right-click for Show/Exit)
//! and is restored on exit. The live listen keeps listening across battles.

use std::process::{Child, Command, Stdio};

use bevy::prelude::*;
use bevy::window::{ExitCondition, PrimaryWindow, WindowCloseRequested};
use bevy::winit::WinitWindows;
use bevy_egui::{egui, EguiContexts, EguiPlugin};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tactical3d_render::camera::RtsCamera;
use tactical3d_render::state::TacticalState;
use tactical3d_render::ui::CenterAnchor;
use tactical3d_render::Tactical3dPlugin;
use tactical3d_render::{
    fonts,
    icons::{init_icons_once, IconId, IconSet},
    locale::LocaleRes,
};
use tactical_core::hex::HexDirection;
use tactical_core::unit::Side;
use tactical_locale::{Language, Locale};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, GetWindowThreadProcessId, IsIconic,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOP,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SW_MINIMIZE, SW_RESTORE, SW_SHOW, WS_EX_TOOLWINDOW,
};

use crate::scenario::{with_generator, ForceChoice, MapChoice, Scenario, TACTICS};
use crate::settings::{
    detect_hoi4_settings_txt, force_text_saves, list_saves, read_save_as_binary, AppSettings,
    TextSaveFix,
};

/// Entry point (default, no CLI args): run the menu until Exit Game.
pub fn menu_loop() {
    let mut settings = AppSettings::load();
    // Sweep stale per-process injection batch files
    // (`tac_inject_<pid>.txt`) left behind by battle instances — one small
    // file per battle used to linger in the HOI4 user dir forever. The
    // mtime guard keeps any still-running battle's fresh batch untouched.
    if let Some(dir) = settings
        .saves_dir()
        .and_then(|d| d.parent().map(|p| p.to_path_buf()))
    {
        match tactical_inject::cleanup_stale_batch_files(&dir, tactical_inject::STALE_BATCH_MAX_AGE)
        {
            Ok(0) => {}
            Ok(n) => battle_log(&format!(
                "[menu] swept {n} stale tac_inject_<pid> batch file(s) from {}",
                dir.display()
            )),
            Err(e) => battle_log(&format!("[menu] stale batch sweep failed: {e}")),
        }
    }
    menu_app(&mut settings);
}

// ---------------------------------------------------------------------------
// Menu app state

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Settings,
    About,
    DebugForm,
}

#[derive(Resource)]
struct MenuState {
    page: Page,
    settings: AppSettings,
    status: String,
    live_listen: bool,
    /// First launch (no settings.json at startup): land on the
    /// Settings page so the player confirms the auto-detected HOI4 paths;
    /// cleared on the first successful save.
    first_run: bool,
    /// Exit Game clicked and the confirm modal is up (minimize to
    /// tray / quit / cancel).
    confirm_exit: bool,
    /// Render-quality values changed on the Settings page —
    /// `apply_menu_quality` pushes them onto the camera / shadow resource.
    quality_dirty: bool,
    /// Diagnostic-export checkbox: bundle the newest .hoi4 save
    /// too. Default on — live/injection bug reports need it.
    diag_include_save: bool,
    /// About-page disclaimer window is open.
    show_disclaimer: bool,
    /// CenterAnchor state for the centered menu surfaces (jitter fix —
    /// see tactical3d_render::ui::CenterAnchor): one per window/area.
    anchors: MenuAnchors,
}

/// One CenterAnchor per centered menu surface (pages, exit confirm, the
/// bottom-right caption). The pick dialog's anchor lives in LiveListen
/// (draw_pick_dialog takes MenuState immutably).
#[derive(Default)]
struct MenuAnchors {
    main: CenterAnchor,
    settings_page: CenterAnchor,
    about: CenterAnchor,
    debug_form: CenterAnchor,
    exit_confirm: CenterAnchor,
    disclaimer: CenterAnchor,
    caption: CenterAnchor,
}

/// Backdrop caption shown in the bottom-right corner so the random
/// province id stays visible.
#[derive(Resource)]
struct BackdropInfo(String);

/// A running battle child process; stderr is piped so a failed assembly
/// surfaces in the menu instead of vanishing into the void. The pipe is
/// drained by a reader thread for the child's whole life (a full OS pipe
/// buffer blocks the child's writes forever);
/// the accumulated tail lands in `stderr_buf`.
struct BattleChild {
    child: Child,
    label: String,
    stderr_buf: std::sync::Arc<std::sync::Mutex<String>>,
    /// Drain-thread handle — joined on reap so the error tail is complete
    /// (the pipe hits EOF when the child dies, so the join never blocks).
    drain: Option<std::thread::JoinHandle<()>>,
}

/// The tray icon's hover tooltip shows exactly three states:
/// idle, live listen on, battle running (battle wins over listen).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum TrayTipState {
    #[default]
    Idle,
    Listening,
    Battle,
}

/// Running battle children (normally at most one) + system-tray bookkeeping
/// (the menu is a RESIDENT tray app — the icon installs
/// at startup and lives until exit; the window hides to it on battle start,
/// on X-close, and on the Exit button's "minimize to tray").
#[derive(Resource, Default)]
struct BattleChildren {
    running: Vec<BattleChild>,
    /// Last exit report shown on the main page.
    last_report: String,
    /// Menu window hidden (to the tray) while a battle child holds the screen
    /// or the user closed/X-hid it.
    hidden: bool,
    /// The current hide came from the battle edge (battle start hid the
    /// window). Only that case is auto-restored on battle end — a user X-close
    /// stays hidden (an unconditional falling-edge restore once "popped
    /// the window back" on the next frame after a deliberate close).
    hidden_by_battle: bool,
    /// Previous frame's battle-alive state, for the rising-edge detection.
    was_alive: bool,
    /// The resident tray icon (None only if the shell refused it).
    tray: Option<crate::tray::TrayIcon>,
    /// Tray install was attempted and failed — fall back to the pre-tray
    /// behaviors (battle minimize, X closes the app, Exit quits directly).
    tray_failed: bool,
    /// UI requested hiding the window to the tray (Exit-confirm modal).
    pending_hide: bool,
    /// UI requested showing the window back from the tray (the
    /// multi-battle picker must not sit unseen while the menu is hidden).
    pending_show: bool,
    /// Tooltip state currently applied to the tray icon (change-only update).
    tip_state: TrayTipState,
    /// Raise retries after a restore: the first
    /// `force_window_to_front` can still lose the race — the battle window
    /// may outlive our restore attempt by a few hundred ms, or a busy
    /// foreground app can make the attach attempt fail. While >0 the reaper
    /// re-raises on a 0.25 s cadence until the window owns the foreground.
    restore_raise_retries: u8,
    /// Accumulator for the retry cadence above.
    restore_raise_timer: f32,
}

impl BattleChildren {
    fn alive(&self) -> bool {
        !self.running.is_empty()
    }
}

#[derive(Resource)]
struct DebugForm {
    /// Manual form vs script file (data/battles/*.json).
    use_script: bool,
    script_index: usize,
    script_files: Vec<std::path::PathBuf>,
    map_synthetic: bool,
    province_id: String,
    dirs: String,
    atk_force: ForceSel,
    def_force: ForceSel,
    save_index: usize,
    save_files: Vec<std::path::PathBuf>,
    tactic: usize,
    player_defender: bool,
    atk_tag: String,
    def_tag: String,
    error: String,
    /// Saves-dir path the current `save_files` list was loaded from —
    /// reload the list when the user changes the dir in Settings.
    saves_dir_loaded: String,
    /// The nation selector's cache - the script stem + side it
    /// was computed for, then per resolved tag (first-appearance order) the
    /// tag and its divisions. Reloaded when the selection changes.
    nations_key: (String, bool),
    script_nations: Vec<(String, Vec<String>)>,
    /// Per-tag command override (tag -> player-controlled), seeded from the
    /// script's `divisions:` block on reload, edited by the selector rows.
    /// Serialized to the child as `div_control=TAG:player|ai`.
    div_control: std::collections::HashMap<String, bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForceSel {
    Panzer,
    Infantry,
    Mixed,
    FromSave,
}

impl ForceSel {
    const ALL: [ForceSel; 4] = [
        ForceSel::Panzer,
        ForceSel::Infantry,
        ForceSel::Mixed,
        ForceSel::FromSave,
    ];
    /// Localized dropdown label (DESIGN §15).
    fn label<'a>(self, loc: &'a Locale) -> std::borrow::Cow<'a, str> {
        loc.tr(match self {
            ForceSel::Panzer => "menu.debug.force.panzer",
            ForceSel::Infantry => "menu.debug.force.infantry",
            ForceSel::Mixed => "menu.debug.force.mixed",
            ForceSel::FromSave => "menu.debug.force.from_save",
        })
    }
    fn to_choice(self, tag: &str) -> ForceChoice {
        match self {
            ForceSel::Panzer => ForceChoice::Preset(1),
            ForceSel::Infantry => ForceChoice::Preset(2),
            ForceSel::Mixed => ForceChoice::Preset(3),
            ForceSel::FromSave => ForceChoice::FromSave {
                tag: tag.to_string(),
            },
        }
    }
}

impl Default for DebugForm {
    fn default() -> Self {
        DebugForm {
            use_script: false,
            script_index: 0,
            script_files: crate::script::list_scripts(),
            map_synthetic: true,
            province_id: "3560".into(),
            dirs: "E,NE".into(),
            atk_force: ForceSel::Panzer,
            def_force: ForceSel::Infantry,
            save_index: 0,
            save_files: Vec::new(),
            tactic: 1, // Elastic Defense
            player_defender: false,
            atk_tag: "GER".into(),
            def_tag: "FRA".into(),
            error: String::new(),
            saves_dir_loaded: String::new(),
            nations_key: (String::new(), false),
            script_nations: Vec::new(),
            div_control: std::collections::HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Backdrop

/// Backdrop province pixel-area window (with HOI4
/// configured the backdrop is a RANDOM land province from the whole map,
/// not a fixed list of pre-built ones). Below the minimum the province is
/// an island speck; above the maximum it would be squeezed against the
/// 512×512 grid cap (mega-provinces). Without HOI4 the fallback is the
/// synthetic arena grid.
const BACKDROP_MIN_PX: u32 = 60;
const BACKDROP_MAX_PX: u32 = 4000;

fn backdrop_state(
    settings: &AppSettings,
) -> (
    TacticalState,
    Option<(String, tactical_core::hex::HexCoord)>,
    String,
) {
    // The caption is baked once at startup (before the App exists), so it
    // reads the language straight from settings (DESIGN §15).
    let loc = Locale::load(settings.language());
    // Seeded pick: nanos alone collide on quick relaunches (the same
    // province came up twice in a row) — hash nanos + pid for a real spread.
    let seed = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut h);
        std::process::id().hash(&mut h);
        h.finish()
    };
    let built = with_generator(settings, |gen| {
        let candidates: Vec<u32> = gen
            .land_province_areas()
            .into_iter()
            .filter(|(_, area)| (BACKDROP_MIN_PX..=BACKDROP_MAX_PX).contains(area))
            .map(|(id, _)| id)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        // xorshift stream off the seed for the province pick.
        let mut s = seed;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let id = candidates[(next() as usize) % candidates.len()];
        // Backdrop shows the PURE single province:
        // pass NO attack direction — any dir would make the staging-strip
        // stitching fold a neighbouring strip into the map. (Battles still
        // pass real dirs; only the backdrop suppresses them.)
        // A panicking province must not kill the menu before the first
        // window appears: catch and fall back to the synthetic arena grid.
        // The panic hook still records the details to crash.log, so no
        // evidence is lost.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gen.generate(id, &[]).ok()))
            .ok()
            .flatten()
            .map(|m| (m, id))
    })
    .flatten();
    if let Some((tmap, id)) = built {
        let info = match &tmap.vp_label {
            Some((name, _)) => loc.trf(
                "menu.backdrop.province_vp",
                &[("vp", name), ("id", &id.to_string())],
            ),
            None => loc.trf("menu.backdrop.province", &[("id", &id.to_string())]),
        };
        (
            TacticalState {
                grid: Some(std::sync::Arc::new(tmap.grid.clone())),
                board_colors_dirty: true,
                ..default()
            },
            tmap.vp_label.clone(),
            info,
        )
    } else {
        let g = crate::demo::arena_grid();
        (
            TacticalState {
                grid: Some(std::sync::Arc::new(g)),
                board_colors_dirty: true,
                ..default()
            },
            None,
            loc.tr("menu.backdrop.arena").into_owned(),
        )
    }
}

// ---------------------------------------------------------------------------
// The menu app itself

fn menu_app(settings: &mut AppSettings) {
    let (state, vp_label, backdrop_info) = backdrop_state(settings);
    // UI language from settings.json (DESIGN §15): loaded once here; the
    // Settings page's language selector rebuilds the resource on switch.
    let loc = Locale::load(settings.language());
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: loc.tr("app.title").into_owned(),
            resolution: (1440.0_f32, 900.0_f32).into(),
            present_mode: bevy::window::PresentMode::Fifo,
            // Explicit centering: the OS default is cascade placement, which
            // drifts down-right every launch.
            position: bevy::window::WindowPosition::Centered(
                bevy::window::MonitorSelection::Primary,
            ),
            ..default()
        }),
        // Resident tray: the menu keeps running without its
        // window — the X closes-to-tray (tray_close_requested), never the
        // app. Bevy's default close_when_requested would despawn the window
        // and exit_on_all_closed would quit the app once no windows remain;
        // both are off here.
        exit_condition: ExitCondition::DontExit,
        close_when_requested: false,
        ..default()
    }));
    app.add_plugins(EguiPlugin);
    // CJK font chain + icon textures + string table (DESIGN §15); the real
    // language is inserted below (insert-after-init wins).
    app.init_resource::<IconSet>();
    app.init_resource::<LocaleRes>();
    crate::window::start_maximized(&mut app);
    // MSAA / shadow-map size from settings.json. The menu
    // deliberately does NOT get the idle frame-saver: its orbiting backdrop
    // is an always-on animation (and post-bake it is two draw calls anyway).
    crate::window::apply_render_quality(&mut app, settings);
    // The menu's shadow clamp (the menu-only clamp below) is Low-tier by
    // design — clamp the tier too so spawn_camera builds Hardware2x2
    // filtering and setup_board a single cascade, whatever the battle
    // quality is. (Shadow off stays off.)
    if let Some(mut q) = app
        .world_mut()
        .get_resource_mut::<tactical3d_render::state::RenderQuality>()
    {
        q.shadow_level = q.shadow_level.min(1);
    }
    app.add_plugins(Tactical3dPlugin);
    // Menu GPU pass: a foreground menu measured ~30%
    // GPU at the 60 fps cap for a scene whose only moving part is a 0.045
    // rad/s camera yaw (≈140 s/rev). Two menu-only clamps, applied here at
    // startup (Tactical3dPlugin init'd the gate) and by apply_menu_quality
    // on Settings-page edits — MSAA is deliberately NOT clamped (low MSAA
    // moirés badly on the menu backdrop):
    //  1. the cap rides the separate `menu_fps` setting (default 30) — the
    //     menu never idle-throttles, so this is its only GPU lever, and the
    //     slow cinematic orbit reads identically at 24–30 fps;
    //  2. the shadow map is clamped to Low (1024) — the sun and scene never
    //     move, so High only quadruples a fragment cost nobody sees.
    if let Some(mut gate) = app
        .world_mut()
        .get_resource_mut::<tactical3d_render::gate::RenderGate>()
    {
        gate.max_fps = settings.menu_fps();
    }
    app.insert_resource(bevy::pbr::DirectionalLightShadowMap {
        size: settings.menu_shadow_map_size() as usize,
    });
    // The backdrop orbits on its own — no user camera input in the menu.
    app.insert_resource(tactical3d_render::camera::CameraInputLocked);
    app.insert_resource(tactical3d_render::fx::VpLabel(vp_label));
    app.insert_resource(BackdropInfo(backdrop_info));
    app.insert_resource(state);
    // A fresh install (no settings.json yet) lands on the
    // Settings page so the player confirms the auto-detected HOI4 paths.
    let first_run = !crate::settings::settings_file_exists();
    app.insert_resource(MenuState {
        page: if first_run {
            Page::Settings
        } else {
            Page::Main
        },
        settings: settings.clone(),
        status: String::new(),
        live_listen: false,
        first_run,
        confirm_exit: false,
        quality_dirty: false,
        diag_include_save: true,
        show_disclaimer: false,
        anchors: MenuAnchors::default(),
    });
    app.insert_resource(BattleChildren::default());
    app.insert_resource(LiveListen {
        listener: None,
        timer: 0.0,
        status: String::new(),
        status_error: false,
        pending_start: None,
        pick_dialog: None,
        abort_slot: None,
        pick_anchor: CenterAnchor::default(),
    });
    app.insert_resource(LocaleRes(loc));
    let mut form = DebugForm::default();
    form.save_files = settings
        .saves_dir()
        .map(|d| list_saves(&d, 20))
        .unwrap_or_default();
    app.insert_resource(form);
    app.add_systems(
        Update,
        (
            fonts::install_ui_fonts_once,
            init_icons_once,
            menu_render_gate,
            menu_orbit_camera,
            menu_pointer_guard,
            apply_menu_quality,
            live_listen_tick,
            tray_close_requested,
            reap_battle_children,
            crate::splash::auto_close,
            crate::app_icon::apply,
            // Hidden/minimized: don't even paint the egui menu — nothing is
            // presented. No input can arrive while hidden.
            draw_menu.run_if(tactical3d_render::gate::gate_open),
        )
            .chain(),
    );
    app.run();
    // NB: settings edits persist via the Settings page's Save button
    // (settings.json); nothing to harvest here — Bevy 0.15 App::run()
    // empties the app (mem::replace with App::empty), so resources cannot
    // be read back after run() returns.
}

/// Slow cinematic orbit for the backdrop (user input still works on top).
fn menu_orbit_camera(time: Res<Time>, mut q: Query<&mut RtsCamera>) {
    for mut cam in q.iter_mut() {
        cam.yaw += time.delta_secs() * 0.045;
    }
}

/// Stop ALL GPU work while the menu window is hidden to the tray (battle
/// child running / X-close hide) or minimized: the
/// menu is a Continuous-mode app, and on Windows bevy_winit re-arms redraws
/// forever — a minimized window renders the orbiting backdrop UNCAPPED
/// (~178 fps measured, see the hide_menu_window note below) and steals GPU
/// from the
/// battle child. With no active camera there is no main/shadow pass, and the
/// sleep keeps the Continuous loop from free-running a whole core; the
/// schedule keeps ticking, so live-listen polling / child reaping / the tray
/// are unaffected. (The presented 1×1 frames are never seen — minimized.)
fn menu_render_gate(
    children: Res<BattleChildren>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut gate: ResMut<tactical3d_render::gate::RenderGate>,
    mut q_cam: Query<&mut Camera, With<RtsCamera>>,
    mut last_tick: Local<Option<std::time::Instant>>,
) {
    // Minimized = the OS reports a zero-sized client area (winit Resized(0,0)).
    let minimized = windows.iter().any(|w| w.resolution.physical_width() <= 1);
    let render = !children.hidden && !minimized;
    gate.open = render;
    for mut cam in &mut q_cam {
        if cam.is_active != render {
            cam.is_active = render;
        }
    }
    // Same free-run trap as the battle gate: hidden + presents suppressed =
    // the Continuous loop spins a whole core. The menu's hidden duties
    // (child reaping, tray, live-listen poll) are all ≥100 ms cadence.
    if !render {
        std::thread::sleep(std::time::Duration::from_millis(33));
        *last_tick = None;
        return;
    }
    // Visible: cap the orbiting backdrop to the menu's own frame cap
    // (`menu_fps`, written into gate.max_fps at startup and by
    // apply_menu_quality) — the Vulkan present path does not pace here
    // either.
    if gate.max_fps > 0 {
        let target = std::time::Duration::from_secs_f32(1.0 / gate.max_fps as f32);
        if let Some(prev) = *last_tick {
            let elapsed = prev.elapsed();
            if elapsed < target {
                std::thread::sleep(target - elapsed);
            }
        }
    }
    *last_tick = Some(std::time::Instant::now());
}

/// Hot-apply render-quality edits from the Settings page:
/// MSAA onto the menu camera, the shadow-map size into the shared resource,
/// shadow on/off onto the sun light, and the frame cap into the gate.
/// The values persist via settings.json immediately; battle
/// windows are separate processes and read them at their own startup.
/// The menu-specific clamps: the gate rides
/// `menu_fps` (not `max_fps`) and the shadow map is clamped to Low
/// (`menu_shadow_map_size`); MSAA passes through untouched.
fn apply_menu_quality(
    mut menu: ResMut<MenuState>,
    mut q_cam: Query<&mut bevy::render::view::Msaa>,
    mut shadow: ResMut<bevy::pbr::DirectionalLightShadowMap>,
    mut q_light: Query<&mut DirectionalLight>,
    mut gate: ResMut<tactical3d_render::gate::RenderGate>,
) {
    if !menu.quality_dirty {
        return;
    }
    menu.quality_dirty = false;
    let s = &menu.settings;
    let msaa = match s.msaa_samples() {
        1 => bevy::render::view::Msaa::Off,
        2 => bevy::render::view::Msaa::Sample2,
        _ => bevy::render::view::Msaa::Sample4,
    };
    for mut m in q_cam.iter_mut() {
        *m = msaa;
    }
    shadow.size = s.menu_shadow_map_size() as usize;
    for mut light in &mut q_light {
        light.shadows_enabled = s.shadows_enabled();
    }
    gate.max_fps = s.menu_fps();
}

/// Tactical3dPlugin's camera/picking reads state.pointer_over_ui, which the
/// (absent) battle UI plugin would normally maintain — do it here instead.
fn menu_pointer_guard(mut contexts: EguiContexts, mut state: ResMut<TacticalState>) {
    let over = contexts
        .try_ctx_mut()
        .map(|ctx| ctx.is_pointer_over_area() || ctx.wants_pointer_input())
        .unwrap_or(false);
    state.pointer_over_ui = over;
}

// ---------------------------------------------------------------------------
// Battle child processes

/// Cap the drained stderr tail at ~64KB, keeping the last ~32KB. The cut
/// must snap DOWN to a char boundary — a raw byte cut can split a multi-byte
/// UTF-8 char and String::drain PANICS off-boundary, killing the
/// drain thread and broken-piping the child's next eprintln.
fn cap_stderr_tail(b: &mut String) {
    if b.len() > 64 * 1024 {
        let mut cut = b.len() - 32 * 1024;
        while !b.is_char_boundary(cut) {
            cut -= 1;
        }
        b.drain(..cut);
    }
}

/// Spawn the current exe with CLI args as a battle child (stderr piped).
/// Error strings are localized — they surface in the menu status lines.
fn spawn_battle(args: &[String], label: String, loc: &LocaleRes) -> Result<BattleChild, String> {
    let exe = std::env::current_exe()
        .map_err(|e| loc.trf("error.current_exe", &[("error", &e.to_string())]))?;
    let mut child = Command::new(exe)
        .args(args)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| loc.trf("error.spawn_failed", &[("error", &e.to_string())]))?;
    // Drain stderr on a reader thread — the pipe buffer must never fill
    // while the child runs (a blocked write = an un-reapable "frozen"
    // battle). The tail is kept (capped) for the reaper's error report.
    let stderr_buf: std::sync::Arc<std::sync::Mutex<String>> =
        std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let drain = if let Some(pipe) = child.stderr.take() {
        let buf = stderr_buf.clone();
        Some(std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let mut reader = BufReader::new(pipe);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let mut b = buf.lock().unwrap_or_else(|p| p.into_inner());
                        b.push_str(&line);
                        cap_stderr_tail(&mut b);
                    }
                }
            }
        }))
    } else {
        None
    };
    Ok(BattleChild {
        child,
        label,
        stderr_buf,
        drain,
    })
}

/// Reap finished battle children, report their exit status, and park the
/// menu window in the SYSTEM TRAY while a battle child holds the screen
/// (no taskbar-minimized residue —
/// the menu hides to the notification area with a tray icon). The tray is
/// RESIDENT — installed at startup and kept for the whole session; the
/// icon never comes and goes.
/// This system also polls the tray popup (Show / toggle listen / Exit) and
/// consumes the Exit-confirm modal's "minimize to tray" request. The Win32
/// tray machinery lives on its own thread (tray.rs); this system only
/// polls atomics and shows/hides the menu window.
///
/// THREADING: `NonSend<WinitWindows>` pins this system to the MAIN thread
/// (the window owner) — the synchronous cross-thread SendMessage deadlock
/// (the splash/app_icon trap) must never fire from a Bevy
/// worker. The tray thread's own ShowWindow calls are safe by the same
/// no-cycle argument (the main thread never waits on the tray thread).
fn reap_battle_children(
    mut children: ResMut<BattleChildren>,
    mut menu: ResMut<MenuState>,
    mut listen: ResMut<LiveListen>,
    loc: Res<LocaleRes>,
    time: Res<Time>,
    mut windows: Query<(Entity, &mut Window), With<PrimaryWindow>>,
    primary: Query<Entity, With<PrimaryWindow>>,
    winit_windows: Option<NonSend<WinitWindows>>,
    mut exit: EventWriter<bevy::app::AppExit>,
) {
    let mut report: Option<String> = None;
    children.running.retain_mut(|bc| {
        match bc.child.try_wait() {
            Ok(Some(status)) => {
                // Join the drain thread before taking the tail: the pipe
                // hits EOF when the child exits, so this never blocks — and
                // the final lines are not raced away.
                if let Some(drain) = bc.drain.take() {
                    let _ = drain.join();
                }
                let stderr =
                    std::mem::take(&mut *bc.stderr_buf.lock().unwrap_or_else(|p| p.into_inner()));
                let tail: String = stderr
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .next_back()
                    .unwrap_or("")
                    .chars()
                    .take(160)
                    .collect();
                report = Some(if status.success() {
                    loc.trf("menu.main.child_closed", &[("label", &bc.label)])
                } else {
                    loc.trf(
                        "menu.main.child_failed",
                        &[
                            ("label", &bc.label),
                            ("status", &status.to_string()),
                            ("tail", &tail),
                        ],
                    )
                });
                false
            }
            Ok(None) => true,
            Err(e) => {
                report = Some(loc.trf(
                    "menu.main.child_wait_error",
                    &[("label", &bc.label), ("error", &e.to_string())],
                ));
                false
            }
        }
    });
    if let Some(r) = report {
        children.last_report = r;
    }
    // Menu-window HWND for the Win32 show/hide (window.rs precedent).
    let hwnd: Option<HWND> = winit_windows
        .as_ref()
        .and_then(|winit| menu_window_hwnd(winit, &primary));
    // Resident tray: install ONCE at startup, keep until exit.
    if children.tray.is_none() && !children.tray_failed {
        let installed = hwnd.is_some_and(|h| {
            let tray = crate::tray::TrayIcon::install(
                h.0 as isize,
                &loc.tr("menu.tray.tip_idle"),
                &loc.tr("menu.tray.show"),
                &loc.tr("menu.tray.exit"),
                &loc.tr("menu.tray.listen_on"),
                &loc.tr("menu.tray.listen_off"),
            );
            if let Some(t) = tray {
                children.tray = Some(t);
            }
            children.tray.is_some()
        });
        if !installed {
            children.tray_failed = true;
        }
    }
    let alive = children.alive();
    // Rising edge: a battle started — the battle takes ownership of the
    // window restore: it hides the window (if visible) and marks
    // `hidden_by_battle` so battle end pops the menu back EVEN IF the window
    // was already tray-minimized when the battle began (a live-listen battle
    // raised from the tray — the user must get the menu back afterwards).
    // A user X-close during the battle still clears the flag:
    // battle end must not pop a window the user deliberately hid again.
    if alive && !children.was_alive {
        battle_log(&format!(
            "[battle] rising edge: hidden={} hbb_before={}",
            children.hidden, children.hidden_by_battle
        ));
        children.hidden_by_battle = true;
        if !children.hidden {
            children.hidden = true;
            if children.tray.is_some() {
                if let Some(h) = hwnd {
                    hide_menu_window(h);
                }
            } else {
                // Tray unavailable (install failed) — fall back to the old
                // taskbar minimize so the menu still stays out of the way.
                for (_, mut w) in windows.iter_mut() {
                    w.set_minimized(true);
                }
            }
        }
    }
    children.was_alive = alive;
    // Falling edge: battle over — restore the menu window (tray stays
    // resident; the icon is no longer removed on battle end).
    if !alive && children.hidden && children.hidden_by_battle {
        battle_log("[battle] falling edge: restoring menu window");
        show_menu_window(&mut windows, hwnd);
        children.hidden = false;
        children.hidden_by_battle = false;
        // Arm the delayed re-raise — the battle window can outlive
        // this first restore attempt by a few hundred ms.
        children.restore_raise_retries = RESTORE_RAISE_RETRIES;
        children.restore_raise_timer = 0.0;
    } else if !alive && children.hidden {
        battle_log(&format!(
            "[battle] falling edge SKIPPED (no auto-restore): hbb={}",
            children.hidden_by_battle
        ));
    }
    // Exit-confirm modal chose "minimize to tray".
    if children.pending_hide {
        children.pending_hide = false;
        children.hidden = true;
        children.hidden_by_battle = false;
        if children.tray.is_some() {
            if let Some(h) = hwnd {
                hide_menu_window(h);
            }
        } else {
            for (_, mut w) in windows.iter_mut() {
                w.set_minimized(true);
            }
        }
    }
    // Multi-battle picker: pop the menu back so the dialog does
    // not wait unseen (same restore as the tray double-click path; the
    // re-raise also brings a merely-buried window over HOI4).
    if children.pending_show {
        children.pending_show = false;
        if children.hidden {
            show_menu_window(&mut windows, hwnd);
            children.hidden = false;
            children.hidden_by_battle = false;
        }
        children.restore_raise_retries = RESTORE_RAISE_RETRIES;
        children.restore_raise_timer = 0.0;
    }
    // Hover tooltip: exactly three states, battle > listening > idle
    // NIM_MODIFY only on change. Computed before the tray
    // borrow so the state write doesn't fight the immutable borrow.
    let tip_update = {
        let want = if alive {
            TrayTipState::Battle
        } else if menu.live_listen {
            TrayTipState::Listening
        } else {
            TrayTipState::Idle
        };
        if want != children.tip_state {
            children.tip_state = want;
            Some(match want {
                TrayTipState::Idle => loc.tr("menu.tray.tip_idle"),
                TrayTipState::Listening => loc.tr("menu.tray.tip_listening"),
                TrayTipState::Battle => loc.tr("menu.tray.tip_battle"),
            })
        } else {
            None
        }
    };
    // Tray interactions (resident icon — polled every frame).
    if let Some(tray) = children.tray.as_ref() {
        if let Some(tip) = tip_update {
            tray.set_tip(tip.as_ref());
        }
        if tray.exit_requested() {
            // Tray Exit: kill any running battle child first — an orphaned
            // battle window would outlive the menu (same as Exit Game).
            for bc in &mut children.running {
                let _ = bc.child.kill();
            }
            children.running.clear();
            exit.send(bevy::app::AppExit::Success);
        } else if tray.listen_toggle_requested() {
            // Tray popup: flip the live listen (works with the window
            // hidden — the listener keeps tailing game.log in the
            // background and a tac_start raises a battle from the tray).
            menu.live_listen = !menu.live_listen;
            tray.set_listen_on(menu.live_listen);
            if menu.live_listen {
                listen.status.clear(); // re-arm the lazy listener start
            }
        } else if tray.show_requested() {
            // Double-clicked the tray icon: window back, tray stays.
            show_menu_window(&mut windows, hwnd);
            children.hidden = false;
            children.hidden_by_battle = false;
            children.restore_raise_retries = RESTORE_RAISE_RETRIES;
            children.restore_raise_timer = 0.0;
        }
    }
    // Re-raise follow-through: on a 0.25 s cadence until the window
    // actually owns the foreground (or the retries run out). Cancelled if a
    // new battle started or the user hid the window again.
    if children.restore_raise_retries > 0 {
        if children.hidden || alive {
            children.restore_raise_retries = 0;
        } else if let Some(h) = hwnd {
            let front = unsafe { GetForegroundWindow() };
            if front.0 == h.0 {
                children.restore_raise_retries = 0;
            } else {
                children.restore_raise_timer += time.delta_secs();
                if children.restore_raise_timer >= 0.25 {
                    children.restore_raise_timer = 0.0;
                    children.restore_raise_retries -= 1;
                    battle_log(&format!(
                        "[battle] raise retry ({} left)",
                        children.restore_raise_retries
                    ));
                    unsafe { force_window_to_front(h) };
                }
            }
        }
    }
}

/// The menu window's native HWND (Win32), for SW_HIDE/SW_SHOW (window.rs
/// precedent). None if the window isn't backed by a Win32 handle yet.
fn menu_window_hwnd(
    winit_windows: &WinitWindows,
    primary: &Query<Entity, With<PrimaryWindow>>,
) -> Option<HWND> {
    primary
        .get_single()
        .ok()
        .and_then(|entity| winit_windows.get_window(entity))
        .and_then(|w| w.window_handle().ok())
        .and_then(|h| match h.as_raw() {
            RawWindowHandle::Win32(x) => Some(HWND(x.hwnd.get() as *mut _)),
            _ => None,
        })
}

/// The menu window's close button (X) — a RESIDENT tray app:
/// closing the window hides it to the tray instead of exiting; the program
/// only ends through the Exit Game confirm, the tray popup's Exit, or the
/// tray-less fallback (X exits directly, like before). Bevy's
/// close_when_requested is off, so the window is never despawned here —
/// SW_HIDE keeps the hwnd alive for the tray's Show/restore.
fn tray_close_requested(
    mut close: EventReader<WindowCloseRequested>,
    mut children: ResMut<BattleChildren>,
    primary: Query<Entity, With<PrimaryWindow>>,
    winit_windows: Option<NonSend<WinitWindows>>,
    mut exit: EventWriter<bevy::app::AppExit>,
) {
    if close.is_empty() {
        return;
    }
    close.clear();
    if children.tray.is_some() {
        let hwnd = winit_windows
            .as_ref()
            .and_then(|w| menu_window_hwnd(w, &primary));
        if let Some(h) = hwnd {
            hide_menu_window(h);
            children.hidden = true;
            // User X-close: battle end must NOT pop the window back.
            children.hidden_by_battle = false;
            return;
        }
    }
    // No tray (or no hwnd): the X keeps the old behavior — quit. Kill any
    // running battle children first: the menu was their only reaper, and an
    // orphaned child keeps injecting into HOI4 with nobody to surface its
    // result.
    for bc in children.running.iter_mut() {
        let _ = bc.child.kill();
    }
    exit.send(bevy::app::AppExit::Success);
}

/// Battle-window lifecycle diagnostics: appends to
/// %TEMP%\fc_battle.log — the release exe has no console, so eprintln is
/// invisible there; this file log works for both debug and release.
fn battle_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("fc_battle.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Hide the menu window to the tray — the standard "minimize to tray":
/// MINIMIZE the window (it leaves the desktop; winit still considers it
/// visible so the event loop keeps running at full speed) and remove its
/// taskbar button deterministically via ITaskbarList::DeleteTab. The
/// WS_EX_TOOLWINDOW style + SWP_FRAMECHANGED additionally keeps it out of
/// the taskbar/Alt-Tab even if the shell didn't process the DeleteTab.
///
/// WHY NOT SW_HIDE: hiding the window freezes Bevy
/// 0.15's Update entirely. The winit runner drives updates through redraws
/// keyed on the window's `is_visible()` flag; on a hidden window redraws
/// never fire and the event loop parks in `ControlFlow::Wait` with no wake
/// source — battle children are never reaped (no pop-up, tray tooltip stuck
/// on "battle running"). Even `EventLoopProxy` wake-ups and
/// winit-recognized window messages fail to revive it. A minimized window
/// stays "visible" to winit (verified: fps log continues at ~178 fps
/// through minimize/restore), so Update keeps polling.
fn hide_menu_window(hwnd: HWND) {
    battle_log("[battle] hide_menu_window"); // trace the hide path
    unsafe {
        let mn = ShowWindow(hwnd, SW_MINIMIZE);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW.0 as isize);
        let sp = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        battle_log(&format!(
            "[battle] hide: minimize={} exstyle=0x{:X} setpos={}",
            mn.as_bool(),
            ex,
            sp.is_ok()
        ));
        if let Some(tb) = taskbar_list() {
            battle_log(&format!(
                "[battle] hide: DeleteTab ok={}",
                tb.DeleteTab(hwnd).is_ok()
            ));
        }
    }
}

/// How many times the reaper re-raises the restored menu window (0.25 s
/// cadence → ~2 s of coverage).
const RESTORE_RAISE_RETRIES: u8 = 8;

/// Un-hide the menu window: put the taskbar button back (InsertTab), drop
/// the tool-window style, and restore the window (SW_RESTORE brings back
/// the pre-hide position/size, including maximized — but only when the
/// window is actually minimized: SW_RESTORE on a visible MAXIMIZED window
/// un-maximizes it to the floating rect, which the tray Show path would
/// otherwise do to an already-restored window).
fn show_menu_window(
    windows: &mut Query<(Entity, &mut Window), With<PrimaryWindow>>,
    hwnd: Option<HWND>,
) {
    battle_log(&format!(
        "[battle] show_menu_window: hwnd={:?}",
        hwnd.map(|h| h.0)
    )); // trace the show path
    if let Some(h) = hwnd {
        unsafe {
            let tb = taskbar_list();
            battle_log(&format!("[battle] show: taskbar_list={}", tb.is_some()));
            if let Some(tb) = tb {
                battle_log(&format!(
                    "[battle] show: AddTab ok={}",
                    tb.AddTab(h).is_ok()
                ));
            }
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            let _ = SetWindowLongPtrW(h, GWL_EXSTYLE, ex & !(WS_EX_TOOLWINDOW.0 as isize));
            let sp = SetWindowPos(
                h,
                HWND::default(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            let iconic = IsIconic(h);
            let sw = ShowWindow(
                h,
                if iconic.as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            battle_log(&format!(
                "[battle] show: exstyle=0x{:X} setpos={} showwindow={}",
                ex,
                sp.is_ok(),
                sw.as_bool()
            ));
            // Diagnostics: window state right after the restore calls.
            let vis = windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(h);
            let mut r = windows::Win32::Foundation::RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(h, &mut r);
            battle_log(&format!(
                "[battle] show: pre-restore iconic={} visible={} rect=({},{})-({},{})",
                iconic.as_bool(),
                vis.as_bool(),
                r.left,
                r.top,
                r.right,
                r.bottom
            ));
            // Raise + activate with borrowed
            // foreground rights — see force_window_to_front.
            force_window_to_front(h);
        }
    } else {
        battle_log("[battle] show: hwnd is NONE - skipping Win32 restore"); // trace the show path
    }
    for (_, mut w) in windows.iter_mut() {
        w.set_minimized(false);
    }
}

/// Raise the window to the top of the z-order AND activate it from a
/// BACKGROUND process (the release-only "menu
/// doesn't pop back after the battle" failure). When the menu process holds no
/// foreground rights — the normal case right after a battle child exits,
/// since Windows hands the foreground to whatever window it picks next,
/// never to our hidden window — the foreground lock SILENTLY vetoes both
/// `SetWindowPos(HWND_TOP)` without SWP_NOACTIVATE and
/// `SetForegroundWindow`: the window ends up restored, visible, correctly
/// sized, yet BURIED under every other window (live forensics on the
/// stuck release process: visible=true, iconic=false, cloaked=0, z[7]).
/// Debug builds never hit this because they are console-subsystem: the
/// console shared with the dying battle child keeps foreground rights in
/// the menu process, so the same calls work there.
///
/// The canonical tray-restore recipe: `AttachThreadInput` to the FOREGROUND
/// window's thread borrows its input queue (and with it the foreground
/// rights) for the duration of the raise, then detach. The reaper retries
/// this on a cadence for ~2 s (RESTORE_RAISE_RETRIES) to cover the battle
/// window dying slightly AFTER our first attempt.
unsafe fn force_window_to_front(hwnd: HWND) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    let fore = GetForegroundWindow();
    let fore_tid = if fore.0.is_null() {
        0
    } else {
        GetWindowThreadProcessId(fore, None)
    };
    let our_tid = GetCurrentThreadId();
    let attached = fore_tid != 0
        && fore_tid != our_tid
        && AttachThreadInput(our_tid, fore_tid, true).as_bool();
    let sp = SetWindowPos(
        hwnd,
        HWND_TOP,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
    );
    let fg = SetForegroundWindow(hwnd);
    if attached {
        let _ = AttachThreadInput(our_tid, fore_tid, false);
    }
    battle_log(&format!(
        "[battle] raise: fore=0x{:X} attached={} setpos={} foreground={}",
        fore.0 as isize,
        attached,
        sp.is_ok(),
        fg.as_bool()
    ));
}

/// Lazily-created ITaskbarList (shell COM object) for DeleteTab/InsertTab
/// taskbar-button control. COM is initialized on the calling (main) thread
/// once; failures (no Explorer, COM already in a different mode) degrade
/// gracefully — the TOOLWINDOW style already keeps the button away.
fn taskbar_list() -> Option<windows::Win32::UI::Shell::ITaskbarList> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
        let tb: windows::Win32::UI::Shell::ITaskbarList =
            windows::Win32::System::Com::CoCreateInstance(
                &windows::Win32::UI::Shell::TaskbarList,
                None,
                windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
            )
            .ok()?;
        tb.HrInit().ok()?;
        Some(tb)
    }
}

// ---------------------------------------------------------------------------
// Live listen: the menu's Start Live is a TOGGLE — while on,
/// the menu itself tails game.log; a tac_start spawns a live battle child
/// (the listen keeps running across battles).

#[derive(Resource)]
struct LiveListen {
    listener: Option<tactical_listen::LogListener>,
    /// Seconds since the last poll (poll cadence ~0.4 s, like the CLI loop).
    timer: f32,
    /// Last event line shown under the toggle.
    status: String,
    /// Render `status` in error red instead of ok green (failures used
    /// to get the same green as normal events).
    status_error: bool,
    /// Background post-tac_start pick resolution; the battle child spawns
    /// when the picked province arrives (fallback = legacy inference).
    pending_start: Option<PendingStart>,
    /// The picked state holds SEVERAL battles — the picker dialog
    /// is up and the player chooses which one to fight.
    pick_dialog: Option<PickDialog>,
    /// Picker-cancel injects `event tac_abort.1 <tag>` off the UI
    /// thread (console injection blocks for seconds); the
    /// quiet-state pick injects `event tac_quiet.1 <tag>`. Drained for the
    /// status line (the kind selects the completion text).
    abort_slot: Option<(SharedSlot<()>, AbortInject)>,
    /// CenterAnchor state for the pick dialog (jitter fix — the
    /// dialog's width varies with the battle list, so it re-centers per
    /// measured size instead of oscillating on a .5-px tie).
    pick_anchor: CenterAnchor,
}

/// Which abort-flavored injection is in flight (status text on completion).
#[derive(Clone, Copy)]
enum AbortInject {
    /// Picker cancel (`tac_abort.1`) — "tactical mode reset".
    CancelReset,
    /// Quiet-state pick (`tac_quiet.1`) — re-states the quiet notice.
    QuietNotify,
}

/// A result slot shared with a one-shot worker thread.
type SharedSlot<T> = std::sync::Arc<std::sync::Mutex<Option<Result<T, String>>>>;

/// Spawn `work` on a background thread and return its result slot.
/// A panicking worker must NOT poison the mutex — the drain sites
/// `.lock().unwrap()` on the main thread and a poisoned slot would take the
/// whole menu down with the worker.
fn spawn_worker<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> SharedSlot<T> {
    let slot: SharedSlot<T> = std::sync::Arc::new(std::sync::Mutex::new(None));
    let writer = slot.clone();
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(
            |payload| {
                // The default panic hook only writes the menu's own stderr,
                // which release builds have no console for — file-log the
                // payload so the failure stays diagnosable.
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string payload>".to_string());
                battle_log(&format!("[worker] panicked: {msg}"));
                Err("worker panicked".to_string())
            },
        );
        *writer.lock().unwrap() = Some(outcome);
    });
    slot
}

/// A tac_start waiting for its picked-state resolution.
struct PendingStart {
    slot: SharedSlot<PickOutcome>,
    province_fallback: u32,
    tag: String,
    attack_dirs: Vec<String>,
    enemy_tactic: String,
    is_player_attacker: bool,
}

/// The pick worker's bundle: the resolution plus picker cosmetics (VP
/// names, and for multi-battle picks the graphical picker map;
/// built off the UI thread, the provinces.bmp read is tens of MB).
struct PickOutcome {
    resolution: crate::snapshot::PickResolution,
    vp_names: std::collections::HashMap<u32, String>,
    /// Graphical picker map (only for Multiple); None when the map files
    /// are unreadable — the dialog then falls back to the plain list.
    map: Option<crate::pickmap::PickMap>,
}

/// Multi-battle picker: the picked state holds several player
/// battles — the player picks one on the graphical map or the
/// plain list fallback (or cancels = external tac_abort).
struct PickDialog {
    battles: Vec<crate::snapshot::BattleChoice>,
    /// VP display names for the labels (province id → name).
    vp_names: std::collections::HashMap<u32, String>,
    /// Graphical picker map; None → the list fallback.
    map: Option<crate::pickmap::PickMap>,
    /// The egui texture of the current map render + the hover province it
    /// was rendered with (re-rendered on hover change).
    tex: Option<egui::TextureHandle>,
    tex_hover: Option<u32>,
    /// Battle province currently under the cursor.
    hover: Option<u32>,
    tag: String,
    attack_dirs: Vec<String>,
    enemy_tactic: String,
    is_player_attacker: bool,
}

impl LiveListen {
    fn set_status(&mut self, s: impl Into<String>, is_error: bool) {
        self.status = s.into();
        self.status_error = is_error;
    }
}

/// Start the listener on the configured game.log (settings → auto-detect).
fn start_listener(settings: &AppSettings, listen: &mut LiveListen, loc: &LocaleRes) {
    let path = settings
        .log_path()
        .or_else(tactical_listen::detect_log_path);
    match path {
        Some(p) => match tactical_listen::LogListener::start_at_end(p.clone()) {
            Ok(w) => {
                listen.listener = Some(w);
                listen.set_status(
                    loc.trf(
                        "menu.listen.listening",
                        &[("path", &p.display().to_string())],
                    ),
                    false,
                );
            }
            Err(e) => {
                listen.listener = None;
                listen.set_status(
                    loc.trf("menu.listen.failed", &[("error", &e.to_string())]),
                    true,
                );
            }
        },
        None => {
            listen.listener = None;
            listen.set_status(loc.tr("menu.listen.no_log").into_owned(), true);
        }
    }
}

fn live_listen_tick(
    time: Res<Time>,
    menu: Res<MenuState>,
    mut listen: ResMut<LiveListen>,
    mut children: ResMut<BattleChildren>,
    loc: Res<LocaleRes>,
) {
    // Toggle transitions.
    if menu.live_listen && listen.listener.is_none() && listen.status.is_empty() {
        start_listener(&menu.settings, &mut listen, &loc);
    }
    if !menu.live_listen && listen.listener.is_some() {
        listen.listener = None;
        listen.status.clear();
    }
    // An abort-flavored injection finishing updates the
    // status line.
    let abort_done = listen
        .abort_slot
        .as_ref()
        .and_then(|(s, _)| s.lock().unwrap().take());
    if let Some(res) = abort_done {
        let kind = listen.abort_slot.take().map(|(_, k)| k);
        match res {
            Ok(()) => {
                let text = match kind {
                    Some(AbortInject::CancelReset) => loc.tr("menu.pick.aborted").into_owned(),
                    _ => loc.tr("menu.listen.pick_no_battle").into_owned(),
                };
                listen.set_status(text, false);
            }
            // The worker's raw detail stays English (tooling convention);
            // the UI frame is localized (§15).
            Err(e) => listen.set_status(loc.trf("menu.pick.abort_failed", &[("error", &e)]), true),
        }
    }
    // Drain the background pick resolver: the post-tac_start
    // resolution runs OFF the UI thread (console injection + save parsing
    // take seconds); only the outcome arrives here.
    let start_resolved = listen
        .pending_start
        .as_ref()
        .and_then(|ps| ps.slot.lock().unwrap().take());
    if let Some(result) = start_resolved {
        let Some(ps) = listen.pending_start.take() else {
            return;
        };
        let province = match result {
            Err(e) => {
                listen.set_status(loc.trf("menu.pick.resolve_error", &[("error", &e)]), true);
                None
            }
            Ok(out) => match out.resolution {
                crate::snapshot::PickResolution::Province(p) => Some(p),
                // No pick marker → legacy path (assemble_live infers the
                // largest player battle).
                crate::snapshot::PickResolution::NoPick => Some(ps.province_fallback),
                // Quiet frontier state picked → report, do NOT silently
                // launch a different battle; also notify IN GAME + reset
                // every map decision (mod event tac_quiet.1 shares the
                // tac_abort cleanup payload).
                crate::snapshot::PickResolution::QuietNoBattle { .. } => {
                    listen.set_status(loc.tr("menu.listen.pick_no_battle").into_owned(), true);
                    let tag = ps.tag.clone();
                    listen.abort_slot = Some((
                        spawn_worker(move || {
                            tactical_inject::Injector::new()
                                .inject_commands(
                                    &[
                                        format!("event tac_quiet.1 {tag}"),
                                        // Console-fired effects
                                        // do not refresh the decisions UI —
                                        // reloadinterface forces it, hiding
                                        // the map icons NOW (the injector
                                        // skips its close-toggle after it).
                                        "reloadinterface".to_string(),
                                    ],
                                    None,
                                    false,
                                )
                                .map(|_| ())
                                .map_err(|e| format!("quiet-notify injection failed: {e}"))
                        }),
                        AbortInject::QuietNotify,
                    ));
                    None
                }
                // SEVERAL battles in the picked state → the player
                // picks on the map dialog (plain list when the map
                // files are unreadable); the child spawns there. Pop the
                // menu back if it sits hidden in the tray, or the dialog
                // would wait unseen.
                crate::snapshot::PickResolution::Multiple { battles, .. } => {
                    listen.set_status(loc.tr("menu.listen.pick_multi").into_owned(), false);
                    listen.pick_dialog = Some(PickDialog {
                        battles,
                        vp_names: out.vp_names,
                        map: out.map,
                        tex: None,
                        tex_hover: None,
                        hover: None,
                        // Cloned: `ps` still serves the direct-spawn arms below.
                        tag: ps.tag.clone(),
                        attack_dirs: ps.attack_dirs.clone(),
                        enemy_tactic: ps.enemy_tactic.clone(),
                        is_player_attacker: ps.is_player_attacker,
                    });
                    children.pending_show = true;
                    None
                }
            },
        };
        if let Some(province) = province {
            if let Err(e) = spawn_live_child(
                &mut children,
                &menu,
                &loc,
                province,
                &ps.tag,
                &ps.attack_dirs,
                &ps.enemy_tactic,
                ps.is_player_attacker,
            ) {
                listen.set_status(e, true);
            }
        }
    }
    if listen.listener.is_none() {
        return;
    }
    listen.timer += time.delta_secs();
    if listen.timer < 0.4 {
        return;
    }
    listen.timer = 0.0;
    // Collect first, then process: the borrow must end before the loop body
    // (set_status borrows `listen` again, and the TacStart arm re-borrows
    // the listener for the tactic grace-poll).
    let msgs = match listen.listener.as_mut() {
        Some(l) => l.poll(),
        None => return,
    };
    let mut consumed = vec![false; msgs.len()];
    for i in 0..msgs.len() {
        if consumed[i] {
            continue;
        }
        match &msgs[i] {
            tactical_listen::LogMessage::TacStart {
                province,
                tag,
                attack_dirs,
                is_player_attacker,
                ts,
                ..
            } => {
                listen.set_status(
                    loc.trf(
                        "menu.listen.tac_start",
                        &[("ts", ts), ("province", &province.to_string())],
                    ),
                    false,
                );
                if children.alive()
                    || listen.pick_dialog.is_some()
                    || listen.pending_start.is_some()
                    || listen.abort_slot.is_some()
                {
                    // A picker dialog mid-resolution (or its snapshot worker
                    // still in flight) is a busy listener too: spawning a
                    // second battle while it waits would end in TWO live
                    // battles interleaving console injection.
                    // Same for a picker-cancel/quiet
                    // injection still in flight (abort_slot): a fresh
                    // snapshot worker would interleave console keystrokes
                    // with it.
                    listen.set_status(
                        loc.trf(
                            "menu.listen.tac_start_busy",
                            &[("ts", ts), ("province", &province.to_string())],
                        ),
                        false,
                    );
                    continue;
                }
                // The mod emits tac_enemy_tactic in the same tick right
                // after tac_start — same log flush usually lands
                // it in THIS poll batch, so look ahead first; only grace-
                // poll the file (up to ~1 s, blocks one menu frame) when it
                // is not here.
                let mut enemy_tactic = String::new();
                for (j, later) in msgs.iter().enumerate().skip(i + 1) {
                    if let tactical_listen::LogMessage::TacEnemyTactic {
                        enemy_tactic: t, ..
                    } = later
                    {
                        enemy_tactic = t.clone();
                        consumed[j] = true;
                        break;
                    }
                }
                for _ in 0..10 {
                    if !enemy_tactic.is_empty() {
                        break;
                    }
                    if let Some(l) = listen.listener.as_mut() {
                        for m in l.poll() {
                            if let tactical_listen::LogMessage::TacEnemyTactic {
                                enemy_tactic: t,
                                ..
                            } = m
                            {
                                enemy_tactic = t;
                                break;
                            }
                        }
                    }
                    if enemy_tactic.is_empty() {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
                // The state-target entry decision marks
                // the picked state with tac_pick=1 — resolve the REAL
                // contested province from a fresh snapshot OFF the UI thread
                // (console injection + save parse take seconds); the child
                // spawns when the answer lands (fallback = the legacy
                // largest-battle inference with the mod's province value).
                if menu.settings.hoi4_dir().is_none() {
                    listen.set_status(loc.tr("menu.listen.bad_hoi4_dir").into_owned(), true);
                    continue;
                }
                let saves_dir = menu.settings.saves_dir();
                let hoi4_dir = menu.settings.hoi4_dir();
                let worker_loc = loc.0.clone();
                // Picker VP labels in the UI language.
                let zh = menu.settings.language() == tactical_locale::Language::SimpChinese;
                let slot = spawn_worker(move || -> Result<PickOutcome, String> {
                    let hoi4 = hoi4_dir.ok_or_else(|| {
                        worker_loc.tr("menu.listen.hoi4_dir_unknown").into_owned()
                    })?;
                    let saves = saves_dir.ok_or_else(|| {
                        worker_loc.tr("menu.listen.saves_dir_unknown").into_owned()
                    })?;
                    let p2s = crate::snapshot::load_p2s(&hoi4);
                    // VP names ride along for the picker labels.
                    let names = crate::snapshot::load_vp_names(&hoi4, zh);
                    let resolution = crate::snapshot::resolve_picked_province(
                        &tactical_inject::Injector::new(),
                        &saves,
                        &p2s,
                        false,
                    )?;
                    // Build the graphical picker map here too —
                    // provinces.bmp + definition.csv are far too heavy for
                    // the UI thread.
                    let map = match &resolution {
                        crate::snapshot::PickResolution::Multiple { state, battles } => {
                            let battles: Vec<(u32, bool)> = battles
                                .iter()
                                .map(|b| (b.province, b.player_attacker))
                                .collect();
                            crate::pickmap::build_pick_map_for(&hoi4, &p2s, *state, &battles)
                        }
                        _ => None,
                    };
                    Ok(PickOutcome {
                        resolution,
                        vp_names: names,
                        map,
                    })
                });
                listen.pending_start = Some(PendingStart {
                    slot,
                    province_fallback: *province,
                    tag: tag.clone(),
                    attack_dirs: attack_dirs.clone(),
                    enemy_tactic,
                    is_player_attacker: *is_player_attacker,
                });
                listen.set_status(loc.tr("menu.listen.resolving_pick").into_owned(), false);
            }
            tactical_listen::LogMessage::TacAbort { ts, tag } => {
                listen.set_status(
                    loc.trf("menu.listen.tac_abort", &[("ts", ts), ("tag", tag)]),
                    false,
                );
            }
            tactical_listen::LogMessage::TacEnemyTactic { enemy_tactic, .. } => {
                // The mod sends the tactic token (e.g. "blitz") — resolve it
                // to the localized display name like the debug form does.
                let tactic = tactical_ai::CombatTactic::from_str(enemy_tactic);
                listen.set_status(
                    loc.trf(
                        "menu.listen.enemy_tactic",
                        &[("tactic", &loc.tactic_name(tactic))],
                    ),
                    false,
                );
            }
            _ => {}
        }
    }
}

/// Hand a live trigger to a `--livebattle` child (it does the live assembly +
/// console injection itself). Extracted from the tac_start arm so the
/// picked-state resolver can spawn with the REAL province.
fn spawn_live_child(
    children: &mut BattleChildren,
    menu: &MenuState,
    loc: &LocaleRes,
    province: u32,
    tag: &str,
    attack_dirs: &[String],
    enemy_tactic: &str,
    is_player_attacker: bool,
) -> Result<(), String> {
    let mut args = vec![
        "--livebattle".to_string(),
        format!("province={province}"),
        format!("dirs={}", attack_dirs.join(",")),
        format!("tag={tag}"),
        format!("tactic={enemy_tactic}"),
        format!("player_atk={}", if is_player_attacker { 1 } else { 0 }),
    ];
    if menu.settings.hoi4_dir().is_none() {
        return Err(loc.tr("menu.listen.bad_hoi4_dir").into_owned());
    }
    // Only pass non-empty paths — an empty override would wipe the child's
    // own settings via apply_path_overrides.
    if !menu.settings.hoi4_dir.is_empty() {
        args.push(format!("hoi4_dir={}", menu.settings.hoi4_dir));
    }
    if !menu.settings.saves_dir.is_empty() {
        args.push(format!("saves_dir={}", menu.settings.saves_dir));
    }
    let label = loc.trf(
        "menu.main.child_label_live",
        &[("province", &province.to_string())],
    );
    spawn_battle(&args, label, loc).map(|bc| children.running.push(bc))
}

// ---------------------------------------------------------------------------
// Menu UI

/// HOI4-ish palette (mirrored from ui.rs so the menu stays
/// self-contained). egui 0.31: from_rgba_unmultiplied is not const, so the
/// translucent fills are built by helpers below.
const BRASS: egui::Color32 = egui::Color32::from_rgb(158, 130, 78);
const GOLD: egui::Color32 = egui::Color32::from_rgb(214, 181, 110);
const PARCHMENT: egui::Color32 = egui::Color32::from_rgb(216, 208, 192);
const ERROR_RED: egui::Color32 = egui::Color32::from_rgb(220, 90, 70);
const OK_GREEN: egui::Color32 = egui::Color32::from_rgb(120, 180, 110);

fn panel_fill() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(24, 22, 20, 205)
}

fn frosted() -> egui::Frame {
    egui::Frame::window(&egui::Style::default())
        .fill(panel_fill())
        .stroke(egui::Stroke::new(1.5_f32, BRASS))
        .rounding(egui::Rounding::same(6.0))
        .shadow(egui::epaint::Shadow {
            offset: egui::vec2(6.0, 8.0),
            blur: 18.0,
            spread: 2.0,
            color: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140),
        })
        .inner_margin(egui::Margin::same(18.0))
}

fn draw_menu(
    mut contexts: EguiContexts,
    mut menu: ResMut<MenuState>,
    info: Res<BackdropInfo>,
    mut listen: ResMut<LiveListen>,
    mut children: ResMut<BattleChildren>,
    mut form: ResMut<DebugForm>,
    icons: Res<IconSet>,
    mut loc: ResMut<LocaleRes>,
    mut exit: EventWriter<bevy::app::AppExit>,
) {
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    match menu.page {
        Page::Main => draw_main(
            ctx,
            &mut menu,
            &mut listen,
            &mut children,
            &icons,
            &loc,
            &mut exit,
        ),
        Page::Settings => draw_settings(ctx, &mut menu, &icons, &mut loc),
        Page::About => draw_about(ctx, &mut menu, &icons, &mut loc),
        Page::DebugForm => draw_debug_form(ctx, &mut menu, &mut children, &mut form, &icons, &loc),
    }
    // Backdrop caption, bottom-right corner (small, unobtrusive).
    let area = egui::Area::new(egui::Id::new("backdrop_info"));
    let resp = menu
        .anchors
        .caption
        .area(
            ctx,
            area,
            egui::Align2::RIGHT_BOTTOM,
            egui::vec2(-10.0, -6.0),
        )
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(&info.0)
                    .size(11.0)
                    .color(egui::Color32::from_rgba_unmultiplied(216, 208, 192, 140)),
            );
        });
    menu.anchors
        .caption
        .update(Some(resp.response.rect), ctx.pixels_per_point());
    // Exit confirm: minimize to tray / quit / cancel.
    if menu.confirm_exit {
        draw_exit_confirm(ctx, &mut menu, &mut children, &loc, &mut exit);
    }
    // Disclaimer terms (entry on the About page).
    if menu.show_disclaimer {
        draw_disclaimer(ctx, &mut menu, &loc);
    }
    // Multi-battle picker: modal over whichever
    // page is current.
    if listen.pick_dialog.is_some() {
        draw_pick_dialog(ctx, &menu, &mut listen, &mut children, &loc);
    }
}

/// The Exit Game button asks before leaving — minimize to tray
/// (window hides, tray keeps the program alive), quit (kills any battle
/// child first, then exits), or cancel. Only shown while the tray is
/// resident; without a tray the Exit button quits directly.
fn draw_exit_confirm(
    ctx: &egui::Context,
    menu: &mut MenuState,
    children: &mut BattleChildren,
    loc: &LocaleRes,
    exit: &mut EventWriter<bevy::app::AppExit>,
) {
    let win = egui::Window::new("menu_exit_confirm")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(frosted());
    let resp = menu
        .anchors
        .exit_confirm
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(
                egui::RichText::new(loc.tr("menu.exit.title"))
                    .size(15.0)
                    .color(GOLD)
                    .strong(),
            );
            ui.add_space(10.0);
            // Three options side by side; plain text buttons (no icons).
            ui.horizontal(|ui| {
                if tip(
                    ui.add_sized(
                        [100.0, 30.0],
                        egui::Button::new(
                            egui::RichText::new(loc.tr("menu.exit.minimize")).color(PARCHMENT),
                        ),
                    ),
                    loc.tr("menu.tooltip.exit_minimize"),
                )
                .clicked()
                {
                    children.pending_hide = true;
                    menu.confirm_exit = false;
                }
                if tip(
                    ui.add_sized(
                        [100.0, 30.0],
                        egui::Button::new(
                            egui::RichText::new(loc.tr("menu.exit.quit")).color(PARCHMENT),
                        ),
                    ),
                    loc.tr("menu.tooltip.exit_quit"),
                )
                .clicked()
                {
                    // Kill any running battle child first — an orphaned battle
                    // window would outlive the menu (same as the direct exit).
                    for bc in &mut children.running {
                        let _ = bc.child.kill();
                    }
                    children.running.clear();
                    menu.confirm_exit = false;
                    exit.send(bevy::app::AppExit::Success);
                }
                if ui
                    .add_sized(
                        [100.0, 30.0],
                        egui::Button::new(
                            egui::RichText::new(loc.tr("menu.exit.cancel")).color(PARCHMENT),
                        ),
                    )
                    .clicked()
                {
                    menu.confirm_exit = false;
                }
            });
        });
    menu.anchors
        .exit_confirm
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

/// Multi-battle picker — graphical version: the picked state's map crop
/// (solid state outline, dashed province outlines, battle provinces
/// filled by the player's side, hover
/// emphasis, click to fight). Falls back to the plain list when the map
/// files are unreadable. Cancel = external tac_abort: `tac_abort.1` clears
/// tac_mode_active + tac_pick + the division tags, so every map decision
/// re-arms in HOI4, and the menu hides back to the tray.
fn draw_pick_dialog(
    ctx: &egui::Context,
    menu: &MenuState,
    listen: &mut LiveListen,
    children: &mut BattleChildren,
    loc: &LocaleRes,
) {
    let Some(dialog) = listen.pick_dialog.as_mut() else {
        return;
    };
    let mut chosen: Option<u32> = None;
    let mut cancelled = false;
    let win = egui::Window::new("menu_pick_battle")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(frosted());
    let resp = listen
        .pick_anchor
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(loc.tr("menu.pick.title"))
                    .size(15.0)
                    .color(GOLD)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(loc.tr("menu.pick.hint").into_owned())
                    .size(12.0)
                    .color(PARCHMENT),
            );
            ui.add_space(8.0);
            if dialog.map.is_some() {
                let map = dialog.map.as_ref().unwrap();
                // Cap the display at 940×600, upscale small states ≤12×
                // (NEAREST keeps the pixels crisp; the
                // 620×420/4× version read far too small).
                let (w, h) = (map.width as f32, map.height as f32);
                let scale = (940.0 / w).min(600.0 / h).min(12.0);
                // (Re)render the texture on hover change only.
                if dialog.tex.is_none() || dialog.tex_hover != dialog.hover {
                    let rgba = map.render(dialog.hover);
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [map.width as usize, map.height as usize],
                        &rgba,
                    );
                    if let Some(tex) = dialog.tex.as_mut() {
                        tex.set(image, egui::TextureOptions::NEAREST);
                    } else {
                        dialog.tex = Some(ui.ctx().load_texture(
                            "menu_pick_map",
                            image,
                            egui::TextureOptions::NEAREST,
                        ));
                    }
                    dialog.tex_hover = dialog.hover;
                }
                let size = egui::vec2(w * scale, h * scale);
                let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
                ui.painter().image(
                    dialog.tex.as_ref().unwrap().id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                // Province labels at the battle-province centroids (shadow +
                // parchment top — dynamic names are not localized, §15).
                for (prov, _atk, (cx, cy)) in map.labels() {
                    let name = dialog
                        .vp_names
                        .get(&prov)
                        .cloned()
                        .unwrap_or_else(|| format!("#{prov}"));
                    let pos = rect.min + egui::vec2(cx * scale, cy * scale);
                    ui.painter().text(
                        pos + egui::vec2(1.0, 1.0),
                        egui::Align2::CENTER_CENTER,
                        &name,
                        egui::FontId::proportional(11.0),
                        egui::Color32::from_rgb(20, 18, 16),
                    );
                    ui.painter().text(
                        pos,
                        egui::Align2::CENTER_CENTER,
                        &name,
                        egui::FontId::proportional(11.0),
                        egui::Color32::from_rgb(250, 245, 235),
                    );
                }
                // Hover → emphasis (next frame's render) + pointer cursor;
                // click on a battle texel chooses it.
                let pointer = response
                    .interact_pointer_pos()
                    .or_else(|| response.hover_pos());
                let mut new_hover = None;
                if let Some(pos) = pointer {
                    let tx = ((pos.x - rect.min.x) / scale).floor() as i32;
                    let ty = ((pos.y - rect.min.y) / scale).floor() as i32;
                    if tx >= 0 && ty >= 0 {
                        let prov = map.province_at(tx as u32, ty as u32);
                        if map.battle_side(prov).is_some() {
                            new_hover = Some(prov);
                        }
                    }
                }
                if new_hover.is_some() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if response.clicked() {
                    chosen = new_hover;
                }
                dialog.hover = new_hover;
                // Legend: fill color ↔ the player's side.
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    legend_swatch(
                        ui,
                        crate::pickmap::side_fill(true),
                        &loc.tr("menu.pick.legend_atk"),
                    );
                    ui.add_space(12.0);
                    legend_swatch(
                        ui,
                        crate::pickmap::side_fill(false),
                        &loc.tr("menu.pick.legend_def"),
                    );
                });
            } else {
                // List fallback (map files unreadable): one button per
                // battle — VP name or #province, tags, division counts.
                ui.set_width(380.0);
                for b in &dialog.battles {
                    let province_label = match dialog.vp_names.get(&b.province) {
                        Some(name) => format!("{name} (#{})", b.province),
                        None => format!("#{}", b.province),
                    };
                    let label = loc.trf(
                        "menu.pick.item",
                        &[
                            ("province", &province_label),
                            ("atk", &b.attacker_tags.join("+")),
                            ("atk_units", &b.attacker_units.to_string()),
                            ("def", &b.defender_tags.join("+")),
                            ("def_units", &b.defender_units.to_string()),
                        ],
                    );
                    if ui
                        .add_sized(
                            [360.0, 30.0],
                            egui::Button::new(egui::RichText::new(label).color(PARCHMENT)),
                        )
                        .clicked()
                    {
                        chosen = Some(b.province);
                    }
                }
            }
            ui.add_space(6.0);
            if ui
                .add_sized(
                    [100.0, 28.0],
                    egui::Button::new(
                        egui::RichText::new(loc.tr("menu.exit.cancel")).color(PARCHMENT),
                    ),
                )
                .clicked()
            {
                cancelled = true;
            }
        });
    listen
        .pick_anchor
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
    if let Some(province) = chosen {
        let Some(dialog) = listen.pick_dialog.take() else {
            return;
        };
        if let Err(e) = spawn_live_child(
            children,
            menu,
            loc,
            province,
            &dialog.tag,
            &dialog.attack_dirs,
            &dialog.enemy_tactic,
            dialog.is_player_attacker,
        ) {
            listen.set_status(e, true);
        }
    } else if cancelled {
        let Some(dialog) = listen.pick_dialog.take() else {
            return;
        };
        // Cancel = abort tactical mode ENTIRELY — inject
        // `event tac_abort.1 <tag>` (the mod clears tac_mode_active +
        // tac_pick + the division tags, so every map decision re-arms) and
        // hide the menu back to the tray: the player is back in HOI4.
        listen.set_status(loc.tr("menu.pick.cancelling").into_owned(), false);
        let tag = dialog.tag.clone();
        listen.abort_slot = Some((
            spawn_worker(move || {
                tactical_inject::Injector::new()
                    .inject_commands(
                        &[
                            format!("event tac_abort.1 {tag}"),
                            // Console-fired effects do not
                            // refresh the decisions UI — reloadinterface
                            // forces it (the injector skips its
                            // close-toggle after it).
                            "reloadinterface".to_string(),
                        ],
                        None,
                        false,
                    )
                    .map(|_| ())
                    .map_err(|e| format!("abort injection failed: {e}"))
            }),
            AbortInject::CancelReset,
        ));
        children.pending_hide = true;
    }
}

/// Legend swatch: a small filled rect + a label (picker dialog).
fn legend_swatch(ui: &mut egui::Ui, rgba: [u8; 4], text: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        2.0,
        egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]),
    );
    ui.label(egui::RichText::new(text).size(11.0).color(PARCHMENT));
}

fn menu_button(
    ui: &mut egui::Ui,
    icons: &IconSet,
    icon: Option<IconId>,
    label: &str,
) -> egui::Response {
    ui.add_sized(
        [260.0, 34.0],
        icons.button(
            icon,
            egui::RichText::new(label).size(15.0).color(PARCHMENT),
            16.0,
        ),
    )
}

/// Localized hover tooltip on a button response — attached to
/// BOTH the enabled and the disabled state (egui splits the two): a
/// greyed-out button is exactly where the player needs the hint most.
fn tip(resp: egui::Response, text: impl Into<egui::WidgetText> + Clone) -> egui::Response {
    resp.on_hover_text(text.clone())
        .on_disabled_hover_text(text)
}

fn draw_main(
    ctx: &egui::Context,
    menu: &mut MenuState,
    listen: &mut LiveListen,
    children: &mut BattleChildren,
    icons: &IconSet,
    loc: &LocaleRes,
    exit: &mut EventWriter<bevy::app::AppExit>,
) {
    let win = egui::Window::new("menu_main")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(frosted());
    let resp = menu
        .anchors
        .main
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(loc.tr("app.title").to_uppercase())
                        .size(38.0)
                        .color(GOLD)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(loc.tr("app.subtitle").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                );
                ui.add_space(18.0);
                // Start Live = listening toggle: the menu
                // stays open and tails game.log itself.
                let live_label = if menu.live_listen {
                    loc.tr("menu.main.live_on")
                } else {
                    loc.tr("menu.main.start_live")
                };
                if tip(
                    menu_button(ui, icons, Some(IconId::Listen), &live_label),
                    loc.tr("menu.tooltip.live_listen"),
                )
                .clicked()
                {
                    menu.live_listen = !menu.live_listen;
                    if menu.live_listen {
                        listen.status.clear(); // re-arm the lazy listener start
                    }
                }
                // Status shows regardless of the listen toggle: picker /
                // snapshot / spawn errors arrive while listening is off too,
                // and hiding them made failures invisible.
                if !listen.status.is_empty() {
                    let color = if listen.status_error {
                        ERROR_RED
                    } else {
                        OK_GREEN
                    };
                    ui.label(egui::RichText::new(&listen.status).size(11.0).color(color));
                }
                ui.add_space(6.0);
                if tip(
                    menu_button(
                        ui,
                        icons,
                        Some(IconId::Attack),
                        &loc.tr("menu.main.debug_battle"),
                    ),
                    loc.tr("menu.tooltip.debug_battle"),
                )
                .clicked()
                {
                    menu.page = Page::DebugForm;
                }
                ui.add_space(6.0);
                if tip(
                    menu_button(ui, icons, Some(IconId::Gear), &loc.tr("menu.main.settings")),
                    loc.tr("menu.tooltip.settings"),
                )
                .clicked()
                {
                    menu.page = Page::Settings;
                }
                ui.add_space(6.0);
                if tip(
                    menu_button(ui, icons, Some(IconId::Scroll), &loc.tr("menu.main.about")),
                    loc.tr("menu.tooltip.about"),
                )
                .clicked()
                {
                    menu.page = Page::About;
                }
                ui.add_space(6.0);
                if tip(
                    menu_button(
                        ui,
                        icons,
                        Some(IconId::Door),
                        &loc.tr("menu.main.exit_game"),
                    ),
                    loc.tr("menu.tooltip.exit_game"),
                )
                .clicked()
                {
                    if children.tray.is_some() {
                        // Resident tray: ask — minimize to tray /
                        // quit / cancel. The X button already hides to the
                        // tray, so the window X is never a surprise exit.
                        menu.confirm_exit = true;
                    } else {
                        // Tray unavailable — the X quits already; keep the
                        // old direct-quit behavior for the button too.
                        for bc in &mut children.running {
                            let _ = bc.child.kill();
                        }
                        children.running.clear();
                        exit.send(bevy::app::AppExit::Success);
                    }
                }
                if children.alive() {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(loc.tr("menu.main.battle_running").into_owned())
                            .size(11.0)
                            .color(PARCHMENT),
                    );
                } else if !children.last_report.is_empty() {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(&children.last_report)
                            .size(11.0)
                            .color(PARCHMENT),
                    );
                }
                ui.add_space(10.0);
            });
        });
    menu.anchors
        .main
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

fn draw_settings(ctx: &egui::Context, menu: &mut MenuState, icons: &IconSet, loc: &mut LocaleRes) {
    let win = egui::Window::new("menu_settings")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(frosted());
    let resp = menu
        .anchors
        .settings_page
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                icons.label_with_icon(
                    ui,
                    IconId::Gear,
                    egui::RichText::new(loc.tr("menu.settings.title").into_owned())
                        .size(22.0)
                        .color(GOLD)
                        .strong(),
                    20.0,
                );
            });
            ui.add_space(10.0);
            if menu.first_run {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(loc.tr("menu.settings.first_run").into_owned())
                            .size(14.0)
                            .color(GOLD),
                    );
                });
                ui.add_space(4.0);
            }
            let s = &mut menu.settings;
            path_field(
                ui,
                loc,
                "menu.settings.field.hoi4_dir",
                &mut s.hoi4_dir,
                BrowseKind::Folder,
            );
            path_field(
                ui,
                loc,
                "menu.settings.field.saves_dir",
                &mut s.saves_dir,
                BrowseKind::Folder,
            );
            path_field(
                ui,
                loc,
                "menu.settings.field.log_path",
                &mut s.log_path,
                BrowseKind::LogFile,
            );
            ui.add_space(8.0);
            // Live validation lines (slugs → locale keys, DESIGN §15).
            let checks = s.validate();
            for (field, ok, msg) in checks {
                let color = if ok { OK_GREEN } else { ERROR_RED };
                let field = loc.tr(&format!("menu.settings.field.{field}"));
                let msg = loc.tr(&format!("settings.validate.{msg}"));
                ui.label(
                    egui::RichText::new(format!("{field}: {msg}"))
                        .size(11.5)
                        .color(color),
                );
            }
            ui.add_space(8.0);
            // Language selector (DESIGN §15): radio between the shipping
            // languages. Applies instantly — the LocaleRes swap re-renders
            // the whole menu in the new language next frame — and persists
            // immediately, with the same feedback as the Save button.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.language").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                );
                for lang in Language::ALL {
                    let selected = menu.settings.language() == *lang;
                    if ui.selectable_label(selected, lang.display_name()).clicked() && !selected {
                        menu.settings.language = lang.settings_tag().to_string();
                        *loc = LocaleRes(Locale::load(*lang));
                        menu.status = match menu.settings.save() {
                            Ok(()) => {
                                menu.first_run = false;
                                loc.tr("menu.settings.saved").into_owned()
                            }
                            Err(e) => loc.trf("menu.settings.save_failed", &[("error", &e)]),
                        };
                    }
                }
            });
            // Render quality: MSAA / shadow map /
            // idle frame-saver. Changes persist immediately (same feedback
            // as the language row) and hot-apply to the menu itself via
            // quality_dirty; battle windows read settings.json at startup.
            let mut quality_changed = false;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.msaa").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                )
                .on_hover_text(loc.tr("menu.tooltip.msaa").into_owned());
                for (val, text) in [
                    (4u32, "4×".to_string()),
                    (2, "2×".to_string()),
                    (1, loc.tr("menu.settings.msaa.off").into_owned()),
                ] {
                    let selected = menu.settings.msaa_samples() == val;
                    if ui.selectable_label(selected, text).clicked() && !selected {
                        menu.settings.msaa = val;
                        quality_changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.shadow").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                )
                .on_hover_text(loc.tr("menu.tooltip.shadow").into_owned());
                // Off / low (1024) / high (2048) — replaces the old
                // bare shadow_map size row.
                for (val, key) in [
                    (2u32, "menu.settings.shadow.high"),
                    (1, "menu.settings.shadow.low"),
                    (0, "menu.settings.shadow.off"),
                ] {
                    let selected = menu.settings.shadow_level() == val;
                    if ui
                        .selectable_label(selected, loc.tr(key).into_owned())
                        .clicked()
                        && !selected
                    {
                        menu.settings.shadow = Some(val);
                        quality_changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.render_scale").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                )
                .on_hover_text(loc.tr("menu.tooltip.render_scale").into_owned());
                // The weak-GPU lever — below 100% the 3D scene
                // renders into a smaller offscreen target and upscales.
                // Battle windows apply it at startup; the menu never scales.
                for val in [100u32, 85, 70, 50] {
                    let selected = menu.settings.render_scale_pct() == val;
                    let text = format!("{val}%");
                    if ui.selectable_label(selected, text).clicked() && !selected {
                        menu.settings.render_scale = Some(val);
                        quality_changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.max_fps").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                )
                .on_hover_text(loc.tr("menu.tooltip.max_fps").into_owned());
                // Present does not pace on some Vulkan setups —
                // the cap is the only thing between 60 fps and ~160 fps of
                // wasted renders. Applies to battle windows (the menu has
                // its own lower cap, next row).
                for val in [144u32, 120, 90, 60, 30, 0] {
                    let selected = menu.settings.max_fps() == val;
                    let text = if val == 0 {
                        loc.tr("menu.settings.max_fps.uncapped").into_owned()
                    } else {
                        val.to_string()
                    };
                    if ui.selectable_label(selected, text).clicked() && !selected {
                        menu.settings.max_fps = val;
                        quality_changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.menu_fps").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                )
                .on_hover_text(loc.tr("menu.tooltip.menu_fps").into_owned());
                // The menu never idle-throttles (its backdrop
                // always animates), so it gets its own lower cap; the slow
                // orbit reads identically at 24–30 fps.
                for val in [60u32, 30, 24, 15, 0] {
                    let selected = menu.settings.menu_fps() == val;
                    let text = if val == 0 {
                        loc.tr("menu.settings.max_fps.uncapped").into_owned()
                    } else {
                        val.to_string()
                    };
                    if ui.selectable_label(selected, text).clicked() && !selected {
                        menu.settings.menu_fps = val;
                        quality_changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.low_power").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                )
                .on_hover_text(loc.tr("menu.tooltip.low_power").into_owned());
                let mut lp = menu.settings.low_power;
                if ui.checkbox(&mut lp, "").changed() {
                    menu.settings.low_power = lp;
                    quality_changed = true;
                }
            });
            if quality_changed {
                menu.quality_dirty = true;
                menu.status = match menu.settings.save() {
                    Ok(()) => {
                        menu.first_run = false;
                        loc.tr("menu.settings.saved").into_owned()
                    }
                    Err(e) => loc.trf("menu.settings.save_failed", &[("error", &e)]),
                };
            }
            ui.add_space(8.0);
            // Damage writeback mode — player-facing
            // two choices (org+str province-exact / off) with the
            // limitation + per-mode details on the page (no dev-log
            // references in player-facing text). The org-only middle
            // choice was removed — its whole-army channel zeroed a
            // national army live.
            let mut mode_changed = false;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.settings.writeback").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                )
                .on_hover_text(loc.tr("menu.tooltip.writeback").into_owned());
                for (val, key) in [
                    (
                        tactical_sync::WritebackMode::OrgStr,
                        "menu.settings.writeback.org_str",
                    ),
                    (
                        tactical_sync::WritebackMode::Off,
                        "menu.settings.writeback.off",
                    ),
                ] {
                    let selected = menu.settings.writeback_mode() == val;
                    if ui
                        .selectable_label(selected, loc.tr(key).into_owned())
                        .clicked()
                        && !selected
                    {
                        menu.settings.writeback = val.as_str().to_string();
                        mode_changed = true;
                    }
                }
            });
            ui.label(
                egui::RichText::new(loc.tr("menu.settings.writeback.desc").into_owned())
                    .size(11.5)
                    .color(PARCHMENT)
                    .weak(),
            );
            let detail_key = match menu.settings.writeback_mode() {
                tactical_sync::WritebackMode::OrgStr => "menu.settings.writeback.detail.org_str",
                tactical_sync::WritebackMode::Off => "menu.settings.writeback.detail.off",
            };
            ui.label(
                egui::RichText::new(loc.tr(detail_key).into_owned())
                    .size(11.5)
                    .color(PARCHMENT),
            );
            if mode_changed {
                menu.status = match menu.settings.save() {
                    Ok(()) => {
                        menu.first_run = false;
                        loc.tr("menu.settings.saved").into_owned()
                    }
                    Err(e) => loc.trf("menu.settings.save_failed", &[("error", &e)]),
                };
            }
            ui.add_space(8.0);
            // HOI4 save format (§14): tactical save parsing needs text
            // saves. Live probe of HOI4's own settings.txt + one-click fixer.
            match detect_hoi4_settings_txt(&menu.settings.saves_dir) {
                Some(txt) => {
                    let (msg, ok) = match read_save_as_binary(&txt) {
                        Some(true) => (loc.tr("menu.settings.txt_binary_yes"), false),
                        Some(false) => (loc.tr("menu.settings.txt_binary_no"), true),
                        None => (loc.tr("menu.settings.txt_binary_unset"), false),
                    };
                    ui.label(
                        egui::RichText::new(msg.into_owned())
                            .size(11.5)
                            .color(if ok { OK_GREEN } else { ERROR_RED }),
                    );
                    if !ok
                        && tip(
                            menu_button(
                                ui,
                                icons,
                                Some(IconId::Check),
                                &loc.tr("menu.settings.fix_binary"),
                            ),
                            loc.tr("menu.tooltip.fix_binary"),
                        )
                        .clicked()
                    {
                        menu.status = match force_text_saves(&txt) {
                            Ok(TextSaveFix::AlreadyText) => {
                                loc.tr("menu.settings.fix_already").into_owned()
                            }
                            Ok(TextSaveFix::Rewritten) => {
                                loc.tr("menu.settings.fix_rewritten").into_owned()
                            }
                            Ok(TextSaveFix::Added) => {
                                loc.tr("menu.settings.fix_added").into_owned()
                            }
                            Err(e) => e,
                        };
                    }
                }
                None => {
                    ui.label(
                        egui::RichText::new(loc.tr("menu.settings.txt_not_found").into_owned())
                            .size(11.5)
                            .color(ERROR_RED),
                    );
                }
            }
            ui.add_space(8.0);
            // One-click diagnostic bundle for beta bug reports —
            // crash.log + inject log + settings.json +
            // game.log tail + optional newest save, zipped next to the exe.
            // Runs synchronously; with a big save included the zip build
            // takes a few seconds.
            ui.horizontal(|ui| {
                if tip(
                    menu_button(ui, icons, Some(IconId::Scroll), &loc.tr("menu.settings.diag")),
                    loc.tr("menu.tooltip.diag"),
                )
                .clicked()
                {
                    menu.status = match crate::diag::export(&menu.settings, menu.diag_include_save)
                    {
                        Ok(report) => {
                            let path = report.zip_path.display().to_string();
                            loc.trf("menu.settings.diag.done", &[("path", &path)])
                        }
                        Err(e) => loc.trf("menu.settings.diag.failed", &[("error", &e)]),
                    };
                }
                let mut inc = menu.diag_include_save;
                if ui
                    .checkbox(
                        &mut inc,
                        egui::RichText::new(loc.tr("menu.settings.diag.include_save").into_owned())
                            .size(13.0)
                            .color(PARCHMENT),
                    )
                    .on_hover_text(loc.tr("menu.tooltip.diag.include_save").into_owned())
                    .changed()
                {
                    menu.diag_include_save = inc;
                }
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if tip(
                    menu_button(ui, icons, Some(IconId::Save), &loc.tr("common.save")),
                    loc.tr("menu.tooltip.save"),
                )
                .clicked()
                {
                    menu.status = match menu.settings.save() {
                        Ok(()) => {
                            menu.first_run = false;
                            loc.tr("menu.settings.saved").into_owned()
                        }
                        Err(e) => loc.trf("menu.settings.save_failed", &[("error", &e)]),
                    };
                }
                if menu_button(ui, icons, Some(IconId::BackArrow), &loc.tr("common.back")).clicked()
                {
                    menu.page = Page::Main;
                }
            });
            if !menu.status.is_empty() {
                ui.label(
                    egui::RichText::new(&menu.status)
                        .size(11.5)
                        .color(PARCHMENT),
                );
            }
        });
    menu.anchors
        .settings_page
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

/// About page: version + build date + credits. The build date
/// is the exe's own file mtime — the binary's birth time, which survives
/// zip distribution — rendered in the player's local timezone
/// (diag::local_ymd). Version feedback for beta bug reports reads off
/// this page.
fn draw_about(ctx: &egui::Context, menu: &mut MenuState, icons: &IconSet, loc: &mut LocaleRes) {
    let build_date = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| crate::diag::local_ymd(d.as_secs()))
        .unwrap_or_else(|| "—".to_string());
    let version: &str = env!("CARGO_PKG_VERSION");
    let win = egui::Window::new("menu_about")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(frosted());
    let resp = menu
        .anchors
        .about
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                icons.label_with_icon(
                    ui,
                    IconId::Scroll,
                    egui::RichText::new(loc.tr("menu.about.title").into_owned())
                        .size(22.0)
                        .color(GOLD)
                        .strong(),
                    20.0,
                );
            });
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(loc.trf("menu.about.version", &[("version", version)]))
                        .size(16.0)
                        .color(PARCHMENT)
                        .strong(),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(loc.trf("menu.about.build", &[("date", &build_date)]))
                        .size(12.5)
                        .color(PARCHMENT),
                );
            });
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.about.author").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                );
                ui.label(
                    egui::RichText::new(loc.tr("menu.about.thanks_ai").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(loc.tr("menu.about.thanks_assets").into_owned())
                        .size(11.5)
                        .color(PARCHMENT)
                        .weak(),
                );
            });
            ui.add_space(12.0);
            // Disclaimer entry — the packaged 免责声明.md lives in docs/,
            // but a player in the game should not have to hunt for it.
            ui.vertical_centered(|ui| {
                if ui
                    .add_sized(
                        [260.0, 30.0],
                        icons.button(
                            Some(IconId::Warning),
                            egui::RichText::new(loc.tr("menu.about.disclaimer").into_owned())
                                .size(14.0)
                                .color(GOLD)
                                .strong(),
                            15.0,
                        ),
                    )
                    .clicked()
                {
                    menu.show_disclaimer = true;
                }
            });
            ui.add_space(4.0);
            ui.vertical_centered(|ui| {
                if menu_button(ui, icons, Some(IconId::BackArrow), &loc.tr("common.back")).clicked()
                {
                    menu.page = Page::Main;
                }
            });
        });
    menu.anchors
        .about
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

/// Disclaimer terms from the About page. Same substance as the packaged
/// docs/免责声明.md, wording kept plain for the in-game spot and
/// localized with the selected language. Scrollable: the EN text is long
/// in narrow fonts.
fn draw_disclaimer(ctx: &egui::Context, menu: &mut MenuState, loc: &LocaleRes) {
    let win = egui::Window::new("menu_disclaimer")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(frosted());
    let resp = menu
        .anchors
        .disclaimer
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_width(430.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.about.disclaimer").into_owned())
                        .size(16.0)
                        .color(GOLD)
                        .strong(),
                );
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.about.disclaimer.text").into_owned())
                        .size(12.5)
                        .color(PARCHMENT),
                );
            });
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if ui
                    .add_sized(
                        [120.0, 30.0],
                        egui::Button::new(
                            egui::RichText::new(loc.tr("common.close").into_owned())
                                .color(PARCHMENT),
                        ),
                    )
                    .clicked()
                {
                    menu.show_disclaimer = false;
                }
            });
        });
    menu.anchors
        .disclaimer
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

/// Which native picker a path field's Browse button raises (rfd).
enum BrowseKind {
    Folder,
    LogFile,
}

fn path_field(
    ui: &mut egui::Ui,
    loc: &LocaleRes,
    label_key: &str,
    value: &mut String,
    browse: BrowseKind,
) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(loc.tr(label_key).into_owned())
                .size(13.0)
                .color(PARCHMENT),
        );
        ui.add_sized(
            [360.0, 22.0],
            egui::TextEdit::singleline(value).font(egui::TextStyle::Monospace),
        );
        if tip(
            ui.small_button(loc.tr("menu.settings.browse").into_owned()),
            loc.tr("menu.tooltip.browse"),
        )
        .clicked()
        {
            // Native OS picker (rfd). Starts at the current value when valid.
            let start = std::path::PathBuf::from(value.as_str());
            let start_dir = if start.is_dir() {
                start.clone()
            } else {
                start.parent().map(|p| p.to_path_buf()).unwrap_or_default()
            };
            let picked = match browse {
                BrowseKind::Folder => rfd::FileDialog::new()
                    .set_directory(&start_dir)
                    .pick_folder(),
                BrowseKind::LogFile => rfd::FileDialog::new()
                    .set_directory(&start_dir)
                    .add_filter("game.log", &["log"])
                    .pick_file(),
            };
            if let Some(p) = picked {
                *value = p.display().to_string();
            }
        }
    });
}

fn draw_debug_form(
    ctx: &egui::Context,
    menu: &mut MenuState,
    children: &mut BattleChildren,
    form: &mut DebugForm,
    icons: &IconSet,
    loc: &LocaleRes,
) {
    // The save list was filled once at menu startup — reload it when the
    // saves dir changed under us (Settings page), clamping the selection.
    if form.saves_dir_loaded != menu.settings.saves_dir {
        form.saves_dir_loaded = menu.settings.saves_dir.clone();
        form.save_files = menu
            .settings
            .saves_dir()
            .map(|d| list_saves(&d, 20))
            .unwrap_or_default();
        form.save_index = form.save_index.min(form.save_files.len().saturating_sub(1));
    }
    let win = egui::Window::new("menu_debug_form")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(frosted());
    let resp = menu
        .anchors
        .debug_form
        .window(ctx, win, egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                icons.label_with_icon(
                    ui,
                    IconId::Attack,
                    egui::RichText::new(loc.tr("menu.debug.title").into_owned())
                        .size(20.0)
                        .color(GOLD)
                        .strong(),
                    18.0,
                );
            });
            ui.add_space(8.0);

            // --- Battle source: script file or the manual form ---
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.debug.source").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                );
                tip(
                    ui.radio_value(
                        &mut form.use_script,
                        false,
                        loc.tr("menu.debug.manual").into_owned(),
                    ),
                    loc.tr("menu.tooltip.manual"),
                );
                tip(
                    ui.radio_value(
                        &mut form.use_script,
                        true,
                        loc.tr("menu.debug.script").into_owned(),
                    ),
                    loc.tr("menu.tooltip.script"),
                );
                if form.use_script {
                    if form.script_files.is_empty() {
                        ui.label(
                            egui::RichText::new(loc.tr("menu.debug.no_scripts").into_owned())
                                .size(12.0)
                                .color(ERROR_RED),
                        );
                    } else {
                        let current = form
                            .script_files
                            .get(form.script_index)
                            .and_then(|p| p.file_stem())
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let changed = egui::ComboBox::from_id_salt("script_pick")
                            .selected_text(current)
                            .show_ui(ui, |ui| {
                                for (i, p) in form.script_files.iter().enumerate() {
                                    let name = p
                                        .file_stem()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_default();
                                    ui.selectable_value(&mut form.script_index, i, name);
                                }
                            })
                            .response
                            .changed();
                        // The nation selector reloads when the
                        // script or the side toggle changes (keyed — never
                        // re-parses the script JSON every frame). The rows
                        // themselves render below the Your side row.
                        let key = (
                            form.script_files
                                .get(form.script_index)
                                .and_then(|p| p.file_stem())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            form.player_defender,
                        );
                        if changed || key != form.nations_key {
                            form.nations_key = key;
                            reload_script_nations(form);
                        }
                    }
                }
            });
            ui.add_space(6.0);

            // --- Manual form (disabled while a script is selected) ---
            ui.add_enabled_ui(!form.use_script, |ui| {
                // --- Map ---
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(loc.tr("menu.debug.map").into_owned())
                            .size(13.0)
                            .color(PARCHMENT),
                    );
                    tip(
                        ui.radio_value(
                            &mut form.map_synthetic,
                            true,
                            loc.tr("menu.debug.arena").into_owned(),
                        ),
                        loc.tr("menu.tooltip.arena"),
                    );
                    tip(
                        ui.radio_value(
                            &mut form.map_synthetic,
                            false,
                            loc.tr("menu.debug.province").into_owned(),
                        ),
                        loc.tr("menu.tooltip.province"),
                    );
                    if !form.map_synthetic {
                        ui.add_sized(
                            [70.0, 22.0],
                            egui::TextEdit::singleline(&mut form.province_id),
                        );
                        ui.label(
                            egui::RichText::new(loc.tr("menu.debug.dirs").into_owned())
                                .size(12.0)
                                .color(PARCHMENT),
                        );
                        ui.add_sized([80.0, 22.0], egui::TextEdit::singleline(&mut form.dirs));
                    }
                });

                // --- Forces ---
                force_row(ui, loc, "attacker", &mut form.atk_force, &mut form.atk_tag);
                force_row(ui, loc, "defender", &mut form.def_force, &mut form.def_tag);

                // Save picker (only when a FromSave force is selected).
                let needs_save = matches!(form.atk_force, ForceSel::FromSave)
                    || matches!(form.def_force, ForceSel::FromSave);
                if needs_save {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(loc.tr("menu.debug.save_file").into_owned())
                                .size(13.0)
                                .color(PARCHMENT),
                        );
                        if form.save_files.is_empty() {
                            ui.label(
                                egui::RichText::new(loc.tr("menu.debug.no_saves").into_owned())
                                    .size(12.0)
                                    .color(ERROR_RED),
                            );
                        } else {
                            let current = form
                                .save_files
                                .get(form.save_index)
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            egui::ComboBox::from_id_salt("save_pick")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    for (i, p) in form.save_files.iter().enumerate() {
                                        let name = p
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default();
                                        ui.selectable_value(&mut form.save_index, i, name);
                                    }
                                });
                        }
                    });
                }

                // --- Tactic / side ---
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(loc.tr("menu.debug.enemy_tactic").into_owned())
                            .size(13.0)
                            .color(PARCHMENT),
                    );
                    let combo = egui::ComboBox::from_id_salt("tactic_pick")
                        .selected_text(loc.tactic_name(TACTICS[form.tactic]).into_owned())
                        .show_ui(ui, |ui| {
                            for (i, t) in TACTICS.iter().enumerate() {
                                ui.selectable_value(
                                    &mut form.tactic,
                                    i,
                                    loc.tactic_name(*t).into_owned(),
                                );
                            }
                        });
                    tip(combo.response, loc.tr("menu.tooltip.enemy_tactic"));
                });
            });
            // The side toggle works in BOTH modes — in script mode
            // it overrides the script's own player_side (e.g. play the Warsaw
            // script from the defender side); in manual mode it is the side.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(loc.tr("menu.debug.your_side").into_owned())
                        .size(13.0)
                        .color(PARCHMENT),
                );
                tip(
                    ui.radio_value(
                        &mut form.player_defender,
                        false,
                        loc.tr("menu.debug.attacker").into_owned(),
                    ),
                    loc.tr("menu.tooltip.attacker"),
                );
                tip(
                    ui.radio_value(
                        &mut form.player_defender,
                        true,
                        loc.tr("menu.debug.defender").into_owned(),
                    ),
                    loc.tr("menu.tooltip.defender"),
                );
            });
            // The command-split nation selector — one row per
            // country tag of the player side's `divisions:` block, directly
            // under the side toggle (script mode only).
            if form.use_script && !form.script_nations.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(loc.tr("menu.debug.nations").into_owned())
                        .size(12.0)
                        .color(PARCHMENT),
                );
                for (tag, divisions) in &form.script_nations {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{tag} ({})", divisions.len()))
                                .size(12.0)
                                .color(GOLD),
                        );
                        let own = form.div_control.get(tag).copied().unwrap_or(true);
                        let entry = form.div_control.entry(tag.clone()).or_insert(own);
                        tip(
                            ui.radio_value(
                                entry,
                                true,
                                loc.tr("menu.debug.nation_player").into_owned(),
                            ),
                            loc.tr("menu.tooltip.nation_player"),
                        );
                        tip(
                            ui.radio_value(
                                entry,
                                false,
                                loc.tr("menu.debug.nation_ai").into_owned(),
                            ),
                            loc.tr("menu.tooltip.nation_ai"),
                        );
                    });
                }
                ui.add_space(4.0);
            }

            if !form.error.is_empty() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&form.error).size(12.0).color(ERROR_RED));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let busy = children.alive();
                ui.add_enabled_ui(!busy, |ui| {
                    let start = tip(
                        menu_button(ui, icons, Some(IconId::Attack), &loc.tr("menu.debug.start")),
                        loc.tr("menu.tooltip.start_battle"),
                    )
                    .clicked();
                    if start {
                        // No `?` here — this closure returns bool. Script
                        // mode: the script path rides on the Scenario.file
                        // field, serialized as file=<name> for the child.
                        let sc: Result<Scenario, String> = if form.use_script {
                            match build_scenario(form, loc) {
                                Ok(mut s) => {
                                    match form.script_files.get(form.script_index).cloned() {
                                        Some(p) => {
                                            s.file = Some(p);
                                            Ok(s)
                                        }
                                        None => {
                                            Err(loc.tr("menu.debug.err_no_script").into_owned())
                                        }
                                    }
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            build_scenario(form, loc)
                        };
                        match sc {
                            Ok(sc) => {
                                let mut args = vec!["--battle".to_string()];
                                args.extend(sc.to_cli_args());
                                // Non-empty only (same as the live spawn).
                                if !menu.settings.hoi4_dir.is_empty() {
                                    args.push(format!("hoi4_dir={}", menu.settings.hoi4_dir));
                                }
                                if !menu.settings.saves_dir.is_empty() {
                                    args.push(format!("saves_dir={}", menu.settings.saves_dir));
                                }
                                let label = loc.tr("menu.main.child_label_debug").into_owned();
                                match spawn_battle(&args, label, loc) {
                                    Ok(bc) => {
                                        children.running.push(bc);
                                        menu.page = Page::Main;
                                    }
                                    Err(e) => form.error = e,
                                }
                            }
                            Err(e) => form.error = e,
                        }
                    }
                });
                if menu_button(ui, icons, Some(IconId::BackArrow), &loc.tr("common.back")).clicked()
                {
                    form.error.clear();
                    menu.page = Page::Main;
                }
            });
        });
    menu.anchors
        .debug_form
        .update(resp.as_ref().map(|r| r.response.rect), ctx.pixels_per_point());
}

fn force_row(
    ui: &mut egui::Ui,
    loc: &LocaleRes,
    slug: &str,
    force: &mut ForceSel,
    tag: &mut String,
) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(loc.tr(&format!("menu.debug.{slug}")).into_owned())
                .size(13.0)
                .color(PARCHMENT),
        );
        egui::ComboBox::from_id_salt(format!("force_{slug}"))
            .selected_text(force.label(loc).into_owned())
            .show_ui(ui, |ui| {
                for f in ForceSel::ALL {
                    ui.selectable_value(force, f, f.label(loc).into_owned());
                }
            });
        ui.label(
            egui::RichText::new(loc.tr("menu.debug.tag").into_owned())
                .size(12.0)
                .color(PARCHMENT),
        );
        ui.add_sized([60.0, 22.0], egui::TextEdit::singleline(tag));
    });
}

/// (Re)load the nation selector for the selected script +
/// side — the player side's `divisions:` block grouped by resolved tag
/// (first-appearance order, the same grouping `allied_contingents` uses),
/// seeding the per-tag command overrides from the script's own control
/// values. Scripts without a divisions block clear the selector entirely.
fn reload_script_nations(form: &mut DebugForm) {
    form.script_nations.clear();
    form.div_control.clear();
    let Some(path) = form.script_files.get(form.script_index).cloned() else {
        return;
    };
    let Ok(sf) = crate::script::load(&path) else {
        return;
    };
    let side = if form.player_defender {
        &sf.defender
    } else {
        &sf.attacker
    };
    let mut tags: Vec<(String, Vec<String>)> = Vec::new();
    for d in &side.divisions {
        let tag = d.tag.clone().unwrap_or_else(|| side.tag.clone());
        match tags.iter_mut().find(|(t, _)| *t == tag) {
            Some((_, divs)) => divs.push(d.name.clone()),
            None => tags.push((tag.clone(), vec![d.name.clone()])),
        }
        form.div_control
            .insert(tag, d.control.trim().eq_ignore_ascii_case("player"));
    }
    form.script_nations = tags;
}

/// Validate the form into a Scenario (light checks only — the heavy
/// assembly runs in the child process). Errors are localized: they render
/// in the form's error line.
fn build_scenario(form: &DebugForm, loc: &LocaleRes) -> Result<Scenario, String> {
    let map = if form.map_synthetic {
        MapChoice::Arena
    } else {
        let id: u32 = form.province_id.trim().parse().map_err(|_| {
            loc.trf(
                "menu.debug.err_bad_province",
                &[("value", form.province_id.trim())],
            )
        })?;
        let dirs: Vec<HexDirection> = form
            .dirs
            .split(',')
            .filter_map(|t| HexDirection::from_token(t.trim()))
            .collect();
        if dirs.is_empty() {
            return Err(loc.trf("menu.debug.err_bad_dirs", &[("value", form.dirs.trim())]));
        }
        MapChoice::Province { id, dirs }
    };
    let needs_save = matches!(form.atk_force, ForceSel::FromSave)
        || matches!(form.def_force, ForceSel::FromSave);
    let save_path = if needs_save {
        Some(
            form.save_files
                .get(form.save_index)
                .cloned()
                .ok_or_else(|| loc.tr("menu.debug.err_no_save").into_owned())?,
        )
    } else {
        None
    };
    Ok(Scenario {
        map,
        attacker: form.atk_force.to_choice(&form.atk_tag),
        defender: form.def_force.to_choice(&form.def_tag),
        enemy_tactic: TACTICS[form.tactic],
        player_side: if form.player_defender {
            Side::Defender
        } else {
            Side::Attacker
        },
        // In script mode the form's side wins over the script's
        // own player_side field (the toggle is live for both modes).
        side_override: form.use_script,
        atk_tag: form.atk_tag.trim().to_uppercase(),
        def_tag: form.def_tag.trim().to_uppercase(),
        save_path,
        file: None,
        script_flags: Vec::new(),
        seed: None,
        // The nation selector's per-tag command overrides —
        // only meaningful in script mode (applied by assemble to the
        // player side's divisions block).
        div_control: if form.use_script {
            form.div_control.clone()
        } else {
            std::collections::HashMap::new()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::cap_stderr_tail;

    #[test]
    fn stderr_tail_cap_snaps_off_boundary_cut() {
        // Regression test: 40000 × "中" (3-byte UTF-8) = 120000 bytes;
        // the naive byte cut 120000-32768 = 87232 lands INSIDE a char and
        // String::drain(..87232) panicked pre-fix.
        let mut b = "中".repeat(40000);
        let naive = b.len() - 32 * 1024;
        assert!(
            !b.is_char_boundary(naive),
            "payload must hit the pre-fix trap"
        );
        cap_stderr_tail(&mut b);
        assert!(
            (32 * 1024..=32 * 1024 + 2).contains(&b.len()),
            "kept tail ~32KB: {}",
            b.len()
        );
        assert!(b.chars().all(|c| c == '中'), "content survives intact");
    }

    #[test]
    fn stderr_tail_cap_noop_under_threshold() {
        let mut b = "short log line\n".repeat(100);
        let before = b.clone();
        cap_stderr_tail(&mut b);
        assert_eq!(b, before);
    }

    #[test]
    fn stderr_tail_cap_repeated_mixed_width_flood() {
        // FC_PERF=1-style spam with CJK unit names: capping on every line
        // must never panic and must hold the cap.
        let mut b = String::new();
        for _ in 0..3000 {
            b.push_str("单位 1.Pz fires — 命中 -12.7 org\n");
            cap_stderr_tail(&mut b);
        }
        assert!(b.len() <= 64 * 1024 + 64, "stays capped: {}", b.len());
    }
}
