//! Open the primary window MAXIMIZED but still windowed — not fullscreen
//! (menu and battle windows would otherwise launch unmaximized).
//! Bevy 0.15's `WindowMode` has no `Maximized` variant (it lands in 0.16),
//! and winit's `set_maximized` is POSTED to the event loop: it races
//! bevy_winit's `changed_windows` sync, whose `request_inner_size` restores
//! the window right back (winit's `is_maximized` flag reads true while
//! IsZoomed is false and the window sits at its restored rect). A
//! synchronous Win32 `ShowWindow(SW_MAXIMIZE)` on the HWND executes before
//! any of that machinery wakes up and stays genuinely zoomed (observable
//! via IsZoomed/GetWindowPlacement).

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy::winit::WinitWindows;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow, SetWindowPos, ShowWindow,
    HWND_TOP, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_MAXIMIZE,
};

/// Register the startup-maximize system (menu / battle / demo / debug windows).
pub fn start_maximized(app: &mut App) {
    app.add_systems(Update, maximize_primary_window);
}

/// Apply the render-quality settings: MSAA on the camera (via the render
/// crate's RenderQuality resource), shadow on/off + map size, and the
/// gate's `max_fps`. Used by every windowed mode; battles additionally get
/// [`apply_low_power`]. Defaults reproduce the old look.
pub fn apply_render_quality(app: &mut App, s: &crate::settings::AppSettings) {
    let msaa = match s.msaa_samples() {
        1 => bevy::render::view::Msaa::Off,
        2 => bevy::render::view::Msaa::Sample2,
        _ => bevy::render::view::Msaa::Sample4,
    };
    app.insert_resource(tactical3d_render::state::RenderQuality {
        msaa,
        shadows: s.shadows_enabled(),
        shadow_level: s.shadow_level(),
    });
    app.insert_resource(bevy::pbr::DirectionalLightShadowMap {
        size: s.shadow_level_map_size() as usize,
    });
    // The gate resource is init'd by Tactical3dPlugin during add_plugins.
    if let Some(mut gate) = app
        .world_mut()
        .get_resource_mut::<tactical3d_render::gate::RenderGate>()
    {
        gate.max_fps = s.max_fps();
    }
}

/// Render-resolution scale (settings `render_scale`): insert the state
/// resource the render crate's systems drive. At 100 nothing happens —
/// the 3D camera renders straight to the window, the default path is
/// byte-for-byte the old behaviour. The offscreen setup/sync/teardown
/// systems live in Tactical3dPlugin (render_scale.rs) since the in-battle
/// Settings window flips the scale at runtime. Battle windows only: the
/// menu never inserts the resource.
pub fn apply_render_scale(app: &mut App, pct: u32) {
    app.insert_resource(tactical3d_render::render_scale::RenderScaleState::new(pct));
}

/// In-battle Settings window (Esc menu → Settings): insert the
/// live `BattleSettings` mirror the render crate hot-applies
/// (settings.rs::apply_battle_settings), and register the persistence half —
/// edits save back to settings.json immediately, the same contract as the
/// menu Settings page.
pub fn init_battle_settings(app: &mut App, s: &crate::settings::AppSettings) {
    app.insert_resource(tactical3d_render::settings::BattleSettings {
        msaa: s.msaa_samples(),
        shadow_level: s.shadow_level(),
        render_scale: s.render_scale_pct(),
        max_fps: s.max_fps(),
        low_power: s.low_power,
    });
    app.add_systems(Update, persist_battle_settings);
}

/// Persist Settings-window edits: reload settings.json (a concurrent menu
/// edit is never clobbered), patch the five render keys, save (atomic
/// write-rename). The insertion frame is skipped — nothing edited yet.
fn persist_battle_settings(s: Option<Res<tactical3d_render::settings::BattleSettings>>) {
    let Some(s) = s else { return };
    if !s.is_changed() || s.is_added() {
        return;
    }
    let mut cfg = crate::settings::AppSettings::load();
    cfg.msaa = s.msaa;
    cfg.shadow = Some(s.shadow_level);
    cfg.render_scale = Some(s.render_scale);
    cfg.max_fps = s.max_fps;
    cfg.low_power = s.low_power;
    if let Err(e) = cfg.save() {
        warn!("battle settings: failed to save settings.json: {e}");
    }
}

/// Idle frame-saver (settings `low_power`, default ON): insert the render
/// gate (tactical3d-render/src/gate.rs) plus winit reactive settings.
///
/// NOTE: the reactive `UpdateMode` below does NOT work
/// on Windows — bevy_winit 0.15.3 re-arms `request_redraw()` after every
/// `RedrawRequested` (state.rs:412-420 + 658-663; winit posts a real
/// WM_PAINT), so a visible window renders at present rate forever and a
/// minimized one runs uncapped. The savings come from the RenderGate, which
/// throttles idle frames to ~10 fps (every presented frame is still fully
/// rendered — see gate.rs's module doc for why skipping renders/presents
/// strobes or blanks on this platform). The reactive settings are kept
/// anyway — harmless on Windows, effective if Bevy is ever upgraded.
pub fn apply_low_power(app: &mut App) {
    // Tactical3dPlugin init'd the gate (with max_fps applied by
    // apply_render_quality); just flip the low_power mode on.
    if let Some(mut gate) = app
        .world_mut()
        .get_resource_mut::<tactical3d_render::gate::RenderGate>()
    {
        gate.low_power = true;
    }
    app.insert_resource(bevy::winit::WinitSettings {
        focused_mode: bevy::winit::UpdateMode::reactive(std::time::Duration::from_millis(100)),
        unfocused_mode: bevy::winit::UpdateMode::reactive_low_power(
            std::time::Duration::from_millis(500),
        ),
    });
}

/// One-shot: maximize as soon as the OS window exists, then stop. Runs in
/// `Update` because winit creates the OS window inside its event loop — at
/// `Startup` the winit window may not exist yet. Non-send param pins the
/// system to the main (= event loop = window-owning) thread, so the
/// synchronous `ShowWindow` is same-thread and cannot deadlock.
fn maximize_primary_window(
    mut done: Local<bool>,
    winit_windows: Option<NonSend<WinitWindows>>,
    primary: Query<Entity, With<PrimaryWindow>>,
) {
    if *done {
        return;
    }
    let Some(winit) = winit_windows else { return };
    let Ok(entity) = primary.get_single() else {
        return;
    };
    let Some(window) = winit.get_window(entity) else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(win32.hwnd.get() as *mut _);
    unsafe {
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
    }
    *done = true;
}

// ---------------------------------------------------------------------------
// Startup foreground steal: a battle window must
// TAKE the screen the way the picker's menu pop does — the battle child
// opens while HOI4 owns the foreground, and a plain spawn leaves the window
// buried behind the game. Same retry cadence as the menu's tray restore
// (the first attempt can lose the winit window-creation race), same-thread
// via NonSend (see maximize_primary_window). Foreground rights come from
// main()'s AllowSetForegroundWindow(ASFW_ANY) grant plus the attach trick.

/// Register the startup foreground-steal system (battle windows).
pub fn bring_to_foreground(app: &mut App) {
    app.insert_resource(ForegroundRaise {
        retries: ForegroundRaise::RETRIES,
        timer: 0.0,
    });
    app.add_systems(Update, raise_primary_window);
}

#[derive(Resource)]
struct ForegroundRaise {
    /// Remaining retries (0.25 s cadence → ~2 s of coverage).
    retries: u8,
    timer: f32,
}

impl ForegroundRaise {
    const RETRIES: u8 = 8;
}

fn raise_primary_window(
    time: Res<Time>,
    mut raise: ResMut<ForegroundRaise>,
    winit_windows: Option<NonSend<WinitWindows>>,
    primary: Query<Entity, With<PrimaryWindow>>,
) {
    if raise.retries == 0 {
        return;
    }
    let Some(winit) = winit_windows else { return };
    let Ok(entity) = primary.get_single() else {
        return;
    };
    let Some(window) = winit.get_window(entity) else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(win32.hwnd.get() as *mut _);
    if unsafe { GetForegroundWindow() }.0 == hwnd.0 {
        raise.retries = 0;
        return;
    }
    raise.timer += time.delta_secs();
    if raise.timer < 0.25 {
        return;
    }
    raise.timer = 0.0;
    raise.retries -= 1;
    unsafe { force_window_to_front(hwnd) };
}

/// Raise + activate from a BACKGROUND process: `AttachThreadInput` to the
/// foreground window's thread borrows its input queue (and with it the
/// foreground rights) for the duration of the raise. Silent sibling of the
/// menu's instrumented `force_window_to_front` (menu.rs) — keep
/// the two in step.
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
    let _ = SetWindowPos(
        hwnd,
        HWND_TOP,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
    );
    let _ = SetForegroundWindow(hwnd);
    if attached {
        let _ = AttachThreadInput(our_tid, fore_tid, false);
    }
}
