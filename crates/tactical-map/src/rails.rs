//! Parser for HOI4 `map/railways.txt`: the static rail network. Each
//! line is `<level> <province-count> <pid1> … <pidN>` (whitespace
//! separated) — the supply level and the ordered province path the track
//! runs through. The tactical generator paints `rail` hexes along the
//! path provinces' unitstacks anchors (§4.2 railway overlay).

use std::path::Path;

use crate::Result;

/// One railway line: the track's supply level and its ordered province
/// path (kept in file order — consecutive pairs form the painted
/// segments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailwayLine {
    pub level: u8,
    pub path: Vec<u32>,
}

/// Read `map/railways.txt`; accepts the map directory or the file itself.
/// Malformed lines are skipped; a missing/unreadable file yields an empty
/// list (rail is a bonus overlay, never a hard failure).
pub fn load_railways(path: &Path) -> Result<Vec<RailwayLine>> {
    let file = if path.is_dir() {
        path.join("railways.txt")
    } else {
        path.to_path_buf()
    };
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut it = line.split_whitespace();
        let (Some(Ok(level)), Some(Ok(count))) = (
            it.next().map(str::parse::<u8>),
            it.next().map(str::parse::<usize>),
        ) else {
            continue;
        };
        let path: Vec<u32> = it
            .take(count)
            .filter_map(|t| t.parse::<u32>().ok())
            .collect();
        // A usable line connects at least two provinces.
        if path.len() >= 2 {
            out.push(RailwayLine { level, path });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_railway_lines() {
        let dir = std::env::temp_dir().join(format!("tac_rails_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("railways.txt");
        // Vanilla-shaped lines (trailing whitespace included), a comment,
        // a junk line, and a short line (declared count > actual pids).
        std::fs::write(
            &path,
            "3 4 6521 11444 3312 6282 \n\
             2 5 6282 349 3340 9388 321\n\
             # 1 2 999 998\n\
             junk line\n\
             1 3 6332 11245\n\
             5 1 777\n",
        )
        .unwrap();
        let lines = load_railways(&path).unwrap();
        assert_eq!(lines.len(), 3, "comment/junk/single-pid lines skipped");
        assert_eq!(
            lines[0],
            RailwayLine {
                level: 3,
                path: vec![6521, 11444, 3312, 6282],
            }
        );
        assert_eq!(
            lines[1],
            RailwayLine {
                level: 2,
                path: vec![6282, 349, 3340, 9388, 321],
            }
        );
        // Short line: kept with the pids it actually has (2 = usable).
        assert_eq!(
            lines[2],
            RailwayLine {
                level: 1,
                path: vec![6332, 11245],
            }
        );
        // The directory form resolves railways.txt inside it.
        let lines2 = load_railways(&dir).unwrap();
        assert_eq!(lines2, lines);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        let lines = load_railways(Path::new("no/such/railways.txt")).unwrap();
        assert!(lines.is_empty());
    }
}
