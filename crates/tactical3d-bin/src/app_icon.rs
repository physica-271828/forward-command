//! Runtime window icon (Windows-only).
//!
//! Bevy 0.15 has no window-icon API (`WindowIcon` arrived in 0.16), and
//! winit creates our windows with no class icon — the title bar shows the
//! stock default. We load the exe's embedded icon resource (id 1, compiled
//! in by build.rs) and WM_SETICON it onto the primary window as soon as the
//! window exists. Best-effort and idempotent: once posted, the system
//! returns early every frame.
//!
//! THREADING: WM_SETICON is delivered with PostMessageW, NOT SendMessageW.
//! This system runs on a Bevy compute-pool worker while the main thread
//! (the game window's owner) is blocked waiting for the schedule — a
//! synchronous cross-thread SendMessage deadlocks the whole app on the
//! first frame — the app freezes before presenting a single frame (the
//! same trap splash.rs documents for SetForegroundWindow).
//! Posting is fire-and-forget; DefWindowProc applies the icon when the
//! owner thread pumps the message between frames.

use bevy::prelude::*;

/// Bevy system: set ICON_BIG/ICON_SMALL on the primary window, once.
pub fn apply(
    mut done: Local<bool>,
    window: Query<&bevy::window::RawHandleWrapper, With<bevy::window::PrimaryWindow>>,
) {
    if *done {
        return;
    }
    #[cfg(windows)]
    if let Some(wrapper) = window.iter().next() {
        if let raw_window_handle::RawWindowHandle::Win32(h) = wrapper.window_handle {
            // Retry next frame if posting failed (queue full / window dying).
            *done = win32::set_icon(h.hwnd.get());
        }
    }
}

#[cfg(windows)]
mod win32 {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// Icon resource id written by build.rs (must match the .rc line).
    const ICON_ID: usize = 1;

    /// Post both icon sizes; true if at least ICON_BIG was queued.
    pub fn set_icon(raw_hwnd: isize) -> bool {
        unsafe {
            let Ok(hmod) = GetModuleHandleW(None) else {
                return false;
            };
            let hwnd = HWND(raw_hwnd as *mut _);
            let name = PCWSTR(ICON_ID as *const u16);
            // LR_SHARED: icons are owned by the system — no DestroyIcon.
            let big = LoadImageW(hmod, name, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE | LR_SHARED)
                .unwrap_or_default();
            if big.0.is_null() {
                return false;
            }
            let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
            let small = LoadImageW(hmod, name, IMAGE_ICON, cx, cy, LR_SHARED).unwrap_or_default();
            if !small.0.is_null() {
                let _ = PostMessageW(
                    hwnd,
                    WM_SETICON,
                    WPARAM(ICON_SMALL as usize),
                    LPARAM(small.0 as isize),
                );
            }
            PostMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(big.0 as isize),
            )
            .is_ok()
        }
    }
}
