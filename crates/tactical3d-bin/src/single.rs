//! Single-instance guard.
//!
//! Two resident instances used to run side by side — a tray-hidden one with
//! its live listener still on, plus a freshly launched menu — and both
//! reacted to the same `tac_start`: their console-injection batches toggled
//! each other off (open/close console keystrokes cancel), so no snapshot
//! was taken, no ping landed in game.log, and the console just flickered
//! (the `run tac_inject.txt` batch + ping merged into one Unknown Command
//! line).
//!
//! The fix is a NAMED MUTEX held for the process's whole lifetime
//! (`CreateMutexW` + leak the handle — the OS releases it when the process
//! dies, crash included, so a dead instance never blocks a relaunch). Only
//! the RESIDENT modes take the guard: the menu (no args) and the CLI
//! `--live` loop. Every other mode (`--battle`, `--livebattle`,
//! `--headless`, `--preview`, …) deliberately does NOT — the menu spawns
//! them as children while it is itself alive, so a process-wide lock would
//! kill Debug Battle / live battles.
//!
//! A second launch of the menu never opens a second listener: it posts
//! `WM_SHOW_MENU` to the first instance's tray window (tray.rs), which
//! restores + raises the menu window exactly like a tray double-click, then
//! exits. A second `--live` is a hard CLI error.

use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

use crate::tray;

/// Per-session named mutex: while ANY handle is open the name exists, so
/// `ERROR_ALREADY_EXISTS` on creation ⟺ another instance is alive.
const MUTEX_NAME: &str = "Local\\ForwardCommand_SingleInstance";

/// How long a second menu launch waits for the first instance's tray
/// window before giving up — the first instance can still be cold-starting
/// (its tray installs after the first rendered frame).
const SUMMON_TIMEOUT: Duration = Duration::from_millis(3000);

/// The outcome of the single-instance guard.
pub enum SingleInstance {
    /// This process owns the mutex — it is the only resident instance.
    First,
    /// Another instance owns the mutex — this process must not listen.
    Second,
}

/// Take the single-instance guard for a resident mode (menu / --live).
/// The mutex handle is intentionally leaked: the name must stay alive for
/// the whole process lifetime, and the OS closes the handle at process
/// death (a crash clears the lock just like a clean exit).
pub fn guard() -> SingleInstance {
    unsafe {
        let name = wide(MUTEX_NAME);
        let h = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        // GetLastError must be read immediately after the call (the
        // windows crate Result conversion itself does not clobber it).
        let err = GetLastError();
        let Ok(h) = h else {
            // No lock (odd environment) — degrade to "first" so the app
            // still runs rather than refusing to start.
            eprintln!("[single] mutex creation failed ({err:?}) — running without the guard");
            return SingleInstance::First;
        };
        if err == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(h);
            SingleInstance::Second
        } else {
            // Deliberately keep the handle alive for the process lifetime:
            // HANDLE has no Drop, so simply letting `h` drop does NOT close
            // it — the OS releases it at process death, crash included.
            // (The old `mem::forget(h)` was a semantic no-op on a Copy value
            // and only confused clippy.)
            let _guard_handle = h;
            SingleInstance::First
        }
    }
}

/// Ask the running instance (when it is a menu) to bring its window back —
/// the same path as a tray-icon double-click. Returns false when no tray
/// window exists yet (first instance still cold-starting, or a `--live`
/// instance, which has no window at all).
pub fn summon_menu_window() -> bool {
    unsafe {
        let class = wide(tray::TRAY_CLASS);
        let Ok(hwnd) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) else {
            return false;
        };
        if hwnd.0.is_null() {
            return false;
        }
        PostMessageW(hwnd, tray::WM_SHOW_MENU, WPARAM(0), LPARAM(0)).is_ok()
    }
}

/// Poll `summon_menu_window` until the first instance's window answers or
/// the timeout runs out (a cold-starting first instance has no tray window
/// for the first few seconds).
pub fn summon_and_wait() {
    let deadline = Instant::now() + SUMMON_TIMEOUT;
    while !summon_menu_window() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// NUL-terminated UTF-16 (native string for Win32).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0u16]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard's detection rule: a second CreateMutexW on the same name
    /// reports ERROR_ALREADY_EXISTS. (In-process, with the first handle
    /// still open, that mirrors a second process seeing the first's guard.)
    /// Uses a TEST-SPECIFIC name: the production name is held by any live
    /// tray/menu instance on the machine, which would fail the first
    /// creation through no fault of the code under test.
    #[test]
    fn second_mutex_creation_detects_existing_instance() {
        const TEST_MUTEX_NAME: &str = "Local\\ForwardCommand_SingleInstance_Test";
        unsafe {
            let name = wide(TEST_MUTEX_NAME);
            let Ok(first) = CreateMutexW(None, false, PCWSTR(name.as_ptr())) else {
                panic!("first mutex must create");
            };
            let err1 = GetLastError();
            assert_ne!(
                err1, ERROR_ALREADY_EXISTS,
                "first creation must not see an existing mutex"
            );
            let Ok(second) = CreateMutexW(None, false, PCWSTR(name.as_ptr())) else {
                panic!("second creation must return a handle (already-exists is Ok)");
            };
            let err2 = GetLastError();
            assert_eq!(
                err2, ERROR_ALREADY_EXISTS,
                "second creation must detect the existing mutex"
            );
            let _ = CloseHandle(second);
            let _ = CloseHandle(first);
        }
    }
}
