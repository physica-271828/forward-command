//! Runtime file locations: `data/` tables, battle scripts, settings.json.
//!
//! One rule serves both layouts:
//! - **Shipped package** (beta/release zip): the exe sits next to `data/`
//!   (and `localisation/`, `assets/`) — everything resolves relative to the
//!   exe's own directory, so the folder is portable across machines.
//! - **Dev workspace**: the exe lives in `target/<profile>/` with no `data/`
//!   beside it — fall back to the compile-time workspace root
//!   (`CARGO_MANIFEST_DIR/../..`), where `data/` actually is.

use std::path::PathBuf;

/// Compile-time workspace root (dev layout).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Pure resolution rule, split out for tests: prefer the exe's own directory
/// when it carries `data/`, else the dev workspace root — and only when that
/// root really carries `data/` too. The workspace path is baked into the
/// binary at build time, so on a player machine it names a directory that
/// cannot exist; without the second check a player who copies the exe out of
/// its folder would silently get that stranger's path as the runtime root
/// (and every later failure would name it).
fn resolve_root_with(exe_dir: Option<PathBuf>, workspace: PathBuf) -> PathBuf {
    if let Some(dir) = &exe_dir {
        if dir.join("data").is_dir() {
            return dir.clone();
        }
    }
    if workspace.join("data").is_dir() {
        return workspace;
    }
    // Nothing holds `data/`: return the exe's own directory so the error the
    // caller prints names the folder the player actually controls.
    exe_dir.unwrap_or(workspace)
}

/// Runtime root directory: the exe's directory in a shipped package, the
/// workspace root in dev.
pub fn runtime_root() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    resolve_root_with(exe_dir, workspace_root())
}

/// Fatal startup check: `data/` must be reachable from [`runtime_root()`], or
/// every later table load / settings write fails one layer away from the
/// cause. Prints the actionable line and exits — the exe is portable by
/// design, so a missing `data/` means the player moved the exe out of its
/// folder.
pub fn require_data_dir() {
    if runtime_root().join("data").is_dir() {
        return;
    }
    eprintln!(
        "forward-command: installation incomplete — the exe must sit next to its `data/` folder \
         (looked in {})",
        runtime_root().display()
    );
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_dir_with_data_wins() {
        let base = std::env::temp_dir().join(format!("fc_dirs_test_data_{}", std::process::id()));
        let exe_dir = base.join("pkg");
        std::fs::create_dir_all(exe_dir.join("data")).unwrap();
        let workspace = base.join("ws");
        assert_eq!(
            resolve_root_with(Some(exe_dir.clone()), workspace.clone()),
            exe_dir
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn exe_dir_without_data_falls_back_to_workspace() {
        let base =
            std::env::temp_dir().join(format!("fc_dirs_test_nodata_{}", std::process::id()));
        let exe_dir = base.join("pkg"); // no data/ inside
        std::fs::create_dir_all(&exe_dir).unwrap();
        let workspace = base.join("ws");
        std::fs::create_dir_all(workspace.join("data")).unwrap();
        assert_eq!(
            resolve_root_with(Some(exe_dir), workspace.clone()),
            workspace
        );
        // Missing exe dir (current_exe failed) also falls back.
        assert_eq!(resolve_root_with(None, workspace.clone()), workspace);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn no_data_anywhere_returns_the_exe_dir_not_a_stranger_path() {
        // The workspace path is baked in at build time; on a player machine
        // it cannot exist. Without `data/` there, the fallback must NOT be
        // handed back as the runtime root — the caller would then report
        // failures against a directory the player has never seen.
        let base = std::env::temp_dir().join(format!("fc_dirs_test_bare_{}", std::process::id()));
        let exe_dir = base.join("pkg"); // exe copied out of its folder
        std::fs::create_dir_all(&exe_dir).unwrap();
        let workspace = base.join("build-machine-workspace"); // no data/
        assert_eq!(
            resolve_root_with(Some(exe_dir.clone()), workspace.clone()),
            exe_dir,
            "fall back to the folder the player actually controls"
        );
        // No exe dir either: nothing better to offer than the workspace.
        assert_eq!(resolve_root_with(None, workspace.clone()), workspace);
        std::fs::remove_dir_all(&base).ok();
    }
}
