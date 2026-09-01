//! Native Win32 loading splash.
//!
//! Cold first launches look dead for many seconds — AV scan of the 100 MB
//! exe, wgpu/Vulkan device init, driver shader compilation — all BEFORE any
//! Bevy window can exist, so an in-engine loading screen cannot cover the
//! gap. This module shows a tiny plain-Win32 window from `main()` on its own
//! thread (own message loop, so Windows never marks anything "not
//! responding") and closes it once the Bevy app has presented its first
//! frames (`auto_close` system). Everything is best-effort: any Win32
//! failure just means "no splash", never an app failure.
//!
//! Plain GDI only — no common controls (a marquee progress bar would need a
//! comctl32 v6 manifest we don't embed).

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

/// Set when the Bevy app presented its first frames; the splash thread
/// polls it on its 100 ms timer and destroys the window.
static DONE: AtomicBool = AtomicBool::new(false);
/// Animation tick (100 ms steps), read by WM_PAINT.
static TICK: AtomicUsize = AtomicUsize::new(0);
/// True while the splash thread owns a window.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The game window's HWND, handed from the Bevy side to the splash thread
/// (see auto_close — the raise MUST run on the splash thread, see below).
static GAME_HWND: AtomicIsize = AtomicIsize::new(0);

/// Show the splash on a background thread. Call once from main() for GUI
/// modes; a second call in the same process is a no-op.
pub fn start() {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if let Err(e) = win32::run() {
            eprintln!("[splash] disabled: {e}");
        }
        ACTIVE.store(false, Ordering::SeqCst);
    });
}

/// Ask the splash to close (idempotent; the window dies within ~100 ms and
/// the thread with it — no join needed, process exit cleans up either way).
pub fn close() {
    DONE.store(true, Ordering::SeqCst);
}

/// Bevy system for every GUI app (menu / battle / demo): close the splash
/// once a few frames have been presented — by then wgpu device init AND the
/// first driver-side shader compiles are through, so the real window is
/// alive and responsive. The game window's HWND is handed to the splash
/// thread, which raises it above the other apps just before destroying
/// itself: the TOPMOST splash was this process's first window, so the app
/// window can stay buried behind whatever had focus before.
///
/// THREADING WARNING: this system runs on a compute-pool worker while the
/// main thread (the game window's owner) blocks waiting for the schedule —
/// calling SetWindowPos/SetForegroundWindow HERE deadlocks (synchronous
/// cross-thread SendMessage into a non-pumping owner). All Win32 window
/// calls therefore live on the splash thread, which nobody ever waits on.
pub fn auto_close(
    mut frames: bevy::prelude::Local<u8>,
    window: bevy::prelude::Query<
        &bevy::window::RawHandleWrapper,
        bevy::prelude::With<bevy::window::PrimaryWindow>,
    >,
) {
    if DONE.load(Ordering::SeqCst) {
        return;
    }
    *frames = frames.saturating_add(1);
    #[cfg(windows)]
    if let Some(wrapper) = window.iter().next() {
        if let raw_window_handle::RawWindowHandle::Win32(h) = wrapper.window_handle {
            GAME_HWND.store(h.hwnd.get(), Ordering::SeqCst);
        }
    }
    if *frames >= 6 {
        close();
    }
}

// ---------------------------------------------------------------------------
#[cfg(windows)]
mod win32 {
    use super::{DONE, GAME_HWND, TICK};
    use std::sync::atomic::Ordering;
    use windows::core::w;
    use windows::Win32::Foundation::{
        GetLastError, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// Logical (96-DPI) window size; scaled by the system DPI at creation so
    /// the splash looks identical at 100%/150%/200% scaling. The process is
    /// already per-monitor DPI-aware (main() sets it first thing), so these
    /// metrics are PHYSICAL pixels and the window never gets re-interpreted
    /// mid-startup (otherwise the window visibly shrinks and jumps off-center).
    const BASE_W: f32 = 460.0;
    const BASE_H: f32 = 150.0;
    const TIMER_ID: usize = 1;

    /// GDI objects shared by the paint handler; owned by the window via
    /// GWLP_USERDATA so no !Sync statics are needed.
    struct Gdi {
        bg: HBRUSH,
        track: HBRUSH,
        block: HBRUSH,
        title_font: HFONT,
        sub_font: HFONT,
        scale: f32,
        /// Localized loading line (settings.json language, DESIGN §15) —
        /// the splash runs BEFORE Bevy, so it reads the setting itself.
        sub_text: String,
    }

    const GOLD: u32 = 0x006EB5D6; // COLORREF is 0x00BBGGRR
    const PARCHMENT: u32 = 0x00C0D0D8;
    const BG: u32 = 0x00141618;
    const TRACK: u32 = 0x00282E33;
    const BRASS: u32 = 0x004E829E;

    /// Pull the app's real window above the other apps once the splash is
    /// done with the activation. z-order first (works even when activation
    /// rights are denied), then try to claim the foreground.
    /// SW_SHOW, NOT SW_RESTORE: activate while KEEPING the current size and
    /// show state — SW_RESTORE would undo the startup maximize that
    /// window::maximize_primary_window may already have applied (the menu
    /// opens maximized; this raise would restore it right back to normal).
    pub fn raise_to_foreground(hwnd: HWND) {
        unsafe {
            let shown = ShowWindow(hwnd, SW_SHOW);
            let pos = SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            let fg = SetForegroundWindow(hwnd);
            eprintln!(
                "[splash] raise hwnd={}: show={} pos={:?} fg={}",
                hwnd.0 as isize,
                shown.as_bool(),
                pos,
                fg.as_bool()
            );
        }
    }

    pub fn run() -> Result<(), String> {
        unsafe {
            let hmodule = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
            let hinstance = HINSTANCE(hmodule.0);
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                hbrBackground: CreateSolidBrush(COLORREF(BG)),
                lpszClassName: w!("ForwardCommandSplash"),
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                // 1410 = class already exists (e.g. after a quick relaunch in
                // the same session) — proceed, the class is ours anyway.
                let err = GetLastError();
                if err.0 != 1410 {
                    return Err(format!("RegisterClassW: {}", err.0));
                }
            }
            let (sx, sy) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let scale = GetDpiForSystem() as f32 / 96.0;
            let (win_w, win_h) = (
                (BASE_W * scale).round() as i32,
                (BASE_H * scale).round() as i32,
            );
            let gdi = Box::new(Gdi {
                bg: CreateSolidBrush(COLORREF(BG)),
                track: CreateSolidBrush(COLORREF(TRACK)),
                block: CreateSolidBrush(COLORREF(BRASS)),
                title_font: mk_font((28.0 * scale).round() as i32, 600),
                sub_font: mk_font((14.0 * scale).round() as i32, 400),
                scale,
                // Segoe UI covers CJK via Windows font linking (DESIGN §15).
                sub_text: tactical_locale::Locale::load(
                    crate::settings::AppSettings::load().language(),
                )
                .tr("splash.loading")
                .into_owned(),
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("ForwardCommandSplash"),
                w!("Forward Command"),
                WS_POPUP | WS_VISIBLE,
                (sx - win_w).max(0) / 2,
                (sy - win_h).max(0) / 2,
                win_w,
                win_h,
                None,
                None,
                hinstance,
                Some(Box::into_raw(gdi) as *const _),
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;
            // SW_SHOWNOACTIVATE: show WITHOUT taking the activation — the
            // topmost splash must never steal this launch's foreground
            // right, or the real window ends up buried behind other apps.
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(hwnd);
            SetTimer(hwnd, TIMER_ID, 100, None);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            Ok(())
        }
    }

    fn mk_font(height: i32, weight: i32) -> HFONT {
        unsafe {
            CreateFontW(
                height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                DEFAULT_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                w!("Segoe UI"),
            )
        }
    }

    fn gdi_of(hwnd: HWND) -> &'static Gdi {
        unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Gdi;
            // Before WM_CREATE stores it there is no state; paint handlers
            // only run after, so this is never dereferenced null in practice.
            &*ptr
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CREATE => {
                let cs = lparam.0 as *const CREATESTRUCTW;
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if DONE.load(Ordering::SeqCst) {
                    unsafe {
                        // Raise the game window BEFORE going away, from THIS
                        // thread (nobody blocks on the splash thread, so the
                        // cross-thread SendMessage inside SetWindowPos /
                        // SetForegroundWindow can never deadlock — unlike
                        // calling them from a Bevy worker, which deadlocks
                        // against the schedule-blocked main thread).
                        let game = GAME_HWND.load(Ordering::SeqCst);
                        if game != 0 {
                            raise_to_foreground(HWND(game as *mut _));
                        }
                        let _ = KillTimer(hwnd, TIMER_ID);
                        let _ = DestroyWindow(hwnd);
                    }
                } else {
                    TICK.fetch_add(1, Ordering::SeqCst);
                    unsafe {
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_PAINT => {
                unsafe {
                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    paint(hwnd, hdc);
                    let _ = EndPaint(hwnd, &ps);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe {
                    let gdi = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Gdi;
                    if !gdi.is_null() {
                        let gdi = Box::from_raw(gdi);
                        let _ = DeleteObject(gdi.bg);
                        let _ = DeleteObject(gdi.track);
                        let _ = DeleteObject(gdi.block);
                        let _ = DeleteObject(gdi.title_font);
                        let _ = DeleteObject(gdi.sub_font);
                    }
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    unsafe fn paint(hwnd: HWND, hdc: HDC) {
        let gdi = gdi_of(hwnd);
        let s = gdi.scale;
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
            // Solid background (class brush may not cover InvalidateRect(false)).
            FillRect(hdc, &rc, gdi.bg);

            SetBkMode(hdc, TRANSPARENT);
            // Title.
            SetTextColor(hdc, COLORREF(GOLD));
            SelectObject(hdc, gdi.title_font);
            let mut rc_title = RECT {
                top: (22.0 * s) as i32,
                bottom: (60.0 * s) as i32,
                ..rc
            };
            let mut title: Vec<u16> = "FORWARD COMMAND".encode_utf16().collect();
            DrawTextW(
                hdc,
                &mut title,
                &mut rc_title,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );
            // Subtitle.
            SetTextColor(hdc, COLORREF(PARCHMENT));
            SelectObject(hdc, gdi.sub_font);
            let mut rc_sub = RECT {
                top: (58.0 * s) as i32,
                bottom: (84.0 * s) as i32,
                ..rc
            };
            let mut sub: Vec<u16> = gdi.sub_text.encode_utf16().collect();
            DrawTextW(
                hdc,
                &mut sub,
                &mut rc_sub,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );

            // Sliding progress block (marquee stand-in, no comctl32 v6).
            let margin = (40.0 * s) as i32;
            let (y0, y1) = (rc.bottom - (34.0 * s) as i32, rc.bottom - (24.0 * s) as i32);
            let track = RECT {
                left: margin,
                top: y0,
                right: rc.right - margin,
                bottom: y1,
            };
            FillRect(hdc, &track, gdi.track);
            let span = (track.right - track.left).max(1) as usize;
            let block_w = ((90.0 * s) as usize).min(span);
            let tick = TICK.load(Ordering::SeqCst);
            // Ping-pong the block across the track (~2.5 s per traverse).
            let period = (span - block_w).max(1) * 2;
            let phase = tick % period;
            let x = if phase <= span - block_w {
                phase
            } else {
                period - phase
            };
            let block = RECT {
                left: track.left + x as i32,
                top: y0,
                right: track.left + (x + block_w) as i32,
                bottom: y1,
            };
            FillRect(hdc, &block, gdi.block);
        }
    }
}
