//! Render gate / idle frame throttle — makes the
//! `low_power` setting real on Windows.
//!
//! bevy_winit 0.15.3 drives updates from `WindowEvent::RedrawRequested` on
//! Windows and UNCONDITIONALLY re-arms `request_redraw()` afterwards
//! (bevy_winit-0.15.3 src/state.rs:412-420 + 658-663; winit's
//! `request_redraw` posts a real `WM_PAINT`), so the `WinitSettings` reactive
//! mode configured in `apply_low_power` never actually idles: a visible
//! battle window renders at present rate forever, a minimized one UNCAPPED
//! (~178 fps measured) with the shadow pass still at full resolution.
//!
//! Two "skip the render" designs were tried and rejected by hardware reality
//! (RTX 4060 Ti / Vulkan / driver 596.49 — do NOT retry them):
//!  1. Camera off, presents continue → the rotating swapchain ring (2–3
//!     buffered images) holds stale frames with different cosmetic phases
//!     (border pulse / marching ants), so the screen STROBES between them.
//!  2. Camera off + present suppressed (window dropped from the render
//!     world's `ExtractedWindows` for the frame) → the window goes BLANK
//!     GREY instead of retaining the last presented frame; content returns
//!     only while input flows.
//! Conclusion: on this setup every presented frame must be freshly rendered,
//! and presents may only pause when nobody is looking (minimized).
//!
//! So the gate is a plain THROTTLE:
//!  - idle (low_power on, no input / dirty flags / AI playback / fx / anims):
//!    the frame sleeps ~100 ms → ~10 fps; every frame is still fully
//!    rendered, so cosmetics (pulse, ants) step at 10 fps and nothing can
//!    strobe or blank;
//!  - any stimulus → full speed that same frame (input events are read at
//!    PreUpdate, so hover/click response adds at most one nap of latency);
//!  - minimized window → camera off + 16 ms pacing sleep: the surface is
//!    1×1 but the shadow pass would otherwise keep running full-size,
//!    uncapped. This guard applies even with low_power off.
//!
//! `open` means "the window is live" (visible & rendering) and drives the
//! run conditions that skip egui painters / picking / cosmetic rebuilds when
//! minimized. The throttle itself is just the sleeps below.

use bevy::ecs::system::SystemParam;
use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::{MouseButtonInput, MouseWheel};
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, WindowFocused, WindowResized};
use std::time::{Duration, Instant};

use crate::board::RouteArrowAnim;
use crate::camera::RtsCamera;
use crate::fx::{Debris, Flash, Floaters, FxQueue, Projectile};
use crate::game::AiTurn;
use crate::state::{BattleTour, TacticalState};
use crate::units::MoveAnims;

/// Idle frame pacing with low_power on (~10 fps). Worst-case added input
/// latency when the user touches the mouse/keyboard is one nap.
const IDLE_NAP: Duration = Duration::from_millis(100);
/// Pacing for minimized windows (nothing renders; just keep the loop alive).
const MINIMIZED_NAP: Duration = Duration::from_millis(16);
/// Full-speed tail after the last stimulus. Without it, a mouse moving at
/// ~125 Hz against a ~160 Hz schedule left event-less frames that each slept
/// 100 ms — mid-drag stutter, "labels trail the models". Any activity
/// extends full speed by this much; OS key-repeat (~30 Hz) and slow drags
/// stay smooth too.
const ACTIVITY_HYSTERESIS: Duration = Duration::from_millis(150);
/// Grace before a visible-but-unfocused window drops to the idle cadence:
/// brief focus blips (alt-tab check, taskbar peek) must not throttle;
/// beyond it, nobody is watching and full-cost frames are waste.
const UNFOCUSED_GRACE: Duration = Duration::from_millis(1500);

/// Always present (Tactical3dPlugin inits it). With `low_power` off only the
/// minimize guard runs. `apply_low_power` (window.rs) inserts the
/// `low_power()` variant for battle windows; the menu drives its own gate
/// (hidden-to-tray / minimized = throttled) in `menu_render_gate`.
#[derive(Resource)]
pub struct RenderGate {
    /// "The window is live": visible and rendering. Read by the run
    /// conditions that skip egui painters / picking / cosmetic rebuilds, and
    /// reported by the FC_PERF stats below.
    pub open: bool,
    /// Full idle throttling (the `low_power` setting); off = minimize-only.
    pub low_power: bool,
    /// Frame-rate cap for full-speed frames (settings `max_fps`;
    /// 0 = uncapped). The Vulkan present path does not pace on the dev
    /// machine (~160 fps of wasted renders) — full-speed frames are padded
    /// to this cadence instead. Idle frames keep their own ~10 fps nap.
    pub max_fps: u32,
    /// Last frame that had activity (hysteresis tail — see ACTIVITY_HYSTERESIS).
    last_activity: Instant,
    /// Last full-speed tick end (cap pacing reference).
    last_tick: Instant,
    /// When the window last lost OS focus (unfocused throttle):
    /// a VISIBLE but unfocused battle window — the player is in HOI4 waiting
    /// out the sync — used to keep rendering animations at full cost. After
    /// UNFOCUSED_GRACE it renders at the idle cadence instead (like the
    /// minimize guard, this applies even with low_power off); the first
    /// input/focus event restores full speed via the normal stimulus path.
    unfocused_since: Option<Instant>,
}

impl Default for RenderGate {
    fn default() -> Self {
        RenderGate {
            open: true,
            low_power: false,
            max_fps: 60,
            last_activity: Instant::now(),
            last_tick: Instant::now(),
            unfocused_since: None,
        }
    }
}

/// Run condition for per-frame painters / picking / cosmetics: with the gate
/// present and closed (minimized), skip — nothing is presented. Without the
/// resource everything runs as before.
pub fn gate_open(gate: Option<Res<RenderGate>>) -> bool {
    gate.is_none_or(|g| g.open)
}

/// Everything the gate sniffs, bundled to stay under Bevy's system-param
/// count limit.
#[derive(SystemParam)]
pub struct GateWorld<'w, 's> {
    windows: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
    cursor: EventReader<'w, 's, CursorMoved>,
    mouse_btn: EventReader<'w, 's, MouseButtonInput>,
    wheel: EventReader<'w, 's, MouseWheel>,
    key_ev: EventReader<'w, 's, KeyboardInput>,
    resized: EventReader<'w, 's, WindowResized>,
    focused: EventReader<'w, 's, WindowFocused>,
    // HELD inputs: a held camera-pan key only re-arrives as ~30 Hz OS key
    // repeat, and a held drag button with a slowly-moving cursor produces
    // few events — without these, both stuttered.
    keys: Res<'w, ButtonInput<KeyCode>>,
    mouse: Res<'w, ButtonInput<MouseButton>>,
    // World state that animates without input.
    state: Res<'w, TacticalState>,
    ai_turn: Res<'w, AiTurn>,
    tour: Res<'w, BattleTour>,
    anims: Res<'w, MoveAnims>,
    arrows: Res<'w, RouteArrowAnim>,
    fx_queue: Res<'w, FxQueue>,
    floaters: Res<'w, Floaters>,
    fx_live: Query<'w, 's, (), Or<(With<Projectile>, With<Flash>, With<Debris>)>>,
}

/// FC_PERF open-ratio bookkeeping (see the log line at the bottom).
/// `pub` only because system params must be nameable at the registration
/// site (game.rs).
#[derive(Default)]
pub struct GateStats {
    perf: Option<bool>,
    frames: u32,
    full_speed: u32,
    window: Option<Instant>,
}

/// PreUpdate: decide whether this frame runs full-speed or idles, and pace
/// accordingly. Runs before the Update chains so the egui painters / picking
/// read a fresh `gate.open`.
pub fn render_gate(
    mut gate: ResMut<RenderGate>,
    mut q_cam: Query<&mut Camera, With<RtsCamera>>,
    mut w: GateWorld,
    mut stats: Local<GateStats>,
) {
    let minimized = w
        .windows
        .get_single()
        .map(|win| win.resolution.physical_width() <= 1)
        .unwrap_or(false);

    // Minimized: stop rendering entirely (1×1 surface, full-size shadow pass,
    // uncapped rate otherwise) and just keep the loop alive.
    gate.open = !minimized;
    for mut cam in &mut q_cam {
        cam.is_active = !minimized;
    }
    if minimized {
        std::thread::sleep(MINIMIZED_NAP);
        return;
    }

    // Unfocused throttle: a VISIBLE
    // but unfocused battle window — the player is in HOI4 between syncs —
    // otherwise keeps animating at full cost (the low_power branch below
    // only idles when nothing moves; AI turns/floaters count as activity).
    // Applies even with low_power off, like the minimize guard above. The
    // frame still renders (the battle stays watchable at ~10 fps); the
    // WindowFocused/CursorMoved event on refocus restores full speed
    // through the normal stimulus path with at most one nap of latency.
    let focused = w
        .windows
        .get_single()
        .map(|win| win.focused)
        .unwrap_or(true);
    if focused {
        gate.unfocused_since = None;
    } else {
        let since = gate.unfocused_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= UNFOCUSED_GRACE {
            std::thread::sleep(IDLE_NAP);
            gate.last_tick = Instant::now();
            return;
        }
    }

    let mut full_speed = true;
    if gate.low_power {
        let input = w.cursor.read().count() > 0
            || w.mouse_btn.read().count() > 0
            || w.wheel.read().count() > 0
            || w.key_ev.read().count() > 0
            || w.resized.read().count() > 0
            || w.focused.read().count() > 0
            || w.keys.get_pressed().next().is_some()
            || w.mouse.get_pressed().next().is_some();
        let animating = w.ai_turn.active
            || !w.ai_turn.actions.is_empty()
            || w.tour.active
            || !w.anims.0.is_empty()
            || w.arrows.active
            || !w.fx_queue.0.is_empty()
            || w.floaters.any()
            || !w.fx_live.is_empty();
        let dirty = w.state.board_mesh_dirty
            || w.state.board_colors_dirty
            || w.state.units_dirty
            || w.state.orders_dirty
            || w.state.ally_sectors_dirty
            || w.state.arrows_grow;
        if input || animating || dirty {
            gate.last_activity = Instant::now();
        }
        full_speed = gate.last_activity.elapsed() < ACTIVITY_HYSTERESIS;
        if !full_speed {
            // Idle: render at ~10 fps instead of flat out. The frame is still
            // fully rendered and presented (see the module doc for why there
            // is no cheaper shortcut on this platform).
            std::thread::sleep(IDLE_NAP);
        }
    }

    // Frame-rate cap: pad full-speed frames to the configured
    // cadence. Present does not pace on this setup, so without the cap a
    // busy battle renders ~160 fps into a 60–180 Hz display.
    if full_speed && gate.max_fps > 0 {
        let target = Duration::from_secs_f32(1.0 / gate.max_fps as f32);
        let elapsed = gate.last_tick.elapsed();
        if elapsed < target {
            std::thread::sleep(target - elapsed);
        }
    }
    gate.last_tick = Instant::now();

    // Opt-in (FC_PERF=1) log: "[gate] full-speed 37/600 frames (6%)" once
    // per 5 s — true idle should read ~0–5%, active play ~100%.
    let perf = *stats
        .perf
        .get_or_insert_with(|| std::env::var_os("FC_PERF").is_some());
    if perf {
        let stats = &mut *stats;
        stats.frames += 1;
        stats.full_speed += u32::from(full_speed);
        if stats
            .window
            .map(|t| t.elapsed() >= Duration::from_secs(5))
            .unwrap_or(true)
        {
            let pct = stats.full_speed * 100 / stats.frames.max(1);
            eprintln!(
                "[gate] full-speed {}/{} frames ({pct}%)",
                stats.full_speed, stats.frames
            );
            stats.frames = 0;
            stats.full_speed = 0;
            stats.window = Some(Instant::now());
        }
    }
}
