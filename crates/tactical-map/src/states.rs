//! Parser for HOI4 `history/states/*.txt` victory points: a VP
//! on the battle province places Urban hex(es) on the tactical map.
//! `cities.bmp` turned out to be a building-style region map, so state
//! files are the authoritative source for city locations (province id → VP
//! level; the in-province pixel position is approximated by the province
//! centroid in the generator).

use std::collections::HashMap;
use std::path::Path;

use crate::Result;

/// Scan `history/states/*.txt` for `victory_points = { <pid> <level> }`
/// blocks. Multiple VPs per state and entries spanning several lines are
/// supported; the highest level wins when a province is listed twice.
/// A missing/unreadable directory yields an empty map (VPs are a bonus
/// overlay, never a hard failure).
pub fn load_victory_points(states_dir: &Path) -> Result<HashMap<u32, u32>> {
    let mut out: HashMap<u32, u32> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(states_dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        parse_state_file(&text, &mut out);
    }
    Ok(out)
}

/// Parse one state file: every `victory_points = { … }` block's number
/// pairs (province id, level). Comments (`#` to end of line) are stripped.
fn parse_state_file(text: &str, out: &mut HashMap<u32, u32>) {
    // Strip comments.
    let no_comments: String = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let tokens: Vec<&str> = no_comments
        .split(|c: char| c.is_whitespace() || c == '{' || c == '}' || c == '=')
        .filter(|t| !t.is_empty())
        .collect();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "victory_points" {
            // Number pairs follow until a non-numeric token.
            let mut j = i + 1;
            while j + 1 < tokens.len() {
                let (Ok(pid), Ok(level)) = (tokens[j].parse::<u32>(), tokens[j + 1].parse::<u32>())
                else {
                    break;
                };
                out.entry(pid)
                    .and_modify(|l| *l = (*l).max(level))
                    .or_insert(level);
                j += 2;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
}

/// Convenience: states dir under a HOI4 install root.
pub fn states_dir_of(hoi4_dir: &Path) -> std::path::PathBuf {
    hoi4_dir.join("history").join("states")
}

/// Scan `history/states/*.txt` for each state's `state_category = <name>`
/// (a top-level key of the state block, e.g. `state_category = town`).
/// The category drives the tactical village density — urbanised states
/// spawn a sparser countryside scatter. A missing/unreadable directory
/// yields an empty map (never a hard failure); the first file wins when a
/// state id repeats across start-date variants.
pub fn load_state_categories(states_dir: &Path) -> Result<HashMap<u32, String>> {
    let mut out: HashMap<u32, String> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(states_dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        parse_state_category(&text, &mut out);
    }
    Ok(out)
}

/// Scan `history/states/*.txt` for each state's industry level: the sum of
/// the `industrial_complex = N` and `arms_factory = N` levels in the
/// file's `history = { buildings = { … } }` block. Only TOP-LEVEL building
/// keys count — nested per-province blocks (`3838 = { naval_base = 3 }`)
/// hold local structures, not state industry. The sum feeds the tactical
/// factory-district overlay. Missing dir → empty map; first file wins.
pub fn load_state_industry(states_dir: &Path) -> Result<HashMap<u32, u32>> {
    let mut out: HashMap<u32, u32> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(states_dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        parse_state_industry(&text, &mut out);
    }
    Ok(out)
}

/// Scan `history/states/*.txt` for each state's infrastructure level: the
/// TOP-LEVEL `infrastructure = N` key in the file's `buildings = { … }`
/// block (same depth discipline as [`load_state_industry`]). The level
/// (0..=5) feeds the tactical bridge probability (p = level / 5).
/// Missing dir → empty map; first file wins.
pub fn load_state_infrastructure(states_dir: &Path) -> Result<HashMap<u32, u32>> {
    let mut out: HashMap<u32, u32> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(states_dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        parse_state_infrastructure(&text, &mut out);
    }
    Ok(out)
}

/// Tokenize one state file into words plus the structural tokens `{`,
/// `}` and `=`. Comments (`#` to end of line) are stripped. Unlike the
/// flat VP/provinces tokenizer, this KEEPS the structure tokens so the
/// industry parser can tell top-level building keys from nested
/// per-province blocks.
fn tokenize_state(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        for c in line.chars() {
            if c.is_whitespace() {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            } else if c == '{' || c == '}' || c == '=' {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            } else {
                cur.push(c);
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The state id is the first `id = <n>` of the file (the state block
/// opens the file; same convention as [`parse_state_provinces`]).
fn first_state_id(tokens: &[String]) -> Option<u32> {
    tokens
        .windows(3)
        .find(|w| w[0] == "id" && w[1] == "=")
        .and_then(|w| w[2].parse::<u32>().ok())
}

/// Parse one state file: state id → its `state_category` value.
fn parse_state_category(text: &str, out: &mut HashMap<u32, String>) {
    let tokens = tokenize_state(text);
    let Some(sid) = first_state_id(&tokens) else {
        return;
    };
    let Some(w) = tokens
        .windows(3)
        .find(|w| w[0] == "state_category" && w[1] == "=")
    else {
        return;
    };
    out.entry(sid).or_insert_with(|| w[2].clone());
}

/// Parse one state file: state id → the sum of `industrial_complex` +
/// `arms_factory` levels over every `buildings = { … }` block, top-level
/// keys only (depth 1 inside the block; nested per-province blocks are
/// skipped by the depth counter).
fn parse_state_industry(text: &str, out: &mut HashMap<u32, u32>) {
    let tokens = tokenize_state(text);
    let Some(sid) = first_state_id(&tokens) else {
        return;
    };
    let mut sum = 0u32;
    let mut i = 0;
    while i + 2 < tokens.len() {
        if tokens[i] == "buildings" && tokens[i + 1] == "=" && tokens[i + 2] == "{" {
            let mut depth = 1usize;
            let mut j = i + 3;
            while j < tokens.len() && depth > 0 {
                match tokens[j].as_str() {
                    "{" => depth += 1,
                    "}" => depth -= 1,
                    "industrial_complex" | "arms_factory" if depth == 1 => {
                        if tokens.get(j + 1).map(String::as_str) == Some("=") {
                            if let Some(Ok(n)) = tokens.get(j + 2).map(|t| t.parse::<u32>()) {
                                sum += n;
                            }
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out.entry(sid).or_insert(sum);
}

/// Parse one state file: state id → the top-level `infrastructure` level
/// over every `buildings = { … }` block (same depth-1 discipline as
/// [`parse_state_industry`]; the max wins across blocks — the key is
/// unique in vanilla, so this is defensive).
fn parse_state_infrastructure(text: &str, out: &mut HashMap<u32, u32>) {
    let tokens = tokenize_state(text);
    let Some(sid) = first_state_id(&tokens) else {
        return;
    };
    let mut level = 0u32;
    let mut i = 0;
    while i + 2 < tokens.len() {
        if tokens[i] == "buildings" && tokens[i + 1] == "=" && tokens[i + 2] == "{" {
            let mut depth = 1usize;
            let mut j = i + 3;
            while j < tokens.len() && depth > 0 {
                match tokens[j].as_str() {
                    "{" => depth += 1,
                    "}" => depth -= 1,
                    "infrastructure" if depth == 1 => {
                        if tokens.get(j + 1).map(String::as_str) == Some("=") {
                            if let Some(Ok(n)) = tokens.get(j + 2).map(|t| t.parse::<u32>()) {
                                level = level.max(n);
                            }
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out.entry(sid).or_insert(level);
}

/// Scan `history/states/*.txt` for each state's `id = N` + `provinces = { … }`
/// and build province id → state id (the live refresh flow maps a
/// `land_combat.location` province back to its state to flag battle states).
/// A missing/unreadable directory yields an empty map (never a hard failure).
pub fn load_province_to_state(states_dir: &Path) -> Result<HashMap<u32, u32>> {
    let mut out: HashMap<u32, u32> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(states_dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        parse_state_provinces(&text, &mut out);
    }
    Ok(out)
}

/// Parse one state file: the first `id = <n>` is the state id; every number
/// in the `provinces = { … }` block maps to it. Comments stripped. State ids
/// repeat across files for different start dates (e.g. `123-FOO.txt` variants
/// are separate files), so a later file only fills provinces not yet mapped —
/// the base-map state list is authoritative for live battles.
fn parse_state_provinces(text: &str, out: &mut HashMap<u32, u32>) {
    let no_comments: String = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let tokens: Vec<&str> = no_comments
        .split(|c: char| c.is_whitespace() || c == '{' || c == '}' || c == '=')
        .filter(|t| !t.is_empty())
        .collect();
    let mut state_id: Option<u32> = None;
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "id" && state_id.is_none() {
            state_id = tokens.get(i + 1).and_then(|t| t.parse::<u32>().ok());
            i += 2;
        } else if tokens[i] == "provinces" {
            let Some(sid) = state_id else {
                i += 1;
                continue;
            };
            let mut j = i + 1;
            while j < tokens.len() {
                let Ok(pid) = tokens[j].parse::<u32>() else {
                    break;
                };
                out.entry(pid).or_insert(sid);
                j += 1;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vp_blocks() {
        let text = r#"
            state = {
                id = 18
                name = "STATE_18"
                provinces = { 520 3560 623 44 }
                victory_points = { 3560 5 }
                victory_points = {
                    623 10
                }
                # victory_points = { 999 99 }  (commented out)
            }
        "#;
        let mut out = HashMap::new();
        parse_state_file(text, &mut out);
        assert_eq!(out.get(&3560), Some(&5));
        assert_eq!(out.get(&623), Some(&10));
        assert_eq!(out.get(&999), None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn highest_level_wins() {
        let text = "victory_points = { 5 1 }\nvictory_points = { 5 8 }";
        let mut out = HashMap::new();
        parse_state_file(text, &mut out);
        assert_eq!(out.get(&5), Some(&8));
    }

    #[test]
    fn parses_province_to_state() {
        let text = r#"
            state = {
                id = 2
                name = "STATE_2"
                provinces = { 623 3560 9892 }  # Lazio
                victory_points = { 623 30 }
                history = { owner = ITA }
            }
        "#;
        let mut out = HashMap::new();
        parse_state_provinces(text, &mut out);
        assert_eq!(out.get(&623), Some(&2));
        assert_eq!(out.get(&3560), Some(&2));
        assert_eq!(out.get(&9892), Some(&2));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn province_map_keeps_first_file_wins() {
        let mut out = HashMap::new();
        parse_state_provinces("state = { id = 2 provinces = { 623 } }", &mut out);
        parse_state_provinces("state = { id = 99 provinces = { 623 700 } }", &mut out);
        assert_eq!(out.get(&623), Some(&2)); // first file wins on conflict
        assert_eq!(out.get(&700), Some(&99));
    }

    #[test]
    fn parses_state_category() {
        let text = r#"
            state = {
                id = 1
                name = "STATE_1" # Corsica
                manpower = 322900
                state_category = town
                # state_category = megalopolis (commented out)
                history = { owner = FRA }
                provinces = { 3838 9851 11804 }
            }
        "#;
        let mut out = HashMap::new();
        parse_state_category(text, &mut out);
        assert_eq!(out.get(&1).map(String::as_str), Some("town"));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn parses_state_industry_top_level_keys_only() {
        // Vanilla shape (history/states/1-France.txt): the buildings block
        // mixes state-level industry with a nested per-province block; the
        // nested `industrial_complex` must NOT leak into the sum.
        let text = r#"
            state = {
                id = 1
                state_category = town
                history = {
                    owner = FRA
                    victory_points = { 3838 1 }
                    buildings = {
                        infrastructure = 2
                        industrial_complex = 1
                        arms_factory = 2
                        air_base = 1
                        3838 = {
                            naval_base = 3
                            industrial_complex = 9
                        }
                    }
                    add_core_of = COR
                }
                provinces = { 3838 9851 11804 }
            }
        "#;
        let mut out = HashMap::new();
        parse_state_industry(text, &mut out);
        assert_eq!(out.get(&1), Some(&3), "1 civ + 2 mil, nested 9 ignored");
    }

    #[test]
    fn industry_respects_comments_and_missing_blocks() {
        // A commented-out factory line must not count; a state without a
        // buildings block sums to zero but still registers.
        let text = "state = {\n\
                    \tid = 5\n\
                    \thistory = {\n\
                    \t\tbuildings = {\n\
                    \t\t\tindustrial_complex = 2 # arms_factory = 4\n\
                    \t\t}\n\
                    \t}\n\
                    \tprovinces = { 11 12 }\n\
                    }\n";
        let mut out = HashMap::new();
        parse_state_industry(text, &mut out);
        assert_eq!(out.get(&5), Some(&2));
        let mut out = HashMap::new();
        parse_state_industry("state = { id = 6 provinces = { 7 } }", &mut out);
        assert_eq!(out.get(&6), Some(&0));
        // No state id → no entry at all.
        let mut out = HashMap::new();
        parse_state_industry("buildings = { industrial_complex = 3 }", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn parses_state_infrastructure_top_level_keys_only() {
        // Vanilla shape: top-level `infrastructure` counts; a nested
        // per-province block must NOT leak in.
        let text = r#"
            state = {
                id = 1
                state_category = town
                history = {
                    owner = FRA
                    buildings = {
                        infrastructure = 3
                        industrial_complex = 1
                        3838 = {
                            naval_base = 3
                            infrastructure = 5
                        }
                    }
                }
                provinces = { 3838 9851 11804 }
            }
        "#;
        let mut out = HashMap::new();
        parse_state_infrastructure(text, &mut out);
        assert_eq!(out.get(&1), Some(&3), "nested 5 must not leak");
        // Commented-out infrastructure must not count; missing block → 0.
        let text = "state = {\n\
                    \tid = 7\n\
                    \thistory = {\n\
                    \t\tbuildings = {\n\
                    \t\t\t# infrastructure = 4\n\
                    \t\t\tindustrial_complex = 2\n\
                    \t\t}\n\
                    \t}\n\
                    \tprovinces = { 11 }\n\
                    }\n";
        let mut out = HashMap::new();
        parse_state_infrastructure(text, &mut out);
        assert_eq!(out.get(&7), Some(&0));
        let mut out = HashMap::new();
        parse_state_infrastructure("state = { id = 8 provinces = { 7 } }", &mut out);
        assert_eq!(out.get(&8), Some(&0));
    }
}
