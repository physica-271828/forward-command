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
}
