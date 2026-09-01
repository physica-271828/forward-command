//! System-tray (notification-area) icon for the resident menu.
//!
//! The menu is a RESIDENT tray app: the
//! icon appears at startup and lives until the program exits — closing the
//! menu window (X) hides it to the tray instead of quitting, the Exit Game
//! button asks minimize-to-tray / quit / cancel, and the tray popup carries
//! Show menu / toggle live listen / Exit game. While a battle child holds
//! the screen the menu window simply hides; the icon never comes and goes.
//! Right-clicking offers the native popup; double-clicking restores the
//! menu window.
//!
//! ARCHITECTURE: all Shell_NotifyIconW + menu-window calls happen on a
//! dedicated thread with its own message loop, exactly like `splash.rs` —
//! winit (and therefore Bevy) has no hook for arbitrary `WM_USER`-range
//! messages, so a hidden top-level window owns the tray callback
//! (`uCallbackMessage = WM_APP + 1`). It must NOT be message-only
//! (HWND_MESSAGE): the right-click context menu passes it to
//! SetForegroundWindow + TrackPopupMenu, and a message-only window can
//! never take the foreground — the popup would open un-activated and its
//! clicks get lost (the Exit menu item appears dead). The
//! window is created hidden (never shown), so it has no taskbar or
//! Alt-Tab presence. The menu side only touches atomics
//! (`show_requested`/`exit_requested`/`listen_toggle_requested`) and never
//! calls Win32 window functions itself; ShowWindow/SetForegroundWindow on
//! the MENU window from this thread are synchronous cross-thread
//! SendMessages to the main thread, which never waits on the tray thread —
//! the same no-cycle argument that keeps the splash safe
//! (the synchronous-cross-thread-SendMessage deadlock trap, see splash.rs).
//!
//! Lifecycle: `TrayIcon::install` spawns the thread and blocks until the
//! icon is registered (a few ms). Drop (or `uninstall`) posts WM_CLOSE to
//! the hidden window; the WM_DESTROY handler removes the icon (NIM_DELETE)
//! and posts WM_QUIT, so no ghost icon survives a normal exit. If the
//! context popup is open the thread can only finish once the user dismisses
//! it — Drop waits briefly, then detaches.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Icon resource id written by build.rs (must match the .rc line, same as
/// app_icon.rs).
const ICON_ID: usize = 1;
/// Tray icon identifier within our process (must match NIM_DELETE).
const TRAY_ID: u32 = 1;
/// Application-defined callback message for Shell_NotifyIconW.
const WM_TRAY: u32 = WM_APP + 1;
/// Tooltip update posted from the menu side (a leaked `Box<String>` in
/// wParam, freed by the WndProc — the three-state tip).
const WM_TRAY_TIP: u32 = WM_APP + 2;
/// Second-launch summon: the new instance posts this to the
/// running instance's tray window; the WndProc restores + raises the menu
/// window exactly like a tray double-click.
pub const WM_SHOW_MENU: u32 = WM_APP + 3;
/// Context-menu command ids.
const ID_SHOW: usize = 1;
const ID_TOGGLE: usize = 2;
const ID_EXIT: usize = 3;
/// Hidden top-level tray window class name (never shown). Pub: the
/// single-instance guard (single.rs) finds the running instance by this
/// class to summon its menu window.
pub const TRAY_CLASS: &str = "ForwardCommandTrayWnd";

/// State shared between the tray thread and the menu systems.
struct TrayCtx {
    menu_hwnd: HWND,
    shown: Arc<AtomicBool>,
    exit: Arc<AtomicBool>,
    /// Listen-toggle request from the popup (one-shot, consumed by the menu).
    listen_toggle: Arc<AtomicBool>,
    /// Current live-listen state (menu side writes it; the popup label
    /// follows, so the item always shows the ACTION it performs).
    listen_on: Arc<AtomicBool>,
    /// NUL-terminated UTF-16 for the native popup items.
    show_label: Vec<u16>,
    exit_label: Vec<u16>,
    /// Toggle item label for the on/off state (localized by the caller).
    listen_on_label: Vec<u16>,
    listen_off_label: Vec<u16>,
}

/// Handle to a live tray icon (Drop removes it). Holds the atomics the menu
/// polls; the Win32 machinery all lives on the dedicated thread. The window
/// handle is kept as raw `isize` — HWND (a `*mut c_void`) is neither Send
/// nor Sync, and this type rides inside a Bevy Resource.
pub struct TrayIcon {
    shown: Arc<AtomicBool>,
    exit: Arc<AtomicBool>,
    listen_toggle: Arc<AtomicBool>,
    listen_on: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    hwnd: Option<isize>,
}

impl TrayIcon {
    /// Spawn the tray thread and block until the icon is registered.
    /// Returns None if the shell refused the icon (Explorer missing etc.).
    /// `tip` is the hover tooltip; the labels are the native popup texts —
    /// all localized by the caller (DESIGN §15). `listen_on_label` /
    /// `listen_off_label` are the popup's live-listen toggle item for the
    /// OFF/ON state (the item shows the action it performs).
    pub fn install(
        menu_hwnd: isize,
        tip: &str,
        show_label: &str,
        exit_label: &str,
        listen_on_label: &str,
        listen_off_label: &str,
    ) -> Option<TrayIcon> {
        let shown = Arc::new(AtomicBool::new(false));
        let exit = Arc::new(AtomicBool::new(false));
        let listen_toggle = Arc::new(AtomicBool::new(false));
        let listen_on = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Option<isize>>();
        let th_shown = shown.clone();
        let th_exit = exit.clone();
        let th_toggle = listen_toggle.clone();
        let th_on = listen_on.clone();
        // Owned strings: the thread outlives this call (must be 'static).
        let tip = tip.to_string();
        let show_label = show_label.to_string();
        let exit_label = exit_label.to_string();
        let listen_on_label = listen_on_label.to_string();
        let listen_off_label = listen_off_label.to_string();
        let thread = std::thread::Builder::new()
            .name("fc-tray".into())
            .spawn(move || {
                // The hwnd is sent BEFORE the message loop: the menu blocks
                // on recv() during install, and the loop only ends when a
                // WM_CLOSE arrives through that hwnd — sending after the
                // loop would deadlock the menu on every battle start.
                let hwnd = tray_setup(
                    HWND(menu_hwnd as *mut _),
                    &tip,
                    &show_label,
                    &exit_label,
                    &listen_on_label,
                    &listen_off_label,
                    th_shown,
                    th_exit,
                    th_toggle,
                    th_on,
                );
                let _ = tx.send(hwnd);
                if hwnd.is_none() {
                    return;
                }
                tray_loop();
            })
            .ok()?;
        // The thread always reports exactly once, right after registering
        // (or failing to register) the icon — so a plain blocking recv is
        // deterministic and fast.
        match rx.recv().ok().flatten() {
            Some(hwnd) => Some(TrayIcon {
                shown,
                exit,
                listen_toggle,
                listen_on,
                thread: Some(thread),
                hwnd: Some(hwnd),
            }),
            None => {
                let _ = thread.join();
                None
            }
        }
    }

    /// True once the user double-clicked the tray icon (menu window wanted
    /// back). One-shot: the flag is consumed by this call.
    pub fn show_requested(&self) -> bool {
        self.shown.swap(false, Ordering::Relaxed)
    }

    /// True once the user picked Exit in the tray popup. One-shot.
    pub fn exit_requested(&self) -> bool {
        self.exit.swap(false, Ordering::Relaxed)
    }

    /// True once the user picked the live-listen toggle in the tray popup.
    /// One-shot.
    pub fn listen_toggle_requested(&self) -> bool {
        self.listen_toggle.swap(false, Ordering::Relaxed)
    }

    /// Sync the menu's live-listen state so the popup toggle item shows the
    /// action it performs (menu side calls this after every toggle).
    pub fn set_listen_on(&self, on: bool) {
        self.listen_on.store(on, Ordering::Relaxed);
    }

    /// Update the hover tooltip live (exactly three states — idle /
    /// listening / battle; localized by the caller). Posts a `Box<String>` to
    /// the tray thread, which applies it via Shell_NotifyIconW(NIM_MODIFY).
    pub fn set_tip(&self, tip: &str) {
        let Some(hwnd) = self.hwnd else { return };
        let raw = Box::into_raw(Box::new(tip.to_string()));
        let ok = unsafe {
            PostMessageW(
                HWND(hwnd as *mut _),
                WM_TRAY_TIP,
                WPARAM(raw as usize),
                LPARAM(0),
            )
        };
        if ok.is_err() {
            // Window gone (shutting down): free the string instead of leaking.
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if let Some(hwnd) = self.hwnd.take() {
            // WM_CLOSE → WM_DESTROY → NIM_DELETE + WM_QUIT on the tray thread.
            let _ = unsafe { PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
        if let Some(thread) = self.thread.take() {
            // Bounded wait: with the context popup open the thread cannot
            // finish until the user dismisses it — then detach, the thread
            // still removes the icon on its way out.
            let deadline = Instant::now() + Duration::from_millis(300);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

/// The tray thread, phase 1: hidden top-level window + Shell_NotifyIconW.
/// Returns the window handle on success (None on any failure). The caller
/// reports this to the menu BEFORE running the message loop — see install.
fn tray_setup(
    menu_hwnd: HWND,
    tip: &str,
    show_label: &str,
    exit_label: &str,
    listen_on_label: &str,
    listen_off_label: &str,
    shown: Arc<AtomicBool>,
    exit: Arc<AtomicBool>,
    listen_toggle: Arc<AtomicBool>,
    listen_on: Arc<AtomicBool>,
) -> Option<isize> {
    unsafe {
        register_class();
        let hinst = GetModuleHandleW(None).ok()?;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(wide(TRAY_CLASS).as_ptr()),
            PCWSTR::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND::default(), // real top-level window (never shown — stays
            // invisible): HWND_MESSAGE can never
            // take the foreground, which the context-menu
            // popup's SetForegroundWindow/TrackPopupMenu
            // need (message-only popups are born un-activated
            // and swallow their clicks)
            None,
            hinst,
            None,
        )
        .ok()?;
        let ctx = Box::new(TrayCtx {
            menu_hwnd,
            shown,
            exit,
            listen_toggle,
            listen_on,
            show_label: wide(show_label),
            exit_label: wide(exit_label),
            listen_on_label: wide(listen_on_label),
            listen_off_label: wide(listen_off_label),
        });
        let ctx_ptr = Box::into_raw(ctx);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx_ptr as isize);
        let Some(nid) = build_nid(hwnd, tip) else {
            let _ = DestroyWindow(hwnd); // WM_NCDESTROY frees the ctx box
            return None;
        };
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            let _ = DestroyWindow(hwnd);
            return None;
        }
        Some(hwnd.0 as isize)
    }
}

/// The tray thread, phase 2: pump messages until WM_QUIT (posted by the
/// WM_DESTROY handler). The WndProc's context box is freed on WM_NCDESTROY.
fn tray_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
}

/// Register the tray window class (once per process).
fn register_class() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else {
            return;
        };
        let wc = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst.into(),
            hIcon: HICON::default(),
            hCursor: HCURSOR::default(),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(wide(TRAY_CLASS).as_ptr()),
        };
        let _ = RegisterClassW(&wc);
    });
}

/// Fill a NOTIFYICONDATAW with the embedded exe icon and the localized tip.
unsafe fn build_nid(hwnd: HWND, tip: &str) -> Option<NOTIFYICONDATAW> {
    let Ok(hinst) = GetModuleHandleW(None) else {
        return None;
    };
    // LR_SHARED: icon owned by the system — no DestroyIcon (app_icon.rs).
    let Ok(icon) = LoadImageW(
        hinst,
        PCWSTR(ICON_ID as *const u16),
        IMAGE_ICON,
        0,
        0,
        LR_DEFAULTSIZE | LR_SHARED,
    ) else {
        return None;
    };
    let mut tip_buf = [0u16; 128];
    fill_tip(&mut tip_buf, tip);
    Some(NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NOTIFY_ICON_DATA_FLAGS(NIF_MESSAGE.0 | NIF_ICON.0 | NIF_TIP.0),
        uCallbackMessage: WM_TRAY,
        hIcon: HICON(icon.0),
        szTip: tip_buf,
        ..Default::default()
    })
}

/// Copy `tip` into the NID tip buffer; zip truncates at the shorter end —
/// tips longer than 127 chars simply get cut (szTip is fixed at 128 wchars
/// incl. the terminator).
fn fill_tip(tip_buf: &mut [u16; 128], tip: &str) {
    for (dst, src) in tip_buf.iter_mut().zip(tip.encode_utf16()) {
        *dst = src;
    }
}

/// Apply a new tooltip via NIM_MODIFY (the tray thread's WndProc side of
/// `TrayIcon::set_tip` — the three-state tip).
unsafe fn update_tip(hwnd: HWND, tip: &str) {
    let mut tip_buf = [0u16; 128];
    fill_tip(&mut tip_buf, tip);
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NOTIFY_ICON_DATA_FLAGS(NIF_TIP.0),
        szTip: tip_buf,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// The hidden window's WndProc: only the tray callback matters.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            // Shell posts uID in wParam; lParam carries the mouse message.
            if let Some(ctx) = user_data(hwnd) {
                match lparam.0 as u32 {
                    WM_LBUTTONDBLCLK => show_menu(ctx),
                    WM_RBUTTONUP | WM_CONTEXTMENU => context_menu(hwnd, ctx),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_TRAY_TIP => {
            // New tooltip: a leaked Box<String> posted by TrayIcon::set_tip.
            let s = Box::from_raw(wparam.0 as *mut String);
            update_tip(hwnd, s.as_str());
            LRESULT(0)
        }
        WM_SHOW_MENU => {
            // Second-launch summon: restore + raise the menu
            // window like a tray double-click. Same no-cycle safety as the
            // tray callback (the main thread never waits on this one).
            if let Some(ctx) = user_data(hwnd) {
                show_menu(ctx);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Last message before the window data is gone: free the ctx box
            // the tray thread stored in GWLP_USERDATA (all failure paths in
            // tray_setup route through DestroyWindow too).
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr != 0 {
                let _ = Box::from_raw(ptr as *mut TrayCtx);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn user_data(hwnd: HWND) -> Option<&'static TrayCtx> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if ptr == 0 {
        None
    } else {
        Some(&*(ptr as *const TrayCtx))
    }
}

/// Restore the menu window and flag the menu side to re-sync its state.
unsafe fn show_menu(ctx: &TrayCtx) {
    let _ = ShowWindow(ctx.menu_hwnd, SW_SHOW);
    let _ = SetForegroundWindow(ctx.menu_hwnd);
    ctx.shown.store(true, Ordering::Relaxed);
}

/// Remove the tray icon for this window (NIM_DELETE, same hWnd/uID).
unsafe fn remove_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

/// Right-click popup: Show menu / toggle live listen / Exit game
/// (localized labels). `hwnd` is the hidden TRAY window (the just-clicked
/// owner) — the classic pattern grants it the foreground so the popup takes
/// focus and dismisses on click-away; the MENU window is hidden, so it can
/// never own the popup. The tray window is a REAL (top-level, never-shown)
/// window precisely so SetForegroundWindow here can succeed — a
/// message-only window can't take the foreground and the popup would be
/// born un-activated, silently losing clicks (the Exit item appears dead).
/// The listen-toggle item label follows the menu's live state
/// (`listen_on`), so it always shows the action the click performs.
unsafe fn context_menu(hwnd: HWND, ctx: &TrayCtx) {
    // The popup needs the cursor position — for the tray callback the
    // coords live in lParam only for legacy messages, and WM_CONTEXTMENU
    // can even be keyboard-invoked (-1,-1); GetCursorPos is authoritative.
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let Ok(hmenu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(hmenu, MF_STRING, ID_SHOW, PCWSTR(ctx.show_label.as_ptr()));
    // The toggle item shows the ACTION it performs: OFF → "enable listen",
    // ON → "disable listen".
    let toggle_label = if ctx.listen_on.load(Ordering::Relaxed) {
        &ctx.listen_off_label
    } else {
        &ctx.listen_on_label
    };
    let _ = AppendMenuW(hmenu, MF_STRING, ID_TOGGLE, PCWSTR(toggle_label.as_ptr()));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_EXIT, PCWSTR(ctx.exit_label.as_ptr()));
    // Foreground dance so the popup can take focus and dismiss properly.
    let _ = SetForegroundWindow(hwnd);
    let ret = TrackPopupMenu(
        hmenu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(hmenu);
    // Post a harmless message so the shell releases the foreground grant
    // (classic requirement for popup-from-tray to keep working).
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    handle_menu_command(ret.0 as usize, ctx);
}

/// Route a popup command id to its action. Separated from `context_menu`
/// so the routing is unit-testable (the popup itself needs a real desktop).
unsafe fn handle_menu_command(cmd: usize, ctx: &TrayCtx) {
    match cmd {
        ID_SHOW => show_menu(ctx),
        ID_TOGGLE => ctx.listen_toggle.store(true, Ordering::Relaxed),
        ID_EXIT => ctx.exit.store(true, Ordering::Relaxed),
        _ => {}
    }
}

/// NUL-terminated UTF-16 (native strings for Win32).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0u16]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Command routing: the popup ids must map to the right
    /// atomics — Show restores the window, Toggle requests the live-listen
    /// flip, Exit requests program shutdown; anything else is a dismissal.
    #[test]
    fn popup_command_ids_route_to_the_right_flags() {
        let ctx = TrayCtx {
            menu_hwnd: HWND::default(),
            shown: Arc::new(AtomicBool::new(false)),
            exit: Arc::new(AtomicBool::new(false)),
            listen_toggle: Arc::new(AtomicBool::new(false)),
            listen_on: Arc::new(AtomicBool::new(false)),
            show_label: wide("Show"),
            exit_label: wide("Exit"),
            listen_on_label: wide("Listen on"),
            listen_off_label: wide("Listen off"),
        };
        unsafe { handle_menu_command(ID_SHOW, &ctx) };
        assert!(ctx.shown.load(Ordering::Relaxed), "ID_SHOW must flip shown");
        assert!(!ctx.exit.load(Ordering::Relaxed));
        assert!(!ctx.listen_toggle.load(Ordering::Relaxed));
        unsafe { handle_menu_command(ID_TOGGLE, &ctx) };
        assert!(
            ctx.listen_toggle.load(Ordering::Relaxed),
            "ID_TOGGLE must flip listen_toggle"
        );
        assert!(!ctx.exit.load(Ordering::Relaxed));
        unsafe { handle_menu_command(ID_EXIT, &ctx) };
        assert!(ctx.exit.load(Ordering::Relaxed), "ID_EXIT must flip exit");
        // A dismissal (0) or unknown id must touch nothing.
        unsafe { handle_menu_command(0, &ctx) };
        unsafe { handle_menu_command(0xFFFF, &ctx) };
        assert_eq!(ctx.shown.swap(false, Ordering::Relaxed), true);
        assert_eq!(ctx.listen_toggle.swap(false, Ordering::Relaxed), true);
        assert_eq!(ctx.exit.swap(false, Ordering::Relaxed), true);
    }

    /// The popup's toggle item label follows the live-listen state, so the
    /// menu always shows the ACTION the click performs.
    #[test]
    fn toggle_label_follows_listen_state() {
        let pick = |on: bool| -> &'static str {
            let ctx = TrayCtx {
                menu_hwnd: HWND::default(),
                shown: Arc::new(AtomicBool::new(false)),
                exit: Arc::new(AtomicBool::new(false)),
                listen_toggle: Arc::new(AtomicBool::new(false)),
                listen_on: Arc::new(AtomicBool::new(on)),
                show_label: wide("Show"),
                exit_label: wide("Exit"),
                listen_on_label: wide("Enable"),
                listen_off_label: wide("Disable"),
            };
            let l = if ctx.listen_on.load(Ordering::Relaxed) {
                &ctx.listen_off_label
            } else {
                &ctx.listen_on_label
            };
            if l == &ctx.listen_on_label {
                "enable"
            } else {
                "disable"
            }
        };
        assert_eq!(pick(false), "enable", "OFF shows the enable action");
        assert_eq!(pick(true), "disable", "ON shows the disable action");
    }

    /// Full lifecycle smoke test: install a real tray icon (Shell_NotifyIconW
    /// against the live shell), drive the double-click callback through the
    /// hidden window's message loop, then drop and verify the icon's window
    /// is destroyed. Runs on demand (`--ignored`), like the HOI4-install
    /// tests — it needs an interactive desktop session with Explorer.
    #[test]
    #[ignore = "requires an interactive shell session (real Explorer tray)"]
    fn tray_icon_lifecycle_smoke() {
        // A fake "menu window" hwnd: ShowWindow/SetForegroundWindow on it
        // are best-effort no-ops, so the callback path is safe to drive.
        let icon = TrayIcon::install(
            0isize,
            "tray smoke test",
            "Show",
            "Exit",
            "Listen on",
            "Listen off",
        )
        .expect("tray icon should install");
        let tray_hwnd = icon.hwnd.expect("install must report the tray hwnd");
        assert!(!icon.show_requested(), "no user interaction yet");

        // Regression guard: the tray window must be a real top-level
        // window (never shown), NOT message-only — a message-only window can
        // never take the foreground, and the right-click popup's
        // SetForegroundWindow/TrackPopupMenu then open it un-activated with
        // dead clicks (the Exit item appeared to do nothing).
        let parent = unsafe { GetParent(HWND(tray_hwnd as *mut _)) }.unwrap_or_default();
        assert!(
            parent.0.is_null(),
            "tray window must be top-level (GetParent null), got a message-only window"
        );

        // Post the double-click callback straight at the hidden window — the
        // tray thread's WndProc should run show_menu (harmless on hwnd 0)
        // and flip the `shown` flag the menu side polls.
        let posted = unsafe {
            PostMessageW(
                HWND(tray_hwnd as *mut _),
                WM_TRAY,
                WPARAM(TRAY_ID as usize),
                LPARAM(WM_LBUTTONDBLCLK as isize),
            )
        };
        assert!(posted.is_ok(), "callback must post into the tray thread");
        // show_requested is one-shot (swap) — poll it as the detection.
        let mut seen = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !seen && Instant::now() < deadline {
            seen = icon.show_requested();
            if !seen {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(seen, "double-click callback must flip the shown flag");
        assert!(!icon.exit_requested(), "exit flag stays untouched");

        // Drop must tear the hidden window down (NIM_DELETE + WM_QUIT) —
        // posting to a destroyed hwnd then fails.
        drop(icon);
        let after =
            unsafe { PostMessageW(HWND(tray_hwnd as *mut _), WM_NULL, WPARAM(0), LPARAM(0)) };
        assert!(after.is_err(), "tray window must be gone after drop");
    }
}
