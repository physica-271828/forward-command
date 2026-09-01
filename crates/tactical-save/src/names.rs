//! HOI4 division names-groups (`common/units/names_divisions/*.txt`) read
//! LIVE from the install — resolves a save's `division_name={type,name_order}`
//! token pair (plus the template's `division_names_group`) back into the
//! in-game division name (§5.2). Reading at runtime keeps the table fresh
//! across game updates; pre-extracting would ship a stale, bulky snapshot.
//!
//! Resolution model (reverse-engineered from 1.19.2 saves): every division's
//! template carries a `division_names_group`; `name_order` is the issue
//! number within that group. The displayed name is the group's `ordered[N]`
//! format string, or its `fallback_name` when N has no scripted entry; `%d`
//! expands to the decimal number, `%s` to the ROMAN numeral. Ordinal
//! suffixes are baked into the format strings ("%dst Infantry Division",
//! "%da Divisione di Fanteria"). `type` is always 0 in 1.19.2 (reserved) and
//! `can_use`/`for_countries` gates are irrelevant here — the name was already
//! assigned when the division was created. `unordered` lists are parsed and
//! ignored (unused by vanilla; their issue-number semantics are undocumented).

use std::collections::HashMap;
use std::path::Path;

/// One names-group: the scripted per-number names plus the exhaustion
/// fallback. Both are printf-style format strings with a single `%d`/`%s`.
#[derive(Debug, Default, Clone)]
pub struct NameGroup {
    pub fallback: Option<String>,
    pub ordered: HashMap<u32, String>,
}

/// All names-groups keyed by their script tag (`GER_Inf_01`, ...).
#[derive(Debug, Default)]
pub struct NameGroups {
    groups: HashMap<String, NameGroup>,
}

impl NameGroups {
    /// Load every `*.txt` in a `common/units/names_divisions` directory.
    /// Files load in alphabetical order (HOI4 load order); a group tag
    /// redefined by a later file merges per entry number (mod override
    /// semantics per the format's own header). Unreadable/missing dirs and
    /// unparseable files degrade to an empty table (callers fall back to
    /// synthesized names), never to an error.
    pub fn load_from_dir(dir: &Path) -> NameGroups {
        let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file() && p.extension().map(|x| x == "txt").unwrap_or(false)
                })
                .collect(),
            Err(_) => return NameGroups::default(),
        };
        files.sort();
        let mut out = NameGroups::default();
        for f in files {
            if let Ok(bytes) = std::fs::read(&f) {
                out.merge_bytes(&bytes);
            }
        }
        out
    }

    /// Parse and merge one names-divisions text (already-decoded form).
    pub fn merge_str(&mut self, text: &str) {
        self.merge_bytes(text.as_bytes());
    }

    /// Parse and merge one names-divisions file body. The files are UTF-8
    /// (usually with a BOM); a leading BOM is stripped for the tape.
    pub fn merge_bytes(&mut self, bytes: &[u8]) {
        let body = match bytes {
            b if b.starts_with(&[0xEF, 0xBB, 0xBF]) => &b[3..],
            b => b,
        };
        let Ok(tape) = jomini::TextTape::from_slice(body) else {
            return;
        };
        let root = tape.utf8_reader();
        for (key, group) in root.field_groups() {
            let tag = key.read_str().to_string();
            for (_op, value) in group.values() {
                let Ok(obj) = value.read_object() else {
                    continue;
                };
                let entry = self.groups.entry(tag.clone()).or_default();
                for (field, fg) in obj.field_groups() {
                    match field.read_str().as_ref() {
                        "fallback_name" => {
                            if let Some(s) = first_string(&fg) {
                                entry.fallback = Some(s);
                            }
                        }
                        "ordered" => {
                            for (_op, v) in fg.values() {
                                let Ok(ord) = v.read_object() else {
                                    continue;
                                };
                                for (num_key, num_group) in ord.field_groups() {
                                    let Ok(n) = num_key.read_str().parse::<u32>() else {
                                        continue;
                                    };
                                    // N = { "name" "tooltip key" "url" } —
                                    // the value is a bare array of 1-3
                                    // quoted args; only the name (the
                                    // first) is wanted.
                                    for (_op, nv) in num_group.values() {
                                        let Ok(arr) = nv.read_array() else {
                                            continue;
                                        };
                                        for sv in arr.values() {
                                            if let Ok(s) = sv.read_str() {
                                                entry.ordered.insert(n, s.to_string());
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// The in-game name for `name_order` issued from `group`: the scripted
    /// `ordered` entry when present, else the group's `fallback_name`; None
    /// when neither exists (caller keeps its synthesized fallback name).
    pub fn resolve(&self, group: &str, order: u32) -> Option<String> {
        let g = self.groups.get(group)?;
        let fmt = g.ordered.get(&order).or(g.fallback.as_ref())?;
        Some(substitute(fmt, order))
    }
}

/// The first quoted scalar of a jomini field group.
fn first_string(group: &jomini::text::GroupEntry<'_, '_, jomini::Utf8Encoding>) -> Option<String> {
    for (_op, v) in group.values() {
        if let Ok(s) = v.read_str() {
            return Some(s.to_string());
        }
    }
    None
}

/// Expand the single `%d` (decimal) / `%s` (Roman numeral) placeholder.
/// Literal ordinal suffixes ride along untouched ("%dst" → "1st").
fn substitute(fmt: &str, n: u32) -> String {
    let mut out = fmt.to_string();
    if out.contains("%s") {
        out = out.replace("%s", &roman(n));
    }
    if out.contains("%d") {
        out = out.replace("%d", &n.to_string());
    }
    out
}

/// 1..=3999 as a Roman numeral (HOI4 `%s` placement); out-of-range falls
/// back to the decimal form.
fn roman(n: u32) -> String {
    const TABLE: &[(u32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    let mut rest = n;
    let mut out = String::new();
    for (v, s) in TABLE {
        while rest >= *v {
            out.push_str(s);
            rest -= v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}# header comment\n\
        GER_Inf_01 = {\n\
        \tname = \"Infantry Divisions\"\n\
        \tfor_countries = { GER }\n\
        \tcan_use = { always = yes }\n\
        \tdivision_types = { \"infantry\" }\n\
        \tfallback_name = \"%d. Infanterie-Division\"\n\
        \tordered = {\n\
        \t\t1 = { \"%d. Infanterie-Division\" }\n\
        \t\t2 = { \"%d. Infanterie-Division 'Großdeutschland'\" } # trailing comment\n\
        \t\t14 = { \"%d. Infanterie-Division (mot.)\" \"TOOLTIP_KEY\" \"https://x\" }\n\
        \t}\n\
        }\n\
        ENG_INF_01 = {\n\
        \tfallback_name = \"%d Infantry Division\"\n\
        \tordered = {\n\
        \t\t1 = { \"%dst Infantry Division\" }\n\
        \t\t2 = { \"%dnd Infantry Division\" }\n\
        \t}\n\
        }\n\
        SOV_GDS_01 = {\n\
        \tfallback_name = \"%s-ya Gv. Strelk. Div.\"\n\
        \tordered = {\n\
        \t\t14 = { \"%s-ya 'Vitebskaya' Gv. Strelk. Div.\" }\n\
        \t}\n\
        }\n\
        EMPTY_01 = {\n\
        \tordered = { 1 = { \"One\" } }\n\
        }\n";

    #[test]
    fn resolves_ordered_entries_with_literal_suffixes() {
        let mut ng = NameGroups::default();
        ng.merge_str(SAMPLE);
        assert_eq!(
            ng.resolve("GER_Inf_01", 2).as_deref(),
            Some("2. Infanterie-Division 'Großdeutschland'")
        );
        // Only the first arg is the name (tooltip key / URL dropped).
        assert_eq!(
            ng.resolve("GER_Inf_01", 14).as_deref(),
            Some("14. Infanterie-Division (mot.)")
        );
        assert_eq!(
            ng.resolve("ENG_INF_01", 1).as_deref(),
            Some("1st Infantry Division")
        );
        assert_eq!(
            ng.resolve("ENG_INF_01", 2).as_deref(),
            Some("2nd Infantry Division")
        );
    }

    #[test]
    fn falls_back_when_the_number_is_not_scripted() {
        let mut ng = NameGroups::default();
        ng.merge_str(SAMPLE);
        assert_eq!(
            ng.resolve("GER_Inf_01", 25).as_deref(),
            Some("25. Infanterie-Division")
        );
        assert_eq!(
            ng.resolve("ENG_INF_01", 42).as_deref(),
            Some("42 Infantry Division")
        );
        // A group with no fallback and no scripted entry resolves to None.
        assert_eq!(ng.resolve("EMPTY_01", 7), None);
        assert_eq!(ng.resolve("NO_SUCH_GROUP", 1), None);
    }

    #[test]
    fn percent_s_is_a_roman_numeral() {
        let mut ng = NameGroups::default();
        ng.merge_str(SAMPLE);
        assert_eq!(
            ng.resolve("SOV_GDS_01", 14).as_deref(),
            Some("XIV-ya 'Vitebskaya' Gv. Strelk. Div.")
        );
        assert_eq!(
            ng.resolve("SOV_GDS_01", 9).as_deref(),
            Some("IX-ya Gv. Strelk. Div.")
        );
    }

    #[test]
    fn later_files_merge_per_entry_number() {
        let mut ng = NameGroups::default();
        ng.merge_str(SAMPLE);
        // Mod-style override: same group tag, one number replaced, the
        // fallback swapped, every other scripted number kept.
        ng.merge_str(
            "GER_Inf_01 = {\n\
            \tfallback_name = \"%d. Infanterie-Division (mod)\"\n\
            \tordered = { 1 = { \"%d. Leib-Division\" } }\n\
            }",
        );
        assert_eq!(
            ng.resolve("GER_Inf_01", 1).as_deref(),
            Some("1. Leib-Division")
        );
        assert_eq!(
            ng.resolve("GER_Inf_01", 2).as_deref(),
            Some("2. Infanterie-Division 'Großdeutschland'")
        );
        assert_eq!(
            ng.resolve("GER_Inf_01", 25).as_deref(),
            Some("25. Infanterie-Division (mod)")
        );
    }

    #[test]
    fn unparseable_input_is_skipped() {
        let mut ng = NameGroups::default();
        ng.merge_bytes(b"\xff\xfe not clausewitz at all {{{");
        ng.merge_str(SAMPLE);
        assert!(ng.resolve("GER_Inf_01", 1).is_some());
    }

    #[test]
    fn roman_numerals() {
        assert_eq!(roman(1), "I");
        assert_eq!(roman(4), "IV");
        assert_eq!(roman(9), "IX");
        assert_eq!(roman(14), "XIV");
        assert_eq!(roman(40), "XL");
        assert_eq!(roman(99), "XCIX");
        assert_eq!(roman(499), "CDXCIX");
        assert_eq!(roman(1994), "MCMXCIV");
        assert_eq!(roman(0), "0");
        assert_eq!(roman(4000), "4000");
    }

    /// End-to-end against the real install (needs a local HOI4; run with
    /// `--ignored`). Reads the vanilla `common/units/names_divisions` set
    /// and resolves known scripted entries — this is the exact code path
    /// the live assembly uses.
    #[test]
    #[ignore]
    fn real_hoi4_names_divisions_load_and_resolve() {
        let dir = std::env::var("HOI4_DIR")
            .unwrap_or_else(|_| r"D:\Steam\steamapps\common\Hearts of Iron IV".to_string());
        let ng = NameGroups::load_from_dir(
            &std::path::Path::new(&dir).join("common/units/names_divisions"),
        );
        // Scripted ordered entries.
        assert_eq!(
            ng.resolve("GER_Inf_01", 25).as_deref(),
            Some("25. Infanterie-Division")
        );
        assert_eq!(
            ng.resolve("ITA_INF_01", 3).as_deref(),
            Some("3a Divisione di Fanteria 'Ravenna'")
        );
        assert_eq!(
            ng.resolve("ENG_INF_01", 1).as_deref(),
            Some("1st Infantry Division")
        );
        // %s Roman + a non-ASCII (UTF-8) literal.
        assert_eq!(
            ng.resolve("ALB_INF_01", 3).as_deref(),
            Some("III Këmbësori Pjesëtim")
        );
        // Unscripted number → group fallback.
        assert_eq!(
            ng.resolve("GER_Inf_01", 499).as_deref(),
            Some("499. Infanterie-Division")
        );
    }
}
