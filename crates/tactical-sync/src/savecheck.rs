//! Post-sync HOI4 battle-alive check (DESIGN.md §3.2/§8.2).
//!
//! While a live tactical battle runs, the mirrored HOI4 battle is frozen
//! (state modifier zeroes vanilla damage) and only the injected
//! `damage_units` lines move division org — landing once per sync, during
//! the clock advance. A division at 0 org is forced out of combat (vanilla
//! engine rule; in-combat divisions do not regenerate org, so org only
//! ever decreases), and the HOI4 battle ends once a whole side has routed.
//! That end can therefore ONLY happen at a sync boundary, which makes a
//! ground-truth check exact: after every sync, snapshot the save and look
//! for our `land_combat` record. (A fixed "accumulated org damage ratio"
//! threshold was rejected: the defender `damage_units` line distributes
//! org as equal POINTS per division, so mixed max-org rosters rout in
//! staggered order and any fixed ratio fires early or late depending on
//! composition and starting org.)
//!
//! The scan is deliberately dumb text matching over the raw save — the two
//! questions asked here ("does a `land_combat` at province P still
//! exist?", "is any defender-tag division still HOLDING P?") do not
//! justify a full jomini parse of a 60+ MB file. 1.19.2 saves serialize NO
//! per-province controller field, so the winner derives from the
//! defenders' posture at the province:
//!
//! * a routing division does NOT teleport out — retreat is a slow move
//!   (vanilla `RETREAT_SPEED_FACTOR`), and a moving division serializes as
//!   `location` = the province it STANDS IN plus `movement_progress`/`path`
//!   for the way out. So right after the HOI4 battle ends, the routed
//!   defenders are typically still located AT the contested province — a
//!   naive "defenders present ⇒ defender victory" reads the outcome
//!   backwards during exactly the window we scan in. The discriminator,
//!   verified against the c5a_now.hoi4 ground truth (three routed 13238
//!   defenders mid-retreat), is the division record's **`retreat=yes`
//!   key** — present only on routing divisions
//!   (with org ≈ 0); normally-marching divisions carry
//!   `movement_progress`/`path` but never a `retreat` key (a marching
//!   defender pinned into the battle still holds — the interception
//!   counterexample), and standing divisions carry neither. A defender-tag
//!   division at the province WITHOUT `retreat=yes` is holding (attack
//!   repelled ⇒ defender victory); defenders present but all routing (or
//!   gone entirely) = the province fell (attacker victory). Anything
//!   unreadable fails OPEN ([`Hoi4BattleCheck::Unknown`] = "keep
//!   fighting"): a false ending would strand the player mid-battle, a
//!   missed one just costs a sync. One-line division REFERENCES in the
//!   country rosters are not records and are skipped outright, never
//!   failed open (see [`defender_holds`]).
//!
//! Completeness guard: a fresh save is only scanned once its tail carries
//! the `checksum="..."` line — Clausewitz text saves end with it, so a
//! file still streaming to disk is never mistaken for a battle-less save.

use tactical_core::Side;

/// Outcome of the post-sync battle-alive check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hoi4BattleCheck {
    /// The `land_combat` for the contested province still exists.
    Alive,
    /// The record is gone — the HOI4 battle ended under the tactical one.
    /// `winner` derives from defender-division presence at the province
    /// (see module docs).
    Ended { winner: Side },
    /// The scan could not decide (unreadable division blocks) — the caller
    /// treats this as [`Hoi4BattleCheck::Alive`] (fail open).
    Unknown,
}

/// The trailing line every complete Clausewitz text save ends with.
const SAVE_TAIL_MARKER: &str = "checksum=\"";

/// Does the save look fully written? Checked on the last chunk only — the
/// checksum line is the serializer's final write.
pub fn save_looks_complete(save_text: &str) -> bool {
    let from = floor_boundary(save_text, save_text.len().saturating_sub(512));
    let tail = &save_text[from..];
    tail.contains(SAVE_TAIL_MARKER)
}

/// Clamp `i` down to the nearest char boundary. Fixed-width scan windows
/// (`start + N` bytes into a record, `len - N` from the tail) can land
/// INSIDE a multi-byte UTF-8 char — saves carry non-ASCII bytes in
/// localized unit/fleet names — and slicing there panics.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// First `key<digits>` number in `window` (the key includes the `=`).
fn number_after(window: &str, key: &str) -> Option<u32> {
    let i = window.find(key)?;
    let digits: String = window[i + key.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// First `key"..."` quoted string in `window` (the key includes the `=`).
fn quoted_after<'a>(window: &'a str, key: &str) -> Option<&'a str> {
    let i = window.find(key)?;
    let rest = window[i + key.len()..].strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Every `land_combat` / `border_war_combat`'s `location` in the save
/// (duplicates kept — only membership is ever queried). Border wars are a
/// separate record type whose `location` is the DEFENDER's province — the
/// same role a land combat's `location` plays — so a running border war
/// must report `Alive` exactly like a running land battle.
fn battle_locations(text: &str) -> Vec<u32> {
    const TOKENS: [&str; 2] = ["land_combat={", "border_war_combat={"];
    let mut out = Vec::new();
    for token in TOKENS {
        let mut rest = text;
        while let Some(i) = rest.find(token) {
            let start = i + token.len();
            // `location=` is the record's second line. Bound the window by
            // the NEXT same-type record so a (hypothetical) location-less
            // record can never pair with its successor's location.
            let next = rest[start..].find(token).map(|j| start + j);
            let end = floor_boundary(rest, next.unwrap_or(rest.len()).min(start + 512));
            if let Some(loc) = number_after(&rest[start..end], "location=") {
                out.push(loc);
            }
            rest = &rest[start..];
        }
    }
    out
}

/// Is a defender-side division HOLDING `contested`? Holding = NOT in
/// rout. The discriminator is the division record's
/// `retreat=yes` key — present ONLY on routing divisions (c5a_now.hoi4
/// ground truth: the three routed 13238 defenders all carry
/// `retreat=yes` + org ≈ 0 while located at the province mid-retreat;
/// normally-marching divisions carry `movement_progress`/`path` but NEVER
/// a `retreat` key, and standing divisions carry neither). Movement keys
/// alone must NOT count as leaving — a marching defender pinned into the
/// battle and still holding shows `movement_progress` too (the
/// interception counterexample). `defender_tags` covers the whole
/// defending SIDE — an allied co-defender still standing means the
/// province holds. `None` when a division AT the province has an
/// unreadable tag (fail open).
///
/// Division records carry `location` / `logical_country` as their 3rd/4th
/// lines (movement/retreat keys, when present, right around them). The
/// window is bounded by the NEXT `division={` — never let a standing
/// division see a routed neighbor's `retreat=yes` (the bleed case has
/// been observed in the real save: a full-org bystander followed by a
/// routed division).
///
/// Country-level rosters ALSO serialize one-line references to divisions
/// (`division={ id=884 type=51 }`) whose next real record can sit
/// megabytes away. A reference is not a record and is skipped before any
/// read ([`is_division_reference`]) — otherwise the fail-open below would
/// fire on every real save and no ending could ever be reported.
fn defender_holds(text: &str, contested: u32, defender_tags: &[&str]) -> Option<bool> {
    const TOKEN: &str = "division={";
    /// Byte cap on one record's scan window (see the doc above).
    const RECORD_WINDOW: usize = 1536;
    let mut unreadable = false;
    let mut rest = text;
    while let Some(i) = rest.find(TOKEN) {
        let start = i + TOKEN.len();
        let end_of_record = rest[start..].find(TOKEN).map(|j| start + j);
        let limit = end_of_record.unwrap_or(rest.len());
        let cut_by_cap = limit > start + RECORD_WINDOW;
        let end = floor_boundary(rest, limit.min(start + RECORD_WINDOW));
        let window = &rest[start..end];
        // Before ANY read: a reference one-liner is not a record, and its
        // window may otherwise reach a neighbouring block's `location=`.
        if is_division_reference(window) {
            rest = &rest[start..];
            continue;
        }
        match number_after(window, "location=") {
            Some(loc) if loc == contested => match quoted_after(window, "logical_country=") {
                Some(tag) if defender_tags.contains(&tag) => {
                    if !window.contains("retreat=yes") {
                        return Some(true); // holding — standing or marching
                    }
                    // routing out — not holding, keep scanning.
                }
                Some(_) => {} // another tag's division at the province
                None => unreadable = true,
            },
            Some(_) => {} // a division elsewhere in the save
            // No `location=` at all: on a record the cap cut short, the
            // division may be one of ours AT the province — fail open
            // rather than let the truncated neighbour read as a victory.
            None if cut_by_cap => unreadable = true,
            None => {}
        }
        rest = &rest[start..];
    }
    if unreadable {
        None
    } else {
        Some(false)
    }
}

/// A one-line REFERENCE to a division (`division={ id=884 type=51 }`, the
/// shape country rosters and training queues serialize) closes its block
/// before any nested `{` and carries no `location=` — it is not a
/// record. Real saves put hundreds of kilobytes between such an entry
/// and the next `division={` record, so a reference mistaken for a
/// truncated record would fail the whole scan open and freeze the
/// judgement forever; a real record always opens `id={ … }` (a `{`
/// before the first `}`), which is exactly what this rejects.
fn is_division_reference(window: &str) -> bool {
    /// How far a reference's block may reach before it stops counting.
    const CLOSED_WINDOW: usize = 256;
    let head = &window[..floor_boundary(window, window.len().min(CLOSED_WINDOW))];
    let Some(close) = head.find('}') else {
        return false;
    };
    let content = &head[..close];
    !content.contains('{') && !content.contains("location=")
}

/// The post-sync check: is the mirrored HOI4 battle still alive? See the
/// module docs for the winner derivation and the fail-open rule. A save
/// that fails the completeness guard is `Unknown`. `defender_tags` = the
/// whole defending side's country tags (allied co-defenders included).
pub fn check_hoi4_battle(save_text: &str, contested: u32, defender_tags: &[&str]) -> Hoi4BattleCheck {
    if !save_looks_complete(save_text) {
        return Hoi4BattleCheck::Unknown;
    }
    if battle_locations(save_text).contains(&contested) {
        return Hoi4BattleCheck::Alive;
    }
    match defender_holds(save_text, contested, defender_tags) {
        Some(true) => Hoi4BattleCheck::Ended {
            winner: Side::Defender,
        },
        Some(false) => Hoi4BattleCheck::Ended {
            winner: Side::Attacker,
        },
        None => Hoi4BattleCheck::Unknown,
    }
}

/// The played country of the save: the root `player="TAG"` header key.
/// The root keys serialize first, so only the file HEAD is scanned. `None`
/// when the key is absent (multiplayer/odd saves) — the save parser treats
/// a missing root player the same way (never an error).
pub fn root_player_tag(save_text: &str) -> Option<String> {
    const HEAD_BYTES: usize = 64 * 1024;
    let head = &save_text[..floor_boundary(save_text, save_text.len().min(HEAD_BYTES))];
    quoted_after(head, "player=").map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMBAT_AT_13238: &str =
        "\tland_combat={\n\t\tid={ id=1 type=62 }\n\t\tlocation=13238\n\t\tday=1\n\t}\n";
    const COMBAT_AT_4995: &str =
        "\tland_combat={\n\t\tid={ id=2 type=62 }\n\t\tlocation=4995\n\t\tday=1\n\t}\n";

    fn division(id: u32, location: u32, tag: &str) -> String {
        format!(
            "\t\t\tdivision={{\n\t\t\t\tid={{ id={id} type=51 }}\n\t\t\t\tlast_combat_date=\"1.1.1.1\"\n\t\t\t\tlocation={location}\n\t\t\t\tlogical_country=\"{tag}\"\n\t\t\t\tseed=1156537371\n\t\t\t}}\n"
        )
    }

    /// A division mid-MARCH: `movement_progress`/`move_priority`/`path` but
    /// no `retreat` key (1.19.2 real-save shape) — the interception case:
    /// marching defenders pinned into a battle still hold when they win.
    fn marching_division(id: u32, location: u32, tag: &str) -> String {
        format!(
            "\t\t\tdivision={{\n\t\t\t\tid={{ id={id} type=51 }}\n\t\t\t\tlast_combat_date=\"1.1.1.1\"\n\t\t\t\tmovement_progress=3\n\t\t\t\tmove_priority=front_order\n\t\t\t\tpath={{\n\t\t\t\t\t9334 9361 9306\n\t\t\t\t}}\n\t\t\t\tlocation={location}\n\t\t\t\tlogical_country=\"{tag}\"\n\t\t\t\tseed=811255986\n\t\t\t}}\n"
        )
    }

    /// A ROUTING division: the real c5a_now shape — `movement_progress` +
    /// `path` to the retreat target + `retreat=yes`, org zeroed.
    fn routed_division(id: u32, location: u32, tag: &str) -> String {
        format!(
            "\t\t\tdivision={{\n\t\t\t\tid={{ id={id} type=51 }}\n\t\t\t\tlast_combat_date=\"1936.1.2.7\"\n\t\t\t\tmovement_progress=1.19894\n\t\t\t\tpath={{\n\t\t\t\t\t2072\n\t\t\t\t}}\n\t\t\t\tlocation={location}\n\t\t\t\tretreat=yes\n\t\t\t\tlogical_country=\"{tag}\"\n\t\t\t\tseed=1156537371\n\t\t\t\torganisation=0\n\t\t\t}}\n"
        )
    }

    fn save(body: &str) -> String {
        format!("HOI4txt\nplayer=\"ITA\"\n{body}checksum=\"39fede44\"\n")
    }

    #[test]
    fn alive_when_land_combat_lists_the_contested_province() {
        let text = save(&format!("{COMBAT_AT_4995}{COMBAT_AT_13238}"));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Alive
        );
    }

    #[test]
    fn alive_when_border_war_lists_the_contested_province() {
        // A running border war is its own record type (§10.7) whose
        // `location` is the defender's province — it must report Alive
        // exactly like a land battle, or the first sync would declare the
        // battle over.
        let bw = "border_war_combat={\n\t\tid={ id=3459 type=4713 }\n\t\tlocation=11879\n\t\tday=9\n\t\tduration=9\n";
        let text = save(bw);
        assert_eq!(
            check_hoi4_battle(&text, 11879, &["SOV"]),
            Hoi4BattleCheck::Alive
        );
    }

    #[test]
    fn ended_attacker_when_battle_gone_and_no_defender_at_the_province() {
        // Battle ended, defenders routed away; only the attacker marched in.
        let text = save(&format!(
            "{COMBAT_AT_4995}{}{}",
            division(1833, 13238, "ITA"),
            division(1834, 12766, "ETH") // routed defender, elsewhere
        ));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Attacker
            }
        );
    }

    #[test]
    fn ended_defender_when_battle_gone_and_defender_still_stands() {
        // Attack repelled: the defenders never left the province.
        let text = save(&format!("{COMBAT_AT_4995}{}", division(1833, 13238, "ETH")));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Defender
            }
        );
    }

    #[test]
    fn unknown_when_a_division_at_the_province_has_no_readable_tag() {
        let mut ghost = division(1833, 13238, "ETH");
        ghost = ghost.replace("logical_country=\"ETH\"\n", "");
        let text = save(&ghost);
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Unknown
        );
    }

    #[test]
    fn unknown_when_the_save_is_still_streaming() {
        // No checksum trailer = partial write; never report an ending.
        let text = format!("HOI4txt\n{COMBAT_AT_4995}");
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Unknown
        );
    }

    #[test]
    fn root_player_reads_the_save_header_tag() {
        let text = save(COMBAT_AT_4995);
        assert_eq!(root_player_tag(&text), Some("ITA".to_string()));
        // Header absent → None (never an error).
        let bare = format!("HOI4txt\n{COMBAT_AT_4995}checksum=\"x\"\n");
        assert_eq!(root_player_tag(&bare), None);
        // A `player=` deep in the body must NOT be mistaken for the root
        // header key (the head window stops before it).
        let deep = format!(
            "HOI4txt\nplayer=\"ITA\"\n{}",
            "\n".repeat(70_000).to_owned() + "player=\"GER\"\nchecksum=\"x\"\n"
        );
        assert_eq!(root_player_tag(&deep), Some("ITA".to_string()));
    }

    #[test]
    fn a_cap_cut_division_record_fails_open() {
        // A record longer than the scan window whose `location=` sits past
        // the cut: the division may well be AT the contested province, so
        // this is "unreadable" (→ Unknown), never "no defender holding"
        // (→ a false attacker victory).
        let padding = "component".to_string() + &" 0".repeat(900) + "\n";
        let record = format!(
            "\t\t\tdivision={{\n\t\t\t\tid={{ id=1 type=51 }}\n\t\t\t\t{padding}\t\t\t\tlocation=13238\n\t\t\t\tlogical_country=\"ETH\"\n\t\t\t}}\n"
        );
        assert!(
            record.find("location=").unwrap() > 1536,
            "test layout: the record must exceed the window"
        );
        let text = save(&record);
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Unknown
        );
    }

    #[test]
    fn a_divisions_window_never_pairs_with_the_next_divisions_location() {
        // Division at 12766 whose own location line is (hypothetically)
        // missing must not see the next division's 13238.
        let broken = "\t\t\tdivision={\n\t\t\t\tid={ id=1 type=51 }\n\t\t\t}\n";
        let text = save(&format!("{broken}{}", division(1833, 13238, "ETH")));
        // The broken block's window is bounded, but the NEXT block is still
        // a real division at 13238 — the battle record is gone, so this
        // resolves as a defender-held ending, not a mispairing artifact.
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Defender
            }
        );
    }

    #[test]
    fn other_provinces_battles_do_not_count() {
        let text = save(COMBAT_AT_4995);
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Attacker
            } // no division anywhere near — province empty
        );
        assert_eq!(
            check_hoi4_battle(&text, 4995, &["ETH"]),
            Hoi4BattleCheck::Alive
        );
    }

    #[test]
    fn retreat_window_defenders_still_at_the_province_but_moving_is_attacker_victory() {
        // THE motivating case for the `retreat=yes` discriminator: the
        // HOI4 battle just ended, routed defenders are still LOCATED at
        // the province (retreat is a slow move) — presence alone must
        // not read as a defender victory; the `retreat=yes` key gives
        // the rout away.
        let text = save(&format!(
            "{COMBAT_AT_4995}{}{}",
            routed_division(1833, 13238, "ETH"), // routing out
            routed_division(1834, 13238, "ETH")  // routing out
        ));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Attacker
            }
        );
    }

    /// The real-save country-roster shape: a one-line REFERENCE to a
    /// division. Real saves put 145 KB–8.6 MB of unrelated content between
    /// such an entry and the next `division={` record.
    fn roster_reference(id: u32) -> String {
        format!("\t\t\t\tdivision={{ id={id} type=51 }}\n")
    }

    /// Filler pushing the next `division={` past the 1536-byte record
    /// window — the gap class that used to poison the scan.
    fn reference_gap() -> String {
        "\t\t\t\tcomponent=0\n".repeat(100)
    }

    #[test]
    fn a_roster_reference_is_not_read_as_a_truncated_record() {
        // Live regression (border war): rosters carry reference one-liners
        // whose next `division={` record sits far past the window cap; each
        // one used to flip the scan-wide `unreadable` flag, so a battle
        // whose record was gone could NEVER be reported as ended and the
        // app froze on "battle continues" forever.
        let text = save(&format!(
            "{}{}{}",
            roster_reference(884),
            reference_gap(),
            routed_division(1833, 13238, "ETH") // routed out — province fell
        ));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Attacker
            }
        );
    }

    #[test]
    fn a_roster_reference_never_hides_a_standing_defender() {
        let text = save(&format!(
            "{}{}{}",
            roster_reference(884),
            reference_gap(),
            division(1833, 13238, "ETH")
        ));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Defender
            }
        );
    }

    #[test]
    fn a_roster_reference_never_lends_its_window_to_neighbouring_keys() {
        // A reference's scan window can reach a NEIGHBOURING block's
        // `location=`/`logical_country=` — reading those as the reference's
        // own would fabricate a defender holding the province.
        let body = format!(
            "{}reinforcement={{\n\t\t\t\tlocation=13238\n\t\t\t\tlogical_country=\"ETH\"\n\t\t\t}}\n",
            roster_reference(884)
        );
        let text = save(&body);
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Attacker
            } // nothing actually holds the province
        );
    }

    #[test]
    fn marching_pinned_defender_still_holds_the_province() {
        // The interception counterexample: a defender that was MARCHING
        // (movement keys, no retreat key) and got pinned into the battle
        // still holds when the attack is repelled — movement keys alone
        // must never count as leaving.
        let text = save(&format!(
            "{COMBAT_AT_4995}{}",
            marching_division(1833, 13238, "ETH")
        ));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Defender
            }
        );
    }

    #[test]
    fn one_holding_defender_outweighs_routing_ones() {
        // Mixed postures: someone is still holding the ground.
        let text = save(&format!(
            "{}{}",
            routed_division(1833, 13238, "ETH"),
            division(1834, 13238, "ETH") // standing
        ));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Defender
            }
        );
    }

    #[test]
    fn a_standing_division_never_inherits_its_routed_neighbors_retreat_key() {
        // Bleed regression (observed in the real c5a_now save): a standing
        // division immediately followed by a routed one — the first
        // division's scan window must stop at the sibling boundary.
        let text = save(&format!(
            "{}{}",
            division(1840, 13238, "ETH"), // standing bystander
            routed_division(1841, 13238, "ETH")
        ));
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Defender
            }
        );
    }

    #[test]
    fn a_window_cut_inside_a_multibyte_char_does_not_panic() {
        // Live-crash regression: localized names put multi-byte UTF-8 into
        // division records; the fixed 1536-byte scan window can land on a
        // byte that is not a char boundary — slicing there panicked. The
        // ASCII name prefix is lengthened until the cut provably lands
        // mid-char (a 3-byte char keeps the cut's mod-3 alignment, so the
        // shift has to come from single-byte padding).
        let mut shifted = String::new();
        let text = loop {
            let record = format!(
                "\t\t\tdivision={{\n\t\t\t\tid={{ id=1 type=51 }}\n\t\t\t\tlocation=13238\n\t\t\t\tlogical_country=\"ETH\"\n\t\t\t\tname=\"{shifted}{}\"\n\t\t\t}}\n",
                "挪".repeat(700)
            );
            let text = save(&record);
            let start = text.find("division={").unwrap() + "division={".len();
            if !text.is_char_boundary(start + 1536) {
                break text;
            }
            shifted.push('x');
        };
        assert_eq!(
            check_hoi4_battle(&text, 13238, &["ETH"]),
            Hoi4BattleCheck::Ended {
                winner: Side::Defender
            }
        );
    }

    #[test]
    fn the_completeness_tail_cut_inside_a_multibyte_char_does_not_panic() {
        // Same hazard at the other end: `len - 512` can land inside a
        // multi-byte char when a non-ASCII name sits near the file tail.
        // The ASCII padding goes AFTER the run — padding before it would
        // shift the cut and the run start together and never change the
        // cut's alignment inside the chars.
        let mut shifted = String::new();
        let text = loop {
            let text = save(&format!("{}{shifted}", "挪".repeat(200)));
            if !text.is_char_boundary(text.len() - 512) {
                break text;
            }
            shifted.push('x');
        };
        assert!(save_looks_complete(&text));
    }
}
