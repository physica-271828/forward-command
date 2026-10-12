//! Persistent user settings: HOI4 install dir, saves dir and
//! game.log path, editable from the main-menu Settings page. Stored as
//! `settings.json` under the runtime root (next to the exe in shipped
//! packages, workspace root in dev — see [`crate::dirs`]); every field
//! falls back to the auto-detected value when missing/blank.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tactical_inject::Injector;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadWarning {
    /// an existing settings.json could not be parsed and was renamed aside.
    Corrupt {
        error: String,
        backup: Option<String>,
    },
}

/// Load the settings for the resident menu: [`AppSettings::load_checked`]
/// plus the localized status line when the file had to be set aside.
pub fn load_for_menu() -> (AppSettings, Option<String>) {
    let (settings, warning) = AppSettings::load_checked();
    let loc = tactical_locale::Locale::load(settings.language());
    let line = warning.map(|w| match w {
        LoadWarning::Corrupt { error, backup } => match backup {
            Some(name) => loc.trf(
                "menu.status.settings_reset",
                &[("error", &error), ("backup", &name)],
            ),
            None => loc.trf("menu.status.settings_reset_nobackup", &[("error", &error)]),
        },
    });
    (settings, line)
}
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
    /// UI language: "en" / "zh" / "ru" (unknown/blank → SimpChinese — the
    /// primary audience is Chinese-speaking). Switched on the Settings page;
    /// battle child processes read it at startup (DESIGN §15).
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
    /// Manual-command retention (DESIGN §7.4): what happens to a
    /// manually commanded battalion once its command completes.
    /// `false` (default) = the override expires and the battalion returns
    /// to its division order; `true` = the battalion stays manual until
    /// the player stands it by (right-click). Read by each battle child
    /// at startup from settings.json.
    #[serde(default)]
    pub keep_manual_override: bool,
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

/// Parse one settings file. An unparsable file is moved aside —
/// `settings.json` → `settings.json.bad` — so the hand-edited original
/// survives the next `save()` (which would otherwise persist the defaults
/// over it) and the caller can name the file in its status line.
fn read_settings_file(path: &Path) -> (AppSettings, Option<LoadWarning>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return (AppSettings::default(), None),
    };
    match serde_json::from_str(&text) {
        Ok(s) => (s, None),
        Err(e) => {
            let backup = path.with_extension("json.bad");
            let moved = std::fs::rename(path, &backup).is_ok();
            let name = backup
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (
                AppSettings::default(),
                Some(LoadWarning::Corrupt {
                    error: e.to_string(),
                    backup: moved.then_some(name),
                }),
            )
        }
    }
}

impl AppSettings {
    /// Load from settings.json; missing file/fields fall back to
    /// auto-detected defaults (so a fresh install "just works"). A
    /// MISSING file (true first launch) also picks the interface
    /// language from the OS — Chinese systems get Simplified Chinese,
    /// everything else English — instead of shipping every new player
    /// into the Chinese UI. An existing file keeps its stored language
    /// untouched (older files without the key still fall back to
    /// Simplified Chinese via [`Self::language`]).
    ///
    /// A file that fails to PARSE is moved aside ([`LoadWarning::Corrupt`])
    /// instead of being silently replaced by the defaults: the caller shows
    /// the line, and the hand-edited original survives the next `save()`.
    pub fn load() -> Self {
        Self::load_checked().0
    }

    /// [`Self::load`] plus the file's condition: `Some(..)` when an existing
    /// settings.json could not be parsed and was renamed aside.
    pub fn load_checked() -> (Self, Option<LoadWarning>) {
        let fresh = !settings_file_exists();
        let (mut s, warning) = read_settings_file(&settings_path());
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
        if fresh {
            s.language = detect_first_run_language().settings_tag().to_string();
        }
        (s, warning)
    }

    /// Store a chosen interface language's settings tag (pure field
    /// write — unit-testable without touching settings.json).
    pub fn set_language_tag(&mut self, lang: tactical_locale::Language) {
        self.language = lang.settings_tag().to_string();
    }

    /// Commit a chosen interface language: store its tag and persist
    /// immediately. Shared by the first-launch picker's confirm and the
    /// settings row — one write path.
    pub fn commit_language(&mut self, lang: tactical_locale::Language) -> Result<(), String> {
        self.set_language_tag(lang);
        self.save()
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

    /// Per-field validation for the Settings page: (field slug, level,
    /// message slug). The page translates both through
    /// `menu.settings.field.*` / `settings.validate.*` locale keys
    /// (DESIGN §15) so the status lines follow the UI language; slugs stay
    /// stable English identifiers.
    pub fn validate(&self) -> Vec<(&'static str, CheckLevel, &'static str)> {
        let mut checks = vec![
            {
                let ok = self.hoi4_dir().is_some();
                (
                    "hoi4_dir",
                    level(ok),
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
                    level(ok),
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
                ("log_path", level(ok), msg)
            },
        ];
        // Cross-field warning: the saves dir's parent and the game.log path
        // must imply the SAME HOI4 user directory. The console batch file
        // and the snapshot round trip both derive theirs from the saves
        // dir, so a saves dir one level off (the user directory itself,
        // missing the `save games` tail) passes its own check above and
        // then fails everything in the field — silently. Never an error:
        // the player must stay free to save and play.
        if let (Some(from_saves), Some(from_log)) = (
            user_dir_from_saves(&self.saves_dir),
            user_dir_from_log(&self.log_path),
        ) {
            if !same_dir(&from_saves, &from_log) {
                checks.push(("saves_dir", CheckLevel::Warn, "saves_user_dir_mismatch"));
            }
        }
        checks
    }
}

/// Severity of one Settings-page validation line — green, yellow, red. A
/// WARN is a configuration that passes its own field check but fails in
/// the field (see [`AppSettings::validate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    Ok,
    Warn,
    Error,
}

fn level(ok: bool) -> CheckLevel {
    if ok {
        CheckLevel::Ok
    } else {
        CheckLevel::Error
    }
}

/// Lexical parent of a configured path string; a blank string has none.
/// One trailing separator is dropped first — `Path::parent` reads it as a
/// final empty component and would hand back the directory itself (a
/// pasted `…\save games\` must not look like a mismatch). A bare drive
/// keeps its separator: `C:` alone is a RELATIVE path.
fn parent_of(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let dir = match trimmed.chars().last() {
        Some('\\') | Some('/') if trimmed.len() > 3 => &trimmed[..trimmed.len() - 1],
        _ => trimmed,
    };
    PathBuf::from(dir).parent().map(Path::to_path_buf)
}

/// HOI4 user directory implied by the configured saves dir (its parent) —
/// the derivation the console-injection channel actually uses.
fn user_dir_from_saves(saves_dir: &str) -> Option<PathBuf> {
    parent_of(saves_dir)
}

/// HOI4 user directory implied by the configured `game.log` path
/// (`<user dir>/logs/game.log`).
fn user_dir_from_log(log_path: &str) -> Option<PathBuf> {
    parent_of(log_path).and_then(|logs| logs.parent().map(Path::to_path_buf))
}

/// Windows-equality for two directory paths: separator style unified, one
/// trailing separator dropped, case folded — the settings fields are free
/// text and Windows paths are case-insensitive.
fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('/', "\\");
        s.strip_suffix('\\').unwrap_or(&s).to_lowercase()
    };
    norm(a) == norm(b)
}

// ---------------------------------------------------------------------------
// Console-channel injection targets (§3.2): the batch file and the ping
// readback must live in the HOI4 user directory the game actually uses.

/// HOI4 user directory + `game.log` for the menu/live console channel,
/// derived from the configured paths: the user directory is the save dir's
/// parent, the log path falls back to the user dir's `logs\game.log` when
/// unset. `None` when the save dir is unset or pathless.
fn user_dir_injection_paths(
    saves_dir: Option<&Path>,
    log_path: Option<&Path>,
) -> Option<(PathBuf, PathBuf)> {
    let dir = saves_dir?.parent()?.to_path_buf();
    let log = log_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dir.join("logs").join("game.log"));
    Some((dir, log))
}

/// Injector for the menu/live console channel (snapshot, abort, quiet
/// notify). Unlike [`tactical_inject::Injector::new`], both paths come from
/// the settings — the built-in default assumes the English
/// `%USERPROFILE%\Documents`, which does not exist where Documents is
/// redirected (OneDrive) or localized (e.g. Turkish `Belgeler`), so every
/// injection there fails before reaching the game.
pub fn live_channel_injector(saves_dir: Option<&Path>, log_path: Option<&Path>) -> Injector {
    match user_dir_injection_paths(saves_dir, log_path) {
        Some((dir, log)) => Injector::for_user_dir(dir, Some(log)),
        None => Injector::new(),
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

// ---------------------------------------------------------------------------
// First-launch interface language (§12.1): no settings.json exists yet, so
// the OS UI language decides instead of defaulting everyone to Chinese.
// ---------------------------------------------------------------------------

/// The Win32 LANGID's primary language field (low ten bits) — Chinese is
/// 0x04 in every sublanguage (Simplified, Traditional, Hong Kong…);
/// Russian is 0x19.
const LANG_PRIMARY_MASK: u16 = 0x3FF;
const LANG_CHINESE: u16 = 0x04;
const LANG_RUSSIAN: u16 = 0x19;

/// Interface language for a first launch: the OS UI language when it can
/// be read, Simplified Chinese otherwise (the historical fallback — and
/// the only shipped Chinese table, so every Chinese sublanguage maps
/// here). Russian systems get Russian; other non-Chinese systems get
/// English.
pub fn detect_first_run_language() -> tactical_locale::Language {
    first_run_language(os_ui_language())
}

/// Pure mapping (unit-tested): an optional OS LANGID → interface
/// language. Chinese in any sublanguage → Simplified Chinese; Russian in
/// any sublanguage → Russian; any other readable language → English;
/// `None` (detection failed) → Simplified Chinese, the historical
/// default.
fn first_run_language(langid: Option<u16>) -> tactical_locale::Language {
    match langid {
        Some(id) if id & LANG_PRIMARY_MASK == LANG_CHINESE => {
            tactical_locale::Language::SimpChinese
        }
        Some(id) if id & LANG_PRIMARY_MASK == LANG_RUSSIAN => {
            tactical_locale::Language::Russian
        }
        Some(_) => tactical_locale::Language::English,
        None => tactical_locale::Language::SimpChinese,
    }
}

/// Best-effort OS UI language, LANGID form: the per-user UI language,
/// then the system UI language, then the user locale. Each API returns 0
/// when the value is unavailable/unsupported — treat it as "unknown" and
/// fall through. The Win32 calls are isolated in this thin shell so the
/// mapping above stays unit-testable without a Windows API surface.
fn os_ui_language() -> Option<u16> {
    unsafe {
        let user = windows::Win32::Globalization::GetUserDefaultUILanguage();
        if user != 0 {
            return Some(user);
        }
        let system = windows::Win32::Globalization::GetSystemDefaultUILanguage();
        if system != 0 {
            return Some(system);
        }
        let locale = windows::Win32::Globalization::GetUserDefaultLangID();
        if locale != 0 {
            return Some(locale);
        }
    }
    None
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
    fn corrupt_settings_file_is_set_aside_not_overwritten() {
        // A hand-edit typo used to reset every field silently and let the
        // next save() persist that loss over the player's file. The bad
        // file must survive as `<name>.bad`, and the caller must get a
        // warning it can show.
        let dir = std::env::temp_dir().join(format!("fc_settings_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ \"hoi4_dir\": ").unwrap();
        let (s, warning) = read_settings_file(&path);
        let LoadWarning::Corrupt { backup, .. } = warning.expect("a corrupt file warns");
        assert_eq!(backup.as_deref(), Some("settings.json.bad"));
        assert!(!path.exists(), "the bad file must not stay in place");
        let moved = std::fs::read_to_string(dir.join("settings.json.bad")).unwrap();
        assert!(moved.contains("hoi4_dir"), "the original survives verbatim");
        // Defaults (auto-detected fields fill in later, in load_checked).
        assert!(s.hoi4_dir.is_empty());
        // A good file loads silently.
        std::fs::write(&path, "{\"hoi4_dir\":\"D:/HOI4\"}").unwrap();
        let (good, warning) = read_settings_file(&path);
        assert!(warning.is_none());
        assert_eq!(good.hoi4_dir, "D:/HOI4");
        // A missing file is not a corruption either.
        let (_, warning) = read_settings_file(&dir.join("nope.json"));
        assert!(warning.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

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
            keep_manual_override: true,
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
        assert!(back.keep_manual_override, "roundtrip keeps the flag");
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
        // Missing keep_manual_override → false (the default hand-back).
        assert!(!partial.keep_manual_override);
        let keep: AppSettings =
            serde_json::from_str(r#"{"keep_manual_override":true}"#).unwrap();
        assert!(keep.keep_manual_override);
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
        assert!(!s.keep_manual_override, "fresh install hands commands back");
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

    #[test]
    fn first_run_language_mapping_covers_os_langids() {
        use tactical_locale::Language;
        // Any Chinese sublanguage → Simplified Chinese (zh-CN 0x0804,
        // zh-TW 0x0404, zh-HK 0x0c04 — primary 0x04 throughout).
        assert_eq!(first_run_language(Some(0x0804)), Language::SimpChinese);
        assert_eq!(first_run_language(Some(0x0404)), Language::SimpChinese);
        assert_eq!(first_run_language(Some(0x0c04)), Language::SimpChinese);
        // Russian in any sublanguage → Russian (ru-RU 0x0419, ru-MD 0x0819).
        assert_eq!(first_run_language(Some(0x0419)), Language::Russian);
        assert_eq!(first_run_language(Some(0x0819)), Language::Russian);
        // Every other system → English (en-US 0x0409, ja-JP 0x0411,
        // de-DE 0x0407).
        assert_eq!(first_run_language(Some(0x0409)), Language::English);
        assert_eq!(first_run_language(Some(0x0411)), Language::English);
        assert_eq!(first_run_language(Some(0x0407)), Language::English);
        // Detection failure → Simplified Chinese (the historical default).
        assert_eq!(first_run_language(None), Language::SimpChinese);
    }

    #[test]
    fn language_tag_write_roundtrips_the_chosen_language() {
        // The picker bug: choosing English in the dropdown must stick —
        // the confirm path writes the tag and the accessor reads it back.
        // (Pure field path only — commit_language's save() targets the
        // real runtime settings.json and must never run in tests.)
        use tactical_locale::Language;
        let mut s = AppSettings::default();
        s.set_language_tag(Language::English);
        assert_eq!(s.language, "en");
        assert_eq!(s.language(), Language::English);
        s.set_language_tag(Language::SimpChinese);
        assert_eq!(s.language(), Language::SimpChinese);
        assert_eq!(s.language(), Language::from_code(&s.language));
    }

    #[test]
    fn injection_paths_derive_the_user_dir_from_the_save_dir() {
        let saves = PathBuf::from(
            r"C:\Users\X\OneDrive\Documents\Paradox Interactive\Hearts of Iron IV\save games",
        );
        let (dir, log) = user_dir_injection_paths(Some(&saves), None).unwrap();
        assert_eq!(
            dir,
            PathBuf::from(r"C:\Users\X\OneDrive\Documents\Paradox Interactive\Hearts of Iron IV")
        );
        assert_eq!(log, dir.join("logs").join("game.log"));
    }

    #[test]
    fn injection_paths_prefer_the_configured_log() {
        let saves = PathBuf::from(r"C:\HOI4\save games");
        let log = PathBuf::from(r"D:\logs\game.log");
        assert_eq!(
            user_dir_injection_paths(Some(&saves), Some(&log)),
            Some((PathBuf::from(r"C:\HOI4"), log))
        );
        // Unset or pathless save dir → the caller falls back to the
        // built-in default injector.
        assert!(user_dir_injection_paths(None, None).is_none());
        assert!(user_dir_injection_paths(Some(Path::new("")), None).is_none());
    }

    /// Temp HOI4 user-dir tree `<root>` with the real child layout: a
    /// `save games` folder and a `logs\game.log` file.
    fn temp_user_dir(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "fc_settings_userdir_{}_{tag}",
            std::process::id()
        ));
        let saves = root.join("save games");
        std::fs::create_dir_all(&saves).unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs").join("game.log"), "").unwrap();
        (root, saves)
    }

    fn has_warn(s: &AppSettings) -> bool {
        s.validate().iter().any(|(_, l, _)| *l == CheckLevel::Warn)
    }

    /// The misconfiguration seen live (2026-09-21 group report): the saves
    /// dir pointing at the HOI4 user directory itself, one level above
    /// `save games`. The console batch file AND the snapshot both derive
    /// their user dir from it, so the whole live channel fails silently —
    /// the page must say so, as a WARNING (the player stays free to save).
    #[test]
    fn validate_warns_when_saves_dir_sits_one_level_above_save_games() {
        let (root, _) = temp_user_dir("warn");
        let s = AppSettings {
            saves_dir: root.display().to_string(),
            log_path: root.join("logs").join("game.log").display().to_string(),
            ..Default::default()
        };
        let checks = s.validate();
        assert!(
            checks.iter().any(|(field, level, msg)| *field == "saves_dir"
                && *level == CheckLevel::Warn
                && *msg == "saves_user_dir_mismatch"),
            "a one-level-off saves dir must warn: {checks:?}"
        );
        // The field's own line stays green — the directory DOES exist; the
        // warning rides alongside it.
        assert!(checks
            .iter()
            .any(|(f, l, m)| *f == "saves_dir" && *l == CheckLevel::Ok && *m == "dir_exists"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn validate_stays_quiet_on_a_matching_layout() {
        let (root, saves) = temp_user_dir("quiet");
        let log = root.join("logs").join("game.log").display().to_string();
        let ok = AppSettings {
            saves_dir: saves.display().to_string(),
            log_path: log.clone(),
            ..Default::default()
        };
        assert!(!has_warn(&ok), "{:?}", ok.validate());
        // Free-text tolerance: forward slashes, upper case and one trailing
        // separator all describe the same two directories.
        let loose = AppSettings {
            saves_dir: format!("{}\\", saves.display().to_string().replace('\\', "/")),
            log_path: log.replace('\\', "/").to_uppercase(),
            ..Default::default()
        };
        assert!(!has_warn(&loose), "{:?}", loose.validate());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn validate_mismatch_check_needs_both_paths() {
        // Nothing to compare with a blank log path or a blank saves dir —
        // the check stays silent instead of guessing.
        let no_log = AppSettings {
            saves_dir: r"C:\HOI4\save games".into(),
            ..Default::default()
        };
        assert!(!has_warn(&no_log));
        let no_saves = AppSettings {
            log_path: r"C:\HOI4\logs\game.log".into(),
            ..Default::default()
        };
        assert!(!has_warn(&no_saves));
    }
}
