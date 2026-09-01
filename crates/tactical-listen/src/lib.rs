//! `tactical-listen` — listens to HOI4's `game.log` for tactical trigger messages.
//!
//! Implements the external side of **Channel 1: game.log (Mod → External,
//! JSON Lines)** from DESIGN.md §3.1: the mod `log = "..."`s single-line JSON
//! objects (`{"type":"tac_...",...}`) into `game.log`, and this crate detects,
//! extracts, and parses them into [`LogMessage`] values.
//!
//! The mod emits the JSON wrapped in HOI4's standard log prefix and square
//! brackets — 1.19.2 shape, note the game-date-with-hour bracket:
//!
//! ```text
//! [23:59:59][1939.09.01.08][gamestate.cpp:123]: [{"type":"tac_start","ts":"1939.9.1.08","province":6334,"tag":"GER","leader_id":3,"attack_dirs":["NW","W"],"is_player_attacker":true}]
//! ```
//!
//! so [`parse_log_line`] extracts the substring from the first `{` to the
//! last `}` before parsing. Anything that does not yield one of the five
//! known message types (garbage lines, unrelated HOI4 output, unknown
//! `tac_*` types) is ignored gracefully. The prefix's game-date bracket is
//! ALSO used: heartbeat/tac_state lines whose JSON `hour` is the mod's 0
//! placeholder (1.19 has no scriptable current-hour value) inherit the real
//! game hour from the prefix (see [`game_hour_prefix`]).
//!
//! [`LogListener`] tails the file with `notify` (DESIGN.md §2.1 "Log
//! Listening"), reading only bytes appended since the last poll. Because HOI4
//! truncates `game.log` on every launch, a file that shrank below the last
//! read offset is re-read from the start (see [`resume_offset`]).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crossbeam_channel::{Receiver, TryRecvError};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

/// One tactical trigger message parsed from `game.log`.
///
/// Variants mirror the message catalog in DESIGN.md §3.1 exactly; field names
/// match the JSON keys written by the mod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogMessage {
    /// `tac_start` — player pressed F12 while in battle (DESIGN.md §3.1,
    /// §10.1). `province` is the contested province ID, `leader_id` the
    /// army leader, `attack_dirs` the attack directions (e.g. `["NW","W"]`)
    /// later used for frontline placement (DESIGN.md §4.1). The mod
    /// currently ships `province:0` — the external program takes the
    /// battle from the save's `combat.land_combat` record instead (see
    /// scenario::assemble_live). `is_player_attacker` is currently a
    /// constant `true` in the mod; missing (old logs) defaults to `true`
    /// for backward compatibility.
    TacStart {
        ts: String,
        province: u32,
        tag: String,
        leader_id: u64,
        attack_dirs: Vec<String>,
        is_player_attacker: bool,
    },
    /// `tac_abort` — player used the "Force Exit Tactical Mode" decision
    /// (DESIGN.md §3.1, §10.4).
    TacAbort { ts: String, tag: String },
    /// `tac_heartbeat` — `on_hourly` proof that the mod is alive during
    /// tactical mode (DESIGN.md §3.1, §10.5).
    TacHeartbeat { ts: String, tag: String, hour: u32 },
    /// `tac_state` — mod reports the current strategic state after each sync
    /// (DESIGN.md §3.1, §10.3).
    TacState {
        ts: String,
        tag: String,
        hour: u32,
        phase: String,
    },
    /// `tac_enemy_tactic` — enemy tactic on battle start or tactic change
    /// (DESIGN.md §3.1); drives the AI objective mapping of §7.2.
    TacEnemyTactic {
        ts: String,
        tag: String,
        enemy_tactic: String,
    },
    /// `tac_refresh` — player clicked "Refresh Battle Markers" (DESIGN.md
    /// §10.1): the external program snapshots the save (savegame),
    /// computes the states holding player land_combats and injects
    /// set_state_flag=tac_hot on exactly those states (+ tac_hot_count).
    /// No tag — read from the snapshot's `player=` key (the mod's
    /// interpolated tag cannot be trusted).
    TacRefresh { ts: String },
}

/// Parse a single `game.log` line into a [`LogMessage`].
///
/// The line may be bare JSON (the canonical form in DESIGN.md §3.1) or JSON
/// embedded in a full HOI4 log line (`[time][file:line]: [{...}]`, DESIGN.md
/// §10.1): the substring from the first `{` to the last `}` is extracted and
/// parsed. Returns `None` for garbage lines, non-tac JSON, unknown `type`
/// values, and known types with missing or wrongly-typed fields.
pub fn parse_log_line(line: &str) -> Option<LogMessage> {
    let start = line.find('{')?;
    let end = line.rfind('}')?;
    if end < start {
        return None;
    }
    let prefix_date = game_date_prefix(&line[..start]);
    let prefix_hour = game_hour_prefix(&line[..start]);
    let v: serde_json::Value = serde_json::from_str(&line[start..=end]).ok()?;
    match v.get("type")?.as_str()? {
        "tac_start" => Some(LogMessage::TacStart {
            ts: real_ts(&v, &prefix_date),
            province: u32::try_from(get_u64(&v, "province")?).ok()?,
            tag: real_tag(&v),
            leader_id: get_u64(&v, "leader_id")?,
            // The mod DROPPED attack_dirs (a literal `[...]` array cannot
            // survive HOI4's log interpolation) — fall back to the same
            // placeholder pair the mod used to ship.
            attack_dirs: match v.get("attack_dirs") {
                Some(a) => a
                    .as_array()?
                    .iter()
                    .map(|d| d.as_str().map(str::to_owned))
                    .collect::<Option<Vec<String>>>()?,
                None => vec!["W".to_owned(), "NW".to_owned()],
            },
            // Missing (old logs without the field) = true: the live MVP
            // assumption stays the backward-compatible default.
            is_player_attacker: v
                .get("is_player_attacker")
                .and_then(|b| b.as_bool())
                .unwrap_or(true),
        }),
        "tac_abort" => Some(LogMessage::TacAbort {
            ts: real_ts(&v, &prefix_date),
            tag: real_tag(&v),
        }),
        "tac_heartbeat" => Some(LogMessage::TacHeartbeat {
            ts: real_ts(&v, &prefix_date),
            tag: real_tag(&v),
            hour: real_hour(get_u64(&v, "hour")?, prefix_hour),
        }),
        "tac_state" => Some(LogMessage::TacState {
            ts: real_ts(&v, &prefix_date),
            tag: real_tag(&v),
            hour: real_hour(get_u64(&v, "hour")?, prefix_hour),
            phase: get_str(&v, "phase")?.to_owned(),
        }),
        "tac_enemy_tactic" => Some(LogMessage::TacEnemyTactic {
            ts: real_ts(&v, &prefix_date),
            tag: real_tag(&v),
            enemy_tactic: get_str(&v, "enemy_tactic")?.to_owned(),
        }),
        "tac_refresh" => Some(LogMessage::TacRefresh {
            ts: real_ts(&v, &prefix_date),
        }),
        // Unknown types (e.g. tac_damage_applied / tac_battle_ended from
        // DESIGN.md §10.2, or future additions) are ignored gracefully.
        _ => None,
    }
}

fn get_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn get_u64(v: &serde_json::Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

/// Tag from the mod, with repair: the mangled interpolation prints `"0"`,
/// which is not a country — treat it as absent (empty = infer from save).
fn real_tag(v: &serde_json::Value) -> String {
    match get_str(v, "tag") {
        Some(s) if s != "0" => s.to_owned(),
        _ => String::new(),
    }
}

/// JSON ts with the log-prefix fallback: the mod drops `ts` (the
/// `[?ROOT.date]` form prints `0` — `[?]` is the VARIABLE syntax), so a
/// missing or `"0"` ts inherits the line prefix's full game date
/// (`1936.01.01.12`); a bare JSON line without a prefix gets `""`.
fn real_ts(v: &serde_json::Value, prefix_date: &Option<String>) -> String {
    match get_str(v, "ts") {
        Some(s) if !s.is_empty() && s != "0" => s.to_owned(),
        _ => prefix_date.clone().unwrap_or_default(),
    }
}

/// JSON hour with the log-prefix fallback: the mod ships `hour:0` (1.19 has
/// no scriptable current-hour value), so a 0 inherits the game-date prefix
/// hour when the line carries one; a non-zero JSON hour always wins.
fn real_hour(json_hour: u64, prefix_hour: Option<u32>) -> u32 {
    let json = u32::try_from(json_hour).unwrap_or(0);
    if json != 0 {
        json
    } else {
        prefix_hour.unwrap_or(0)
    }
}

/// Hour component of a `yyyy.mm.dd.hh` game-date string.
fn hour_of_date(date: &str) -> Option<u32> {
    date.rsplit('.').next()?.parse().ok()
}

/// 1.19 `game.log` lines carry the game date (with hour) in a prefix
/// bracket: `[03:30:39][1936.01.01.12][effectbase.cpp:1783]: ...`. Return
/// whichever bracket holds four dot-separated numeric groups (the wall
/// clock `[03:30:39]` has colons, the file bracket has one dot).
pub fn game_date_prefix(prefix: &str) -> Option<String> {
    for seg in prefix.split('[').skip(1) {
        let Some(seg) = seg.split(']').next() else {
            continue;
        };
        let parts: Vec<&str> = seg.split('.').collect();
        if parts.len() == 4
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            return Some(seg.to_owned());
        }
    }
    None
}

/// Hour from the line prefix's game-date bracket (see [`game_date_prefix`]).
pub fn game_hour_prefix(prefix: &str) -> Option<u32> {
    game_date_prefix(prefix).and_then(|d| hour_of_date(&d))
}

/// Clock-advance receipt (DESIGN §8.4): the game-date prefix
/// (`yyyy.mm.dd.hh`) of the LAST line in `text` containing `marker` — the
/// freshest probe line's stamp, i.e. the game hour HOI4 currently sits at.
/// `None` when no line carries the marker, or the newest marker line has
/// no date bracket (no fallback to older lines: a stale compare is worse
/// than an unknown).
pub fn marker_date_prefix(text: &str, marker: &str) -> Option<String> {
    text.lines()
        .rev()
        .find(|line| line.contains(marker))
        .and_then(game_date_prefix)
}

/// Offset-reset rule for truncation/rotation (DESIGN.md §3.1 resilience:
/// HOI4 truncates `game.log` on every launch). If the file shrank below our
/// last read offset, the bytes we remember are gone — restart from 0.
fn resume_offset(file_size: u64, last_offset: u64) -> u64 {
    if file_size < last_offset {
        0
    } else {
        last_offset
    }
}

/// Seek-based tail reader: remembers how far the file has been consumed and
/// yields only messages from bytes appended since. Kept free of any `notify`
/// types so the tail logic is testable without a filesystem listener.
struct TailReader {
    path: PathBuf,
    offset: u64,
    /// Bytes of a trailing line not yet terminated by `\n`.
    partial: Vec<u8>,
}

impl TailReader {
    fn new(path: PathBuf, offset: u64) -> TailReader {
        TailReader {
            path,
            offset,
            partial: Vec::new(),
        }
    }

    /// The file's current length differs from our read offset: the
    /// correctness fallback when a `notify` event is lost or coalesced —
    /// which happens routinely on Windows with a writer holding the file
    /// open. Also true after truncation/rotation (offset > size), which
    /// [`resume_offset`] then handles.
    fn grew(&self) -> bool {
        std::fs::metadata(&self.path)
            .map(|m| m.len() != self.offset)
            .unwrap_or(false)
    }

    /// Read all complete lines appended since the last call and parse them.
    ///
    /// A missing/unreadable file (e.g. deleted between polls, or HOI4 not yet
    /// started) simply yields no messages. A truncated file is re-read from
    /// the beginning per [`resume_offset`].
    fn read_new(&mut self) -> Vec<LogMessage> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let size = match file.metadata() {
            Ok(m) => m.len(),
            Err(_) => return Vec::new(),
        };
        let resume = resume_offset(size, self.offset);
        if resume != self.offset {
            // Truncated/rotated: the buffered partial line belonged to the
            // old stream and is meaningless now.
            self.partial.clear();
        }
        self.offset = resume;
        if size == self.offset {
            return Vec::new();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);

        let mut messages = Vec::new();
        let mut consumed = 0;
        for (i, &b) in self.partial.iter().enumerate() {
            if b == b'\n' {
                if let Ok(line) = std::str::from_utf8(&self.partial[consumed..i]) {
                    if let Some(msg) = parse_log_line(line) {
                        messages.push(msg);
                    }
                }
                consumed = i + 1;
            }
        }
        self.partial.drain(..consumed);
        messages
    }
}

/// Listens to HOI4's `game.log` for new tactical trigger messages
/// (DESIGN.md §3.1).
///
/// A `notify` [`RecommendedWatcher`] on the log's parent directory feeds a
/// crossbeam channel; [`poll`](LogListener::poll) drains that channel and, if
/// the log file was touched, tail-reads the newly appended bytes and parses
/// complete lines. No background threads of our own: the caller drives the
/// loop (e.g. once per frame).
///
/// By default the listener starts at the current end of the file so ancient
/// triggers from previous HOI4 sessions are not replayed on startup.
pub struct LogListener {
    /// Kept alive for the listener's lifetime; events stop if it is dropped.
    _listener: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    tail: TailReader,
    /// File name used to recognize events for our log file.
    file_name: std::ffi::OsString,
}

impl LogListener {
    /// Listen to `path`, starting at the current end of the file (default; no
    /// replay of old triggers — DESIGN.md §3.1 usage note).
    ///
    /// Errors if `path`'s parent directory cannot be listened to (e.g. the HOI4
    /// `logs` directory does not exist yet).
    pub fn new(path: PathBuf) -> notify::Result<LogListener> {
        LogListener::start_at_end(path)
    }

    /// Listen to `path`, starting at the current end of the file.
    pub fn start_at_end(path: PathBuf) -> notify::Result<LogListener> {
        let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        LogListener::build(path, offset)
    }

    /// Listen to `path`, reading from the beginning of the file (replays any
    /// triggers already present; mainly useful for tests and diagnostics).
    pub fn from_beginning(path: PathBuf) -> notify::Result<LogListener> {
        LogListener::build(path, 0)
    }

    fn build(path: PathBuf, offset: u64) -> notify::Result<LogListener> {
        let listen_dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let file_name = path.file_name().unwrap_or_default().to_os_string();

        // Bounded channel: an unbounded channel can grow without limit on a
        // notify event burst; the per-poll drain plus the `grew()` fallback
        // already make dropped events harmless, so `try_send` on a full
        // queue degrades to "file grew → full re-read" instead of unbounded
        // memory.
        let (tx, rx) = crossbeam_channel::bounded(1024);
        let mut listener = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                // Receiver may be gone if the LogListener was dropped; the
                // listener itself is dropped right after, so ignoring send
                // errors is fine.
                let _ = tx.try_send(res);
            },
            Config::default(),
        )?;
        // Listen on the parent directory rather than the file itself: HOI4 may
        // recreate game.log on launch, and a file-level registration can go
        // stale across delete/recreate cycles.
        listener.watch(&listen_dir, RecursiveMode::NonRecursive)?;

        Ok(LogListener {
            _listener: listener,
            rx,
            tail: TailReader::new(path, offset),
            file_name,
        })
    }

    /// Drain pending filesystem events; if the log file was touched, read and
    /// parse all complete lines appended since the last poll.
    ///
    /// Returns an empty `Vec` when nothing relevant happened (the common
    /// case), when the file is missing, or when new lines contain no
    /// recognizable tac messages.
    pub fn poll(&mut self) -> Vec<LogMessage> {
        let mut touched = false;
        loop {
            match self.rx.try_recv() {
                Ok(Ok(event)) => {
                    if self.matches_target(&event) {
                        touched = true;
                    }
                }
                // A notify error (e.g. event queue overflow) may mean missed
                // events — rescan conservatively.
                Ok(Err(_)) => touched = true,
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        // A lost/coalesced notify event must not blind the listener — the
        // file-length check re-arms every poll, capping the worst-case
        // latency at one poll interval. (A single lost event can otherwise
        // leave a tac_start unread indefinitely despite the caller polling
        // at full rate.)
        if !touched && self.tail.grew() {
            touched = true;
        }
        if touched {
            self.tail.read_new()
        } else {
            Vec::new()
        }
    }

    fn matches_target(&self, event: &Event) -> bool {
        event
            .paths
            .iter()
            .any(|p| p.file_name() == Some(self.file_name.as_os_str()))
    }
}

/// Default location of HOI4's `game.log` (DESIGN.md §12: `hoi4_log_path:
/// "auto"` resolves via known Windows user directories):
/// `%USERPROFILE%\Documents\Paradox Interactive\Hearts of Iron IV\logs\game.log`.
///
/// Returns `Some` only when `%USERPROFILE%` is set and the file actually
/// exists; `None` otherwise (e.g. non-Windows host, or HOI4 never ran).
pub fn detect_log_path() -> Option<PathBuf> {
    let user_profile = std::env::var_os("USERPROFILE")?;
    let path = PathBuf::from(user_profile)
        .join("Documents")
        .join("Paradox Interactive")
        .join("Hearts of Iron IV")
        .join("logs")
        .join("game.log");
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique temp file path per call (tests run in parallel).
    fn temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tactical_listen_{}_{}_{}.log",
            tag,
            std::process::id(),
            n
        ))
    }

    // --- parse_log_line: the five catalog messages (DESIGN.md §3.1) ---

    #[test]
    fn parses_tac_start_example_line() {
        let line = r#"{"type":"tac_start",  "ts":"1939.9.1.08","province":6334,  "tag":"GER","leader_id":3,      "attack_dirs":["NW","W"]}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacStart {
                ts: "1939.9.1.08".to_owned(),
                province: 6334,
                tag: "GER".to_owned(),
                leader_id: 3,
                attack_dirs: vec!["NW".to_owned(), "W".to_owned()],
                // Field absent (old logs without it) → backward-compatible default true.
                is_player_attacker: true,
            })
        );
    }

    #[test]
    fn parses_tac_start_player_defending() {
        let line = r#"{"type":"tac_start","ts":"1936.1.1.13","province":0,"tag":"ITA","leader_id":0,"attack_dirs":["W","NW"],"is_player_attacker":false}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacStart {
                ts: "1936.1.1.13".to_owned(),
                province: 0,
                tag: "ITA".to_owned(),
                leader_id: 0,
                attack_dirs: vec!["W".to_owned(), "NW".to_owned()],
                is_player_attacker: false,
            })
        );
    }

    #[test]
    fn parses_tac_abort_example_line() {
        let line = r#"{"type":"tac_abort",  "ts":"1939.9.1.09","tag":"GER"}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacAbort {
                ts: "1939.9.1.09".to_owned(),
                tag: "GER".to_owned(),
            })
        );
    }

    #[test]
    fn parses_tac_refresh_with_prefix_date_fallback() {
        // The mod ships a bare {"type":"tac_refresh"} — ts inherits the log
        // line's [yyyy.mm.dd.hh] prefix.
        let line = r#"[13:58:58][1936.01.02.04][effectbase.cpp:1783]: {"type":"tac_refresh"}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacRefresh {
                ts: "1936.01.02.04".to_owned(),
            })
        );
    }

    #[test]
    fn parses_tac_heartbeat_example_line() {
        let line = r#"{"type":"tac_heartbeat","ts":"1939.9.1.10","tag":"GER","hour":3}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacHeartbeat {
                ts: "1939.9.1.10".to_owned(),
                tag: "GER".to_owned(),
                hour: 3,
            })
        );
    }

    #[test]
    fn parses_tac_state_example_line() {
        let line =
            r#"{"type":"tac_state",  "ts":"1939.9.1.10","tag":"GER","hour":3,"phase":"active"}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacState {
                ts: "1939.9.1.10".to_owned(),
                tag: "GER".to_owned(),
                hour: 3,
                phase: "active".to_owned(),
            })
        );
    }

    #[test]
    fn parses_tac_enemy_tactic_example_line() {
        let line =
            r#"{"type":"tac_enemy_tactic","ts":"1939.9.1.10","tag":"GER","enemy_tactic":"blitz"}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacEnemyTactic {
                ts: "1939.9.1.10".to_owned(),
                tag: "GER".to_owned(),
                enemy_tactic: "blitz".to_owned(),
            })
        );
    }

    // --- parse_log_line: robustness ---

    #[test]
    fn garbage_lines_are_ignored() {
        for line in [
            "",
            "   ",
            "ordinary HOI4 log output with no json at all",
            "[23:59:59][gamestate.cpp:123]: some normal message",
            "{broken json without closing brace",
            "}reversed braces{",
            "[1,2,3]",
            "\"just a string\"",
            r#"{"no_type_field":true}"#,
            // Missing required fields for a known type.
            r#"{"type":"tac_start","ts":"1939.9.1.08"}"#,
            r#"{"type":"tac_heartbeat","ts":"1939.9.1.10","tag":"GER"}"#,
            // Wrongly-typed fields.
            r#"{"type":"tac_state","ts":"x","tag":"GER","hour":"three","phase":"active"}"#,
        ] {
            assert_eq!(
                parse_log_line(line),
                None,
                "line should be ignored: {line:?}"
            );
        }
    }

    #[test]
    fn json_embedded_in_hoi4_log_line_is_extracted() {
        // The exact shape the mod produces (DESIGN.md §10.1): HOI4 prefix,
        // JSON wrapped in square brackets.
        let line = r#"[23:59:59][gamestate.cpp:123]: [{"type":"tac_start","ts":"1939.9.1.08","province":6334,"tag":"GER","leader_id":3,"attack_dirs":["NW","W"]}]"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacStart {
                ts: "1939.9.1.08".to_owned(),
                province: 6334,
                tag: "GER".to_owned(),
                leader_id: 3,
                attack_dirs: vec!["NW".to_owned(), "W".to_owned()],
                is_player_attacker: true,
            })
        );
    }

    // --- game-date prefix hour (1.19 line shape; mod ships hour:0) ---

    #[test]
    fn heartbeat_zero_hour_inherits_game_date_prefix() {
        // Real 1.19.2 prefix shape captured from game.log: wall clock, then
        // the game date with hour, then the source file.
        let line = r#"[03:30:39][1936.01.01.12][effectbase.cpp:1783]: [{"type":"tac_heartbeat","ts":"1936.1.1.12","tag":"ITA","hour":0}]"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacHeartbeat {
                ts: "1936.1.1.12".to_owned(),
                tag: "ITA".to_owned(),
                hour: 12,
            })
        );
    }

    #[test]
    fn heartbeat_nonzero_json_hour_wins_over_prefix() {
        let line = r#"[03:30:39][1936.01.01.12][effectbase.cpp:1783]: [{"type":"tac_heartbeat","ts":"1936.1.1.12","tag":"ITA","hour":5}]"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacHeartbeat {
                ts: "1936.1.1.12".to_owned(),
                tag: "ITA".to_owned(),
                hour: 5,
            })
        );
    }

    #[test]
    fn tac_state_zero_hour_inherits_prefix_and_bare_json_stays_zero() {
        let prefixed = r#"[01:02:03][1936.01.02.07][effectbase.cpp:1]: [{"type":"tac_state","ts":"1936.1.2.07","tag":"ITA","hour":0,"phase":"synced"}]"#;
        assert_eq!(
            parse_log_line(prefixed),
            Some(LogMessage::TacState {
                ts: "1936.1.2.07".to_owned(),
                tag: "ITA".to_owned(),
                hour: 7,
                phase: "synced".to_owned(),
            })
        );
        // Bare JSON (no prefix — tests, piped logs) keeps the mod's 0.
        let bare =
            r#"{"type":"tac_state","ts":"1936.1.2.07","tag":"ITA","hour":0,"phase":"synced"}"#;
        assert_eq!(
            parse_log_line(bare),
            Some(LogMessage::TacState {
                ts: "1936.1.2.07".to_owned(),
                tag: "ITA".to_owned(),
                hour: 0,
                phase: "synced".to_owned(),
            })
        );
    }

    #[test]
    fn game_hour_prefix_ignores_wallclock_and_file_brackets() {
        assert_eq!(game_hour_prefix(""), None);
        assert_eq!(game_hour_prefix("[03:30:39]"), None);
        assert_eq!(game_hour_prefix("[03:30:39][effectbase.cpp:1783]: "), None);
        assert_eq!(
            game_hour_prefix("[03:30:39][1936.01.01.12][effectbase.cpp:1783]: "),
            Some(12)
        );
        // Day/hour with no zero-padding still parses.
        assert_eq!(
            game_hour_prefix("[03:30:39][1936.1.1.6][x.cpp:1]: "),
            Some(6)
        );
    }

    #[test]
    fn marker_date_prefix_picks_the_newest_marker_line() {
        // Clock receipt: interleaved spam is skipped, the LAST marker
        // line's date bracket wins (here: hour advanced 12 → 13).
        let text = concat!(
            "[01:00:00][1936.01.01.12][effectbase.cpp:1783]: TAC_CLOCK_1_0\n",
            "[01:00:01][1936.01.01.12][effectbase.cpp:1783]: other log spam\n",
            "[01:00:02][1936.01.01.13][effectbase.cpp:1783]: TAC_CLOCK_1_1\n",
            "[01:00:03][effectbase.cpp:1783]: trailing undated line\n",
        );
        assert_eq!(
            marker_date_prefix(text, "TAC_CLOCK_"),
            Some("1936.01.01.13".to_owned())
        );
        // No marker line at all → None.
        assert_eq!(marker_date_prefix(text, "TAC_NOPE_"), None);
        assert_eq!(marker_date_prefix("", "TAC_CLOCK_"), None);
        // The newest marker line lacking a date bracket → None (no
        // fallback to older marker lines).
        assert_eq!(
            marker_date_prefix(
                "[01:00:00][1936.01.01.12][x.cpp:1]: TAC_CLOCK_1_0\n[01:00:01]: TAC_CLOCK_1_1\n",
                "TAC_CLOCK_"
            ),
            None
        );
    }

    // --- the mod drops ts/tag/attack_dirs (interpolation-fragile) ---

    #[test]
    fn round9_minimal_tac_start_line_gets_external_fallbacks() {
        // The exact shape the current mod emits (no ts/tag/attack_dirs).
        let line = r#"[13:58:58][1936.01.01.13][effectbase.cpp:1783]: {"type":"tac_start","province":0,"leader_id":0,"is_player_attacker":true}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacStart {
                // ts inherits the prefix game date…
                ts: "1936.01.01.13".to_owned(),
                province: 0,
                // …tag stays empty (the live assembler takes the save's
                // root `player="TAG"` instead)…
                tag: String::new(),
                leader_id: 0,
                // …and dirs fall back to the mod's old placeholder pair.
                attack_dirs: vec!["W".to_owned(), "NW".to_owned()],
                is_player_attacker: true,
            })
        );
    }

    #[test]
    fn round9_broken_interpolation_values_are_repaired() {
        // Lines with the mangled `"ts":"0"`/`"tag":"0"` interpolation
        // output: "0" is treated as missing and repaired from the prefix.
        let line = r#"[13:58:58][1936.01.01.13][effectbase.cpp:1783]: {"type":"tac_heartbeat","ts":"0","tag":"0","hour":0}"#;
        assert_eq!(
            parse_log_line(line),
            Some(LogMessage::TacHeartbeat {
                ts: "1936.01.01.13".to_owned(),
                tag: String::new(),
                hour: 13,
            })
        );
    }

    #[test]
    fn round9_minimal_enemy_tactic_and_abort_lines() {
        let tactic = r#"{"type":"tac_enemy_tactic","enemy_tactic":"default"}"#;
        assert_eq!(
            parse_log_line(tactic),
            Some(LogMessage::TacEnemyTactic {
                ts: String::new(),
                tag: String::new(),
                enemy_tactic: "default".to_owned(),
            })
        );
        let abort = r#"[01:02:03][1936.01.02.07][effectbase.cpp:1]: {"type":"tac_abort"}"#;
        assert_eq!(
            parse_log_line(abort),
            Some(LogMessage::TacAbort {
                ts: "1936.01.02.07".to_owned(),
                tag: String::new(),
            })
        );
    }

    #[test]
    fn unknown_types_are_ignored() {
        // Emitted by the mod (DESIGN.md §10.2) but not part of the §3.1
        // catalog consumed by this crate.
        for line in [
            r#"{"type":"tac_unknown","ts":"1939.9.1.10","tag":"GER"}"#,
            r#"{"type":"tac_damage_applied","org":"0.15","str":"0.08","ts":"1939.9.1.10"}"#,
            r#"{"type":"tac_battle_ended","ts":"1939.9.1.11","result":"attacker_victory"}"#,
        ] {
            assert_eq!(
                parse_log_line(line),
                None,
                "line should be ignored: {line:?}"
            );
        }
    }

    // --- truncation / offset-reset logic (pure helper) ---

    #[test]
    fn resume_offset_resets_only_when_file_shrank() {
        // File truncated below our read position (HOI4 relaunch) → from 0.
        assert_eq!(resume_offset(10, 50), 0);
        assert_eq!(resume_offset(0, 50), 0);
        // Unchanged or grown file → keep reading where we stopped.
        assert_eq!(resume_offset(50, 50), 50);
        assert_eq!(resume_offset(100, 50), 50);
        // Fresh file, nothing read yet.
        assert_eq!(resume_offset(0, 0), 0);
        assert_eq!(resume_offset(12, 0), 0);
    }

    // --- tail reading against a real temp file (no notify involved) ---
    #[test]
    fn tail_reads_only_appended_lines() {
        let path = temp_path("tail_append");
        fs::write(&path, "ancient trigger line\n").unwrap();

        // Start at end: pre-existing content is not replayed.
        let offset = fs::metadata(&path).unwrap().len();
        let mut tail = TailReader::new(path.clone(), offset);
        assert!(tail.read_new().is_empty());

        // Append a heartbeat and an abort; only the new lines are parsed.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"tac_heartbeat","ts":"1939.9.1.10","tag":"GER","hour":3}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"tac_abort","ts":"1939.9.1.11","tag":"GER"}}"#
        )
        .unwrap();
        drop(f);

        assert_eq!(
            tail.read_new(),
            vec![
                LogMessage::TacHeartbeat {
                    ts: "1939.9.1.10".to_owned(),
                    tag: "GER".to_owned(),
                    hour: 3,
                },
                LogMessage::TacAbort {
                    ts: "1939.9.1.11".to_owned(),
                    tag: "GER".to_owned(),
                },
            ]
        );
        // Nothing new since → empty.
        assert!(tail.read_new().is_empty());

        fs::remove_file(&path).ok();
    }

    #[test]
    fn grew_detects_appends_and_truncation() {
        // The file-length fallback behind poll() — a lost notify event must
        // never leave new content unread.
        let path = temp_path("tail_grew");
        fs::write(&path, "seed\n").unwrap();
        let offset = fs::metadata(&path).unwrap().len();
        let mut tail = TailReader::new(path.clone(), offset);
        assert!(!tail.grew());

        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "more").unwrap();
        drop(f);
        assert!(tail.grew());
        tail.read_new();
        assert!(!tail.grew());

        // Truncation (offset > size) also counts — resume_offset re-reads.
        fs::write(&path, "x\n").unwrap();
        assert!(tail.grew());

        fs::remove_file(&path).ok();
    }

    #[test]
    fn poll_reads_despite_lost_notify_event() {
        // Drain the event channel to simulate a lost/coalesced notify event —
        // poll() must still read the appended line via the file-length
        // fallback.
        let path = temp_path("poll_lost_event");
        fs::write(&path, "seed\n").unwrap();
        let mut listener = LogListener::start_at_end(path.clone()).unwrap();

        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"tac_abort","ts":"1939.9.1.11","tag":"GER"}}"#
        )
        .unwrap();
        drop(f);
        // Let the watcher deliver, then drain-and-discard every pending
        // event = the event is "lost" as far as poll() can tell.
        std::thread::sleep(std::time::Duration::from_millis(400));
        while listener.rx.try_recv().is_ok() {}

        assert_eq!(
            listener.poll(),
            vec![LogMessage::TacAbort {
                ts: "1939.9.1.11".to_owned(),
                tag: "GER".to_owned(),
            }]
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_recovers_from_truncation() {
        let path = temp_path("tail_truncate");
        fs::write(
            &path,
            concat!(
                r#"{"type":"tac_start","ts":"1939.9.1.08","province":6334,"#,
                r#""tag":"GER","leader_id":3,"attack_dirs":["NW"]}"#,
                "\n"
            ),
        )
        .unwrap();

        let mut tail = TailReader::new(path.clone(), 0);
        assert_eq!(tail.read_new().len(), 1);

        // HOI4 relaunches and truncates game.log: the new content is shorter
        // than our last offset, so the tail must restart from the beginning.
        fs::write(
            &path,
            "{\"type\":\"tac_abort\",\"ts\":\"1\",\"tag\":\"F\"}\n",
        )
        .unwrap();
        assert_eq!(
            tail.read_new(),
            vec![LogMessage::TacAbort {
                ts: "1".to_owned(),
                tag: "F".to_owned(),
            }]
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn incomplete_line_is_buffered_until_terminated() {
        let path = temp_path("tail_partial");
        // First half of a line, no newline yet.
        fs::write(&path, r#"{"type":"tac_abort","ts":"1939.9.1.09","#).unwrap();

        let mut tail = TailReader::new(path.clone(), 0);
        assert!(tail.read_new().is_empty());

        // Rest of the line arrives later; only then is it parsed.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "\"tag\":\"GER\"}}\r\n").unwrap();
        drop(f);

        assert_eq!(
            tail.read_new(),
            vec![LogMessage::TacAbort {
                ts: "1939.9.1.09".to_owned(),
                tag: "GER".to_owned(),
            }]
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_on_missing_file_yields_nothing() {
        let path = temp_path("tail_missing"); // never created
        let mut tail = TailReader::new(path, 0);
        assert!(tail.read_new().is_empty());
    }
}
