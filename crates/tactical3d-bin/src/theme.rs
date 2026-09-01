//! Country theme colors: bind attacker/defender side colors to the
//! HOI4 map colors of the battling countries, extracted into
//! `data/country_colors.json` by `extractor/extract_country_colors.py`.
//! The same table feeds the per-country base-plate colors
//! (`TagColors` — one entry per tag the battle fields).

use std::collections::HashSet;

use tactical3d_render::models::{SideColors, TagColors};

/// SideColors for the two battling country tags. A missing table or unknown
/// tag falls back to the default blue/red for that side.
pub fn side_colors(attacker_tag: &str, defender_tag: &str) -> SideColors {
    let mut colors = SideColors::default();
    let path = crate::dirs::runtime_root()
        .join("data")
        .join("country_colors.json");
    let Ok(table) = tactical_save::CountryColorTable::load(&path) else {
        return colors;
    };
    if let Some(rgb) = table.color(attacker_tag) {
        colors.attacker = [rgb[0], rgb[1], rgb[2], 1.0];
    }
    if let Some(rgb) = table.color(defender_tag) {
        colors.defender = [rgb[0], rgb[1], rgb[2], 1.0];
    }
    colors
}

/// Per-country base-plate colors for every tag the battle
/// fields (script division→tag table). Unknown tags are omitted — the
/// renderer falls back to the plain side color for them.
pub fn tag_colors(tags: &HashSet<String>) -> TagColors {
    let mut out = TagColors::default();
    if tags.is_empty() {
        return out;
    }
    let path = crate::dirs::runtime_root()
        .join("data")
        .join("country_colors.json");
    let Ok(table) = tactical_save::CountryColorTable::load(&path) else {
        return out;
    };
    for tag in tags {
        if let Some(rgb) = table.color(tag) {
            out.0.insert(tag.clone(), [rgb[0], rgb[1], rgb[2], 1.0]);
        }
    }
    out
}
