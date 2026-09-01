//! Parser for HOI4 `map/unitstacks.txt`: the index-0 stack
//! position of each province is where the game draws the province's main
//! city / VP label, so it is the authoritative in-province VP location —
//! far better than the province centroid for placing the tactical city.
//!
//! Line format: `pid;index;x;elevation;z;rotation;scale`. The bitmap y
//! axis is BOTTOM-UP here (z = height - 1 - bitmap_row); callers convert
//! with the province bitmap height.

use std::collections::HashMap;
use std::path::Path;

/// Read the index-0 stack position per province. Returns raw (x, z) pairs
/// in the unitstacks coordinate system (z bottom-up); a missing/unreadable
/// file yields an empty map (VP placement falls back to the centroid).
pub fn load_unit_stacks(path: &Path) -> HashMap<u32, (f32, f32)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut it = line.trim().split(';');
        let (Some(Ok(pid)), Some(Ok(idx))) = (
            it.next().map(str::parse::<u32>),
            it.next().map(str::parse::<u32>),
        ) else {
            continue;
        };
        if idx != 0 {
            continue;
        }
        let (Some(Ok(x)), Some(_elev), Some(Ok(z))) = (
            it.next().map(str::parse::<f32>),
            it.next().map(str::parse::<f32>),
            it.next().map(str::parse::<f32>),
        ) else {
            continue;
        };
        out.entry(pid).or_insert((x, z));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_zero_only() {
        let dir = std::env::temp_dir().join(format!("tac_us_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unitstacks.txt");
        std::fs::write(
            &path,
            "306;0;3306.00;10.00;1616.00;0.00;0.18\n\
             306;1;3308.48;10.05;1614.00;0.89;0.18\n\
             3560;0;2887.00;10.10;1506.00;0.00;0.24\n\
             junk line\n",
        )
        .unwrap();
        let m = load_unit_stacks(&path);
        assert_eq!(m.get(&306), Some(&(3306.0, 1616.0)));
        assert_eq!(m.get(&3560), Some(&(2887.0, 1506.0)));
        assert_eq!(m.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        let m = load_unit_stacks(Path::new("no/such/unitstacks.txt"));
        assert!(m.is_empty());
    }
}
