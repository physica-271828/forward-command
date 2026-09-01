//! Build the tactical-save [`UnitNaming`] table from the session
//! [`Locale`] — the one place that knows both the core enums and the
//! `unit_abbrev.*` / `support_abbrev.*` locale keys. The English yml carries
//! the legacy hardcoded abbreviations, so an English session is
//! byte-identical to the pre-localization hardcoded behavior.

use tactical3d_render::locale::camel_to_snake;
use tactical_core::unit::{SupportKind, UnitType};
use tactical_locale::Locale;
use tactical_save::UnitNaming;

/// Every generated battalion label (type abbreviations, the special
/// subunit tokens, support tags) in the locale's language.
pub fn localized_unit_naming(loc: &Locale) -> UnitNaming {
    let mut naming = UnitNaming::default();
    for ut in UnitType::ALL {
        naming.set_type(
            ut,
            loc.tr(&format!(
                "unit_abbrev.{}",
                camel_to_snake(&format!("{ut:?}"))
            ))
            .into_owned(),
        );
    }
    for token in [
        "militia",
        "irregular_infantry",
        "penal_battalion",
        "hq_infantry",
    ] {
        naming.set_token(token, loc.tr(&format!("unit_abbrev.{token}")).into_owned());
    }
    for kind in SupportKind::ALL {
        naming.set_support(
            kind,
            loc.tr(&format!(
                "support_abbrev.{}",
                camel_to_snake(&format!("{kind:?}"))
            ))
            .into_owned(),
        );
    }
    naming
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactical_locale::Language;

    #[test]
    fn english_locale_matches_the_legacy_default() {
        // The English yml must carry the legacy hardcoded strings —
        // drift here silently renames every English counter.
        let naming = localized_unit_naming(&Locale::load(Language::English));
        let default = UnitNaming::default();
        for ut in UnitType::ALL {
            assert_eq!(
                naming.subunit_abbrev("_", ut),
                default.subunit_abbrev("_", ut),
                "type {ut:?}"
            );
        }
        for token in [
            "militia",
            "irregular_infantry",
            "penal_battalion",
            "hq_infantry",
        ] {
            assert_eq!(
                naming.subunit_abbrev(token, UnitType::Infantry),
                default.subunit_abbrev(token, UnitType::Infantry),
                "token {token}"
            );
        }
        for kind in SupportKind::ALL {
            assert_eq!(
                naming.support_tag(kind),
                default.support_tag(kind),
                "{kind:?}"
            );
        }
        assert_eq!(naming.hq(), "HQ");
    }

    #[test]
    fn chinese_locale_carries_chinese_labels() {
        let naming = localized_unit_naming(&Locale::load(Language::SimpChinese));
        assert_eq!(naming.subunit_abbrev("_", UnitType::Infantry), "步兵营");
        assert_eq!(
            naming.subunit_abbrev("militia", UnitType::Infantry),
            "民兵营"
        );
        assert_eq!(naming.hq(), "指挥部");
        assert_eq!(naming.support_tag(SupportKind::AntiTank), "反坦克连");
    }
}
