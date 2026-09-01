//! tactical-locale — HOI4-style key-value localization (DESIGN.md §15).
//!
//! UI strings live in `localisation/<language>/*.yml` files using the HOI4
//! modding format:
//!
//! ```text
//! l_english:
//!  menu.main.start_live:0 "Start Live Listen"
//!  log.move.eta:0 "{name} → ({q}, {r}) — ETA {n} turn(s)"   # comment
//! ```
//!
//! - Both shipping languages are ALSO embedded via `include_str!`, so the exe
//!   works standalone; files in the external `localisation/` dir override the
//!   embedded values key-by-key (alphabetical file order, later wins) — this
//!   is the user customization / third-language entry point.
//! - Lookup falls back zh → en → the raw key itself (HOI4 shows the key on a
//!   miss too; it makes missing translations visible during development).
//! - Dynamic values use `{name}` placeholders via [`Locale::trf`] instead of
//!   positional `format!` args, because word order differs across languages.
//!
//! Zero external dependencies; the parser is hand-rolled and tolerant
//! (missing header, missing version number, BOM, stray blank lines all pass).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Supported UI languages. The `code()` names match the folder names under
/// `localisation/` (HOI4 convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Language {
    #[default]
    English,
    SimpChinese,
}

impl Language {
    pub const ALL: &'static [Language] = &[Language::English, Language::SimpChinese];

    /// Folder / `l_<code>:` header name (HOI4 convention).
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "english",
            Language::SimpChinese => "simp_chinese",
        }
    }

    /// Display name for the Settings selector (endonym: a language names
    /// itself so every user can find their own).
    pub fn display_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::SimpChinese => "中文",
        }
    }

    /// Parse a settings.json `language` value; unknown/empty → SimpChinese
    /// (the primary user base is Chinese-speaking). Explicit "en" still
    /// selects English.
    pub fn from_code(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "en" | "english" => Language::English,
            _ => Language::SimpChinese,
        }
    }

    /// Short tag stored in settings.json.
    pub fn settings_tag(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::SimpChinese => "zh",
        }
    }
}

// ---------------------------------------------------------------------------
// Embedded defaults (compiled in — standalone exe works without localisation/)

/// Embedded file set per language, loaded in order (later keys win).
const EMBEDDED_EN: &[&str] = &[
    include_str!("../../../localisation/english/00-core_l_english.yml"),
    include_str!("../../../localisation/english/10-battle-ui_l_english.yml"),
    include_str!("../../../localisation/english/20-battle-log_l_english.yml"),
    include_str!("../../../localisation/english/30-names_l_english.yml"),
];

const EMBEDDED_ZH: &[&str] = &[
    include_str!("../../../localisation/simp_chinese/00-core_l_simp_chinese.yml"),
    include_str!("../../../localisation/simp_chinese/10-battle-ui_l_simp_chinese.yml"),
    include_str!("../../../localisation/simp_chinese/20-battle-log_l_simp_chinese.yml"),
    include_str!("../../../localisation/simp_chinese/30-names_l_simp_chinese.yml"),
];

/// The `localisation/` override dir: next to the exe in shipped packages
/// (recognized by the sibling `data/` dir), else under the compile-time
/// workspace root (dev layout).
fn default_localisation_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("data").is_dir() {
                return dir.join("localisation");
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("localisation")
}

// ---------------------------------------------------------------------------
// HOI4-style file parser

/// Parse one HOI4-style localisation file into the map (later keys override).
/// Tolerant: BOM stripped, `l_<code>:` header optional, `#` comments and
/// blank lines skipped, unparseable lines ignored. Inside quotes, `\n`,
/// `\"` and `\\` are unescaped.
pub fn parse_into(map: &mut HashMap<String, String>, text: &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Header line: `l_english:` / `l_simp_chinese:` (no quotes).
        if line.ends_with(':') && !line.contains('"') {
            continue;
        }
        // key[:version] "value" — the version number is accepted but ignored
        // (load order, not versions, decides overrides here).
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let Some(start) = rest.find('"') else {
            continue;
        };
        let mut value = String::with_capacity(rest.len() - start);
        let mut chars = rest[start + 1..].chars();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some('n') => value.push('\n'),
                    Some('"') => value.push('"'),
                    Some('\\') => value.push('\\'),
                    Some(other) => {
                        value.push('\\');
                        value.push(other);
                    }
                    None => value.push('\\'),
                },
                '"' => {
                    closed = true;
                    break;
                }
                _ => value.push(c),
            }
        }
        if closed {
            map.insert(key.to_string(), value);
        }
    }
}

/// Convenience: parse a full file into a fresh map.
pub fn parse(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    parse_into(&mut map, text);
    map
}

// ---------------------------------------------------------------------------
// Locale

/// Runtime string table: selected language + English fallback.
#[derive(Debug, Clone)]
pub struct Locale {
    lang: Language,
    primary: HashMap<String, String>,
    fallback: HashMap<String, String>,
}

impl Locale {
    /// Load `lang`: embedded defaults first, then external `localisation/`
    /// override files (alphabetical, later wins). The English fallback map is
    /// loaded the same way so external files can also FIX the English text.
    pub fn load(lang: Language) -> Self {
        Self::load_from(lang, &default_localisation_dir())
    }

    /// Test hook / custom roots: same as [`Locale::load`] with an explicit
    /// override directory (missing dir is not an error).
    pub fn load_from(lang: Language, dir: &Path) -> Self {
        let mut primary = HashMap::new();
        for text in embedded(lang) {
            parse_into(&mut primary, text);
        }
        load_override_dir(&mut primary, &dir.join(lang.code()));
        let mut fallback = HashMap::new();
        if lang != Language::English {
            for text in embedded(Language::English) {
                parse_into(&mut fallback, text);
            }
            load_override_dir(&mut fallback, &dir.join(Language::English.code()));
        }
        Locale {
            lang,
            primary,
            fallback,
        }
    }

    /// Build directly from two source texts (unit tests).
    pub fn from_text(lang: Language, primary: &str, fallback_en: &str) -> Self {
        Locale {
            lang,
            primary: parse(primary),
            fallback: parse(fallback_en),
        }
    }

    pub fn language(&self) -> Language {
        self.lang
    }

    /// Number of keys in the active language table (tests/diagnostics).
    pub fn len(&self) -> usize {
        self.primary.len()
    }

    pub fn is_empty(&self) -> bool {
        self.primary.is_empty()
    }

    /// Translate `key`: active language → English fallback → the key itself
    /// (HOI4-style visible miss marker; the miss case allocates).
    pub fn tr<'a>(&'a self, key: &str) -> Cow<'a, str> {
        if let Some(v) = self.primary.get(key) {
            return Cow::Borrowed(v.as_str());
        }
        if let Some(v) = self.fallback.get(key) {
            return Cow::Borrowed(v.as_str());
        }
        Cow::Owned(key.to_owned())
    }

    /// Translate + substitute `{name}` placeholders, e.g.
    /// `trf("log.move.eta", &[("name", "2.Pz"), ("n", "3")])`.
    /// Unknown placeholders are left untouched.
    pub fn trf(&self, key: &str, args: &[(&str, &str)]) -> String {
        let mut out = self.tr(key).into_owned();
        for (name, value) in args {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }
}

fn embedded(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::English => EMBEDDED_EN,
        Language::SimpChinese => EMBEDDED_ZH,
    }
}

/// Merge every `*.yml` in `dir` into `map`, alphabetical file order, later
/// keys winning (HOI4 override semantics). Unreadable files warn + skip.
fn load_override_dir(map: &mut HashMap<String, String>, dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "yml").unwrap_or(false))
        .collect();
    files.sort();
    for f in files {
        match std::fs::read_to_string(&f) {
            Ok(text) => parse_into(map, &text),
            Err(e) => eprintln!("[locale] skipping {}: {e}", f.display()),
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_basics() {
        let text = "\u{feff}l_english:\n\
                    # a comment\n\
                    menu.main.exit:0 \"Exit Game\"\n\
                    plain:1 \"no version works too: {n}\"\n\
                    \n\
                    dup:0 \"first\"\n\
                    dup:0 \"second\"\n";
        let map = parse(text);
        assert_eq!(map["menu.main.exit"], "Exit Game");
        assert_eq!(map["plain"], "no version works too: {n}");
        assert_eq!(map["dup"], "second", "later key wins");
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn parser_escapes_and_trailing_comment() {
        let map = parse("l_english:\n a:0 \"line\\nbreak\" # tail\n b:0 \"say \\\"hi\\\"\"\n");
        assert_eq!(map["a"], "line\nbreak");
        assert_eq!(map["b"], "say \"hi\"");
    }

    #[test]
    fn parser_skips_garbage_lines() {
        let map = parse("l_english:\nnot a kv line\nnokey: \n:0 \"no key\"\nok:0 \"fine\"\n");
        assert_eq!(map.len(), 1);
        assert_eq!(map["ok"], "fine");
    }

    #[test]
    fn fallback_chain_and_missing_key() {
        let loc = Locale::from_text(
            Language::SimpChinese,
            "l_simp_chinese:\n a:0 \"甲\"\n",
            "l_english:\n a:0 \"A\"\n b:0 \"B\"\n",
        );
        assert_eq!(loc.tr("a"), "甲");
        assert_eq!(loc.tr("b"), "B", "zh miss falls back to English");
        assert_eq!(loc.tr("zzz"), "zzz", "total miss returns the key");
    }

    #[test]
    fn trf_substitutes_placeholders() {
        let loc = Locale::from_text(
            Language::English,
            "l_english:\n m:0 \"{name} → ({q}, {r}) — ETA {n} turn(s)\"\n",
            "",
        );
        assert_eq!(
            loc.trf(
                "m",
                &[("name", "2.Pz"), ("q", "3"), ("r", "-1"), ("n", "2")]
            ),
            "2.Pz → (3, -1) — ETA 2 turn(s)"
        );
        // Unknown placeholders stay as-is.
        assert_eq!(loc.trf("m", &[]), "{name} → ({q}, {r}) — ETA {n} turn(s)");
    }

    #[test]
    fn language_code_roundtrip() {
        for lang in Language::ALL {
            assert_eq!(Language::from_code(lang.settings_tag()), *lang);
        }
        assert_eq!(Language::from_code(""), Language::SimpChinese);
        assert_eq!(Language::from_code("ZH"), Language::SimpChinese);
        assert_eq!(Language::from_code("en"), Language::English);
    }

    /// The embedded shipping files must define exactly the same key set in
    /// both languages — a missing translation is a build-time failure, not a
    /// runtime surprise.
    #[test]
    fn embedded_en_zh_key_parity() {
        let en = EMBEDDED_EN.iter().fold(HashMap::new(), |mut m, t| {
            parse_into(&mut m, t);
            m
        });
        let zh = EMBEDDED_ZH.iter().fold(HashMap::new(), |mut m, t| {
            parse_into(&mut m, t);
            m
        });
        let mut only_en: Vec<_> = en.keys().filter(|k| !zh.contains_key(*k)).collect();
        let mut only_zh: Vec<_> = zh.keys().filter(|k| !en.contains_key(*k)).collect();
        only_en.sort();
        only_zh.sort();
        assert!(
            only_en.is_empty(),
            "keys missing in simp_chinese: {only_en:?}"
        );
        assert!(only_zh.is_empty(), "keys missing in english: {only_zh:?}");
        // Placeholder-set parity: a zh string missing a `{name}` placeholder
        // used to pass the key-parity check and render the literal `{name}`
        // at runtime. trf substitution is keyed by name, so the SETS must
        // match exactly.
        let placeholders = |s: &str| -> std::collections::BTreeSet<String> {
            let mut set = std::collections::BTreeSet::new();
            let mut rest = s;
            while let Some(start) = rest.find('{') {
                let Some(end) = rest[start + 1..].find('}').map(|e| start + 1 + e) else {
                    break;
                };
                let name = &rest[start + 1..end];
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    set.insert(name.to_string());
                }
                rest = &rest[end + 1..];
            }
            set
        };
        for k in en.keys() {
            assert!(!en[k].trim().is_empty(), "empty english value for {k}");
            assert!(!zh[k].trim().is_empty(), "empty simp_chinese value for {k}");
            assert_eq!(
                placeholders(&en[k]),
                placeholders(&zh[k]),
                "placeholder mismatch for {k}: en={:?} zh={:?}",
                placeholders(&en[k]),
                placeholders(&zh[k])
            );
        }
    }

    #[test]
    fn embedded_files_load() {
        let loc = Locale::load(Language::SimpChinese);
        assert!(!loc.is_empty(), "embedded zh table should not be empty");
    }

    #[test]
    fn external_override_dir_wins() {
        let dir = std::env::temp_dir().join(format!("fc_locale_test_{}", std::process::id()));
        let sub = dir.join("english");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("zz_custom_l_english.yml"),
            "l_english:\n app.title:0 \"Modded Title\"\n new.key:0 \"New\"\n",
        )
        .unwrap();
        let loc = Locale::load_from(Language::English, &dir);
        assert_eq!(
            loc.tr("app.title"),
            "Modded Title",
            "external file overrides embedded"
        );
        assert_eq!(loc.tr("new.key"), "New", "external files can add keys");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
