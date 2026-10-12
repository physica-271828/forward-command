//! The scan→publish round trip (DESIGN.md §10.7).
//!
//! The player clicks "Scan for Commandable Battles"; the mod logs `tac_scan`
//! and arms a scan-waiting flag; this module snapshots the save, derives the
//! enterable battles ([`crate::enterable::enterable_battles`] — the same
//! judgment the post-pick resolution uses) and publishes their states into
//! the decision layer via `activate_targeted_decision`, one `eval_effect`
//! line per state (HOI4 silently ignores a second effect on the same line).
//! Activation bypasses the decision's always-false candidate trigger and
//! lights the map icon instantly, no interface reload needed.
//!
//! The receipt half of the round trip answers that waiting flag — and stops
//! the clock. The click carries the country tag, so the answer can go out
//! FIRST, before the snapshot and parse — seconds of real time, during
//! which the mod's in-game watchdog already counts game hours at max game
//! speed; an answer that arrived only after the parse would read as "no
//! external program is running" and pop its warning. The receipt also
//! pauses the game (`gamespeed 0`): the scan exists so the player can pick
//! a state on the map, and a clock still running would race that watchdog
//! and let the map change under the scan. The answer is unconditional, too:
//! a scan that finds no commandable battle still clears the flag and pops
//! the mod's empty-scan notice, so a scan this program received is never
//! answered with silence.
//!
//! The pause lands only as early as the tag allows. A scan that carried a
//! usable tag is answered before the snapshot, so the game is already
//! stopped while the save is read; the tagless fallback (an old log, or one
//! this crate rejected) can only be answered after the parse, so its
//! receipt — pause included — rides the publish batch and the snapshot
//! itself still runs against a live clock.
//!
//! The click's tag is RECEIPT-ONLY: every activation is scoped to the
//! save's own `player=` key, the same tag the enterable judgment resolved
//! the player's side against, so the decision layer is never pointed at a
//! country the snapshot does not describe.
//!
//! A publish is SAME-DAY ONLY by design: the engine's daily decision pulse
//! re-evaluates the candidate trigger and drops every activated record at
//! midnight. The pulse is the background retraction mechanism — stale icons
//! left by a crashed session clean themselves up, and a fresh scan
//! re-publishes from the save of the moment. There is deliberately no
//! cross-day keep-alive. One scripted retraction rides on top: the moment a
//! battle actually launches, the caller retracts every state the last scan
//! published ([`retract_lines`]) so a finished battle never leaves its
//! entry decisions lit.

use tactical_save::SaveParser;

use crate::settings::AppSettings;

/// Result of one scan round trip: how many states were published, the
/// published set itself and the save's player tag the activations were
/// scoped to (the battle-launch retraction sweep needs both).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub published: usize,
    pub states: Vec<u32>,
    pub tag: String,
}

/// The injection lines publishing `states`: sorted, deduplicated, one
/// activate per line.
pub fn publish_lines(tag: &str, states: &[u32]) -> Vec<String> {
    let mut states = states.to_vec();
    states.sort_unstable();
    states.dedup();
    states
        .into_iter()
        .map(|state| {
            format!(
                "eval_effect {tag} = {{ activate_targeted_decision = {{ target = {state} decision = tactical_command_entry }} }}"
            )
        })
        .collect()
}

/// The injection lines RETRACTING `states` (one remove per line, same
/// shape as [`publish_lines`]) — fired the moment a battle actually
/// launches, so a finished battle leaves none of the published entry
/// decisions lit. Scoped to the same tag the activations used.
pub fn retract_lines(tag: &str, states: &[u32]) -> Vec<String> {
    let mut states = states.to_vec();
    states.sort_unstable();
    states.dedup();
    states
        .into_iter()
        .map(|state| {
            format!(
                "eval_effect {tag} = {{ remove_targeted_decision = {{ target = {state} decision = tactical_command_entry }} }}"
            )
        })
        .collect()
}

/// The receipt answering one scanned click: pause the game, then clear the
/// mod's scan-waiting flag.
///
/// The pause leads because a running clock is what the scan is working
/// against: the player is about to pick a state on the map, and while the
/// external program reads the save the game would race the mod's in-game
/// watchdog and let the map change under the scan. `gamespeed 0` is the
/// vanilla-documented immediate pause (vanilla
/// `console_commands_documentation.md`: "Set the current game speed,
/// pausing the game at speed 0") — a console command rather than an effect,
/// so it rides this console batch directly.
pub fn receipt_lines(tag: &str) -> Vec<String> {
    vec![
        "gamespeed 0".to_string(),
        format!("eval_effect {tag} = {{ clr_country_flag = tac_scan_waiting }}"),
    ]
}

/// Receipt line for a scan that found no commandable battle: the mod's
/// visible "nothing to command right now" popup, in the `event <id> <tag>`
/// console form the quiet-state pick already uses.
pub fn empty_scan_line(tag: &str) -> String {
    format!("event tac_scan.2 {tag}")
}

/// The batch answering one scan, in injection order: the receipt (fallback
/// path only — a scan that carried its tag was already answered before the
/// snapshot), then the per-state activations, then the empty-scan popup
/// when nothing came up. The returned count is the activate lines
/// ([`ScanOutcome::published`]).
///
/// Two tags, deliberately: `publish_tag` scopes the activations and is
/// always the save's own `player=` key — the tag the enterable judgment
/// resolved the player's side against, so the decision layer is never
/// pointed at anything else. `receipt_tag` scopes the mod-facing lines
/// only: the click's tag when the log carried one, the save's otherwise.
fn scan_batch(
    publish_tag: &str,
    receipt_tag: &str,
    states: &[u32],
    fallback_ack: bool,
) -> (Vec<String>, usize) {
    let publish = publish_lines(publish_tag, states);
    let published = publish.len();
    let mut lines = if fallback_ack {
        receipt_lines(receipt_tag)
    } else {
        Vec::new()
    };
    lines.extend(publish);
    if states.is_empty() {
        lines.push(empty_scan_line(receipt_tag));
    }
    (lines, published)
}

/// Snapshot the save, derive the enterable states and inject the publish
/// batch.
///
/// `tag_hint` is the country tag off the `tac_scan` log line, and it drives
/// the RECEIPT only: a non-empty hint means the receipt is injected FIRST,
/// before the snapshot, so the mod stops counting its waiting window while
/// the snapshot and parse run. An empty hint (a log from before the tag, or
/// one this crate rejected) falls back to the save's own `player=` tag, and
/// the receipt then rides the publish batch instead. The save has to parse
/// either way: the enterable judgment resolves the player's side against its
/// `player=` key, that key is what every activation is scoped to, and a
/// snapshot without one stays an error.
pub fn scan_and_publish(
    settings: &AppSettings,
    dry: bool,
    tag_hint: &str,
) -> Result<ScanOutcome, String> {
    let hoi4 = settings
        .hoi4_dir()
        .or_else(tactical_map::detect_hoi4_dir)
        .ok_or_else(|| "HOI4 install directory not found".to_string())?;
    let saves = settings
        .saves_dir()
        .ok_or_else(|| "save directory not configured".to_string())?;
    let injector =
        crate::settings::live_channel_injector(Some(&saves), settings.log_path().as_deref());

    // The receipt goes out ahead of the snapshot, pause included: at max
    // game speed the mod's waiting window is a few real seconds, and the
    // snapshot plus parse takes longer than that — the game must be stopped
    // before the save is read, not after.
    if !tag_hint.is_empty() {
        let receipt = receipt_lines(tag_hint);
        injector
            .inject_commands(&receipt, None, dry)
            .map_err(|e| format!("scan receipt injection failed: {e}"))?;
    }

    // A FRESH save is required: the scan answers "what is running RIGHT NOW".
    let path = crate::snapshot::take_scan_snapshot(&injector, &saves, dry)?;
    let save =
        SaveParser::parse_save(&path).map_err(|e| format!("scan snapshot parse failed: {e}"))?;
    let save_tag = save
        .player
        .clone()
        .ok_or_else(|| "scan snapshot carries no player tag".to_string())?;
    let p2s = crate::snapshot::load_p2s(&hoi4);
    let states: Vec<u32> = crate::enterable::enterable_battles(&save, &p2s, &save_tag)
        .iter()
        .map(|b| b.state)
        .collect();
    // The click's tag answers the mod; the save's tag is the truth the
    // judgment above resolved against — and the one the decision layer is
    // published under.
    let receipt_tag = if tag_hint.is_empty() {
        save_tag.as_str()
    } else {
        tag_hint
    };
    let (lines, published) = scan_batch(&save_tag, receipt_tag, &states, tag_hint.is_empty());
    injector
        .inject_commands(&lines, None, dry)
        .map_err(|e| format!("publish injection failed: {e}"))?;
    Ok(ScanOutcome {
        published,
        states,
        tag: save_tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_activates_each_state_once_sorted() {
        let lines = publish_lines("JAP", &[1028, 408]);
        assert_eq!(
            lines,
            vec![
                "eval_effect JAP = { activate_targeted_decision = { target = 408 decision = tactical_command_entry } }"
                    .to_string(),
                "eval_effect JAP = { activate_targeted_decision = { target = 1028 decision = tactical_command_entry } }"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn duplicate_states_collapse() {
        assert_eq!(publish_lines("JAP", &[5, 5, 5]).len(), 1);
    }

    #[test]
    fn retract_removes_each_state_once_sorted() {
        let lines = retract_lines("JAP", &[1028, 408, 408]);
        assert_eq!(
            lines,
            vec![
                "eval_effect JAP = { remove_targeted_decision = { target = 408 decision = tactical_command_entry } }"
                    .to_string(),
                "eval_effect JAP = { remove_targeted_decision = { target = 1028 decision = tactical_command_entry } }"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn empty_scan_emits_no_lines() {
        assert!(publish_lines("JAP", &[]).is_empty());
    }

    #[test]
    fn receipt_pauses_the_game_then_clears_the_waiting_flag() {
        assert_eq!(
            receipt_lines("JAP"),
            vec![
                "gamespeed 0".to_string(),
                "eval_effect JAP = { clr_country_flag = tac_scan_waiting }".to_string(),
            ]
        );
    }

    #[test]
    fn empty_scan_line_targets_the_country() {
        assert_eq!(empty_scan_line("JAP"), "event tac_scan.2 JAP");
    }

    #[test]
    fn tagged_scan_batch_keeps_the_activations_alone() {
        // A scan that carried its tag was answered before the snapshot, so
        // the batch is exactly the publish batch.
        let (lines, published) = scan_batch("JAP", "JAP", &[1028, 408], false);
        assert_eq!(published, 2);
        assert_eq!(lines, publish_lines("JAP", &[1028, 408]));
    }

    #[test]
    fn untagged_scan_batch_answers_ahead_of_the_activations() {
        // A scan without a tag could only be answered after the parse: the
        // receipt rides this batch, ahead of the activations.
        let (lines, published) = scan_batch("JAP", "JAP", &[1028, 408], true);
        assert_eq!(published, 2);
        assert_eq!(&lines[..2], receipt_lines("JAP").as_slice());
        assert_eq!(&lines[2..], publish_lines("JAP", &[1028, 408]).as_slice());
    }

    #[test]
    fn receipt_lines_follow_the_click_tag_activations_the_saves() {
        // The click's tag answers the mod; the save's tag scopes the
        // decision layer — the two never swap roles.
        let (lines, published) = scan_batch("JAP", "GER", &[408], true);
        assert_eq!(published, 1);
        assert_eq!(&lines[..2], receipt_lines("GER").as_slice());
        assert_eq!(lines[2], publish_lines("JAP", &[408])[0]);
        // The empty-scan popup is a receipt line too.
        let (lines, published) = scan_batch("JAP", "GER", &[], false);
        assert_eq!(published, 0);
        assert_eq!(lines, vec![empty_scan_line("GER")]);
    }

    #[test]
    fn an_empty_scan_still_answers() {
        // Tagged empty scan: the popup alone (the receipt already went out).
        let (lines, published) = scan_batch("JAP", "JAP", &[], false);
        assert_eq!(published, 0);
        assert_eq!(lines, vec![empty_scan_line("JAP")]);
        // Untagged empty scan: receipt plus popup — never silence.
        let (lines, published) = scan_batch("JAP", "JAP", &[], true);
        assert_eq!(published, 0);
        assert_eq!(lines.len(), 3);
        assert_eq!(&lines[..2], receipt_lines("JAP").as_slice());
        assert_eq!(lines[2], empty_scan_line("JAP"));
    }
}
