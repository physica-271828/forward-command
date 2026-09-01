//! `tactical-inject` — Win32 console command injection into the running HOI4
//! process (DESIGN.md §3.2, Channel 3).
//!
//! Primary chain ([`InjectBackend::PostMessage`], verified in-game on HOI4
//! 1.19.2): the Clausewitz console reads keyboard input off its message pump,
//! so commands are *posted* straight to the HOI4 window — no foreground steal,
//! no focus change. `WM_KEYDOWN`/`WM_KEYUP`(`VK_OEM_3`) toggles the console,
//! `WM_CHAR` types `run tac_inject.txt` one UTF-16 code unit at a time, and
//! `WM_KEYDOWN`/`WM_KEYUP`(`VK_RETURN`) executes. The batch file lives in the
//! HOI4 user directory (`Documents\Paradox Interactive\Hearts of Iron IV\`),
//! because the console `run` command only resolves user-dir-relative paths
//! (an absolute path fails with "Couldn't find file"). The window is located
//! by title *prefix* ("Hearts of Iron IV") via `EnumWindows`, since the real
//! title carries a renderer suffix ("Hearts of Iron IV (DirectX 11)") that
//! defeats `FindWindowW` exact matching.
//!
//! The console toggle is stateless and input focus occasionally drops, so a
//! `PostMessage` injection first proves the console is listening: a probe line
//! (`eval_effect log = "<marker>"`, marker = pid + timestamp + counter) is
//! typed and the marker is read back from the tail of `game.log`. On a miss
//! the console is closed, reopened, and pinged once more; a second miss yields
//! [`InjectError::PingTimeout`], and [`Injector::inject_commands`] then
//! automatically falls back to the legacy foreground chain
//! ([`InjectBackend::ForegroundSendInput`]: `SetForegroundWindow` +
//! `SendInput`, the original procedure) and retries once. Without a
//! `log_path` the probe is skipped and the PostMessage chain runs blind
//! (old fire-and-forget semantics).
//!
//! Error recovery is intentionally minimal on this side: if injection fails,
//! the player can always use the in-game "Force Exit Tactical Mode" decision
//! (DESIGN.md §11.3); unsynced tactical results are simply lost.
//!
//! The crate is only ever built on Windows (HOI4 is a Windows process); all
//! FFI is kept in thin, individually `SAFETY`-documented helpers so the
//! testable logic (batch writing, prefix matching, ping markers, log-tail
//! readback, key lParam encoding, dry-run) stays pure Rust.

#![cfg(windows)]

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    VIRTUAL_KEY, VK_OEM_3, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, IsWindowVisible, PostMessageW, SendMessageTimeoutW, SetForegroundWindow,
    SMTO_ABORTIFHUNG, WM_CHAR, WM_GETTEXT, WM_KEYDOWN, WM_KEYUP,
};

/// Default HOI4 window title *prefix* used to locate the game (DESIGN.md
/// §3.2). The real 1.19.2 title is "Hearts of Iron IV (DirectX 11)" and the
/// renderer suffix varies, so matching is by prefix, never exact.
const DEFAULT_HOI4_WINDOW_TITLE_PREFIX: &str = "Hearts of Iron IV";

/// Batch file name inside the HOI4 user directory (DESIGN.md §3.2: the
/// console `run` command only resolves user-dir-relative paths).
const BATCH_FILE_NAME: &str = "tac_inject.txt";

/// Default stale threshold for [`cleanup_stale_batch_files`]: one day. An
/// active battle rewrites its batch file on every injection (each sync hour
/// at most), so anything this old belongs to a long-dead process — and even
/// a wrongly deleted batch is recreated by the next injection's write.
pub const STALE_BATCH_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Scan code of the backtick/tilde key (console toggle), carried in the
/// lParam of the posted key messages.
const SCAN_CONSOLE_TOGGLE: u16 = 0x29;

/// Scan code of the RETURN key.
const SCAN_RETURN: u16 = 0x1C;

/// Buffer for the title read during window enumeration; the real HOI4 title
/// is well under 100 UTF-16 code units.
const MAX_WINDOW_TITLE_LEN: usize = 256;

/// Settle time after each posted console toggle before typing begins
/// (the console needs a few hundred ms to (re)open).
const POSTMSG_CONSOLE_SETTLE: Duration = Duration::from_millis(350);

/// Time given to HOI4 to execute the batch before the console is closed.
const POSTMSG_EXECUTE_SETTLE: Duration = Duration::from_millis(200);

/// Total time to wait for the ping marker to appear in `game.log`.
const PING_TIMEOUT: Duration = Duration::from_millis(1500);

/// Poll interval while waiting for the ping marker.
const PING_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How many bytes from the end of `game.log` are scanned for the ping marker
/// (the marker is always a fresh last line; 8 KiB is plenty).
const LOG_TAIL_BYTES: u64 = 8192;

/// Settle time after `SetForegroundWindow` before sending keys (legacy
/// foreground fallback, DESIGN.md §3.2 pseudocode step 2).
const FOREGROUND_SETTLE: Duration = Duration::from_millis(100);

/// Time given to HOI4 to execute the batch before the console is closed
/// (legacy foreground fallback, DESIGN.md §3.2 pseudocode step 3).
const CONSOLE_EXECUTE: Duration = Duration::from_millis(150);

/// Errors returned by [`Injector::inject_commands`].
///
/// Any failure leaves HOI4 untouched apart from possibly a written batch file
/// (and at most console toggles); the player recovers via the in-game
/// "Force Exit Tactical Mode" decision (DESIGN.md §11.3).
#[derive(Debug)]
pub enum InjectError {
    /// `inject_commands` was called with an empty command slice; no batch file
    /// was written and no input was sent.
    EmptyCommandList,
    /// No window whose title starts with the configured HOI4 prefix exists
    /// (game not running, or running under a different title).
    WindowNotFound,
    /// `SetForegroundWindow` failed for the located HOI4 window (foreground
    /// fallback chain only).
    FocusFailed,
    /// Input delivery failed: `SendInput` reported undelivered events
    /// (foreground chain) or `PostMessageW` returned an error (posted chain).
    SendInputFailed,
    /// The console never acknowledged the ping probe: the marker was typed
    /// twice (initial try + close/reopen retry) and never read back from
    /// `game.log` within [`PING_TIMEOUT`].
    PingTimeout,
    /// Writing the batch file failed.
    Io(io::Error),
}

impl fmt::Display for InjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InjectError::EmptyCommandList => write!(f, "command list is empty"),
            InjectError::WindowNotFound => write!(f, "HOI4 window not found"),
            InjectError::FocusFailed => write!(f, "failed to bring HOI4 window to foreground"),
            InjectError::SendInputFailed => write!(f, "input delivery to HOI4 failed"),
            InjectError::PingTimeout => write!(
                f,
                "console ping timed out (no marker readback in game.log after two attempts)"
            ),
            InjectError::Io(err) => write!(f, "failed to write batch file: {err}"),
        }
    }
}

impl std::error::Error for InjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InjectError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for InjectError {
    fn from(err: io::Error) -> Self {
        InjectError::Io(err)
    }
}

/// Injection chain used by [`Injector::inject_commands`] (DESIGN.md §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InjectBackend {
    /// Background injection: `EnumWindows` prefix lookup + `PostMessageW`
    /// keyboard messages + game.log ping readback. No focus steal; verified
    /// in-game. This is the default.
    #[default]
    PostMessage,
    /// Legacy chain: `SetForegroundWindow` + `SendInput`. Kept as
    /// the automatic fallback when the posted chain fails, and force-able via
    /// [`Injector::backend`].
    ForegroundSendInput,
}

/// One step of the legacy foreground injection sequence (DESIGN.md §3.2
/// pseudocode, kept as the fallback chain).
///
/// Kept FFI-free so the ordering can be unit-tested without a running HOI4.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InjectAction {
    /// `SetForegroundWindow(hoi4_hwnd)` — window is resolved at execution time.
    FocusHoi4,
    /// `std::thread::sleep` for the given duration.
    Sleep(Duration),
    /// Key down + key up of a virtual-key code (e.g. `VK_OEM_3`, `VK_RETURN`).
    KeyPress(u16),
    /// Type a string via `KEYEVENTF_UNICODE` events.
    TypeText(String),
    /// `SetForegroundWindow(self_hwnd)` to hand focus back to our own window.
    RestoreFocus,
}

/// Win32 console injector (DESIGN.md §3.2).
pub struct Injector {
    hoi4_window_title_prefix: String,
    batch_file_path: PathBuf,
    log_path: Option<PathBuf>,
    /// Injection chain to use. Defaults to [`InjectBackend::PostMessage`];
    /// when the posted chain fails, `inject_commands` automatically retries
    /// once with [`InjectBackend::ForegroundSendInput`]. Set to
    /// `ForegroundSendInput` to force the legacy chain.
    pub backend: InjectBackend,
}

impl Injector {
    /// Creates an injector with the defaults from DESIGN.md §3.2:
    /// window title prefix "Hearts of Iron IV", batch file
    /// `%USERPROFILE%\Documents\Paradox Interactive\Hearts of Iron IV\tac_inject.txt`,
    /// and ping readback against `game.log` in that directory's `logs\`
    /// subdirectory.
    pub fn new() -> Self {
        let user_dir = hoi4_user_dir();
        Self {
            hoi4_window_title_prefix: DEFAULT_HOI4_WINDOW_TITLE_PREFIX.to_string(),
            batch_file_path: user_dir.join(BATCH_FILE_NAME),
            log_path: Some(user_dir.join("logs").join("game.log")),
            backend: InjectBackend::default(),
        }
    }

    /// Creates an injector with explicit paths: the batch file is written to
    /// `batch_file_path` (which must live in the HOI4 user directory — the
    /// console `run` command is given only its bare file name), and
    /// `log_path` points at `game.log` for the ping readback (`None` skips
    /// the probe and injects blind). The window title prefix stays the
    /// default; use [`Injector::with_config`] to override it.
    pub fn with_paths(batch_file_path: PathBuf, log_path: Option<PathBuf>) -> Self {
        Self {
            hoi4_window_title_prefix: DEFAULT_HOI4_WINDOW_TITLE_PREFIX.to_string(),
            batch_file_path,
            log_path,
            backend: InjectBackend::default(),
        }
    }

    /// Creates an injector with an explicit window title prefix and batch
    /// file path, and no ping readback (blind injection). Mainly useful for
    /// tests and for localized HOI4 window titles.
    pub fn with_config(
        hoi4_window_title_prefix: impl Into<String>,
        batch_file_path: PathBuf,
    ) -> Self {
        Self {
            hoi4_window_title_prefix: hoi4_window_title_prefix.into(),
            batch_file_path,
            log_path: None,
            backend: InjectBackend::default(),
        }
    }

    /// The `game.log` path used for the ping readback, when one is
    /// configured. The clock-advance receipt (DESIGN §8.4) scans the same
    /// file's tail for clock-probe lines.
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    /// Locates the HOI4 main window by enumerating visible top-level windows
    /// and matching the title by prefix (the real title is e.g.
    /// "Hearts of Iron IV (DirectX 11)", so exact `FindWindowW` matching is
    /// impossible). Prefers the match whose process verifies as `hoi4.exe` —
    /// a same-title browser tab or the Paradox launcher otherwise outranks
    /// the game in the z-order. Returns the first verified match, then any
    /// prefix match as fallback, `None` when no window matches at all.
    pub fn find_hoi4_window(&self) -> Option<HWND> {
        let mut search = WindowSearch {
            prefix: &self.hoi4_window_title_prefix,
            verified: Vec::new(),
            matches: Vec::new(),
        };
        // SAFETY: `search` is a live stack value for the whole duration of
        // the EnumWindows call, and the callback only dereferences the
        // pointer while that call runs. Enumeration is synchronous, so no
        // aliasing can occur.
        unsafe {
            EnumWindows(
                Some(enum_window_proc),
                LPARAM(&mut search as *mut WindowSearch as isize),
            )
        }
        .ok()?;
        search
            .verified
            .first()
            .or_else(|| search.matches.first())
            .copied()
    }

    /// Writes the console command batch file (`commands` joined with `\n`,
    /// exactly as in DESIGN.md §3.2) and returns the path written.
    /// A TRAILING newline is mandatory — HOI4's `run` file parser drops the
    /// final unterminated line, so a one-line batch without it executes
    /// NOTHING (verified in-game: `run tac_inject.txt` no-ops until the `\n`
    /// is appended).
    pub fn write_batch_file(&self, commands: &[String]) -> io::Result<PathBuf> {
        let content = format!("{}\n", commands.join("\n"));
        std::fs::write(&self.batch_file_path, content)?;
        Ok(self.batch_file_path.clone())
    }

    /// The console command that executes the batch file: `run` plus the bare
    /// file name only — the batch lives in the HOI4 user directory and the
    /// console `run` command resolves user-dir-relative paths (absolute
    /// paths fail with "Couldn't find file").
    fn run_command(&self) -> String {
        let name = self
            .batch_file_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.batch_file_path.display().to_string());
        format!("run {name}")
    }

    /// Injects `commands` into the running HOI4 console (DESIGN.md §3.2).
    ///
    /// * `self_window` — our own window handle; used only by the legacy
    ///   foreground chain, where focus is restored to it afterwards (step 4).
    ///   Best-effort: a failure to restore focus is not reported as an error.
    /// * `dry_run` — when `true`, only the batch file is written and the
    ///   actions that *would* be sent are logged to stdout; no Win32 window or
    ///   input calls are made, so this is safe on machines without HOI4.
    ///
    /// With the default [`InjectBackend::PostMessage`] chain, any failure
    /// (window missing, post failed, ping timeout) is retried once with the
    /// legacy [`InjectBackend::ForegroundSendInput`] chain; the degradation
    /// is logged to stderr but the returned `Ok`/`Err` reflects the final
    /// outcome only.
    ///
    /// Returns [`InjectError::EmptyCommandList`] before touching the disk when
    /// `commands` is empty.
    pub fn inject_commands(
        &self,
        commands: &[String],
        self_window: Option<HWND>,
        dry_run: bool,
    ) -> Result<(), InjectError> {
        if commands.is_empty() {
            return Err(InjectError::EmptyCommandList);
        }

        // 1. Write batch file (into the HOI4 user directory).
        let batch_path = self.write_batch_file(commands)?;

        if dry_run {
            println!(
                "[tactical-inject] dry run: wrote batch file {}",
                batch_path.display()
            );
            match self.backend {
                InjectBackend::PostMessage => {
                    println!(
                        "[tactical-inject] dry run: backend PostMessage (no Win32 calls made):"
                    );
                    println!(
                        "[tactical-inject] dry run:   find HOI4 window by title prefix {:?}",
                        self.hoi4_window_title_prefix
                    );
                    println!(
                        "[tactical-inject] dry run:   toggle console (VK_OEM_3), settle {POSTMSG_CONSOLE_SETTLE:?}"
                    );
                    if self.log_path.is_some() {
                        println!(
                            "[tactical-inject] dry run:   ping `eval_effect log = \"<marker>\"`, readback from game.log tail (timeout {PING_TIMEOUT:?}, one close/reopen retry)"
                        );
                    } else {
                        println!("[tactical-inject] dry run:   no log_path — blind injection (ping probe skipped)");
                    }
                    println!(
                        "[tactical-inject] dry run:   type `{}` + RETURN, settle {POSTMSG_EXECUTE_SETTLE:?}, toggle console closed",
                        self.run_command()
                    );
                    println!("[tactical-inject] dry run:   on failure, fall back once to the foreground plan:");
                    for action in self.injection_plan(self_window) {
                        println!("[tactical-inject] dry run:     fallback would {action:?}");
                    }
                }
                InjectBackend::ForegroundSendInput => {
                    for action in self.injection_plan(self_window) {
                        println!("[tactical-inject] dry run: would {action:?}");
                    }
                }
            }
            return Ok(());
        }

        match self.backend {
            InjectBackend::PostMessage => match self.inject_postmessage() {
                Ok(()) => Ok(()),
                Err(err) => {
                    eprintln!(
                        "[tactical-inject] PostMessage injection failed ({err}); retrying once with foreground SendInput"
                    );
                    self.inject_foreground(self_window)
                }
            },
            InjectBackend::ForegroundSendInput => self.inject_foreground(self_window),
        }
    }

    /// Primary chain: posted keyboard messages, no focus steal.
    ///
    /// Sequence: locate window by prefix → toggle console open (assumed
    /// closed) → settle → ping probe (skipped without `log_path`; on a miss,
    /// close/reopen and retry once) → type `run tac_inject.txt` → RETURN →
    /// settle → toggle console closed.
    ///
    /// The toggle is stateless, so the chain tracks the PRESUMED console
    /// state and sends the closing toggle only when the console is presumed
    /// open: an error path that leaves it OPEN desyncs the NEXT injection
    /// (its opening toggle would close it and every keystroke would go
    /// nowhere), while a closing toggle fired after a FAILED reopen would
    /// OPEN the console instead, silently.
    fn inject_postmessage(&self) -> Result<(), InjectError> {
        let hwnd = self.find_hoi4_window().ok_or(InjectError::WindowNotFound)?;

        // The console toggle is stateless: assume it starts closed, so a
        // successful first toggle leaves it (presumed) open.
        post_console_toggle(hwnd)?;
        let mut console_open = true;
        thread::sleep(POSTMSG_CONSOLE_SETTLE);

        let body = |console_open: &mut bool| -> Result<(), InjectError> {
            if self.log_path.is_some() && !self.ping_console(hwnd) {
                // Focus may have dropped: close, reopen, and probe once more.
                post_console_toggle(hwnd)?;
                *console_open = false;
                thread::sleep(POSTMSG_CONSOLE_SETTLE);
                post_console_toggle(hwnd)?;
                *console_open = true;
                thread::sleep(POSTMSG_CONSOLE_SETTLE);
                if !self.ping_console(hwnd) {
                    return Err(InjectError::PingTimeout);
                }
            }

            post_text(hwnd, &self.run_command())?;
            post_key(hwnd, VK_RETURN.0, SCAN_RETURN)?;
            thread::sleep(POSTMSG_EXECUTE_SETTLE);
            Ok(())
        };

        let result = body(&mut console_open);
        // Close only when the console is presumed open. Even after a batch
        // ending in `reloadinterface` (reset batches) the console stays
        // open (in-game evidence): skipping this toggle desyncs the NEXT
        // injection (the "snapshot never ran" bug). Firing it when a FAILED
        // reopen left the console closed would open it instead — the
        // presumed-state guard prevents that.
        if !console_open {
            return result;
        }
        match post_console_toggle(hwnd) {
            Ok(()) => result,
            Err(toggle_err) => {
                eprintln!(
                    "[tactical-inject] closing-toggle failed ({toggle_err}) — console left OPEN while the next injection assumes closed"
                );
                // The original error (when there is one) is more informative
                // than the toggle failure.
                result
            }
        }
    }

    /// Types the probe line `eval_effect log = "<marker>"` and waits for the
    /// marker to appear in the tail of `game.log`. The marker
    /// embeds the process id, a timestamp, and a counter, so stale lines from
    /// earlier injections can never produce a false positive.
    fn ping_console(&self, hwnd: HWND) -> bool {
        let Some(log_path) = &self.log_path else {
            return false;
        };
        let marker = ping_marker();
        let probe = format!("eval_effect log = \"{marker}\"");
        if post_text(hwnd, &probe).is_err() || post_key(hwnd, VK_RETURN.0, SCAN_RETURN).is_err() {
            return false;
        }
        let deadline = Instant::now() + PING_TIMEOUT;
        loop {
            if let Ok(tail) = read_log_tail(log_path, LOG_TAIL_BYTES) {
                if tail.contains(&marker) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(PING_POLL_INTERVAL);
        }
    }

    /// Builds the exact action sequence of the legacy foreground chain
    /// (DESIGN.md §3.2 steps 2–4).
    fn injection_plan(&self, self_window: Option<HWND>) -> Vec<InjectAction> {
        let run_command = self.run_command();
        let mut plan = vec![
            // 2. Bring HOI4 to foreground, let it settle.
            InjectAction::FocusHoi4,
            InjectAction::Sleep(FOREGROUND_SETTLE),
            // 3. Open console, run batch, close console.
            InjectAction::KeyPress(VK_OEM_3.0), // ` (console toggle)
            InjectAction::TypeText(run_command),
            InjectAction::KeyPress(VK_RETURN.0),
            InjectAction::Sleep(CONSOLE_EXECUTE),
            InjectAction::KeyPress(VK_OEM_3.0), // ` (console close)
        ];
        // 4. Return focus to our own window (only when the caller has one).
        if self_window.is_some() {
            plan.push(InjectAction::RestoreFocus);
        }
        plan
    }

    /// Legacy fallback chain: executes a plan produced by
    /// [`Injector::injection_plan`] via `SetForegroundWindow` + `SendInput`.
    fn inject_foreground(&self, self_window: Option<HWND>) -> Result<(), InjectError> {
        let plan = self.injection_plan(self_window);
        for action in &plan {
            match action {
                InjectAction::FocusHoi4 => {
                    let hwnd = self.find_hoi4_window().ok_or(InjectError::WindowNotFound)?;
                    // SAFETY: `hwnd` is a non-null window handle returned by
                    // the EnumWindows search above; SetForegroundWindow is
                    // safe to call with any valid top-level window handle.
                    if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
                        return Err(InjectError::FocusFailed);
                    }
                }
                InjectAction::Sleep(duration) => thread::sleep(*duration),
                InjectAction::KeyPress(vk) => {
                    if !send_key(VIRTUAL_KEY(*vk)) {
                        return Err(InjectError::SendInputFailed);
                    }
                }
                InjectAction::TypeText(text) => {
                    if !send_text(text) {
                        return Err(InjectError::SendInputFailed);
                    }
                }
                InjectAction::RestoreFocus => {
                    if let Some(hwnd) = self_window {
                        // SAFETY: caller guarantees `hwnd` is our own valid
                        // window handle. Best-effort per DESIGN.md §3.2 step 4;
                        // the return value is intentionally ignored.
                        unsafe {
                            let _ = SetForegroundWindow(hwnd);
                        };
                    }
                }
            }
        }
        Ok(())
    }
}

impl Default for Injector {
    fn default() -> Self {
        Self::new()
    }
}

/// Derives the HOI4 user directory (`%USERPROFILE%\Documents\Paradox
/// Interactive\Hearts of Iron IV`). Falls back to `%TEMP%`-rooted when
/// `USERPROFILE` is unset, so `Injector::new()` still produces a writable
/// batch path in degenerate environments.
fn hoi4_user_dir() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Documents")
        .join("Paradox Interactive")
        .join("Hearts of Iron IV")
}

/// Sweeps stale per-process batch files (`tac_inject_<pid>.txt`)
/// from the HOI4 user directory. Battle instances write their batch under a
/// pid-suffixed name so two concurrent battles never clobber one shared
/// file — but where the fixed `tac_inject.txt` overwrote itself, the pid
/// names leave one small file behind per battle. Every
/// `tac_inject_<digits>.txt` whose mtime is older than `max_age` is
/// deleted; the fixed-name `tac_inject.txt` (menu-level injections) and any
/// non-matching name are left alone. The mtime guard protects concurrent
/// battles: a live battle's batch is rewritten (mtime refreshed) on every
/// injection. A missing directory is `Ok(0)`; per-entry failures (locked
/// file, unreadable metadata, future mtime from clock skew) are skipped,
/// not fatal — this is a best-effort sweep. Returns the number removed.
pub fn cleanup_stale_batch_files(dir: &Path, max_age: Duration) -> io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_pid_batch = name
            .to_str()
            .and_then(|n| n.strip_prefix("tac_inject_"))
            .and_then(|n| n.strip_suffix(".txt"))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid_batch {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            // A future mtime (clock skew) counts as fresh.
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= max_age);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Pure title matcher behind [`Injector::find_hoi4_window`]: a window belongs
/// to HOI4 when its title *starts with* the configured prefix (the real title
/// carries a renderer suffix, e.g. "Hearts of Iron IV (DirectX 11)").
fn window_title_matches(title: &str, prefix: &str) -> bool {
    title.starts_with(prefix)
}

/// Generates a unique ping marker (`TAC_PING_<pid>_<millis>_<counter>`); the
/// counter keeps markers unique even for two probes inside the same
/// millisecond.
fn ping_marker() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "TAC_PING_{}_{}_{}",
        std::process::id(),
        millis,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Token every clock-probe line carries (DESIGN §8.4): the
/// receipt scan searches the game.log tail for the newest line containing
/// it, so the probe counter only needs to keep the lines distinguishable.
pub const CLOCK_PROBE_TOKEN: &str = "TAC_CLOCK_";

/// One clock-probe console line (DESIGN §8.4):
/// `eval_effect log = "TAC_CLOCK_<pid>_<n>"`. Once run, the probe's
/// game.log line carries the current game hour in its `[yyyy.mm.dd.hh]`
/// prefix — comparing it against the pre-injection prefix is the receipt
/// that a batch's trailing `pause_in_hours 1` actually advanced the clock.
pub fn clock_probe_line() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "eval_effect log = \"{}{}_{}\"",
        CLOCK_PROBE_TOKEN,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Reads the last `max_bytes` of the file at `path` as lossy UTF-8 (the slice
/// may start mid-codepoint; lossy conversion keeps the tail intact, which is
/// all the marker search needs). Pub for the clock-advance receipt scan
/// (DESIGN §8.4).
pub fn read_log_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max_bytes)))?;
    let mut buf = Vec::new();
    file.take(max_bytes).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Builds the lParam for a posted `WM_KEYDOWN`/`WM_KEYUP`: repeat count 1,
/// scan code in bits 16–23; key-up additionally sets the previous-state (30)
/// and transition (31) bits.
fn key_lparam(scan_code: u16, key_up: bool) -> LPARAM {
    let mut bits: u32 = 1 | ((scan_code as u32) << 16);
    if key_up {
        bits |= 0xC000_0000;
    }
    LPARAM(bits as i32 as isize)
}

/// Posts the console toggle (`VK_OEM_3`, scan 0x29) to `hwnd`.
fn post_console_toggle(hwnd: HWND) -> Result<(), InjectError> {
    post_key(hwnd, VK_OEM_3.0, SCAN_CONSOLE_TOGGLE)
}

/// Posts a virtual-key press (`WM_KEYDOWN` + `WM_KEYUP`) to `hwnd`.
fn post_key(hwnd: HWND, vk: u16, scan_code: u16) -> Result<(), InjectError> {
    // SAFETY: `hwnd` is a live top-level HOI4 window located via EnumWindows;
    // PostMessageW is safe to call with any valid window handle and never
    // blocks (unlike SendMessage, it queues and returns).
    unsafe {
        PostMessageW(
            hwnd,
            WM_KEYDOWN,
            WPARAM(vk as usize),
            key_lparam(scan_code, false),
        )
    }
    .map_err(|_| InjectError::SendInputFailed)?;
    // SAFETY: same handle and message contract as above.
    unsafe {
        PostMessageW(
            hwnd,
            WM_KEYUP,
            WPARAM(vk as usize),
            key_lparam(scan_code, true),
        )
    }
    .map_err(|_| InjectError::SendInputFailed)?;
    Ok(())
}

/// Types `text` into `hwnd` as `WM_CHAR` messages, one per UTF-16 code unit
/// (`encode_utf16` keeps non-BMP characters intact as surrogate pairs).
fn post_text(hwnd: HWND, text: &str) -> Result<(), InjectError> {
    for unit in text.encode_utf16() {
        // SAFETY: `hwnd` is a live HOI4 window handle; WM_CHAR carries the
        // UTF-16 code unit in wParam and is queued, never blocking.
        unsafe { PostMessageW(hwnd, WM_CHAR, WPARAM(unit as usize), LPARAM(1)) }
            .map_err(|_| InjectError::SendInputFailed)?;
    }
    Ok(())
}

/// Enumeration state threaded through `EnumWindows`'s lParam.
struct WindowSearch<'a> {
    prefix: &'a str,
    /// Title-prefix matches whose process is verified `hoi4.exe` (preferred).
    verified: Vec<HWND>,
    /// Title-prefix matches without process verification (fallback).
    matches: Vec<HWND>,
}

/// True when the process `pid` is `hoi4.exe` (image name, case-insensitive).
/// The title-prefix match alone can pick a browser tab ("Hearts of Iron IV
/// on Steam — …") or the Paradox launcher sitting above the game in the
/// z-order — posted keystrokes then land in the wrong process.
fn process_image_matches_hoi4(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: pid came from GetWindowThreadProcessId on a live window.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return false;
    };
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `buf`/`len` are valid out-params; the handle is live.
    let name = if unsafe {
        QueryFullProcessImageNameW(
            process,
            windows::Win32::System::Threading::PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok()
    {
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    } else {
        None
    };
    // SAFETY: `process` is the handle opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(process) };
    name.and_then(|n| {
        n.rsplit(['\\', '/'])
            .next()
            .map(|f| f.eq_ignore_ascii_case("hoi4.exe"))
    })
    .unwrap_or(false)
}

/// `EnumWindows` callback: records visible top-level windows whose title
/// starts with the search prefix, verified-HOI4-process ones first.
unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut WindowSearch` passed to EnumWindows in
    // `find_hoi4_window`; it outlives the enumeration and the callback runs
    // on the same thread, so the reference is unique and valid.
    let search = unsafe { &mut *(lparam.0 as *mut WindowSearch) };
    // SAFETY: `hwnd` is a live window handle supplied by EnumWindows.
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        // Live deadlock trap: the caller of this enumeration may be a Bevy
        // WORKER thread while our own window-owning main thread is blocked
        // in the schedule barrier — a cross-thread SendMessage (plain
        // `GetWindowTextW`) to one of OUR OWN windows then waits on a
        // thread that is itself waiting for this worker: process-wide
        // self-deadlock. Never message our own windows (they can never be
        // HOI4 anyway); `GetWindowThreadProcessId` is a kernel-side read
        // and never blocks, so the pid is fetched BEFORE the title.
        let mut pid = 0u32;
        // SAFETY: `hwnd` is live; `pid` is a valid out-param.
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, Some(&mut pid))
        };
        if pid == std::process::id() {
            return TRUE;
        }
        let mut buf = [0u16; MAX_WINDOW_TITLE_LEN];
        let mut copied = 0usize;
        // Bounded title read: a HUNG third-party window would block plain
        // `GetWindowTextW` (a SendMessage) forever — `SendMessageTimeoutW`
        // with SMTO_ABORTIFHUNG caps the wait and treats the window as
        // untitled instead.
        // SAFETY: `buf` is a valid writable UTF-16 buffer of `buf.len()`
        // code units; `copied` is a valid out-param; the call is bounded
        // by the 500 ms timeout.
        let lres = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETTEXT,
                WPARAM(buf.len()),
                LPARAM(buf.as_mut_ptr() as isize),
                SMTO_ABORTIFHUNG,
                500,
                Some(&mut copied),
            )
        };
        if lres.0 != 0 && copied > 0 {
            let title = String::from_utf16_lossy(&buf[..copied]);
            if window_title_matches(&title, search.prefix) {
                if process_image_matches_hoi4(pid) {
                    search.verified.push(hwnd);
                } else {
                    search.matches.push(hwnd);
                }
            }
        }
    }
    TRUE
}

/// Sends a virtual-key press (key down + key up) via `SendInput` (legacy
/// foreground chain). Returns `false` when not all events were delivered.
fn send_key(vk: VIRTUAL_KEY) -> bool {
    let size = std::mem::size_of::<INPUT>() as i32;
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: Default::default(), // key down
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    // SAFETY: `input` is a fully initialized keyboard INPUT and `size` is
    // exactly `size_of::<INPUT>()`, as SendInput requires.
    let sent = unsafe { SendInput(&[input], size) };
    if sent != 1 {
        return false;
    }

    input.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;

    // SAFETY: same buffer as above, only the flags field changed to KEYUP.
    let sent = unsafe { SendInput(&[input], size) };
    sent == 1
}

/// Types `text` via `KEYEVENTF_UNICODE` events (down + up per UTF-16 code
/// unit; legacy foreground chain). Encoding with `encode_utf16` (rather than
/// truncating `char` to `u16`) keeps non-BMP characters intact as surrogate
/// pairs.
fn send_text(text: &str) -> bool {
    let size = std::mem::size_of::<INPUT>() as i32;

    for unit in text.encode_utf16() {
        let mut input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0), // must be 0 for KEYEVENTF_UNICODE
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE, // key down
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        // SAFETY: `input` is a fully initialized Unicode keyboard INPUT and
        // `size` is exactly `size_of::<INPUT>()`.
        let sent = unsafe { SendInput(&[input], size) };
        if sent != 1 {
            return false;
        }

        input.Anonymous.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;

        // SAFETY: same buffer as above, only the flags field changed to
        // UNICODE | KEYUP.
        let sent = unsafe { SendInput(&[input], size) };
        if sent != 1 {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Injector writing to a unique temp file so parallel tests never collide.
    fn test_injector(tag: &str) -> Injector {
        let path = std::env::temp_dir().join(format!(
            "tac_inject_test_{}_{}.txt",
            std::process::id(),
            tag
        ));
        Injector::with_config("Hearts of Iron IV", path)
    }

    #[test]
    fn new_uses_design_defaults() {
        let injector = Injector::new();
        assert_eq!(injector.hoi4_window_title_prefix, "Hearts of Iron IV");
        assert_eq!(
            injector.batch_file_path,
            hoi4_user_dir().join("tac_inject.txt")
        );
        assert_eq!(
            injector.log_path,
            Some(hoi4_user_dir().join("logs").join("game.log"))
        );
        assert_eq!(injector.backend, InjectBackend::PostMessage);
    }

    #[test]
    fn with_paths_sets_paths_and_keeps_defaults() {
        let injector = Injector::with_paths(
            PathBuf::from(r"C:\HOI4\tac_inject.txt"),
            Some(PathBuf::from(r"C:\HOI4\logs\game.log")),
        );
        assert_eq!(injector.hoi4_window_title_prefix, "Hearts of Iron IV");
        assert_eq!(
            injector.batch_file_path,
            PathBuf::from(r"C:\HOI4\tac_inject.txt")
        );
        assert_eq!(
            injector.log_path,
            Some(PathBuf::from(r"C:\HOI4\logs\game.log"))
        );
        assert_eq!(injector.backend, InjectBackend::PostMessage);
    }

    #[test]
    fn write_batch_file_joins_commands_with_newlines() {
        let injector = test_injector("write");
        let commands = vec![
            "set_var tac_org_dmg 0.15".to_string(),
            "set_var tac_str_dmg 0.08".to_string(),
            "d_tac_apply_damage".to_string(),
        ];

        let written = injector.write_batch_file(&commands).unwrap();
        assert_eq!(written, injector.batch_file_path);

        let content = std::fs::read_to_string(&written).unwrap();
        assert_eq!(
            content,
            "set_var tac_org_dmg 0.15\nset_var tac_str_dmg 0.08\nd_tac_apply_damage\n"
        );

        std::fs::remove_file(&written).unwrap();
    }

    #[test]
    fn write_batch_file_single_line_gets_trailing_newline() {
        // HOI4's `run` parser drops the final UNTERMINATED line — a
        // one-line batch without the trailing \n executes nothing at all
        // (the tac_snap savegame no-op).
        let injector = test_injector("single");
        let written = injector
            .write_batch_file(&["savegame tac_snap".to_string()])
            .unwrap();
        let content = std::fs::read_to_string(&written).unwrap();
        assert_eq!(content, "savegame tac_snap\n");
        std::fs::remove_file(&written).unwrap();
    }

    #[test]
    fn inject_commands_rejects_empty_command_list() {
        let injector = test_injector("empty");
        let _ = std::fs::remove_file(&injector.batch_file_path);

        let err = injector.inject_commands(&[], None, true).unwrap_err();
        assert!(matches!(err, InjectError::EmptyCommandList));
        // Nothing must have been written for an empty command list.
        assert!(!injector.batch_file_path.exists());
    }

    #[test]
    fn dry_run_writes_batch_and_never_touches_win32() {
        let injector = test_injector("dry");
        let _ = std::fs::remove_file(&injector.batch_file_path);
        let commands = vec![
            "set_var tac_org_dmg 0.45".to_string(),
            "d_tac_end_battle".to_string(),
        ];

        // There is no HOI4 window on a test machine; a dry run must still
        // succeed, proving no window/input calls were made.
        injector.inject_commands(&commands, None, true).unwrap();

        let content = std::fs::read_to_string(&injector.batch_file_path).unwrap();
        // The batch always ends with a trailing newline (HOI4's `run`
        // parser drops the final unterminated line).
        assert_eq!(content, format!("{}\n", commands.join("\n")));

        std::fs::remove_file(&injector.batch_file_path).unwrap();
    }

    #[test]
    fn injection_plan_matches_design_3_2_sequence() {
        let injector = test_injector("plan");
        let self_window = HWND(std::ptr::null_mut());
        let plan = injector.injection_plan(Some(self_window));

        assert_eq!(
            plan,
            vec![
                InjectAction::FocusHoi4,
                InjectAction::Sleep(Duration::from_millis(100)),
                InjectAction::KeyPress(VK_OEM_3.0),
                InjectAction::TypeText(injector.run_command()),
                InjectAction::KeyPress(VK_RETURN.0),
                InjectAction::Sleep(Duration::from_millis(150)),
                InjectAction::KeyPress(VK_OEM_3.0),
                InjectAction::RestoreFocus,
            ]
        );
    }

    #[test]
    fn injection_plan_omits_restore_focus_without_self_window() {
        let injector = test_injector("plan_nofocus");
        let plan = injector.injection_plan(None);

        assert!(!plan.contains(&InjectAction::RestoreFocus));
        // The rest of the §3.2 sequence is unchanged.
        assert_eq!(plan.len(), 7);
    }

    #[test]
    fn run_command_uses_bare_file_name() {
        let injector = test_injector("runcmd");
        // The console `run` command resolves user-dir-relative paths only, so
        // no directory component may leak into the command.
        assert_eq!(
            injector.run_command(),
            format!("run tac_inject_test_{}_runcmd.txt", std::process::id())
        );
    }

    #[test]
    fn window_title_matching_is_prefix_based() {
        // Real 1.19.2 title, renderer suffix included.
        assert!(window_title_matches(
            "Hearts of Iron IV (DirectX 11)",
            "Hearts of Iron IV"
        ));
        assert!(window_title_matches(
            "Hearts of Iron IV",
            "Hearts of Iron IV"
        ));
        // Prefix must anchor at the start.
        assert!(!window_title_matches(
            "Not Hearts of Iron IV (DirectX 11)",
            "Hearts of Iron IV"
        ));
        // Title shorter than the prefix never matches.
        assert!(!window_title_matches("Hearts of Iron", "Hearts of Iron IV"));
        assert!(!window_title_matches("", "Hearts of Iron IV"));
    }

    #[test]
    fn ping_markers_are_unique_and_carry_pid() {
        let a = ping_marker();
        let b = ping_marker();
        assert_ne!(a, b);
        assert!(a.starts_with(&format!("TAC_PING_{}_", std::process::id())));
        assert!(b.starts_with(&format!("TAC_PING_{}_", std::process::id())));
    }

    #[test]
    fn clock_probe_lines_are_unique_and_carry_token_and_pid() {
        // The receipt scan keys on the token; the counter keeps consecutive
        // probes distinguishable.
        let a = clock_probe_line();
        let b = clock_probe_line();
        assert_ne!(a, b);
        let head = format!(
            "eval_effect log = \"{}{}_",
            CLOCK_PROBE_TOKEN,
            std::process::id()
        );
        assert!(a.starts_with(&head));
        assert!(a.ends_with('"'));
        assert!(b.starts_with(&head));
    }

    #[test]
    fn read_log_tail_scans_only_the_end_of_the_file() {
        let path =
            std::env::temp_dir().join(format!("tac_inject_logtail_{}.log", std::process::id()));
        let marker = "TAC_PING_TEST_MARKER";
        // Marker sits near the end of a file larger than the tail window.
        let filler = "x".repeat(16 * 1024);
        std::fs::write(&path, format!("{filler}\n[game] {marker}\n")).unwrap();

        let tail = read_log_tail(&path, LOG_TAIL_BYTES).unwrap();
        assert!(tail.contains(marker));
        // The filler at the very start is outside the tail window.
        assert!(tail.len() <= (LOG_TAIL_BYTES as usize) + 8);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_log_tail_reports_missing_file() {
        let path = std::env::temp_dir().join(format!(
            "tac_inject_logtail_missing_{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        assert!(read_log_tail(&path, LOG_TAIL_BYTES).is_err());
    }

    #[test]
    fn key_lparam_encodes_scan_code_and_transition_bits() {
        // Key down: repeat count 1, scan code in bits 16-23, bits 30/31 clear.
        assert_eq!(key_lparam(0x29, false).0, 0x0029_0001);
        // Key up: additionally previous-state (30) + transition (31) bits.
        assert_eq!(key_lparam(0x29, true).0, 0xC029_0001u32 as i32 as isize);
    }

    #[test]
    fn cleanup_stale_batch_files_removes_only_old_pid_files() {
        // Pid-suffixed batches older than the threshold go; the fixed-name
        // batch, fresh pid batches and look-alike names stay.
        let dir =
            std::env::temp_dir().join(format!("tac_inject_cleanup_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("tac_inject_111.txt");
        let fresh = dir.join("tac_inject_222.txt");
        let fixed = dir.join("tac_inject.txt");
        let non_numeric = dir.join("tac_inject_abc.txt");
        let empty_pid = dir.join("tac_inject_.txt");
        for p in [&stale, &fresh, &fixed, &non_numeric, &empty_pid] {
            std::fs::write(p, "x").unwrap();
        }
        let old = SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        for p in [&stale, &fixed, &non_numeric, &empty_pid] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        let removed = cleanup_stale_batch_files(&dir, STALE_BATCH_MAX_AGE).unwrap();
        assert_eq!(removed, 1, "only the stale pid batch is removed");
        assert!(!stale.exists());
        for p in [&fresh, &fixed, &non_numeric, &empty_pid] {
            assert!(p.exists(), "{} must survive", p.display());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cleanup_stale_batch_files_missing_dir_is_ok() {
        let dir =
            std::env::temp_dir().join(format!("tac_inject_cleanup_missing_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            cleanup_stale_batch_files(&dir, STALE_BATCH_MAX_AGE).unwrap(),
            0
        );
    }
}
