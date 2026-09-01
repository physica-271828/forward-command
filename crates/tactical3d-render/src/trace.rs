//! `FC_TRACE=1` stage tracing (hang forensics): the game-loop
//! stages log enter/exit markers, so when the window freezes the LAST
//! "enter" without a matching "exit" in the log names the hanging stage.
//! Enabled once at startup from the `FC_TRACE` env var; costs one atomic
//! load per checkpoint when off. Pair with `FC_PERF=1` — the per-second
//! FPS line stopping is the freeze signal.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Adopt the env setting (called once from the plugin build).
pub fn init_from_env() {
    ENABLED.store(std::env::var_os("FC_TRACE").is_some(), Ordering::Relaxed);
}

/// Cheap gate for the [`crate::stage!`] macro.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Log a `[stage]` marker when FC_TRACE is on. Usage mirrors `info!`.
#[macro_export]
macro_rules! stage {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            bevy::log::info!("[stage] {}", format!($($arg)*));
        }
    };
}
