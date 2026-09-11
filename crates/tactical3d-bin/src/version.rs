//! Version handshake (DESIGN §3.3): the mod advertises its version in
//! `tac_start`; this module turns (own version, mod version) into the
//! injected verdict + the exe-side notice.
//!
//! The mod and this program ship as one package, but players update the two
//! halves at different times — a mod loaded from the HOI4 user dir against
//! an older exe (or the reverse) is the normal failure mode. The handshake
//! makes it visible: the mod's verdict effects raise an in-game popup when
//! the PROGRAM is stale, and the exe warns in its own surfaces when the MOD
//! sits below the compatibility floor its code requires.

use std::cmp::Ordering;

use tactical_locale::Locale;

/// Parsed `X.Y.Z` / `X.Y.Z-beta.N`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ver {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// Pre-release counter: `None` = a release, which outranks every beta
    /// of the same `major.minor.patch` (SemVer).
    pub beta: Option<u32>,
}

impl Ver {
    /// Parse a version string (surrounding whitespace tolerated). The mod's
    /// advertised value is compared, never repaired: anything else —
    /// garbage, a missing field, an unknown suffix — is `None`, and the
    /// handshake then injects nothing.
    pub fn parse(s: &str) -> Option<Ver> {
        let s = s.trim();
        let (numeric, beta) = match s.split_once('-') {
            Some((numeric, pre)) => (numeric, Some(pre.strip_prefix("beta.")?.parse::<u32>().ok()?)),
            None => (s, None),
        };
        let mut parts = numeric.split('.');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts.next()?.parse::<u32>().ok()?;
        let patch = parts.next()?.parse::<u32>().ok()?;
        // A fourth numeric component is not a version shape we know.
        if parts.next().is_some() {
            return None;
        }
        Some(Ver {
            major,
            minor,
            patch,
            beta,
        })
    }
}

impl Ord for Ver {
    fn cmp(&self, other: &Self) -> Ordering {
        // A release (`u32::MAX`) sorts above every beta of the same triple,
        // so `0.3.2-beta.4 < 0.3.2`.
        let rank = |v: &Ver| (v.major, v.minor, v.patch, v.beta.unwrap_or(u32::MAX));
        rank(self).cmp(&rank(other))
    }
}

impl PartialOrd for Ver {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Append-only compatibility ladder. A row takes effect once this build's
/// version reaches `from`; the effective floor is the LAST applicable row.
/// Bump a row only when an exe change makes newer mod content mandatory.
#[derive(Debug)]
struct ModFloor {
    from: &'static str,
    min_mod: &'static str,
    reason: &'static str,
}

const MOD_FLOOR_LADDER: &[ModFloor] = &[ModFloor {
    from: "0.3.2",
    min_mod: "0.3.2",
    reason: "version handshake (mod-side events + verdict effects introduced)",
}];

/// The newest ladder row this build has reached (None = no floor yet).
fn effective_floor<'a>(own: Ver, ladder: &'a [ModFloor]) -> Option<&'a ModFloor> {
    ladder
        .iter()
        .filter(|row| Ver::parse(row.from).is_some_and(|from| own >= from))
        .next_back()
}

/// Why this build demands the floor it does (the applicable ladder row's
/// rationale) — the exe-side log trail's explanation for a stale-mod
/// warning.
pub fn floor_reason(own: &str) -> Option<&'static str> {
    effective_floor(Ver::parse(own)?, MOD_FLOOR_LADDER).map(|row| row.reason)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Mod ≤ program and above the floor — silent.
    Ok,
    /// The mod is NEWER than this program: the player must update the program.
    ProgramStale,
    /// The mod reports a version below the exe's compatibility floor:
    /// exe-internal notice only (never an in-game popup).
    ModStale { min: String },
    /// No/unparsable version (pre-handshake mod): never guess, never inject.
    Unknown,
}

/// The handshake's verdict for one battle.
pub fn verdict(own: &str, mod_ver: Option<&str>) -> Verdict {
    verdict_with_ladder(own, mod_ver, MOD_FLOOR_LADDER)
}

/// [`verdict`] against an explicit ladder — the seam the floor tests use
/// (today's single row starts at the current version, so no shipped pair of
/// versions exercises `ModStale` on its own).
fn verdict_with_ladder(own: &str, mod_ver: Option<&str>, ladder: &[ModFloor]) -> Verdict {
    let (Some(mod_ver), Some(own)) = (mod_ver.and_then(Ver::parse), Ver::parse(own)) else {
        return Verdict::Unknown;
    };
    if mod_ver > own {
        return Verdict::ProgramStale;
    }
    if let Some(floor) = effective_floor(own, ladder) {
        if Ver::parse(floor.min_mod).is_some_and(|min| mod_ver < min) {
            return Verdict::ModStale {
                min: floor.min_mod.to_owned(),
            };
        }
    }
    Verdict::Ok
}

/// The one console line the verdict injects (None = inject nothing).
///
/// `ModStale` still acks with `d_tac_ver_ok`: the mod's pre-handshake
/// self-check only asks "did ANY verdict reach me?", and an exe-internal
/// problem must never turn into that player popup.
pub fn verdict_line(tag: &str, v: &Verdict) -> Option<String> {
    match v {
        Verdict::Ok | Verdict::ModStale { .. } => {
            Some(format!("eval_effect {tag} = {{ d_tac_ver_ok = yes }}"))
        }
        Verdict::ProgramStale => Some(format!(
            "eval_effect {tag} = {{ d_tac_ver_prog_stale = yes }}"
        )),
        Verdict::Unknown => None,
    }
}

/// Localised notice text for the exe-side surfaces (None for Ok/Unknown).
pub fn notice_text(
    loc: &Locale,
    v: &Verdict,
    own: &str,
    mod_ver: Option<&str>,
) -> Option<String> {
    let mod_ver = mod_ver.unwrap_or("?");
    match v {
        Verdict::ProgramStale => Some(loc.trf(
            "notice.version.program_stale",
            &[("mod", mod_ver), ("exe", own)],
        )),
        Verdict::ModStale { min } => Some(loc.trf("notice.version.mod_stale", &[("mod", mod_ver), ("min", min)])),
        Verdict::Ok | Verdict::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_locale::Language;

    fn v(s: &str) -> Ver {
        Ver::parse(s).unwrap_or_else(|| panic!("{s:?} should parse"))
    }

    #[test]
    fn parses_release_and_beta_versions() {
        let release = v("0.3.2");
        assert_eq!((release.major, release.minor, release.patch), (0, 3, 2));
        assert_eq!(release.beta, None);
        assert_eq!(v("0.3.2-beta.3").beta, Some(3));
        // Surrounding whitespace is tolerated (hand-written literals).
        assert_eq!(v(" 0.3.2-beta.3 "), v("0.3.2-beta.3"));
        assert_eq!(v("1.0.0").beta, None);
    }

    #[test]
    fn garbage_versions_do_not_parse() {
        for bad in [
            "",
            "  ",
            "0.3",
            "0.3.2.1",
            "v0.3.2",
            "0.3.2-rc.1",
            "0.3.2-beta.",
            "0.3.2-beta.x",
            "x.y.z",
        ] {
            assert_eq!(Ver::parse(bad), None, "should not parse: {bad:?}");
        }
    }

    #[test]
    fn ordering_follows_semver() {
        assert!(v("0.3.2-beta.4") < v("0.3.2"));
        assert!(v("0.3.2") < v("0.3.3-beta.1"));
        assert!(v("0.3.3-beta.1") < v("0.3.3"));
        assert!(v("0.3.2-beta.3") < v("0.3.2-beta.4"));
        assert!(v("0.3.2-beta.10") > v("0.3.2-beta.9"));
        assert!(v("0.4.0") > v("0.3.99"));
        assert_eq!(v("0.3.2-beta.3").cmp(&v("0.3.2-beta.3")), Ordering::Equal);
    }

    #[test]
    fn missing_version_is_unknown_and_injects_nothing() {
        assert_eq!(verdict("0.3.2", None), Verdict::Unknown);
        assert_eq!(verdict("0.3.2", Some("")), Verdict::Unknown);
        assert_eq!(verdict("0.3.2", Some("not-a-version")), Verdict::Unknown);
        assert_eq!(verdict_line("GER", &Verdict::Unknown), None);
        let loc = Locale::from_text(Language::English, "", "");
        assert_eq!(notice_text(&loc, &Verdict::Unknown, "0.3.2", None), None);
    }

    #[test]
    fn verdict_matrix() {
        assert_eq!(verdict("0.3.2", Some("0.3.2")), Verdict::Ok);
        // The exe being AHEAD is allowed and silent.
        assert_eq!(verdict("0.3.3", Some("0.3.2")), Verdict::Ok);
        assert_eq!(verdict("0.3.3-beta.1", Some("0.3.2")), Verdict::Ok);
        assert_eq!(
            verdict("0.4.0", Some("0.4.0-beta.1")),
            Verdict::Ok,
            "a beta of a LATER triple is not newer than the release"
        );
        // The mod being NEWER is the in-game popup path.
        assert_eq!(verdict("0.3.2", Some("0.3.3")), Verdict::ProgramStale);
        assert_eq!(
            verdict("0.3.2-beta.3", Some("0.3.2-beta.4")),
            Verdict::ProgramStale
        );
        assert_eq!(verdict("0.3.2", Some("0.4.0")), Verdict::ProgramStale);
        // A beta of the floor's own triple is BELOW the floor (SemVer): the
        // exe-internal warning fires even though the mod is not newer.
        assert_eq!(
            verdict("0.3.2", Some("0.3.2-beta.4")),
            Verdict::ModStale {
                min: "0.3.2".to_owned()
            }
        );
    }

    /// The floor path, exercised against a synthetic ladder: today's single
    /// row starts at the current version, so no shipped version pair reaches
    /// `ModStale` on its own.
    #[test]
    fn below_floor_mod_is_mod_stale_and_still_acks() {
        static SYNTHETIC: &[ModFloor] = &[ModFloor {
            from: "0.3.2",
            min_mod: "0.3.2",
            reason: "test row",
        }];
        assert_eq!(
            verdict_with_ladder("0.3.2", Some("0.3.1"), SYNTHETIC),
            Verdict::ModStale {
                min: "0.3.2".to_owned()
            }
        );
        // A mod exactly at the floor passes…
        assert_eq!(
            verdict_with_ladder("0.3.2", Some("0.3.2"), SYNTHETIC),
            Verdict::Ok
        );
        // …and a ladder the build has not reached yet does not warn.
        static FUTURE: &[ModFloor] = &[ModFloor {
            from: "0.4.0",
            min_mod: "0.4.0",
            reason: "test row",
        }];
        assert_eq!(
            verdict_with_ladder("0.3.2", Some("0.3.1"), FUTURE),
            Verdict::Ok
        );
        // The exe-internal verdict must still ack the mod, or the mod's
        // pre-handshake self-check would raise the PROGRAM popup instead.
        let stale = Verdict::ModStale {
            min: "0.3.2".to_owned(),
        };
        assert_eq!(
            verdict_line("GER", &stale).as_deref(),
            Some("eval_effect GER = { d_tac_ver_ok = yes }")
        );
    }

    /// Every ladder row must be self-consistent, and the current build's
    /// effective floor must be the LAST row it has reached.
    #[test]
    fn ladder_rows_are_sane() {
        for row in MOD_FLOOR_LADDER {
            let from = Ver::parse(row.from).expect("ladder row has an unparsable `from`");
            let min = Ver::parse(row.min_mod).expect("ladder row has an unparsable `min_mod`");
            assert!(min <= from, "row {row:?}: min_mod must not exceed from");
            assert!(!row.reason.trim().is_empty(), "row {row:?}: empty reason");
        }
        let own = Ver::parse(env!("CARGO_PKG_VERSION")).expect("workspace version parses");
        let applicable: Vec<&ModFloor> = MOD_FLOOR_LADDER
            .iter()
            .filter(|r| Ver::parse(r.from).is_some_and(|f| f <= own))
            .collect();
        assert_eq!(
            effective_floor(own, MOD_FLOOR_LADDER).map(|r| r.from),
            applicable.last().map(|r| r.from),
            "the effective floor must be the last applicable row"
        );
        // The trail's explanation follows the same row.
        assert_eq!(
            floor_reason(env!("CARGO_PKG_VERSION")),
            applicable.last().map(|r| r.reason)
        );
        assert_eq!(floor_reason("not-a-version"), None);
    }

    #[test]
    fn verdict_lines_are_the_documented_console_form() {
        assert_eq!(
            verdict_line("GER", &Verdict::Ok).as_deref(),
            Some("eval_effect GER = { d_tac_ver_ok = yes }")
        );
        assert_eq!(
            verdict_line("ITA", &Verdict::ProgramStale).as_deref(),
            Some("eval_effect ITA = { d_tac_ver_prog_stale = yes }")
        );
    }

    #[test]
    fn notice_text_fills_the_placeholder_set() {
        let en = Locale::from_text(
            Language::English,
            "l_english:\n notice.version.program_stale:0 \"stale {mod}/{exe}\"\n notice.version.mod_stale:0 \"mod {mod} needs {min}\"\n",
            "",
        );
        let text = notice_text(&en, &Verdict::ProgramStale, "0.3.2-beta.3", Some("0.3.3")).unwrap();
        assert_eq!(text, "stale 0.3.3/0.3.2-beta.3");
        let text = notice_text(
            &en,
            &Verdict::ModStale {
                min: "0.3.2".to_owned(),
            },
            "0.3.2",
            Some("0.3.1"),
        )
        .unwrap();
        assert_eq!(text, "mod 0.3.1 needs 0.3.2");
        // Ok is silent on the exe surfaces too.
        assert_eq!(notice_text(&en, &Verdict::Ok, "0.3.2", Some("0.3.2")), None);
    }

    /// The shipping files must carry both keys with exactly the placeholders
    /// this module fills — a typo on either side would put the raw key in
    /// front of the player at precisely the wrong moment.
    #[test]
    fn shipping_locales_carry_the_notice_keys() {
        for lang in [Language::English, Language::SimpChinese] {
            let loc = Locale::load(lang);
            let text = notice_text(&loc, &Verdict::ProgramStale, "0.3.2-beta.3", Some("0.3.3"))
                .expect("program-stale notice");
            assert!(
                !text.starts_with("notice.version."),
                "{lang:?}: program_stale key missing — {text}"
            );
            assert!(
                text.contains("0.3.3") && text.contains("0.3.2-beta.3"),
                "{lang:?}: placeholders not substituted — {text}"
            );
            let text = notice_text(
                &loc,
                &Verdict::ModStale {
                    min: "0.3.2".to_owned(),
                },
                "0.3.2",
                Some("0.3.1"),
            )
            .expect("mod-stale notice");
            assert!(
                !text.starts_with("notice.version."),
                "{lang:?}: mod_stale key missing — {text}"
            );
            assert!(
                text.contains("0.3.1") && text.contains("0.3.2"),
                "{lang:?}: placeholders not substituted — {text}"
            );
        }
    }
}
