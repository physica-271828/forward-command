//! Persistent user settings: HOI4 install dir, saves dir and
//! game.log path, editable from the main-menu Settings page. Stored as
//! `settings.json` under the runtime root (next to the exe in shipped
//! packages, workspace root in dev — see [`crate::dirs`]); every field
//! falls back to the auto-detected value when missing/blank.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// HOI4 install root (contains `map/provinces.bmp`).
    #[serde(default)]
    pub hoi4_dir: String,
    /// Save games dir (`Documents/Paradox Interactive/Hearts of Iron IV/save games`).
    #[serde(default)]
    pub saves_dir: String,
    /// Full path to `game.log` (tac_start trigger channel, §3).
    #[serde(default)]
    pub log_path: String,
    /// UI language: "en" / "zh" (unknown/blank → SimpChinese — the primary
    /// audience is Chinese-speaking). Switched on the Settings page; battle
    /// child processes read it at startup (DESIGN §15).
    #[serde(default)]
    pub language: String,
    /// MSAA sample count: 4 / 2 / 1 (= off). Default 2: on weak GPUs (e.g.
    /// an RTX 3050 4GB Laptop) 4× costs ≈ 4× the fullscreen fragment work
    /// on a board that fills the screen, while 2× reads essentially the
    /// same on a near-top-down RTS. The one visual concern with 2× —
    /// far-zoom orbit moiré — is covered by an always-on FXAA pass on the
    /// 3D camera (spawn_camera), which costs a fraction of the MSAA delta.
    /// Hot-applied everywhere — the menu Settings page and the in-battle
    /// Esc → Settings window.
    #[serde(default = "default_msaa")]
    pub msaa: u32,
    /// Directional-light shadow map size: 2048 / 1024. LEGACY — superseded
    /// by `shadow`; kept so old settings.json files parse. No longer
    /// migrates to a level: an unchosen 2048 was the old default, not a
    /// preference.
    #[serde(default = "default_shadow_map")]
    pub shadow_map: u32,
    /// Shadow quality: 0 = off, 1 = low, 2 = high. Default LOW — at RTS
    /// zoom the casters are only the unit minis and props, so High is
    /// near-indistinguishable from Low while the fullscreen PCF cost is
    /// real. The tiers are concrete: low = 1024 + one cascade +
    /// Hardware2x2 filtering, high = 2048 + two cascades + Gaussian 9-tap.
    #[serde(default)]
    pub shadow: Option<u32>,
    /// Frame-rate cap for battle windows and the menu: 30/60/90/
    /// 120/144, 0 = uncapped. Default 60 — the Vulkan present path on the
    /// dev machine does not pace (≈160 fps of wasted renders on a 60–180 Hz
    /// display), and a turn-based wargame reads identically at 60.
    #[serde(default = "default_max_fps")]
    pub max_fps: u32,
    /// Frame-rate cap for the MAIN MENU only: the
    /// menu deliberately never idle-throttles (its orbiting backdrop always
    /// animates), so the foreground menu rendered flat out at `max_fps` —
    /// measured ≈30% GPU at 60 for a scene whose only moving part is a
    /// 0.045 rad/s camera yaw (≈140 s/rev). That slow cinematic reads
    /// identically at 24–30 fps, hence the separate lower cap.
    /// 0 = uncapped; 15/24/30/60 allowed.
    #[serde(default = "default_menu_fps")]
    pub menu_fps: u32,
    /// Idle frame-saver (default ON): battle windows drop to a reactive
    /// ~10 fps when there is no input, instead of redrawing the scene at
    /// vsync rate continuously. Keeps the GPU free for HOI4 running
    /// alongside. The main menu stays continuous — its
    /// orbiting backdrop is deliberately always animating.
    #[serde(default = "default_low_power")]
    pub low_power: bool,
    /// Damage writeback mode (DESIGN §12): how battle
    /// losses are injected back into HOI4 — `"org_str"` (default): org+str,
    /// province precision (attacker str diluted across each source
    /// province); `"off"`: no writeback at all. The retired `"org_only"`
    /// token parses to the default (its whole-army channel zeroed a
    /// national army in a live game — the mode was removed).
    /// Player-facing labels/details live on the menu Settings page.
    #[serde(default)]
    pub writeback: String,
    /// Render-resolution scale percent: 100 / 85 / 70 / 50. Below
    /// 100 the 3D scene renders into a smaller offscreen target and is
    /// upscaled to the window — the weak-GPU lever (quality knobs barely
    /// help there; only rendering fewer pixels does). Anything
    /// invalid/missing → 100 (native). Applied live in battle windows
    /// (Esc → Settings — the offscreen path builds/tears down at
    /// runtime); the menu is never scaled.
    #[serde(default)]
    pub render_scale: Option<u32>,
}

fn default_msaa() -> u32 {
    2
}

impl Default for AppSettings {
    /// Fresh-install defaults: delegate to the serde defaults so a MISSING
    /// settings.json yields the same values as an empty `{}` (a derived
    /// zero-default would bypass them — both fps caps would read as
    /// "unlimited", low_power off, on first launch of a fresh install).
    fn default() -> Self {
        serde_json::from_str("{}").expect("empty object parses via serde defaults")
    }
}

fn default_shadow_map() -> u32 {
    2048
}

fn default_max_fps() -> u32 {
    60
}

fn default_menu_fps() -> u32 {
    30
}

fn default_low_power() -> bool {
    true
}

/// settings.json lives under the runtime root (next to the exe in shipped
/// packages, workspace root in dev — see [`crate::dirs`]). A missing file is
/// not an error — defaults apply.
fn settings_path() -> PathBuf {
    crate::dirs::runtime_root().join("settings.json")
}

/// Does settings.json currently exist? First-run detection: a
/// fresh install lands on the Settings page so the player confirms the
/// auto-detected HOI4 paths before anything else.
pub fn settings_file_exists() -> bool {
    settings_path().is_file()
}

impl AppSettings {
    /// Load from settings.json; missing file/fields fall back to
    /// auto-detected defaults (so a fresh install "just works").
    pub fn load() -> Self {
        let mut s: AppSettings = std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if s.hoi4_dir.is_empty() {
            s.hoi4_dir = tactical_map::detect_hoi4_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
        }
        if s.saves_dir.is_empty() {
            s.saves_dir = default_saves_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
        }
        if s.log_path.is_empty() {
            s.log_path = tactical_listen::detect_log_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
        }
        s
    }

    /// Persist to settings.json (pretty JSON, UTF-8) — atomically: write a
    /// temp file, then rename over the target. A crash mid-write used to
    /// corrupt settings.json (load() then silently reset EVERYTHING to
    /// auto-detect defaults and the next save persisted the loss); a child
    /// reading during the write could also see a half file.
    pub fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let path = settings_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("write settings.json: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("replace settings.json: {e}"))
    }

    pub fn hoi4_dir(&self) -> Option<PathBuf> {
        let p = PathBuf::from(&self.hoi4_dir);
        p.join("map").join("provinces.bmp").is_file().then_some(p)
    }

    pub fn saves_dir(&self) -> Option<PathBuf> {
        let p = PathBuf::from(&self.saves_dir);
        p.is_dir().then_some(p)
    }

    pub fn log_path(&self) -> Option<PathBuf> {
        let p = PathBuf::from(&self.log_path);
        p.parent().map(|d| d.is_dir()).unwrap_or(false).then_some(p)
    }

    /// UI language from the raw settings string (blank/unknown →
    /// SimpChinese — the primary audience is Chinese-speaking).
    pub fn language(&self) -> tactical_locale::Language {
        tactical_locale::Language::from_code(&self.language)
    }

    /// Writeback mode from the raw settings string (blank/unknown →
    /// OrgStr). See [`tactical_sync::WritebackMode`].
    pub fn writeback_mode(&self) -> tactical_sync::WritebackMode {
        tactical_sync::WritebackMode::parse(&self.writeback)
    }

    /// MSAA sample count for the camera (1/2/4; anything else → 2, the
    /// default).
    pub fn msaa_samples(&self) -> u32 {
        match self.msaa {
            1 | 2 | 4 => self.msaa,
            _ => 2,
        }
    }

    /// Shadow quality level: 0 = off, 1 = low, 2 = high. Missing
    /// or out-of-range → LOW (1) (the legacy `shadow_map` migration to
    /// High is retired — an unchosen 2048 was the old default, not a
    /// preference).
    pub fn shadow_level(&self) -> u32 {
        match self.shadow {
            Some(v @ 0..=2) => v,
            _ => 1,
        }
    }

    /// Shadows on at all? (level > 0)
    pub fn shadows_enabled(&self) -> bool {
        self.shadow_level() > 0
    }

    /// The shadow map resolution implied by the level (off/low → 1024).
    pub fn shadow_level_map_size(&self) -> u32 {
        match self.shadow_level() {
            2 => 2048,
            _ => 1024,
        }
    }

    /// Frame-rate cap (0 = uncapped; 30/60/90/120/144 allowed, garbage → 60).
    pub fn max_fps(&self) -> u32 {
        match self.max_fps {
            0 | 30 | 60 | 90 | 120 | 144 => self.max_fps,
            _ => 60,
        }
    }

    /// Menu frame-rate cap (0 = uncapped; 15/24/30/60 allowed, garbage → 30).
    pub fn menu_fps(&self) -> u32 {
        match self.menu_fps {
            0 | 15 | 24 | 30 | 60 => self.menu_fps,
            _ => 30,
        }
    }

    /// Menu-only shadow-map clamp: the backdrop scene and its sun
    /// never move, so High (2048) only quadruples a fragment cost nobody can
    /// see — the menu renders shadows at Low (1024) at most. MSAA is NOT
    /// clamped (low MSAA moirés badly on the menu backdrop).
    pub fn menu_shadow_map_size(&self) -> u32 {
        self.shadow_level_map_size().min(1024)
    }

    /// Render-resolution scale percent: 100 / 85 / 70 / 50,
    /// anything else → 100 (native, offscreen path fully bypassed).
    pub fn render_scale_pct(&self) -> u32 {
        match self.render_scale {
            Some(v @ (100 | 85 | 70 | 50)) => v,
            _ => 100,
        }
    }

    /// Per-field validation for the Settings page: (field slug, ok, message
    /// slug). The page translates both through `menu.settings.field.*` /
    /// `settings.validate.*` locale keys (DESIGN §15) so the status lines
    /// follow the UI language; slugs stay stable English identifiers.
    pub fn validate(&self) -> Vec<(&'static str, bool, &'static str)> {
        vec![
            {
                let ok = self.hoi4_dir().is_some();
                (
                    "hoi4_dir",
                    ok,
                    if ok {
                        "provinces_found"
                    } else {
                        "provinces_missing"
                    },
                )
            },
            {
                let ok = self.saves_dir().is_some();
                (
                    "saves_dir",
                    ok,
                    if ok { "dir_exists" } else { "dir_missing" },
                )
            },
            {
                let p = PathBuf::from(&self.log_path);
                let ok = self.log_path().is_some();
                let msg = if p.is_file() {
                    "log_found"
                } else if ok {
                    "log_parent_exists"
                } else {
                    "log_parent_missing"
                };
                ("log_path", ok, msg)
            },
        ]
    }
}

// ---------------------------------------------------------------------------
// HOI4 save-format fixer (§14): tactical save parsing needs TEXT saves
// (`save_as_binary=no` in HOI4's own settings.txt). These helpers locate the
// file, probe the key, and rewrite it in place so the player does not have
// to hunt it down by hand.

/// Locate HOI4's `settings.txt`: normally a sibling of the saves dir
/// (`.../Hearts of Iron IV/settings.txt`); falls back to the default
/// Documents location. `None` when neither candidate exists.
pub fn detect_hoi4_settings_txt(saves_dir: &str) -> Option<PathBuf> {
    let sibling = PathBuf::from(saves_dir)
        .parent()
        .map(|p| p.join("settings.txt"));
    if let Some(p) = sibling.filter(|p| p.is_file()) {
        return Some(p);
    }
    let default = PathBuf::from(std::env::var("USERPROFILE").ok()?)
        .join("Documents")
        .join("Paradox Interactive")
        .join("Hearts of Iron IV")
        .join("settings.txt");
    default.is_file().then_some(default)
}

/// Probe the `save_as_binary` value in HOI4's settings.txt: `Some(true)` =
/// binary saves (tactical parsing fails), `Some(false)` = text saves,
/// `None` = key absent (HOI4 default is binary) or file unreadable.
pub fn read_save_as_binary(settings_txt: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(settings_txt).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "save_as_binary" {
            return Some(value.trim().trim_matches('"').eq_ignore_ascii_case("yes"));
        }
    }
    None
}

/// What `force_text_saves` did (drives the Settings page status line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSaveFix {
    /// Key was already `no` — nothing written.
    AlreadyText,
    /// An existing value (e.g. `yes`) was rewritten to `no`.
    Rewritten,
    /// Key was missing — appended at top level.
    Added,
}

/// Force `save_as_binary=no` in HOI4's settings.txt. Rewrites only the
/// value line(s), preserving the rest of the file (indentation, CRLF, BOM);
/// appends the key when absent. On the first modification a one-time backup
/// (`settings.txt.bak`) is kept next to the original. Writes nothing when
/// the file is already correct. NB: a running HOI4 overwrites settings.txt
/// on exit — the player must restart HOI4 for this to stick.
pub fn force_text_saves(settings_txt: &Path) -> Result<TextSaveFix, String> {
    let raw = std::fs::read_to_string(settings_txt)
        .map_err(|e| format!("read {}: {e}", settings_txt.display()))?;
    let (bom, text) = match raw.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", raw.as_str()),
    };
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut found = false;
    let mut changed = false;
    let mut out = String::with_capacity(raw.len() + 24);
    for line in text.lines() {
        let is_key = line
            .split_once('=')
            .map(|(k, _)| k.trim() == "save_as_binary")
            .unwrap_or(false);
        if is_key {
            found = true;
            let value = line
                .split_once('=')
                .map(|(_, v)| v.trim().trim_matches('"'))
                .unwrap_or("");
            if value.eq_ignore_ascii_case("no") {
                out.push_str(line);
            } else {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                out.push_str(&indent);
                out.push_str("save_as_binary=no");
                changed = true;
            }
        } else {
            out.push_str(line);
        }
        out.push_str(eol);
    }
    if !found {
        out.push_str("save_as_binary=no");
        out.push_str(eol);
        changed = true;
    }
    if !changed {
        return Ok(TextSaveFix::AlreadyText);
    }
    // One-time safety net: keep the pre-fix original as settings.txt.bak.
    let backup = PathBuf::from(format!("{}.bak", settings_txt.display()));
    if !backup.exists() {
        let _ = std::fs::copy(settings_txt, &backup);
    }
    let mut final_text = String::with_capacity(out.len() + bom.len());
    final_text.push_str(bom);
    final_text.push_str(&out);
    std::fs::write(settings_txt, final_text)
        .map_err(|e| format!("write {}: {e}", settings_txt.display()))?;
    Ok(if found {
        TextSaveFix::Rewritten
    } else {
        TextSaveFix::Added
    })
}

/// `Documents/Paradox Interactive/Hearts of Iron IV/save games`.
fn default_saves_dir() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var("USERPROFILE").ok()?)
            .join("Documents")
            .join("Paradox Interactive")
            .join("Hearts of Iron IV")
            .join("save games"),
    )
}

/// Newest `.hoi4` file in the configured saves dir (shared by live mode and
/// the from-save scenario source).
pub fn newest_save_in(saves_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(saves_dir).ok()? {
        // A single unreadable entry must not abort the whole scan.
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().map(|e| e == "hoi4").unwrap_or(false) {
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                if best.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
                    best = Some((path, mtime));
                }
            }
        }
    }
    best.map(|(p, _)| p)
}

/// List `.hoi4` files in the saves dir, newest first (menu dropdown).
pub fn list_saves(saves_dir: &Path, cap: usize) -> Vec<PathBuf> {
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(saves_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "hoi4").unwrap_or(false) {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                files.push((path, mtime));
            }
        }
    }
    files.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    files.into_iter().take(cap).map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_and_missing_fields_default() {
        let s = AppSettings {
            hoi4_dir: "D:/HOI4".into(),
            saves_dir: String::new(),
            log_path: "C:/logs/game.log".into(),
            language: "zh".into(),
            msaa: 2,
            shadow_map: 1024,
            shadow: Some(2),
            max_fps: 90,
            menu_fps: 24,
            low_power: false,
            writeback: "org_str".into(),
            render_scale: Some(70),
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.hoi4_dir, "D:/HOI4");
        assert_eq!(back.saves_dir, "");
        assert_eq!(back.language, "zh");
        assert_eq!(back.msaa, 2);
        assert_eq!(back.shadow_map, 1024);
        assert!(!back.low_power);
        assert_eq!(back.max_fps, 90);
        assert_eq!(back.menu_fps, 24);
        assert_eq!(back.shadow_level(), 2);
        assert_eq!(back.writeback, "org_str");
        assert_eq!(back.render_scale_pct(), 70);
        assert_eq!(back.writeback_mode(), tactical_sync::WritebackMode::OrgStr);
        // The retired "org_only" token maps to the
        // default OrgStr mode (the org-only mode was removed).
        let legacy: AppSettings =
            serde_json::from_str(r#"{"hoi4_dir":"X","writeback":"org_only"}"#).unwrap();
        assert_eq!(
            legacy.writeback_mode(),
            tactical_sync::WritebackMode::OrgStr
        );
        // Older files missing whole fields still parse (serde(default)).
        let partial: AppSettings = serde_json::from_str(r#"{"hoi4_dir":"X"}"#).unwrap();
        assert_eq!(partial.hoi4_dir, "X");
        assert_eq!(partial.log_path, "");
        assert_eq!(partial.language, "");
        // Blank/missing language → SimpChinese (CN-audience default).
        assert_eq!(partial.language(), tactical_locale::Language::SimpChinese);
        // Missing/blank writeback → the default OrgStr mode.
        assert_eq!(
            partial.writeback_mode(),
            tactical_sync::WritebackMode::OrgStr
        );
        // Render-quality fields: low_power defaults on; MSAA
        // defaults 2 (the far-zoom moiré concern with 2× is covered by
        // the always-on FXAA pass on the camera).
        assert_eq!(partial.msaa_samples(), 2);
        assert!(partial.low_power);
        // No shadow key → LOW (the legacy
        // shadow_map→High migration is retired); max_fps defaults 60,
        // garbage normalizes to 60.
        assert_eq!(partial.shadow_level(), 1);
        assert!(partial.shadows_enabled());
        assert_eq!(partial.max_fps(), 60);
        // Menu cap defaults 30; the menu shadow clamp only ever
        // LOWERS High → Low (off/low stay 1024).
        assert_eq!(partial.menu_fps(), 30);
        assert_eq!(partial.menu_shadow_map_size(), 1024);
        // Missing/invalid render scale → 100 (native, no offscreen).
        assert_eq!(partial.render_scale_pct(), 100);
        let scaled: AppSettings = serde_json::from_str(r#"{"render_scale":85}"#).unwrap();
        assert_eq!(scaled.render_scale_pct(), 85);
        let bad_scale: AppSettings = serde_json::from_str(r#"{"render_scale":73}"#).unwrap();
        assert_eq!(bad_scale.render_scale_pct(), 100);
        let legacy_low: AppSettings = serde_json::from_str(r#"{"shadow_map":1024}"#).unwrap();
        assert_eq!(legacy_low.shadow_level(), 1);
        assert_eq!(legacy_low.shadow_level_map_size(), 1024);
        let off: AppSettings = serde_json::from_str(r#"{"shadow":0}"#).unwrap();
        assert!(!off.shadows_enabled());
        // Garbage values normalize to the safe defaults.
        let junk: AppSettings = serde_json::from_str(
            r#"{"msaa":7,"shadow_map":42,"max_fps":55,"menu_fps":55,"shadow":9}"#,
        )
        .unwrap();
        assert_eq!(junk.msaa_samples(), 2);
        assert_eq!(junk.max_fps(), 60);
        assert_eq!(junk.menu_fps(), 30);
        assert_eq!(junk.shadow_level(), 1);
    }

    #[test]
    fn fresh_install_default_matches_serde_defaults() {
        // A MISSING settings.json must not read as zero-values —
        // AppSettings::default() delegates to the serde defaults (a
        // derived zero-default would show both fps caps as "unlimited",
        // low_power off, on a fresh install's first launch).
        let s = AppSettings::default();
        assert_eq!(s.max_fps(), 60);
        assert_eq!(s.menu_fps(), 30);
        assert_eq!(s.msaa_samples(), 2);
        assert!(s.low_power);
        assert_eq!(s.shadow_level(), 1);
        assert_eq!(s.language(), tactical_locale::Language::SimpChinese);
        assert_eq!(s.writeback_mode(), tactical_sync::WritebackMode::OrgStr);
        assert_eq!(s.render_scale_pct(), 100);
    }

    #[test]
    fn list_saves_empty_dir_is_empty() {
        assert!(list_saves(Path::new("no/such/dir"), 10).is_empty());
        assert!(newest_save_in(Path::new("no/such/dir")).is_none());
    }

    /// Unique temp settings.txt per test case (tests run in parallel).
    fn temp_settings_txt(tag: &str, content: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fc_settings_test_{}_{tag}.txt",
            std::process::id()
        ));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn force_text_saves_rewrites_yes_and_preserves_rest() {
        let p = temp_settings_txt(
            "rewrite",
            "\"Video settings\"\r\n{\r\n\tfullScreen=yes\r\n}\r\nsave_as_binary=yes\r\n",
        );
        assert_eq!(force_text_saves(&p).unwrap(), TextSaveFix::Rewritten);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("save_as_binary=no\r\n"),
            "CRLF kept: {text:?}"
        );
        assert!(!text.contains("save_as_binary=yes"));
        assert!(text.contains("\tfullScreen=yes"), "other lines untouched");
        // A one-time backup of the pre-fix original was left behind.
        let bak = PathBuf::from(format!("{}.bak", p.display()));
        assert!(std::fs::read_to_string(&bak)
            .unwrap()
            .contains("save_as_binary=yes"));
        // Idempotent: second run reports AlreadyText and writes nothing.
        assert_eq!(force_text_saves(&p).unwrap(), TextSaveFix::AlreadyText);
        assert_eq!(read_save_as_binary(&p), Some(false));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&bak);
    }

    #[test]
    fn force_text_saves_quoted_yes_and_missing_key() {
        // Quoted value also flips.
        let p = temp_settings_txt("quoted", "save_as_binary=\"yes\"\n");
        assert_eq!(force_text_saves(&p).unwrap(), TextSaveFix::Rewritten);
        assert_eq!(read_save_as_binary(&p), Some(false));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.bak", p.display())));
        let _ = std::fs::remove_file(&p);

        // Key missing entirely → appended at top level, content preserved.
        let p = temp_settings_txt("append", "fullScreen=yes\nlastVersion=\"1.14\"\n");
        assert_eq!(force_text_saves(&p).unwrap(), TextSaveFix::Added);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("fullScreen=yes\n"));
        assert!(text.ends_with("save_as_binary=no\n"));
        assert_eq!(read_save_as_binary(&p), Some(false));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.bak", p.display())));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_save_as_binary_parsing() {
        let p = temp_settings_txt("probe", "a=1\nsave_as_binary=YES\nb=2\n");
        assert_eq!(read_save_as_binary(&p), Some(true));
        let _ = std::fs::remove_file(&p);
        assert_eq!(read_save_as_binary(Path::new("no/such/file.txt")), None);
        let p = temp_settings_txt("probe2", "a=1\n");
        assert_eq!(read_save_as_binary(&p), None);
        let _ = std::fs::remove_file(&p);
    }
}
