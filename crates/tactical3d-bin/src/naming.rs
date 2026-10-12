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
        "hq_paratrooper",
        "hq_motorized",
        "hq_light_armor",
        "hq_medium_armor",
        "hq_heavy_armor",
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
    // No-modelled-effect companies carry their own display names
    // (`support_name.*`); everything else resolves through its kind tag.
    for token in [
        "light_flame_tank",
        "medium_flame_tank",
        "heavy_flame_tank",
        "super_heavy_armor",
        "land_cruiser",
        "helicopter_brigade",
        "blackshirt_assault_battalion",
        "sturmtruppe_battalion",
        "hq_support_company",
        "hq_specops",
    ] {
        naming.set_support_name(token, loc.tr(&format!("support_name.{token}")).into_owned());
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
        for (token, ut) in [
            ("militia", UnitType::Infantry),
            ("irregular_infantry", UnitType::Infantry),
            ("penal_battalion", UnitType::Infantry),
            ("hq_infantry", UnitType::Infantry),
            ("hq_paratrooper", UnitType::Paratrooper),
            ("hq_motorized", UnitType::Motorized),
            ("hq_light_armor", UnitType::LightArmor),
            ("hq_medium_armor", UnitType::MediumArmor),
            ("hq_heavy_armor", UnitType::HeavyArmor),
        ] {
            assert_eq!(
                naming.subunit_abbrev(token, ut),
                default.subunit_abbrev(token, ut),
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
        // The no-modelled-effect companies' display names ride the same
        // rule: the English yml must carry the table's default strings.
        for token in [
            "light_flame_tank",
            "medium_flame_tank",
            "heavy_flame_tank",
            "super_heavy_armor",
            "land_cruiser",
            "helicopter_brigade",
            "blackshirt_assault_battalion",
            "sturmtruppe_battalion",
            "hq_support_company",
            "hq_specops",
        ] {
            assert_eq!(
                naming.support_name(token, SupportKind::MilitaryPolice),
                default.support_name(token, SupportKind::MilitaryPolice),
                "token {token}"
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
        assert_eq!(
            naming.support_name("light_flame_tank", SupportKind::MilitaryPolice),
            "轻型喷火坦克连"
        );
        assert_eq!(
            naming.support_name("hq_support_company", SupportKind::MilitaryPolice),
            "总参谋部"
        );
    }
}
