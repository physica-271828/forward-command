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
/// when it carries `data/`, else the dev workspace root.
fn resolve_root_with(exe_dir: Option<PathBuf>, workspace: PathBuf) -> PathBuf {
    if let Some(dir) = exe_dir {
        if dir.join("data").is_dir() {
            return dir;
        }
    }
    workspace
}

/// Runtime root directory: the exe's directory in a shipped package, the
/// workspace root in dev.
pub fn runtime_root() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    resolve_root_with(exe_dir, workspace_root())
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
        assert_eq!(
            resolve_root_with(Some(exe_dir), workspace.clone()),
            workspace
        );
        // Missing exe dir (current_exe failed) also falls back.
        assert_eq!(resolve_root_with(None, workspace.clone()), workspace);
        std::fs::remove_dir_all(&base).ok();
    }
}
