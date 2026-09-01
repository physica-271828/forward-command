//! The savegame-snapshot round trip (DESIGN.md §3.2).
//! After the state-target entry decision fires `tac_start`, the picked state
//! (`tac_pick=1`, state variables serialize) only exists in a
//! FRESH save, so the external side snapshots via console-injected
//! `savegame tac_snap`, finds the picked state, matches the player
//! `land_combat` inside it (history/states province→state map) and hands the
//! REAL contested province to `--livebattle`.
//! (The refresh/flagging machinery was removed — HOI4 re-evaluates
//! decision visibility/candidates only on the daily pulse, so injected
//! markers always lag a day; candidates are now script-computed frontier
//! states.)
//! SEVERAL player combats inside the picked state no longer
//! silently pick the largest — `PickResolution::Multiple` hands the
//! candidate list to the caller's player picker (menu dialog / CLI prompt).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tactical_inject::Injector;
use tactical_save::{SaveGame, SaveParser};

/// Snapshot base name: injected as `savegame tac_snap`, the file lands in
/// `<saves_dir>/tac_snap.hoi4` (explicit names work for both `save` and
/// `savegame`; `savegame` is the canonical one).
pub const SNAPSHOT_NAME: &str = "tac_snap";

/// Post-sync battle-alive check: a per-sync `savegame tac_check`
/// snapshot whose `land_combat` list answers whether the mirrored HOI4
/// battle is still alive (tactical-sync `savecheck` scans it). Kept
/// separate from `tac_snap` so the trigger-time baseline survives for
/// post-mortem reconciliation.
pub const CHECK_NAME: &str = "tac_check";

/// Generous upper bound for the save file to appear after injection
/// (late-game saves take seconds; small ones typically land in ~1 s).
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for the size-stability wait once the file exists — HOI4
/// streams 60+ MB to disk, and `is_file()` fires at write start.
const STABLE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn snapshot_path(saves_dir: &Path) -> PathBuf {
    saves_dir.join(format!("{SNAPSHOT_NAME}.hoi4"))
}

/// Delete any stale file, inject `savegame <name>`, wait for the file to
/// exist. Dry runs skip the wait (nothing will appear; dev convenience).
fn inject_and_wait(
    injector: &Injector,
    saves_dir: &Path,
    name: &str,
    dry: bool,
) -> Result<PathBuf, String> {
    let path = saves_dir.join(format!("{name}.hoi4"));
    // Any pre-existing file is deleted first — a stale file must never
    // pass as fresh (the `save` default-name trap family).
    let _ = std::fs::remove_file(&path);
    injector
        .inject_commands(&[format!("savegame {name}")], None, dry)
        .map_err(|e| format!("snapshot injection failed: {e}"))?;
    if dry {
        return Ok(path);
    }
    let start = Instant::now();
    loop {
        if path.is_file() {
            return Ok(path);
        }
        if start.elapsed() > SNAPSHOT_TIMEOUT {
            return Err(format!("timed out waiting for {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Wait until the save's size+mtime stop changing — `is_file()` fires at
/// write start, so a reader without this can catch the file mid-stream.
/// The scanner's `checksum=` tail guard is the second line of defense.
fn wait_stable(path: &Path) -> Result<(), String> {
    let start = Instant::now();
    let mut last: Option<(u64, Option<std::time::SystemTime>)> = None;
    loop {
        let md =
            std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        let sig = (md.len(), md.modified().ok());
        if last.as_ref() == Some(&sig) {
            return Ok(());
        }
        last = Some(sig);
        if start.elapsed() > STABLE_TIMEOUT {
            return Err(format!("save never settled: {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Inject `savegame tac_snap` and wait for a FRESH file. Any pre-existing
/// snapshot is deleted first — a stale file must never pass as fresh (the
/// `save` default-name trap family).
pub fn take_snapshot(injector: &Injector, saves_dir: &Path, dry: bool) -> Result<PathBuf, String> {
    inject_and_wait(injector, saves_dir, SNAPSHOT_NAME, dry)
}

/// Inject `savegame tac_check` and wait for a fresh, fully
/// written file (exists → size-stable). Callers still verify the
/// `checksum=` trailer before trusting a scan.
pub fn take_check_snapshot(
    injector: &Injector,
    saves_dir: &Path,
    dry: bool,
) -> Result<PathBuf, String> {
    let path = inject_and_wait(injector, saves_dir, CHECK_NAME, dry)?;
    if !dry {
        wait_stable(&path)?;
    }
    Ok(path)
}

/// Load and parse the snapshot file.
pub fn parse_snapshot(saves_dir: &Path) -> Result<SaveGame, String> {
    let path = snapshot_path(saves_dir);
    SaveParser::parse_save(&path).map_err(|e| format!("snapshot parse failed: {e}"))
}

/// One candidate battle for the multi-battle picker: a player
/// `land_combat` inside the picked state, flattened for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BattleChoice {
    /// The contested province (`land_combat.location`).
    pub province: u32,
    pub attacker_tags: Vec<String>,
    pub defender_tags: Vec<String>,
    /// Division counts per side (`unit = { id = N … }` entries).
    pub attacker_units: usize,
    pub defender_units: usize,
    /// The player is on the ATTACKING side of this battle (the picker map
    /// colors battle provinces by side).
    pub player_attacker: bool,
}

/// Outcome of the post-tac_start pick resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickResolution {
    /// The picked state holds exactly one player battle — the contested
    /// province.
    Province(u32),
    /// No `tac_pick` in the snapshot (old mod / no pick made) — the caller
    /// falls back to legacy largest-battle inference.
    NoPick,
    /// The picked state currently holds NO player battle (quiet frontier
    /// state, or the battle ended between pick and snapshot) — report it,
    /// do NOT silently launch a different battle.
    QuietNoBattle { state: u32 },
    /// The picked state holds SEVERAL player battles — the
    /// caller must ask the player which one to fight (menu dialog / CLI
    /// prompt); the list is sorted largest-first, province id as tiebreak.
    Multiple {
        state: u32,
        battles: Vec<BattleChoice>,
    },
}

/// Pure pick matching (unit-testable): `tac_pick=1` states → the player
/// land_combats whose locations map into one of them. One match hands its
/// province straight over; several matches go to the player picker.
pub fn pick_province(save: &SaveGame, p2s: &HashMap<u32, u32>) -> PickResolution {
    if save.picked_states.is_empty() {
        return PickResolution::NoPick;
    }
    let Some(tag) = save.player.clone() else {
        return PickResolution::NoPick;
    };
    let picked: std::collections::HashSet<u32> = save.picked_states.iter().copied().collect();
    let mut matches: Vec<&tactical_save::LandCombatData> = save
        .land_combats
        .iter()
        .filter(|c| {
            c.attacker.tags.iter().any(|t| t == &tag) || c.defender.tags.iter().any(|t| t == &tag)
        })
        .filter(|c| {
            p2s.get(&c.location)
                .map(|s| picked.contains(s))
                .unwrap_or(false)
        })
        .collect();
    // Deterministic order: largest battle first, province id as tiebreak.
    matches.sort_by(|a, b| {
        (b.attacker.unit_ids.len() + b.defender.unit_ids.len())
            .cmp(&(a.attacker.unit_ids.len() + a.defender.unit_ids.len()))
            .then(a.location.cmp(&b.location))
    });
    match matches.len() {
        0 => PickResolution::QuietNoBattle {
            state: save.picked_states[0],
        },
        1 => PickResolution::Province(matches[0].location),
        _ => PickResolution::Multiple {
            state: save.picked_states[0],
            battles: matches
                .iter()
                .map(|c| BattleChoice {
                    province: c.location,
                    attacker_tags: c.attacker.tags.clone(),
                    defender_tags: c.defender.tags.clone(),
                    attacker_units: c.attacker.unit_ids.len(),
                    defender_units: c.defender.unit_ids.len(),
                    player_attacker: c.attacker.tags.iter().any(|t| t == &tag),
                })
                .collect(),
        },
    }
}

/// Post-tac_start pick resolution: snapshot → [`pick_province`].
pub fn resolve_picked_province(
    injector: &Injector,
    saves_dir: &Path,
    p2s: &HashMap<u32, u32>,
    dry: bool,
) -> Result<PickResolution, String> {
    take_snapshot(injector, saves_dir, dry)?;
    // `is_file()` can fire while HOI4 still writes the snapshot — a parse
    // failure gets one settle-and-retry before giving up.
    let mut last_err = String::new();
    for attempt in 0..2 {
        match parse_snapshot(saves_dir) {
            Ok(save) => return Ok(pick_province(&save, p2s)),
            Err(e) => {
                last_err = e;
                if attempt == 0 {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
    Err(last_err)
}

/// Build the province→state map for a HOI4 install (empty on failure — the
/// pick resolution then reports QuietNoBattle instead of hard erroring).
pub fn load_p2s(hoi4_dir: &Path) -> HashMap<u32, u32> {
    tactical_map::load_province_to_state(&tactical_map::states_dir_of(hoi4_dir)).unwrap_or_default()
}

/// VP display names (province id → name) for the multi-battle picker labels;
/// non-VP provinces fall back to their id at display time.
/// Empty on failure — names are cosmetic. `simp_chinese` reads the
/// install's Chinese yml so the picker shows the player's own language.
pub fn load_vp_names(hoi4_dir: &Path, simp_chinese: bool) -> HashMap<u32, String> {
    tactical_map::load_vp_names(&tactical_map::vp_names_path(hoi4_dir, simp_chinese))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_save::{LandCombatData, LandCombatSideData};

    fn combat(location: u32, atk_tag: &str, def_tag: &str, units: u64) -> LandCombatData {
        LandCombatData {
            location,
            attacker: LandCombatSideData {
                unit_ids: (0..units).collect(),
                tags: vec![atk_tag.to_string()],
                ..Default::default()
            },
            defender: LandCombatSideData {
                unit_ids: (0..units).collect(),
                tags: vec![def_tag.to_string()],
                ..Default::default()
            },
        }
    }

    #[test]
    fn pick_no_marker_is_nopick() {
        let save = SaveGame::default();
        let p2s: HashMap<u32, u32> = HashMap::new();
        assert_eq!(pick_province(&save, &p2s), PickResolution::NoPick);
    }

    #[test]
    fn pick_matches_combat_in_picked_state() {
        let mut save = SaveGame {
            player: Some("ITA".to_string()),
            ..Default::default()
        };
        save.picked_states = vec![835];
        save.land_combats = vec![
            combat(1001, "ITA", "ETH", 3), // state 835 — picked
            combat(2002, "ITA", "ETH", 9), // state 842 — bigger but NOT picked
        ];
        let p2s: HashMap<u32, u32> = [(1001, 835), (2002, 842)].into_iter().collect();
        // The pick WINS over the larger battle (player choice is
        // authoritative; largest only disambiguates inside the picked state).
        assert_eq!(pick_province(&save, &p2s), PickResolution::Province(1001));
    }

    #[test]
    fn pick_quiet_state_reports_not_falls_back() {
        let mut save = SaveGame {
            player: Some("ITA".to_string()),
            ..Default::default()
        };
        save.picked_states = vec![800]; // quiet frontier state
        save.land_combats = vec![combat(1001, "ITA", "ETH", 3)]; // in state 835
        let p2s: HashMap<u32, u32> = [(1001, 835)].into_iter().collect();
        assert_eq!(
            pick_province(&save, &p2s),
            PickResolution::QuietNoBattle { state: 800 }
        );
    }

    #[test]
    fn pick_multi_combat_state_goes_to_picker_sorted() {
        let mut save = SaveGame {
            player: Some("ITA".to_string()),
            ..Default::default()
        };
        save.picked_states = vec![835];
        save.land_combats = vec![
            combat(1001, "ITA", "ETH", 2), // player attacks
            combat(1002, "ETH", "ITA", 5), // player DEFENDS (bigger → first)
        ];
        let p2s: HashMap<u32, u32> = [(1001, 835), (1002, 835)].into_iter().collect();
        // Several battles in the picked state → the player picks
        // (largest first, province id as tiebreak).
        match pick_province(&save, &p2s) {
            PickResolution::Multiple { state, battles } => {
                assert_eq!(state, 835);
                let provinces: Vec<u32> = battles.iter().map(|b| b.province).collect();
                assert_eq!(provinces, vec![1002, 1001]);
                assert_eq!(battles[0].attacker_units, 5);
                assert_eq!(battles[0].attacker_tags, vec!["ETH".to_string()]);
                assert_eq!(battles[0].defender_tags, vec!["ITA".to_string()]);
                // Per-battle side for the picker map colors.
                assert!(!battles[0].player_attacker);
                assert!(battles[1].player_attacker);
            }
            other => panic!("expected Multiple, got {other:?}"),
        }
    }

    #[test]
    fn pick_multi_tiebreaks_by_province_id() {
        let mut save = SaveGame {
            player: Some("ITA".to_string()),
            ..Default::default()
        };
        save.picked_states = vec![835];
        save.land_combats = vec![combat(1002, "ITA", "ETH", 3), combat(1001, "ITA", "ETH", 3)];
        let p2s: HashMap<u32, u32> = [(1001, 835), (1002, 835)].into_iter().collect();
        match pick_province(&save, &p2s) {
            PickResolution::Multiple { battles, .. } => {
                let provinces: Vec<u32> = battles.iter().map(|b| b.province).collect();
                assert_eq!(provinces, vec![1001, 1002]);
            }
            other => panic!("expected Multiple, got {other:?}"),
        }
    }
}
