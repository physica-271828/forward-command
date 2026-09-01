//! Desync guard (DESIGN.md §3.2/§8.2): the sync pipeline assumes the
//! `tac_check` snapshot reflects the same strategic world this battle was
//! assembled from. If the player loads an earlier save, switches the played
//! country, or unpauses the game mid-battle, that assumption breaks silently
//! — the battle-alive check would misjudge endings and the damage lines
//! would book against the wrong world. Two independent probes guard the
//! pipeline, each a pure function so the calendar math is unit-testable:
//!
//! * [`check_clock_receipt`] — the clock-advance receipt (the game-date
//!   prefix of the freshest probe line, §8.4) must read EXACTLY one game
//!   hour after the prefix probed before the batch. A manual unpause makes
//!   the clock run ahead (+2h or more); a loaded earlier save moves it
//!   backward; anything but the exact expected prefix is a mismatch.
//! * [`check_player_tag`] — the snapshot's root `player="TAG"` must still
//!   be the country this battle belongs to.
//!
//! Hour domain: Clausewitz dates run hour `1..=24` (midnight is hour 24 of
//! the current day; the next tick is the next day's hour 1) — the same
//! 1-based domain as the save's `date=` header, which the live assembly
//! stores in `BattleSession::start_datetime`.

/// Why a sync was refused: the guard verdict consumed by the dialog layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesyncVerdict {
    /// The receipt matched the expected next game hour (or nothing could be
    /// verified — dry run / unparsable prefix — the caller fails open).
    Ok,
    /// The clock receipt differs from `last_prefix + 1 game hour`.
    HourMismatch {
        /// Canonical `yyyy.mm.dd.hh` of the expected hour.
        expected: String,
        /// The prefix actually read back.
        found: String,
    },
    /// The snapshot's played country differs from the battle's country.
    TagMismatch {
        /// The battle's country tag (`BattleSession::country_tag`).
        expected: String,
        /// The snapshot's root `player="TAG"`.
        found: String,
    },
}

/// One parsed game prefix: `(year, month, day, hour)`, hour 1..=24.
pub type GamePrefix = (i32, u32, u32, u32);

/// Parse a `yyyy.mm.dd.hh` prefix. Group count and digit shape come from
/// `tactical_listen::game_date_prefix`; here the VALUE ranges are validated
/// too (month 1..=12, day 1..=31, hour 1..=24) — anything else cannot take
/// part in the exact-match check and returns `None`.
pub fn parse_game_prefix(prefix: &str) -> Option<GamePrefix> {
    let mut parts = prefix.split('.');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let hour: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Hour 24 is the day's last tick (Clausewitz); 0 is the mod's
    // "unknown" placeholder in JSON fields, never a real prefix hour.
    if !(1..=24).contains(&hour) {
        return None;
    }
    Some((year, month, day, hour))
}

/// The canonical zero-padded `yyyy.mm.dd.hh` form — real HOI4 log prefixes
/// are zero-padded, so this is also the lexicographic (chronological) form.
pub fn format_game_prefix(p: GamePrefix) -> String {
    format!("{:04}.{:02}.{:02}.{:02}", p.0, p.1, p.2, p.3)
}

/// The next game hour on the Gregorian calendar (HOI4's calendar is the
/// real one, leap years included): hour 24 rolls into the next day's hour
/// 1, month lengths via `days_in_month`, December rolls the year.
pub fn next_game_prefix(p: GamePrefix) -> GamePrefix {
    let (year, month, day, hour) = p;
    if hour < 24 {
        return (year, month, day, hour + 1);
    }
    if day < crate::days_in_month(year, month) {
        return (year, month, day + 1, 1);
    }
    if month < 12 {
        return (year, month + 1, 1, 1);
    }
    (year + 1, 1, 1, 1)
}

/// `prefix + 1 game hour`, canonical form; `None` when `prefix` is not a
/// validatable game prefix (the caller then skips the exact-match check).
pub fn next_game_hour(prefix: &str) -> Option<String> {
    parse_game_prefix(prefix).map(|p| format_game_prefix(next_game_prefix(p)))
}

/// Exact-match check of one sync's clock receipt against the prefix probed
/// before the batch. Unparsable input fails open (`Ok`) — a guard that
/// cannot understand the clock must not wedge the battle flow.
pub fn check_clock_receipt(last_prefix: &str, found_prefix: &str) -> DesyncVerdict {
    let (Some(last), Some(found)) = (
        parse_game_prefix(last_prefix),
        parse_game_prefix(found_prefix),
    ) else {
        return DesyncVerdict::Ok;
    };
    let expected = next_game_prefix(last);
    if found == expected {
        DesyncVerdict::Ok
    } else {
        DesyncVerdict::HourMismatch {
            expected: format_game_prefix(expected),
            found: format_game_prefix(found),
        }
    }
}

/// The played-country check: `save_player` is the snapshot's root
/// `player="TAG"`. `None` (multiplayer/odd saves ship no root player key)
/// fails open, mirroring the save parser's never-an-error contract.
pub fn check_player_tag(battle_tag: &str, save_player: Option<&str>) -> DesyncVerdict {
    match save_player {
        Some(found) if !found.is_empty() && found != battle_tag => DesyncVerdict::TagMismatch {
            expected: battle_tag.to_string(),
            found: found.to_string(),
        },
        _ => DesyncVerdict::Ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── +1 game hour: calendar boundaries (hour domain 1..=24) ──

    #[test]
    fn next_hour_within_a_day() {
        assert_eq!(
            next_game_hour("1936.01.01.12"),
            Some("1936.01.01.13".to_string())
        );
        // Non-padded input parses; output is the canonical padded form.
        assert_eq!(
            next_game_hour("1936.1.1.6"),
            Some("1936.01.01.07".to_string())
        );
    }

    #[test]
    fn next_hour_23_to_24_stays_the_same_day() {
        assert_eq!(
            next_game_hour("1936.01.01.23"),
            Some("1936.01.01.24".to_string())
        );
    }

    #[test]
    fn hour_24_rolls_into_the_next_day() {
        assert_eq!(
            next_game_hour("1936.01.01.24"),
            Some("1936.01.02.01".to_string())
        );
    }

    #[test]
    fn month_end_31_rolls_forward() {
        assert_eq!(
            next_game_hour("1936.01.31.24"),
            Some("1936.02.01.01".to_string())
        );
    }

    #[test]
    fn month_end_30_rolls_forward() {
        assert_eq!(
            next_game_hour("1936.04.30.24"),
            Some("1936.05.01.01".to_string())
        );
    }

    #[test]
    fn february_28_rolls_in_a_common_year() {
        assert_eq!(
            next_game_hour("1937.02.28.24"),
            Some("1937.03.01.01".to_string())
        );
    }

    #[test]
    fn february_29_exists_in_a_leap_year() {
        assert_eq!(
            next_game_hour("1936.02.28.24"),
            Some("1936.02.29.01".to_string())
        );
        assert_eq!(
            next_game_hour("1936.02.29.24"),
            Some("1936.03.01.01".to_string())
        );
        // The 400-year rule: 2000 is a leap year, 1900 is not.
        assert_eq!(
            next_game_hour("2000.02.28.24"),
            Some("2000.02.29.01".to_string())
        );
        assert_eq!(
            next_game_hour("1900.02.28.24"),
            Some("1900.03.01.01".to_string())
        );
    }

    #[test]
    fn new_year_eve_rolls_the_year() {
        assert_eq!(
            next_game_hour("1936.12.31.24"),
            Some("1937.01.01.01".to_string())
        );
    }

    #[test]
    fn out_of_domain_hours_are_rejected() {
        // Hour 0 is the mod's unknown placeholder, never a real prefix.
        assert_eq!(next_game_hour("1936.01.01.00"), None);
        // Hour 25+ cannot exist on the 1..=24 domain.
        assert_eq!(next_game_hour("1936.01.01.25"), None);
        assert_eq!(next_game_hour(""), None);
        assert_eq!(next_game_hour("1936.01.01"), None);
        assert_eq!(next_game_hour("1936.13.01.12"), None);
        assert_eq!(next_game_hour("1936.01.32.12"), None);
        assert_eq!(next_game_hour("x.y.z.w"), None);
    }

    // ── the receipt check: exact match accept, anything else reject ──

    #[test]
    fn exact_next_hour_accepts() {
        assert_eq!(
            check_clock_receipt("1936.01.01.20", "1936.01.01.21"),
            DesyncVerdict::Ok
        );
        // Day rollover counts as the expected single hour.
        assert_eq!(
            check_clock_receipt("1936.01.01.24", "1936.01.02.01"),
            DesyncVerdict::Ok
        );
    }

    #[test]
    fn running_ahead_rejects() {
        // Manual unpause: the clock advanced two hours under one batch.
        assert_eq!(
            check_clock_receipt("1936.01.01.20", "1936.01.01.22"),
            DesyncVerdict::HourMismatch {
                expected: "1936.01.01.21".to_string(),
                found: "1936.01.01.22".to_string(),
            }
        );
    }

    #[test]
    fn standing_still_rejects() {
        // The prefix never moved — not the expected next hour either.
        assert_eq!(
            check_clock_receipt("1936.01.01.20", "1936.01.01.20"),
            DesyncVerdict::HourMismatch {
                expected: "1936.01.01.21".to_string(),
                found: "1936.01.01.20".to_string(),
            }
        );
    }

    #[test]
    fn moving_backward_rejects() {
        // An earlier save was loaded: the clock sits before the probe.
        assert_eq!(
            check_clock_receipt("1936.01.02.04", "1936.01.01.22"),
            DesyncVerdict::HourMismatch {
                expected: "1936.01.02.05".to_string(),
                found: "1936.01.01.22".to_string(),
            }
        );
    }

    #[test]
    fn unparsable_input_fails_open() {
        assert_eq!(
            check_clock_receipt("not-a-prefix", "1936.01.01.21"),
            DesyncVerdict::Ok
        );
        assert_eq!(
            check_clock_receipt("1936.01.01.20", "25"),
            DesyncVerdict::Ok
        );
    }

    // ── the played-country check ──

    #[test]
    fn matching_tag_accepts() {
        assert_eq!(check_player_tag("ITA", Some("ITA")), DesyncVerdict::Ok);
    }

    #[test]
    fn different_tag_rejects() {
        assert_eq!(
            check_player_tag("ITA", Some("GER")),
            DesyncVerdict::TagMismatch {
                expected: "ITA".to_string(),
                found: "GER".to_string(),
            }
        );
    }

    #[test]
    fn missing_tag_fails_open() {
        assert_eq!(check_player_tag("ITA", None), DesyncVerdict::Ok);
        assert_eq!(check_player_tag("ITA", Some("")), DesyncVerdict::Ok);
    }
}
