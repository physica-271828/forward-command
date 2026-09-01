//! Parsers for HOI4 `map/definition.csv` and `map/adjacencies.csv`
//! (DESIGN.md §4.1). Both are `;`-separated with a header row; malformed
//! lines are skipped, but a file that yields no usable records at all is an
//! error (no panics).

use std::collections::HashMap;
use std::path::Path;

use tactical_core::Terrain;

use crate::{MapError, Result};

/// Province type from the `type` column (§4.1). Sea/lake provinces
/// are KEPT in the table (terrain = [`Terrain::Water`]) so the generator can
/// paint coastal/border water — battles still only generate for `Land`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvinceKind {
    Land,
    Sea,
    Lake,
}

/// One province from `definition.csv` (§4.1).
#[derive(Debug, Clone)]
pub struct ProvinceInfo {
    pub id: u32,
    pub kind: ProvinceKind,
    pub terrain: Terrain,
    pub is_coastal: bool,
    pub continent_id: u32,
    pub rgb: (u8, u8, u8),
}

/// Adjacency type column from `adjacencies.csv`. Only [`AdjacencyKind::River`]
/// produces river edges on the tactical map (§4.2 step 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjacencyKind {
    River,
    Sea,
    Canal,
    Other,
}

/// One row of `adjacencies.csv` (`From;To;Type;Through;...`).
#[derive(Debug, Clone)]
pub struct Adjacency {
    pub from: u32,
    pub to: u32,
    pub kind: AdjacencyKind,
    /// Province the connection goes "through" (-1 when unused).
    pub through: i32,
}

/// Map a definition.csv terrain token to [`Terrain`]. Water/empty tokens
/// return `None` so the caller can skip non-land provinces (§4.1: oceans,
/// lakes and unknowns never get a tactical map).
fn parse_terrain_token(s: &str) -> Option<Terrain> {
    match s.trim().to_ascii_lowercase().as_str() {
        "plains" => Some(Terrain::Plains),
        "forest" => Some(Terrain::Forest),
        "hills" => Some(Terrain::Hills),
        "mountain" => Some(Terrain::Mountain),
        "urban" => Some(Terrain::Urban),
        "jungle" => Some(Terrain::Jungle),
        "marsh" => Some(Terrain::Marsh),
        "desert" => Some(Terrain::Desert),
        "ocean" | "sea" | "lake" | "lakes" | "unknown" | "" => None,
        // Unrecognized land terrain: fall back to Plains instead of failing.
        _ => Some(Terrain::Plains),
    }
}

fn read_csv_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| MapError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Parse `map/definition.csv` into province id → [`ProvinceInfo`].
///
/// The real HOI4 layout is `id;R;G;B;type;coastal;terrain;continent` where
/// `type` is `land`/`sea`/`lake`; a simplified `id;R;G;B;terrain;coastal;
/// continent` layout (terrain directly in column 4) is also accepted. Water
/// provinces are kept as [`Terrain::Water`] records with their
/// [`ProvinceKind`] (kept for coastal/border water rendering); only
/// `Land` provinces can host a tactical battle (the generator filters).
pub fn load_definition_csv(path: &Path) -> Result<HashMap<u32, ProvinceInfo>> {
    let text = read_csv_file(path)?;
    let mut out = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split(';').collect();
        if parts.len() < 4 {
            continue;
        }
        // A non-numeric id means this is the header row (or junk): skip.
        let Ok(id) = parts[0].trim().parse::<u32>() else {
            continue;
        };
        let (Ok(r), Ok(g), Ok(b)) = (
            parts[1].trim().parse::<u8>(),
            parts[2].trim().parse::<u8>(),
            parts[3].trim().parse::<u8>(),
        ) else {
            continue;
        };

        let field4 = parts[4].trim().to_ascii_lowercase();
        // Real HOI4 layout with an explicit province-type column.
        if matches!(field4.as_str(), "land" | "sea" | "lake") {
            if parts.len() < 8 {
                continue;
            }
            let kind = match field4.as_str() {
                "sea" => ProvinceKind::Sea,
                "lake" => ProvinceKind::Lake,
                _ => ProvinceKind::Land,
            };
            // Water provinces are kept as Terrain::Water records.
            // A LAND row with a water/unknown terrain token (modded terrains,
            // e.g. "lake") falls back to Plains instead of dropping the
            // record — a dropped record made the province unbattleable as
            // ProvinceNotFound.
            let terrain = if kind == ProvinceKind::Land {
                parse_terrain_token(parts[6]).unwrap_or(Terrain::Plains)
            } else {
                Terrain::Water
            };
            let is_coastal = parts[5].trim().eq_ignore_ascii_case("true");
            let continent_id = parts[7].trim().parse::<u32>().unwrap_or(0);
            out.insert(
                id,
                ProvinceInfo {
                    id,
                    kind,
                    terrain,
                    is_coastal,
                    continent_id,
                    rgb: (r, g, b),
                },
            );
            continue;
        }
        // Simplified layout: terrain directly in column 4.
        let (terrain_s, coastal_s, continent_s) = {
            if parts.len() < 7 {
                continue;
            }
            (parts[4], parts[5], parts[6])
        };

        // Simplified layout has no kind column — a water terrain token IS
        // the water marker, so those rows keep being skipped (unknown land
        // tokens already fall back to Plains inside parse_terrain_token).
        let Some(terrain) = parse_terrain_token(terrain_s) else {
            continue;
        };
        let is_coastal = coastal_s.trim().eq_ignore_ascii_case("true");
        let continent_id = continent_s.trim().parse::<u32>().unwrap_or(0);

        out.insert(
            id,
            ProvinceInfo {
                id,
                kind: ProvinceKind::Land,
                terrain,
                is_coastal,
                continent_id,
                rgb: (r, g, b),
            },
        );
    }

    if out.is_empty() {
        return Err(MapError::InvalidCsv {
            path: path.to_path_buf(),
            reason: "no province records found".into(),
        });
    }
    Ok(out)
}

/// Parse `map/adjacencies.csv`. All well-formed rows are returned (callers
/// filter on [`AdjacencyKind`]); the `-1;-1;...` terminator rows and comment
/// lines are skipped. An empty file is not an error — some maps simply have
/// no adjacencies.
pub fn load_adjacencies_csv(path: &Path) -> Result<Vec<Adjacency>> {
    let text = read_csv_file(path)?;
    let mut out = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split(';').collect();
        if parts.len() < 3 {
            continue;
        }
        let (Ok(from), Ok(to)) = (
            parts[0].trim().parse::<i32>(),
            parts[1].trim().parse::<i32>(),
        ) else {
            continue; // header row or junk
        };
        if from < 0 || to < 0 {
            continue; // -1;-1 terminator rows
        }
        let kind = match parts[2].trim().to_ascii_lowercase().as_str() {
            "river" => AdjacencyKind::River,
            "sea" => AdjacencyKind::Sea,
            "canal" => AdjacencyKind::Canal,
            _ => AdjacencyKind::Other,
        };
        let through = parts
            .get(3)
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(-1);
        out.push(Adjacency {
            from: from as u32,
            to: to as u32,
            kind,
            through,
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "tactical_map_test_{name}_{}_{n}.csv",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn definition_csv_hoi4_layout() {
        let csv = "province;red;green;blue;type;coastal;terrain;continent\n\
                   1;88;0;0;land;true;plains;1\n\
                   2;99;0;0;land;false;forest;2\n\
                   3;0;0;100;sea;false;ocean;1\n\
                   4;77;0;0;lake;false;lake;1\n";
        let path = write_temp("def_hoi4", csv);
        let defs = load_definition_csv(&path).unwrap();
        // Water provinces are KEPT (kind Sea/Lake, terrain Water)
        // so the generator can paint coastal/border water.
        assert_eq!(defs.len(), 4);
        let p1 = &defs[&1];
        assert_eq!(p1.terrain, Terrain::Plains);
        assert_eq!(p1.kind, crate::csv::ProvinceKind::Land);
        assert!(p1.is_coastal);
        assert_eq!(p1.continent_id, 1);
        assert_eq!(p1.rgb, (88, 0, 0));
        let p2 = &defs[&2];
        assert_eq!(p2.terrain, Terrain::Forest);
        assert!(!p2.is_coastal);
        assert_eq!(p2.continent_id, 2);
        assert_eq!(defs[&3].kind, crate::csv::ProvinceKind::Sea);
        assert_eq!(defs[&3].terrain, Terrain::Water);
        assert_eq!(defs[&4].kind, crate::csv::ProvinceKind::Lake);
        assert_eq!(defs[&4].terrain, Terrain::Water);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn definition_csv_simple_layout_and_unknown_terrain() {
        // Simplified layout: id;R;G;B;terrain;coastal;continent
        let csv = "5;10;20;30;hills;true;2\n\
                   6;11;22;33;urban;false;2\n\
                   7;1;2;3;ocean;false;1\n\
                   8;4;5;6;bog;true;1\n";
        let path = write_temp("def_simple", csv);
        let defs = load_definition_csv(&path).unwrap();
        assert_eq!(defs.len(), 3, "water terrain token must be skipped");
        assert_eq!(defs[&5].terrain, Terrain::Hills);
        assert!(defs[&5].is_coastal);
        assert_eq!(defs[&6].terrain, Terrain::Urban);
        // Unknown land terrain falls back to Plains.
        assert_eq!(defs[&8].terrain, Terrain::Plains);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn definition_csv_rejects_junk_only_file() {
        let path = write_temp("def_junk", "province;red;green;blue\nfoo;bar\n\n");
        let err = load_definition_csv(&path).unwrap_err();
        assert!(
            matches!(err, MapError::InvalidCsv { .. }),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn adjacencies_csv_parse() {
        let csv =
            "From;To;Type;Through;start_x;start_y;stop_x;stop_y;adjacency_rule_name;Comment\n\
                   100;200;river;-1;0;0;0;0;;river crossing\n\
                   100;300;sea;400;0;0;0;0;;strait\n\
                   -1;-1;;;;;;;\n\
                   # a comment line\n";
        let path = write_temp("adj", csv);
        let adjs = load_adjacencies_csv(&path).unwrap();
        assert_eq!(adjs.len(), 2);
        assert_eq!(adjs[0].from, 100);
        assert_eq!(adjs[0].to, 200);
        assert_eq!(adjs[0].kind, AdjacencyKind::River);
        assert_eq!(adjs[0].through, -1);
        assert_eq!(adjs[1].kind, AdjacencyKind::Sea);
        assert_eq!(adjs[1].through, 400);
        let _ = std::fs::remove_file(&path);
    }
}
