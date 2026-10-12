//! Parser for HOI4 `localisation/<lang>/victory_points_l_<lang>.yml`:
//! maps `VICTORY_POINTS_<pid>: "Name"` entries to province ids
//! so the tactical map can float the VP's real name above its city.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The VP-names yml for the display language: the player's own
/// language (`lang_code` = the `localisation/<code>/` folder name, e.g.
/// "russian") when the install ships that localisation, English otherwise
/// (a missing/relocated folder degrades to English, never to an error).
pub fn vp_names_path(hoi4_dir: &Path, lang_code: &str) -> PathBuf {
    if lang_code != "english" {
        let own = hoi4_dir
            .join("localisation")
            .join(lang_code)
            .join(format!("victory_points_l_{lang_code}.yml"));
        if own.is_file() {
            return own;
        }
    }
    hoi4_dir
        .join("localisation")
        .join("english")
        .join("victory_points_l_english.yml")
}

/// Read VP display names (province id → name). A missing/unreadable file
/// yields an empty map (the label is a cosmetic overlay, never a hard
/// failure).
pub fn load_vp_names(path: &Path) -> HashMap<u32, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        // Shape: ` VICTORY_POINTS_306: "Smolensk"` (optional `:0` version
        // suffix in older files: `VICTORY_POINTS_306:0 "Smolensk"`).
        let line = line.trim();
        let Some(rest) = line.strip_prefix("VICTORY_POINTS_") else {
            continue;
        };
        let Some(colon) = rest.find(':') else {
            continue;
        };
        let Ok(pid) = rest[..colon].parse::<u32>() else {
            continue;
        };
        let Some(open) = rest[colon..].find('"') else {
            continue;
        };
        let after = &rest[colon + open + 1..];
        let Some(close) = after.find('"') else {
            continue;
        };
        let name = after[..close].trim();
        if !name.is_empty() {
            out.entry(pid).or_insert_with(|| name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vp_names() {
        let dir = std::env::temp_dir().join(format!("tac_loc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("victory_points_l_english.yml");
        std::fs::write(
            &path,
            "\u{feff}l_english:\n\
             \x20VICTORY_POINTS_306: \"Smolensk\"\n\
             \x20VICTORY_POINTS_3560: \"Sedan\"\n\
             \x20VICTORY_POINTS_999:0 \"Versioned\"\n\
             \x20VICTORY_POINTS_12: \"\"\n\
             \x20OTHER_KEY_5: \"Ignored\"\n",
        )
        .unwrap();
        let m = load_vp_names(&path);
        assert_eq!(m.get(&306).map(String::as_str), Some("Smolensk"));
        assert_eq!(m.get(&3560).map(String::as_str), Some("Sedan"));
        assert_eq!(m.get(&999).map(String::as_str), Some("Versioned"));
        assert_eq!(m.len(), 3, "empty names and non-VP keys skipped: {m:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        assert!(load_vp_names(Path::new("no/such/file.yml")).is_empty());
    }

    #[test]
    fn vp_names_path_prefers_own_language_then_falls_back() {
        let dir = std::env::temp_dir().join(format!("tac_vp_path_{}", std::process::id()));
        let en = dir.join("localisation/english");
        std::fs::create_dir_all(&en).unwrap();
        // No language files yet: any session lands on the English yml.
        assert_eq!(
            vp_names_path(&dir, "simp_chinese"),
            en.join("victory_points_l_english.yml")
        );
        assert_eq!(
            vp_names_path(&dir, "russian"),
            en.join("victory_points_l_english.yml")
        );
        let zh = dir.join("localisation/simp_chinese");
        std::fs::create_dir_all(&zh).unwrap();
        std::fs::write(
            zh.join("victory_points_l_simp_chinese.yml"),
            "l_simp_chinese:\n",
        )
        .unwrap();
        assert_eq!(
            vp_names_path(&dir, "simp_chinese"),
            zh.join("victory_points_l_simp_chinese.yml")
        );
        // A Russian file appearing later is picked up for Russian sessions…
        let ru = dir.join("localisation/russian");
        std::fs::create_dir_all(&ru).unwrap();
        std::fs::write(ru.join("victory_points_l_russian.yml"), "l_russian:\n").unwrap();
        assert_eq!(
            vp_names_path(&dir, "russian"),
            ru.join("victory_points_l_russian.yml")
        );
        // …while English sessions always take the English file.
        assert_eq!(
            vp_names_path(&dir, "english"),
            en.join("victory_points_l_english.yml")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
