//! Clausewitz text save parsing via jomini (DESIGN.md §5.1, §5.2).
//!
//! Handled format quirks:
//! - The `HOI4txt` first line is stripped before parsing.
//! - Binary saves (`HOI4bin` magic, or NUL bytes in the header) are rejected
//!   with an error telling the user to set `save_as_binary=no` (§11.2).
//! - `division_templates` are read at ROOT level (real saves); country-level
//!   `division_template` blocks are merged as a fallback.
//! - Divisions may reference their template by name (`division_template =
//!   "Name"`) or by id (`division_template_id = { id = N ... }`).
//! - Division equipment is read as `token = count` scalars; nested
//!   `equipment = { id = ... amount = ... }` entries (unresolvable without an
//!   id→type table) are skipped gracefully.

use std::collections::HashMap;
use std::path::Path;

use jomini::text::{GroupEntry, ObjectReader, ValueReader};
use jomini::Windows1252Encoding;

use crate::mapping::map_unit_class;
use crate::model::{
    ArmyData, BattalionInfo, CountryData, DivisionData, LandCombatData, LandCombatSideData,
    LeaderData, SaveGame, TemplateData,
};
use crate::SaveError;

type Encoding = Windows1252Encoding;

/// Save-file parser. All methods are associated functions; the parser holds
/// no state.
pub struct SaveParser;

impl SaveParser {
    /// Read and parse a `.hoi4` text save from disk (§5.1).
    ///
    /// Bytes are read raw (not as UTF-8) because real saves are windows-1252
    /// encoded; jomini's `windows1252_reader` does the decoding.
    pub fn parse_save(path: impl AsRef<Path>) -> Result<SaveGame, SaveError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| SaveError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::parse_save_from_bytes(&bytes)
    }

    /// Parse a save from an in-memory string (already decoded text).
    pub fn parse_save_from_str(text: &str) -> Result<SaveGame, SaveError> {
        Self::parse_save_from_bytes(text.as_bytes())
    }

    fn parse_save_from_bytes(bytes: &[u8]) -> Result<SaveGame, SaveError> {
        let body = strip_header(bytes)?;
        let tape = jomini::TextTape::from_slice(body)
            .map_err(|e| SaveError::Parse(format!("Clausewitz text tape error: {e}")))?;
        let root = tape.windows1252_reader();

        // Pass 1: root-level division templates. They may appear after
        // `countries` in the file, so collect them before parsing countries.
        let mut templates: Vec<TemplateData> = Vec::new();
        for (key, group) in root.field_groups() {
            if key.read_str() == "division_templates" {
                templates = parse_root_templates(&group)?;
            }
        }

        // Pass 1.5: the root `equipments` registry — 1.19 stores
        // division equipment as `id = N` references, and this block maps
        // each id to its token: `infantry_equipment_1 = { id = { id = 1326
        // type = 70 } ... }`.
        let mut equip_ids: HashMap<u64, String> = HashMap::new();
        for (key, group) in root.field_groups() {
            if key.read_str() != "equipments" {
                continue;
            }
            for (_op, value) in group.values() {
                let Ok(obj) = value.read_object() else {
                    continue;
                };
                for (token_key, token_group) in obj.field_groups() {
                    // `infantry_equipment_1 = { id = { id = 1326 type = 70 } }`
                    // — the numeric id nests one level below the token.
                    for (_op, tv) in token_group.values() {
                        let Ok(entry_obj) = tv.read_object() else {
                            continue;
                        };
                        for (k, g) in entry_obj.field_groups() {
                            if k.read_str() == "id" {
                                if let Some(id) = read_id_object(&g) {
                                    equip_ids.insert(id, token_key.read_str().to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        // Root `date = "1936.1.1.13"` (save header) — stored on SaveGame
        // (the battle clock starts from it); the pass-1.6 expire filter
        // below compares against it.
        let save_date = root
            .fields()
            .find(|(key, _op, _value)| key.read_str() == "date")
            .and_then(|(_key, _op, value)| value.read_str().ok())
            .and_then(|s| parse_hoi4_date(&s));

        // Pass 1.6: country-leader traits and advisor
        // roles from the character database — `character_manager = {
        // historical = { character = { id = { … } country = "TAG"
        // country_leaders = { country_leader = { traits expire } }
        // advisors = { advisor = { slot idea_token } } } … } }`. Only the
        // needed fields are read; everything else in the (huge) character
        // blocks is skipped by key.
        // The same character blocks also carry the
        // `corps_commander` / `field_marshal` role blocks — the division
        // commanders keyed by leader instance id (type 4713).
        let mut leader_traits: HashMap<String, Vec<String>> = HashMap::new();
        let mut advisor_roles: HashMap<u64, Vec<(String, String)>> = HashMap::new();
        let mut leaders: HashMap<u64, LeaderData> = HashMap::new();
        for (key, group) in root.field_groups() {
            if key.read_str() != "character_manager" {
                continue;
            }
            for (_op, cm_value) in group.values() {
                let Ok(cm_obj) = cm_value.read_object() else {
                    continue;
                };
                for (ck, cg) in cm_obj.field_groups() {
                    if ck.read_str() != "historical" {
                        continue;
                    }
                    for (_op, hv) in cg.values() {
                        let Ok(hist_obj) = hv.read_object() else {
                            continue;
                        };
                        for (hk, hg) in hist_obj.field_groups() {
                            if hk.read_str() != "character" {
                                continue;
                            }
                            for (_op, cv) in hg.values() {
                                parse_character_roles(
                                    &cv,
                                    save_date,
                                    &mut leader_traits,
                                    &mut advisor_roles,
                                );
                                parse_character_leaders(&cv, &mut leaders);
                            }
                        }
                    }
                }
            }
        }

        // Pass 2: countries.
        let mut countries: HashMap<String, CountryData> = HashMap::new();
        for (key, group) in root.field_groups() {
            if key.read_str() == "countries" {
                for (_op, value) in group.values() {
                    let Ok(obj) = value.read_object() else {
                        return Err(SaveError::Parse("`countries` is not an object".to_string()));
                    };
                    for (tag_key, tag_group) in obj.field_groups() {
                        let tag = tag_key.read_str().to_string();
                        // A single malformed country must not abort the parse
                        // (§11.3); skip it and keep going.
                        if let Ok(mut country) =
                            parse_country(&tag, &tag_group, &templates, &equip_ids, &advisor_roles)
                        {
                            // Pass-1.6 leader traits ride on the tag.
                            country.leader_traits = leader_traits.remove(&tag).unwrap_or_default();
                            countries.insert(tag, country);
                        }
                    }
                }
            }
        }

        // Pass 2.5: every country's `theatres` block —
        // armies (`orders_group`, with member divisions and an optional
        // general) and army groups (`field_marshal_group`, with child army
        // refs and an optional field marshal), flattened into one list. The
        // division → general → field-marshal chain resolves through it.
        let mut armies: Vec<ArmyData> = Vec::new();
        for (key, group) in root.field_groups() {
            if key.read_str() != "countries" {
                continue;
            }
            for (_op, value) in group.values() {
                let Ok(obj) = value.read_object() else {
                    continue;
                };
                for (_tag_key, tag_group) in obj.field_groups() {
                    for (_op, country_value) in tag_group.values() {
                        let Ok(country_obj) = country_value.read_object() else {
                            continue;
                        };
                        for (field_key, field_group) in country_obj.field_groups() {
                            if field_key.read_str() == "theatres" {
                                parse_theatres(&field_group, &mut armies);
                            }
                        }
                    }
                }
            }
        }

        // Root-level `player="TAG"` (single-player save header).
        let mut player: Option<String> = None;
        for (key, _op, value) in root.fields() {
            if key.read_str() == "player" {
                player = value.read_str().ok().map(|s| s.to_string());
            }
        }

        // Pass 3: ongoing land battles in the root `combat` block —
        // the save's authoritative battle record.
        let mut land_combats: Vec<LandCombatData> = Vec::new();
        for (key, group) in root.field_groups() {
            if key.read_str() != "combat" {
                continue;
            }
            for (_op, combat_value) in group.values() {
                let Ok(combat_obj) = combat_value.read_object() else {
                    continue;
                };
                for (ck, cg) in combat_obj.field_groups() {
                    if ck.read_str() != "land_combat" {
                        continue;
                    }
                    for (_op, value) in cg.values() {
                        if let Ok(combat) = parse_land_combat(&value) {
                            land_combats.push(combat);
                        }
                    }
                }
            }
        }

        // Pass 4: states carrying tac_pick=1 — the
        // state-target entry decision marks the picked state via a state
        // variable; the post-click snapshot locates the pick here.
        let mut picked_states: Vec<u32> = Vec::new();
        for (key, group) in root.field_groups() {
            if key.read_str() != "states" {
                continue;
            }
            for (_op, states_value) in group.values() {
                let Ok(states_obj) = states_value.read_object() else {
                    continue;
                };
                for (id_key, id_group) in states_obj.field_groups() {
                    let Ok(state_id) = id_key.read_str().parse::<u32>() else {
                        continue;
                    };
                    for (_op, state_value) in id_group.values() {
                        let Ok(state_obj) = state_value.read_object() else {
                            continue;
                        };
                        let mut picked = false;
                        for (sk, sg) in state_obj.field_groups() {
                            if sk.read_str() != "variables" {
                                continue;
                            }
                            for (_op, vars_value) in sg.values() {
                                let Ok(vars_obj) = vars_value.read_object() else {
                                    continue;
                                };
                                for (vk, _op, vv) in vars_obj.fields() {
                                    // Numeric scalar (same pattern as
                                    // `location`): read_scalar + to_f64 —
                                    // read_str() fails on unquoted numbers.
                                    if vk.read_str() == "tac_pick"
                                        && vv.read_scalar().ok().and_then(|s| s.to_f64().ok())
                                            == Some(1.0)
                                    {
                                        picked = true;
                                    }
                                }
                            }
                        }
                        if picked {
                            picked_states.push(state_id);
                        }
                    }
                }
            }
        }

        Ok(SaveGame {
            countries,
            templates,
            player,
            date: save_date,
            land_combats,
            picked_states,
            leaders,
            armies,
        })
    }
}

/// Parse one `land_combat = { ... }` block: the contested
/// province plus both sides' unit ids / tactic id / participating tags.
/// A malformed block degrades to whatever fields were readable (§11.3)
/// but `location` is required — without it the battle is unusable.
fn parse_land_combat(value: &ValueReader<'_, '_, Encoding>) -> Result<LandCombatData, SaveError> {
    let obj = value
        .read_object()
        .map_err(|e| SaveError::Parse(format!("land_combat is not an object: {e}")))?;
    let mut location: Option<u32> = None;
    let mut attacker = LandCombatSideData::default();
    let mut defender = LandCombatSideData::default();
    for (key, group) in obj.field_groups() {
        match key.read_str().as_ref() {
            "location" => {
                for (_op, v) in group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        location = scalar.to_u64().ok().map(|x| x as u32);
                    }
                }
            }
            "attacker" => {
                for (_op, v) in group.values() {
                    if let Ok(side_obj) = v.read_object() {
                        parse_land_combat_side(&side_obj, &mut attacker);
                    }
                }
            }
            "defender" => {
                for (_op, v) in group.values() {
                    if let Ok(side_obj) = v.read_object() {
                        parse_land_combat_side(&side_obj, &mut defender);
                    }
                }
            }
            _ => {}
        }
    }
    let location =
        location.ok_or_else(|| SaveError::Parse("land_combat without location".to_string()))?;
    Ok(LandCombatData {
        location,
        attacker,
        defender,
    })
}

/// Fill one side of a [`LandCombatData`] from an `attacker`/`defender`
/// block: repeated `unit = { id = N type = 51 }`, scalar `tactic = N`, and
/// the nested `log.combat_side_data.tags = { "ITA" }` list.
fn parse_land_combat_side(obj: &ObjectReader<'_, '_, Encoding>, side: &mut LandCombatSideData) {
    for (key, group) in obj.field_groups() {
        match key.read_str().as_ref() {
            "unit" => {
                for (_op, v) in group.values() {
                    if let Some(id) = read_id_from_value(&v) {
                        side.unit_ids.push(id);
                    }
                }
            }
            "tactic" => {
                for (_op, v) in group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        side.tactic = scalar.to_u64().ok().map(|x| x as u32);
                    }
                }
            }
            "log" => {
                for (_op, v) in group.values() {
                    let Ok(log_obj) = v.read_object() else {
                        continue;
                    };
                    for (lk, lg) in log_obj.field_groups() {
                        if lk.read_str() != "combat_side_data" {
                            continue;
                        }
                        for (_op, csd) in lg.values() {
                            let Ok(csd_obj) = csd.read_object() else {
                                continue;
                            };
                            for (tk, tg) in csd_obj.field_groups() {
                                if tk.read_str() != "tags" {
                                    continue;
                                }
                                // `tags = { "ITA" "ETH" }` is an ARRAY of
                                // quoted strings, not key-groups.
                                for (_op, tv) in tg.values() {
                                    let Ok(arr) = tv.read_array() else { continue };
                                    for av in arr.values() {
                                        if let Ok(tag) = av.read_string() {
                                            if !tag.is_empty() {
                                                side.tags.push(tag);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Strip the `HOI4txt` header line and reject binary saves.
fn strip_header(bytes: &[u8]) -> Result<&[u8], SaveError> {
    const BIN_MAGIC: &[u8] = b"HOI4bin";
    const TXT_MAGIC: &[u8] = b"HOI4txt";

    if bytes.starts_with(BIN_MAGIC) {
        return Err(SaveError::Binary);
    }
    // Heuristic: NUL bytes near the start mean this is not text.
    let probe = &bytes[..bytes.len().min(4096)];
    if probe.contains(&0) {
        return Err(SaveError::Binary);
    }

    let mut body = bytes;
    if body.starts_with(TXT_MAGIC) {
        body = &body[TXT_MAGIC.len()..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        } else if body.starts_with(b"\n") {
            body = &body[1..];
        }
    }
    Ok(body)
}

/// All divisions of `tag` standing in `province_id` (§5.1 battle lookup).
pub fn find_divisions_in_province<'a>(
    save: &'a SaveGame,
    tag: &str,
    province_id: u32,
) -> Vec<&'a DivisionData> {
    save.countries
        .get(tag)
        .map(|c| {
            c.divisions
                .iter()
                .filter(|d| d.location == Some(province_id))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

/// Root-level `division_templates = { division_template = { id = {...}
/// name = "..." regiments = {...} support = {...} } ... }` (real save format).
fn parse_root_templates(
    group: &GroupEntry<'_, '_, Encoding>,
) -> Result<Vec<TemplateData>, SaveError> {
    let mut templates = Vec::new();
    for (_op, value) in group.values() {
        let Ok(obj) = value.read_object() else {
            return Err(SaveError::Parse(
                "`division_templates` is not an object".to_string(),
            ));
        };
        for (field_key, field_group) in obj.field_groups() {
            if field_key.read_str() != "division_template" {
                continue;
            }
            for (_op, tmpl_value) in field_group.values() {
                if let Ok(t) = parse_root_template(&tmpl_value) {
                    templates.push(t);
                }
            }
        }
    }
    Ok(templates)
}

fn parse_root_template(value: &ValueReader<'_, '_, Encoding>) -> Result<TemplateData, SaveError> {
    let obj = value
        .read_object()
        .map_err(|e| SaveError::Parse(format!("division_template entry is not an object: {e}")))?;

    let mut id = None;
    let mut name = String::new();
    let mut division_names_group = None;
    let mut battalions = Vec::new();
    let mut support_companies = Vec::new();

    for (field_key, field_group) in obj.field_groups() {
        match field_key.read_str().as_ref() {
            "id" => id = read_id_object(&field_group),
            "name" => {
                for (_op, v) in field_group.values() {
                    name = read_text(&v);
                }
            }
            "division_names_group" => {
                for (_op, v) in field_group.values() {
                    division_names_group = Some(read_text(&v));
                }
            }
            "regiments" => battalions = parse_subunits(&field_group),
            "support" => support_companies = parse_subunits(&field_group),
            _ => {}
        }
    }

    Ok(TemplateData {
        id,
        name,
        division_names_group,
        battalions,
        support_companies,
    })
}

/// Country-level fallback `division_template = { "Name" = { regiments = {...}
/// support = {...} } ... }` (older/synthetic format, keyed by template name).
fn parse_local_templates(group: &GroupEntry<'_, '_, Encoding>) -> Vec<TemplateData> {
    let mut templates = Vec::new();
    for (_op, value) in group.values() {
        let Ok(obj) = value.read_object() else {
            continue;
        };
        for (name_key, name_group) in obj.field_groups() {
            let name = name_key.read_str().to_string();
            let mut battalions = Vec::new();
            let mut support_companies = Vec::new();
            let mut division_names_group = None;
            for (_op, tmpl_value) in name_group.values() {
                let Ok(tmpl_obj) = tmpl_value.read_object() else {
                    continue;
                };
                for (section_key, section_group) in tmpl_obj.field_groups() {
                    match section_key.read_str().as_ref() {
                        "regiments" => battalions = parse_subunits(&section_group),
                        "support" => support_companies = parse_subunits(&section_group),
                        "division_names_group" => {
                            for (_op, v) in section_group.values() {
                                division_names_group = Some(read_text(&v));
                            }
                        }
                        _ => {}
                    }
                }
            }
            templates.push(TemplateData {
                id: None,
                name: name.clone(),
                division_names_group,
                battalions,
                support_companies,
            });
        }
    }
    templates
}

/// `regiments`/`support` body: `{ infantry = {...} infantry = {...} ... }`.
/// The count of a subunit type is the number of repeated keys. Unknown tokens
/// fall back to Infantry but keep their token as a log-friendly marker (§5.3).
fn parse_subunits(group: &GroupEntry<'_, '_, Encoding>) -> Vec<BattalionInfo> {
    let mut out = Vec::new();
    for (_op, value) in group.values() {
        let Ok(obj) = value.read_object() else {
            continue;
        };
        for (unit_key, unit_group) in obj.field_groups() {
            let token = unit_key.read_str().to_string();
            let count = unit_group.values().count();
            if count == 0 {
                continue;
            }
            let class = map_unit_class(&token).unwrap_or_default();
            out.push(BattalionInfo {
                token,
                unit_type: class.unit_type,
                chassis: class.chassis,
                extra_attrs: class.extra_attrs,
                count,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Countries and divisions
// ---------------------------------------------------------------------------

type TemplateMaps = (
    HashMap<String, (Vec<BattalionInfo>, Vec<BattalionInfo>)>,
    HashMap<u64, String>,
    HashMap<u64, Option<String>>,
);

fn build_template_maps(root_templates: &[TemplateData], local: &[TemplateData]) -> TemplateMaps {
    let mut by_name: HashMap<String, (Vec<BattalionInfo>, Vec<BattalionInfo>)> = HashMap::new();
    let mut id_to_name: HashMap<u64, String> = HashMap::new();
    let mut id_to_group: HashMap<u64, Option<String>> = HashMap::new();
    // Root templates first; country-level entries override on name clash.
    for t in root_templates.iter().chain(local.iter()) {
        // Skip nameless templates (1.19 army-HQ templates carry
        // `localization_key=` instead of `name=` — 438 of 698 in a 1936
        // save): inserting them would poison `by_name[""]`, and every
        // division whose resolution fell through would inherit HQ content.
        if !t.name.is_empty() {
            by_name.insert(
                t.name.clone(),
                (t.battalions.clone(), t.support_companies.clone()),
            );
        }
        if let Some(id) = t.id {
            id_to_name.insert(id, t.name.clone());
            id_to_group.insert(id, t.division_names_group.clone());
        }
    }
    (by_name, id_to_name, id_to_group)
}

fn parse_country(
    tag: &str,
    group: &GroupEntry<'_, '_, Encoding>,
    root_templates: &[TemplateData],
    equip_ids: &HashMap<u64, String>,
    advisor_roles: &HashMap<u64, Vec<(String, String)>>,
) -> Result<CountryData, SaveError> {
    // Pass 1: local templates, technologies, ideas, dynamic modifiers,
    // appointed advisors (order-independent).
    let mut local_templates = Vec::new();
    let mut technologies = Vec::new();
    let mut active_ideas = Vec::new();
    let mut dynamic_modifiers = Vec::new();
    let mut appointed_advisors = Vec::new();

    for (_op, value) in group.values() {
        let Ok(country_obj) = value.read_object() else {
            return Err(SaveError::Parse(format!("country {tag} is not an object")));
        };
        for (field_key, field_group) in country_obj.field_groups() {
            match field_key.read_str().as_ref() {
                "division_template" => local_templates = parse_local_templates(&field_group),
                "technologies" => technologies = parse_token_list(&field_group),
                // 1.19 writes national spirits as `ideas = { tok1 tok2 }`
                // (bare token list) — parse_token_list's scalar branch reads
                // exactly that shape.
                "active_ideas" | "ideas" => active_ideas.extend(parse_token_list(&field_group)),
                // 1.19.2 key is singular `dynamic_modifier`; accept the
                // plural too (docs/older notes use it).
                "dynamic_modifier" | "dynamic_modifiers" => {
                    dynamic_modifiers.extend(parse_dynamic_modifiers(&field_group))
                }
                "appointed_advisors" => {
                    appointed_advisors.extend(parse_appointed_advisors(&field_group))
                }
                _ => {}
            }
        }
    }
    // A spirit listed by both keys must not apply twice.
    active_ideas.sort();
    active_ideas.dedup();

    // Resolve each appointed (slot, character id) to the
    // advisor role's idea_token — the same-slot role wins; a character
    // without a same-slot role falls back to its first advisor block
    // (documented approximation). Unresolvable ids skip silently (§11.3).
    let active_advisors: Vec<String> = appointed_advisors
        .iter()
        .filter_map(|(slot, id)| {
            let roles = advisor_roles.get(id)?;
            roles
                .iter()
                .find(|(role_slot, _)| role_slot == slot)
                .or_else(|| roles.first())
                .map(|(_, token)| token.clone())
        })
        .collect();

    let (by_name, id_to_name, id_to_group) = build_template_maps(root_templates, &local_templates);

    // Pass 2: divisions (needs the merged template maps).
    let mut divisions = Vec::new();
    for (_op, value) in group.values() {
        let Ok(country_obj) = value.read_object() else {
            continue;
        };
        for (field_key, field_group) in country_obj.field_groups() {
            if field_key.read_str() != "units" {
                continue;
            }
            for (_op, units_value) in field_group.values() {
                let Ok(units_obj) = units_value.read_object() else {
                    continue;
                };
                for (unit_key, unit_group) in units_obj.field_groups() {
                    if unit_key.read_str() != "division" {
                        continue;
                    }
                    for (_op, div_value) in unit_group.values() {
                        // Skip malformed divisions individually (§11.3).
                        if let Ok(div) =
                            parse_division(&div_value, &by_name, &id_to_name, &id_to_group, equip_ids)
                        {
                            divisions.push(div);
                        }
                    }
                }
            }
        }
    }

    Ok(CountryData {
        tag: tag.to_string(),
        divisions,
        technologies,
        active_ideas,
        dynamic_modifiers,
        // Filled by the caller from the pass-1.6 character-database scan.
        leader_traits: Vec::new(),
        active_advisors,
    })
}

/// `appointed_advisors = { { slot = "…" character = { id = N type = 73 } }
/// … }`: the inner entries are ANONYMOUS blocks — a bare
/// object array (same shape as `value = { floats }`). Yields
/// `(slot, character id)` pairs; entries without both fields are skipped.
fn parse_appointed_advisors(group: &GroupEntry<'_, '_, Encoding>) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for (_op, value) in group.values() {
        let Ok(arr) = value.read_array() else {
            continue;
        };
        for entry in arr.values() {
            let Ok(obj) = entry.read_object() else {
                continue;
            };
            let mut slot = String::new();
            let mut id: Option<u64> = None;
            for (k, g) in obj.field_groups() {
                match k.read_str().as_ref() {
                    "slot" => {
                        for (_op, v) in g.values() {
                            slot = read_text(&v);
                        }
                    }
                    // `character = { id = N type = 73 }` — the id sits one
                    // level down, like the `id = { id = N … }` form.
                    "character" => id = read_id_object(&g),
                    _ => {}
                }
            }
            if slot.is_empty() {
                continue;
            }
            if let Some(id) = id {
                out.push((slot, id));
            }
        }
    }
    out
}

/// `"1936.1.1.13"` → `(1936, 1, 1, 13)`. HOI4 date fields are NOT
/// zero-padded ("1936.9.1.1" vs "1936.12.1.1"), so lexical comparison is
/// unsafe — compare the numeric tuples.
fn parse_hoi4_date(text: &str) -> Option<(i32, u32, u32, u32)> {
    let mut parts = text.split('.');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    let hour = parts.next().and_then(|h| h.parse().ok()).unwrap_or(0);
    Some((year, month, day, hour))
}

/// One `character = { … }` block of `character_manager.historical`:
/// - when it belongs to a country (`country = "TAG"`) and carries at least
///   one UNEXPIRED `country_leader` role, the role traits append to that
///   tag's leader-trait list;
/// - its `advisors` role blocks record `(slot, idea_token)` under the
///   character's own id, for the appointed-advisor resolution in
///   [`parse_country`].
/// Everything else in the block (portraits, corps_commander, variables, …)
/// is skipped by key.
fn parse_character_roles(
    value: &ValueReader<'_, '_, Encoding>,
    save_date: Option<(i32, u32, u32, u32)>,
    out: &mut HashMap<String, Vec<String>>,
    advisor_roles: &mut HashMap<u64, Vec<(String, String)>>,
) {
    let Ok(obj) = value.read_object() else {
        return;
    };
    let mut id: Option<u64> = None;
    let mut country = String::new();
    let mut traits = Vec::new();
    let mut roles: Vec<(String, String)> = Vec::new();
    for (key, group) in obj.field_groups() {
        match key.read_str().as_ref() {
            "id" => id = read_id_object(&group),
            "country" => {
                for (_op, v) in group.values() {
                    country = read_text(&v);
                }
            }
            "country_leaders" => {
                for (_op, v) in group.values() {
                    let Ok(role_obj) = v.read_object() else {
                        continue;
                    };
                    for (rk, rg) in role_obj.field_groups() {
                        if rk.read_str() != "country_leader" {
                            continue;
                        }
                        for (_op, rv) in rg.values() {
                            read_country_leader_role(&rv, save_date, &mut traits);
                        }
                    }
                }
            }
            "advisors" => {
                for (_op, v) in group.values() {
                    let Ok(adv_obj) = v.read_object() else {
                        continue;
                    };
                    for (ak, ag) in adv_obj.field_groups() {
                        if ak.read_str() != "advisor" {
                            continue;
                        }
                        for (_op, av) in ag.values() {
                            if let Some(role) = read_advisor_role(&av) {
                                roles.push(role);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !country.is_empty() && !traits.is_empty() {
        out.entry(country).or_default().extend(traits);
    }
    if let (Some(id), false) = (id, roles.is_empty()) {
        advisor_roles.insert(id, roles);
    }
}

/// One `advisor = { slot = "…" idea_token = "…" … }` role block →
/// `(slot, idea_token)`; either field missing skips the block.
fn read_advisor_role(value: &ValueReader<'_, '_, Encoding>) -> Option<(String, String)> {
    let obj = value.read_object().ok()?;
    let mut slot = String::new();
    let mut token = String::new();
    for (key, group) in obj.field_groups() {
        match key.read_str().as_ref() {
            "slot" => {
                for (_op, v) in group.values() {
                    slot = read_text(&v);
                }
            }
            "idea_token" => {
                for (_op, v) in group.values() {
                    token = read_text(&v);
                }
            }
            _ => {}
        }
    }
    if slot.is_empty() || token.is_empty() {
        return None;
    }
    Some((slot, token))
}

/// One `country_leader = { ideology = … traits = { … } expire = "…" }`
/// role block. `expire` is the role's END date — the role is active while
/// the save date is strictly earlier; a missing or unparseable expire (or a
/// missing save date) is treated as ACTIVE (graceful fallback, §11.3).
fn read_country_leader_role(
    value: &ValueReader<'_, '_, Encoding>,
    save_date: Option<(i32, u32, u32, u32)>,
    traits: &mut Vec<String>,
) {
    let Ok(role) = value.read_object() else {
        return;
    };
    let mut expired = false;
    let mut role_traits = Vec::new();
    for (key, group) in role.field_groups() {
        match key.read_str().as_ref() {
            "expire" => {
                for (_op, v) in group.values() {
                    if let (Some(exp), Some(date)) = (parse_hoi4_date(&read_text(&v)), save_date) {
                        expired = exp <= date;
                    }
                }
            }
            // Bare-token list, same shape as `ideas`.
            "traits" => role_traits = parse_token_list(&group),
            _ => {}
        }
    }
    if !expired {
        traits.extend(role_traits);
    }
}

/// One `character = { … }` block's unit-leader roles: the
/// `corps_commander` (general) and `field_marshal` blocks carry the leader
/// INSTANCE id (type 4713 — the army `leader` join key, distinct from the
/// character's own type-73 id), the serialized skill ints and a bare-token
/// trait list. `navy_leader` blocks share the id space but are skipped by
/// key. A character may carry both role blocks (e.g. a promoted FM keeps
/// his general record) — each records its own leader id.
fn parse_character_leaders(
    value: &ValueReader<'_, '_, Encoding>,
    leaders: &mut HashMap<u64, LeaderData>,
) {
    let Ok(obj) = value.read_object() else {
        return;
    };
    for (key, group) in obj.field_groups() {
        let is_field_marshal = match key.read_str().as_ref() {
            "corps_commander" => false,
            "field_marshal" => true,
            _ => continue,
        };
        for (_op, v) in group.values() {
            if let Some((id, data)) = read_unit_leader_role(&v, is_field_marshal) {
                leaders.insert(id, data);
            }
        }
    }
}

/// One `corps_commander` / `field_marshal` role block →
/// `(leader instance id, LeaderData)`; a missing id skips the block, other
/// missing fields degrade to zero/empty (§11.3).
fn read_unit_leader_role(
    value: &ValueReader<'_, '_, Encoding>,
    is_field_marshal: bool,
) -> Option<(u64, LeaderData)> {
    let obj = value.read_object().ok()?;
    let mut id: Option<u64> = None;
    let mut data = LeaderData {
        is_field_marshal,
        ..Default::default()
    };
    for (key, group) in obj.field_groups() {
        match key.read_str().as_ref() {
            "id" => id = read_id_object(&group),
            "attack_skill" => {
                for (_op, v) in group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        data.attack_skill = scalar.to_f64().unwrap_or(0.0) as f32;
                    }
                }
            }
            "defense_skill" => {
                for (_op, v) in group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        data.defense_skill = scalar.to_f64().unwrap_or(0.0) as f32;
                    }
                }
            }
            // Bare-token list, same shape as `ideas`.
            "traits" => data.traits = parse_token_list(&group),
            _ => {}
        }
    }
    Some((id?, data))
}

/// A country's `theatres = { theatre = { orders_group = {…}
/// field_marshal_group = {…} } }` block: collect every
/// army and army group into the flat list. Unattached divisions (`unit = {…}`
/// directly under the theatre) and `theater_group` refs are skipped by key.
fn parse_theatres(group: &GroupEntry<'_, '_, Encoding>, out: &mut Vec<ArmyData>) {
    for (_op, value) in group.values() {
        let Ok(obj) = value.read_object() else {
            continue;
        };
        for (key, theatre_group) in obj.field_groups() {
            if key.read_str() != "theatre" {
                continue;
            }
            for (_op, tv) in theatre_group.values() {
                let Ok(theatre_obj) = tv.read_object() else {
                    continue;
                };
                for (tk, tg) in theatre_obj.field_groups() {
                    let is_fm_group = match tk.read_str().as_ref() {
                        "orders_group" => false,
                        "field_marshal_group" => true,
                        _ => continue,
                    };
                    for (_op, gv) in tg.values() {
                        if let Some(army) = parse_orders_group(&gv, is_fm_group) {
                            out.push(army);
                        }
                    }
                }
            }
        }
    }
}

/// One `orders_group` / `field_marshal_group` block.
/// Armies carry their divisions as `member = { unit = { id = D type = 51 }
/// }`; army groups carry child army references as bare single-line
/// `orders_group = { id = N type = 53 }` objects (which may precede the
/// group's own `id`). The `leader` id object is omitted when the (group)
/// has no commander. A block without an `id` still records its members
/// (id 0 — harmless: no FM group can reference it).
fn parse_orders_group(
    value: &ValueReader<'_, '_, Encoding>,
    is_fm_group: bool,
) -> Option<ArmyData> {
    let obj = value.read_object().ok()?;
    let mut army = ArmyData {
        is_fm_group,
        ..Default::default()
    };
    for (key, group) in obj.field_groups() {
        match key.read_str().as_ref() {
            "id" => army.id = read_id_object(&group).unwrap_or(0),
            "leader" => army.leader = read_id_object(&group),
            "member" => {
                for (_op, mv) in group.values() {
                    let Ok(member_obj) = mv.read_object() else {
                        continue;
                    };
                    for (mk, mg) in member_obj.field_groups() {
                        if mk.read_str() == "unit" {
                            if let Some(uid) = read_id_object(&mg) {
                                army.members.push(uid);
                            }
                        }
                    }
                }
            }
            // Child army refs only exist inside an army group (repeated
            // keys — one value per ref); at theatre level the same key
            // names a full army block (handled above).
            "orders_group" if is_fm_group => {
                for (_op, cv) in group.values() {
                    if let Some(cid) = read_id_from_value(&cv) {
                        army.child_armies.push(cid);
                    }
                }
            }
            _ => {}
        }
    }
    Some(army)
}

/// `dynamic_modifier = { modifier = { modifier = "NAME" value = { … }
/// enabled = yes } … }` (1.19.2): each inner block carries the
/// definition token, the CURRENT values as a bare float array in the
/// definition's modifier-key order, and an enabled flag. Only enabled
/// entries are collected; a missing value array degrades to empty (the
/// modifier then contributes nothing by index).
fn parse_dynamic_modifiers(group: &GroupEntry<'_, '_, Encoding>) -> Vec<(String, Vec<f32>)> {
    let mut out = Vec::new();
    for (_op, value) in group.values() {
        let Ok(obj) = value.read_object() else {
            continue;
        };
        for (key, mod_group) in obj.field_groups() {
            if key.read_str() != "modifier" {
                continue;
            }
            for (_op, mv) in mod_group.values() {
                let Ok(mod_obj) = mv.read_object() else {
                    continue;
                };
                let mut name = String::new();
                let mut values = Vec::new();
                let mut enabled = false;
                for (fk, fg) in mod_obj.field_groups() {
                    match fk.read_str().as_ref() {
                        "modifier" => {
                            for (_op, v) in fg.values() {
                                name = read_text(&v);
                            }
                        }
                        "value" => {
                            for (_op, v) in fg.values() {
                                // Bare float ARRAY (like `tags = { "ITA" }`),
                                // not key-groups.
                                if let Ok(arr) = v.read_array() {
                                    for av in arr.values() {
                                        if let Ok(s) = av.read_scalar() {
                                            if let Ok(f) = s.to_f64() {
                                                values.push(f as f32);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        "enabled" => {
                            for (_op, v) in fg.values() {
                                enabled = read_text(&v) == "yes";
                            }
                        }
                        _ => {}
                    }
                }
                if enabled && !name.is_empty() {
                    out.push((name, values));
                }
            }
        }
    }
    out
}

fn parse_division(
    value: &ValueReader<'_, '_, Encoding>,
    by_name: &HashMap<String, (Vec<BattalionInfo>, Vec<BattalionInfo>)>,
    id_to_name: &HashMap<u64, String>,
    id_to_group: &HashMap<u64, Option<String>>,
    equip_ids: &HashMap<u64, String>,
) -> Result<DivisionData, SaveError> {
    let obj = value
        .read_object()
        .map_err(|e| SaveError::Parse(format!("division is not an object: {e}")))?;

    let mut id: u64 = 0;
    let mut name = String::new();
    // Player-renamed divisions carry BOTH `name` (auto name) and
    // `division_name.override`; keep the override separate so the winner
    // doesn't depend on file field order.
    let mut name_override: Option<String> = None;
    // `division_name.name_order` — the auto-name issue number within the
    // template's names-group (`type` is reserved and always 0 in 1.19.2).
    let mut name_order: Option<u32> = None;
    let mut raw_template_name = String::new();
    let mut template_id: Option<u64> = None;
    let mut location: Option<u32> = None;
    let mut organization: f32 = 1.0;
    let mut strength: f32 = 1.0;
    // -1 = field absent (synthetic fixtures) → the assembly maps
    // it to neutral Trained; an explicit `experience=0` is real Green.
    let mut experience: f32 = -1.0;
    let mut entrenchment: f32 = 0.0;
    let mut supply_status: Option<f32> = None;
    let mut equipment: HashMap<String, f32> = HashMap::new();

    for (field_key, field_group) in obj.field_groups() {
        match field_key.read_str().as_ref() {
            "id" => {
                for (_op, v) in field_group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        id = scalar.to_u64().unwrap_or(0);
                    } else {
                        id = read_id_from_value(&v).unwrap_or(0);
                    }
                }
            }
            "name" => {
                for (_op, v) in field_group.values() {
                    name = read_text(&v);
                }
            }
            "division_name" => {
                // Real saves: division_name = { type = 0 name_order = N }
                // (+ optional override = "..." for player renames)
                for (_op, v) in field_group.values() {
                    if let Ok(name_obj) = v.read_object() {
                        for (nk, ng) in name_obj.field_groups() {
                            match nk.read_str().as_ref() {
                                "override" => {
                                    for (_op, nv) in ng.values() {
                                        name_override = Some(read_text(&nv));
                                    }
                                }
                                "name_order" => {
                                    for (_op, nv) in ng.values() {
                                        if let Ok(scalar) = nv.read_scalar() {
                                            name_order =
                                                scalar.to_u64().ok().map(|x| x as u32);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            "division_template" => {
                for (_op, v) in field_group.values() {
                    raw_template_name = read_text(&v);
                }
            }
            "division_template_id" => {
                for (_op, v) in field_group.values() {
                    template_id = read_id_from_value(&v);
                }
            }
            "location" => {
                for (_op, v) in field_group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        location = scalar.to_u64().ok().map(|x| x as u32);
                    }
                }
            }
            "organization" | "organisation" => {
                for (_op, v) in field_group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        organization = scalar.to_f64().unwrap_or(1.0) as f32;
                    }
                }
            }
            "strength" => {
                for (_op, v) in field_group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        strength = scalar.to_f64().unwrap_or(1.0) as f32;
                    }
                }
            }
            "experience" => {
                for (_op, v) in field_group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        experience = scalar.to_f64().unwrap_or(0.0) as f32;
                    }
                }
            }
            "entrenchment" | "dig_in" => {
                for (_op, v) in field_group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        entrenchment = scalar.to_f64().unwrap_or(0.0) as f32;
                    }
                }
            }
            "supply_status" => {
                for (_op, v) in field_group.values() {
                    if let Ok(scalar) = v.read_scalar() {
                        supply_status = Some(scalar.to_f64().unwrap_or(1.0) as f32);
                    }
                }
            }
            "equipment" => {
                // Two forms: legacy `token = count` scalars, and the 1.19
                // id-reference form `equipment = { id = { id = N type = 70 }
                // amount = M }` resolved through the root `equipments`
                // registry (without it every division parsed
                // with EMPTY equipment and stats collapsed to zero).
                for (_op, v) in field_group.values() {
                    if let Ok(eq_obj) = v.read_object() {
                        for (eq_key, eq_group) in eq_obj.field_groups() {
                            let eq_name = eq_key.read_str().to_string();
                            for (_op, eq_value) in eq_group.values() {
                                if let Ok(scalar) = eq_value.read_scalar() {
                                    if let Ok(count) = scalar.to_f64() {
                                        equipment.insert(eq_name.clone(), count as f32);
                                    }
                                } else if eq_name == "equipment" {
                                    let Ok(entry) = eq_value.read_object() else {
                                        continue;
                                    };
                                    let mut eid: Option<u64> = None;
                                    let mut amount: f32 = 0.0;
                                    for (k, g) in entry.field_groups() {
                                        match k.read_str().as_ref() {
                                            "id" => eid = read_id_object(&g),
                                            "amount" => {
                                                for (_op, av) in g.values() {
                                                    if let Ok(s) = av.read_scalar() {
                                                        amount = s.to_f64().unwrap_or(0.0) as f32;
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    if let Some(token) = eid.and_then(|i| equip_ids.get(&i)) {
                                        *equipment.entry(token.clone()).or_insert(0.0) += amount;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Resolve template composition: by id FIRST (real saves reference
    // `division_template_id`; the bare `division_template = "name"` field
    // is a legacy/country-level form), then by name. Never look up an
    // EMPTY name — 1.19 army-HQ templates have no `name=` and must not be
    // reachable through `by_name[""]` (every unresolved division would
    // silently inherit HQ content).
    let mut template_name = raw_template_name;
    let mut resolved: Option<&(Vec<BattalionInfo>, Vec<BattalionInfo>)> = None;
    if let Some(name) = template_id.and_then(|tid| id_to_name.get(&tid)) {
        if let Some(entry) = by_name.get(name) {
            template_name = name.clone();
            resolved = Some(entry);
        }
    }
    if resolved.is_none() && !template_name.is_empty() {
        resolved = by_name.get(&template_name);
    }
    let (battalions, support_companies) = match resolved {
        Some((b, s)) => (b.clone(), s.clone()),
        None => (Vec::new(), Vec::new()),
    };

    // A player rename already IS the final name — suppress the token pair
    // so downstream name resolution can never clobber it.
    let renamed = name_override.is_some();
    Ok(DivisionData {
        id,
        // Player rename wins over the auto-generated name, order-independent.
        // 1.19.2: auto names are `division_name={type,name_order}`
        // tokens, not a literal `name=` — most divisions parse with an
        // EMPTY name, and the OOB/HQ/deploy-sector grouping keys off this
        // field (empty = one big "unattached" blob). Synthesize a stable
        // fallback from template + serial so every division stays its own
        // group; the live assembly then upgrades it to the real in-game
        // name through `names_group` + `name_order` (crate::NameGroups).
        name: match name_override.unwrap_or(name) {
            n if !n.is_empty() => n,
            _ if !template_name.is_empty() => format!("{template_name} #{id}"),
            _ => format!("Division #{id}"),
        },
        // A player rename already IS the final name — suppress the token
        // pair so downstream resolution can never clobber it.
        name_order: if renamed { None } else { name_order },
        names_group: template_id
            .and_then(|tid| id_to_group.get(&tid).cloned())
            .flatten(),
        template_name,
        location,
        organization,
        strength,
        experience,
        entrenchment,
        supply_status,
        equipment,
        battalions,
        support_companies,
    })
}

/// `id = { id = N type = ... }` object read through a group entry.
fn read_id_object(group: &GroupEntry<'_, '_, Encoding>) -> Option<u64> {
    for (_op, v) in group.values() {
        if let Some(id) = read_id_from_value(&v) {
            return Some(id);
        }
    }
    None
}

fn read_id_from_value(v: &ValueReader<'_, '_, Encoding>) -> Option<u64> {
    let obj = v.read_object().ok()?;
    for (k, g) in obj.field_groups() {
        if k.read_str() == "id" {
            for (_op, iv) in g.values() {
                if let Ok(scalar) = iv.read_scalar() {
                    if let Ok(id) = scalar.to_u64() {
                        return Some(id);
                    }
                }
            }
        }
    }
    None
}

/// Read a possibly-quoted string scalar.
fn read_text(v: &ValueReader<'_, '_, Encoding>) -> String {
    if let Ok(s) = v.read_string() {
        s
    } else if let Ok(s) = v.read_str() {
        s.to_string()
    } else {
        String::new()
    }
}

/// `technologies` / `active_ideas` / `ideas`: object keys (values may be
/// `{ level = N }` objects), plain scalar lists, or — 1.19 national spirits
/// (`ideas = { tok1 tok2 }`) — a bare-token ARRAY (§5.2).
fn parse_token_list(group: &GroupEntry<'_, '_, Encoding>) -> Vec<String> {
    let mut items = Vec::new();
    for (_op, value) in group.values() {
        if let Ok(arr) = value.read_array() {
            // Bare list: tokens (quoted or not), no `=` anywhere.
            for av in arr.values() {
                let text = read_text(&av);
                if !text.is_empty() {
                    items.push(text);
                }
            }
        } else if let Ok(obj) = value.read_object() {
            for (item_key, _item_group) in obj.field_groups() {
                items.push(item_key.read_str().to_string());
            }
        } else {
            let text = read_text(&value);
            if !text.is_empty() {
                items.push(text);
            }
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic save: root-level template (real format, referenced by id)
    /// plus a country-level template (fallback, referenced by name).
    const SYNTH_SAVE: &str = r#"
division_templates = {
    division_template = {
        id = { id = 7 type = 47 }
        name = "Infanterie-Division"
        regiments = {
            infantry = { x = 0 y = 0 }
            infantry = { x = 1 y = 0 }
            light_armor = { x = 2 y = 0 }
        }
        support = {
            engineer = { x = 0 y = 0 }
        }
    }
}
countries = {
    GER = {
        division_template = {
            "Reserve-Division" = {
                regiments = {
                    cavalry = { x = 0 y = 0 }
                }
            }
        }
        units = {
            division = {
                id = { id = 100 type = 46 }
                name = "1. Infanterie-Division"
                division_template_id = { id = 7 type = 47 }
                location = 6334
                organization = 0.85
                strength = 0.90
                experience = 0.35
                entrenchment = 2.0
                supply_status = 0.75
                equipment = {
                    infantry_equipment_1 = 180
                    light_tank_chassis_1 = 60
                    support_equipment = 30
                }
            }
            division = {
                id = 101
                name = "2. Reserve-Division"
                division_template = "Reserve-Division"
                location = 6334
                organization = 1.0
                strength = 1.0
            }
            division = {
                id = 102
                name = "3. Elsewhere"
                division_template = "Reserve-Division"
                location = 9999
            }
        }
        technologies = {
            infantry_weapons = { level = 1 }
            mobile_warfare = { level = 1 }
        }
        active_ideas = {
            war_economy = { level = 1 }
        }
    }
}
"#;

    #[test]
    fn hoi4txt_header_is_stripped() {
        for prefix in ["HOI4txt\n", "HOI4txt\r\n", "HOI4txt"] {
            let text = format!("{prefix}{SYNTH_SAVE}");
            let save = SaveParser::parse_save_from_str(&text)
                .unwrap_or_else(|e| panic!("parse failed with prefix {prefix:?}: {e}"));
            assert!(save.countries.contains_key("GER"));
        }
    }

    #[test]
    fn binary_save_is_rejected_with_guidance() {
        // HOI4bin magic.
        let err = SaveParser::parse_save_from_str("HOI4bin\u{0}\u{1}\u{2}garbage").unwrap_err();
        assert!(matches!(err, SaveError::Binary));
        let msg = err.to_string();
        assert!(
            msg.contains("save_as_binary=no"),
            "message should guide the user: {msg}"
        );
        // NUL-byte heuristic without the magic.
        let err = SaveParser::parse_save_from_str("pk\u{0}\u{0}not-text").unwrap_err();
        assert!(matches!(err, SaveError::Binary));
    }

    #[test]
    fn division_name_tokens_and_template_names_group_are_captured() {
        // 1.19.2 real-save shapes: the template carries
        // `division_names_group`, the division carries
        // `division_name = { type name_order (override) }`.
        let text = r#"HOI4txt
division_templates = {
    division_template = {
        id = { id = 443 type = 52 }
        name = "Infanterie-Division"
        division_names_group = "GER_Inf_01"
        regiments = { infantry = { x = 0 y = 0 } }
    }
}
countries = {
    GER = {
        units = {
            division = {
                id = { id = 1 type = 51 }
                division_template_id = { id = 443 type = 52 }
                division_name = { type = 0 name_order = 25 }
                location = 6334
            }
            division = {
                id = { id = 2 type = 51 }
                division_template_id = { id = 443 type = 52 }
                division_name = { type = 0 name_order = 3 override = "Meine Division" }
                location = 6334
            }
        }
    }
}
"#;
        let save = SaveParser::parse_save_from_str(text).unwrap();
        let divs = &save.countries["GER"].divisions;
        assert_eq!(divs.len(), 2);
        // Auto-named: token pair captured for downstream resolution; the
        // placeholder name stays the synthesized template+serial fallback.
        assert_eq!(divs[0].names_group.as_deref(), Some("GER_Inf_01"));
        assert_eq!(divs[0].name_order, Some(25));
        assert_eq!(divs[0].name, "Infanterie-Division #1");
        // Player-renamed: the override wins the name AND suppresses the
        // token pair (downstream resolution must never clobber it).
        assert_eq!(divs[1].name, "Meine Division");
        assert_eq!(divs[1].name_order, None);
        assert_eq!(divs[1].names_group.as_deref(), Some("GER_Inf_01"));
    }

    #[test]
    fn malformed_text_returns_error_not_panic() {
        let result = SaveParser::parse_save_from_str("countries = { GER = { units = {");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SaveError::Parse(_)));
    }

    #[test]
    fn root_player_tag_is_parsed() {
        // Real 1.19.2 save header: the live loop takes the tac
        // tag from here because the mod cannot print it through HOI4's
        // log interpolation.
        let text = "HOI4txt\nplayer=\"ITA\"\ndate=\"1936.1.1.13\"\n".to_owned() + SYNTH_SAVE;
        let save = SaveParser::parse_save_from_str(&text).unwrap();
        assert_eq!(save.player.as_deref(), Some("ITA"));
        // Absent (multiplayer / odd saves) → None, never an error.
        let save = SaveParser::parse_save_from_str(SYNTH_SAVE).unwrap();
        assert_eq!(save.player, None);
    }

    #[test]
    fn save_date_header_is_stored() {
        // The live battle clock starts from this header time.
        let text = "HOI4txt\nplayer=\"ITA\"\ndate=\"1936.1.1.13\"\n".to_owned() + SYNTH_SAVE;
        let save = SaveParser::parse_save_from_str(&text).unwrap();
        assert_eq!(save.date, Some((1936, 1, 1, 13)));
        // Absent → None (elapsed-only clock fallback), never an error.
        let save = SaveParser::parse_save_from_str(SYNTH_SAVE).unwrap();
        assert_eq!(save.date, None);
    }

    #[test]
    fn land_combat_block_is_parsed() {
        // Real 1.19.2 shape: root `combat` block
        // with repeated `land_combat` groups.
        let text = r#"HOI4txt
player="ITA"
combat={
    land_combat={
        id={ id=1 type=62 }
        location=13237
        attacker={
            unit={ id=1115 type=51 }
            unit={ id=1116 type=51 }
            tactic=12
            log={
                combat_side_data={
                    tags={
                        "ITA"
                    }
                }
            }
        }
        defender={
            unit={ id=1831 type=51 }
            unit={ id=1832 type=51 }
            tactic=2
            log={
                combat_side_data={
                    tags={
                        "ETH"
                    }
                }
            }
        }
        terrain="hills"
    }
    land_combat={
        location=9999
        attacker={ }
        defender={ }
    }
}
"#;
        let save = SaveParser::parse_save_from_str(text).unwrap();
        assert_eq!(save.land_combats.len(), 2);
        let b = &save.land_combats[0];
        assert_eq!(b.location, 13237);
        assert_eq!(b.attacker.unit_ids, vec![1115, 1116]);
        assert_eq!(b.attacker.tactic, Some(12));
        assert_eq!(b.attacker.tags, vec!["ITA".to_string()]);
        assert_eq!(b.defender.unit_ids, vec![1831, 1832]);
        assert_eq!(b.defender.tactic, Some(2));
        assert_eq!(b.defender.tags, vec!["ETH".to_string()]);
        // Sparse second battle: sides default, location still read.
        assert_eq!(save.land_combats[1].location, 9999);
        assert!(save.land_combats[1].attacker.unit_ids.is_empty());
        // The vanilla id table is 1-based: id 12 = shock, id 2 = basic_defend.
        assert_eq!(crate::COMBAT_TACTIC_IDS[11], "shock");
        assert_eq!(crate::COMBAT_TACTIC_IDS[1], "basic_defend");
    }

    #[test]
    fn picked_states_reads_tac_pick_variable() {
        // The state-target entry decision marks the
        // picked battle state with `tac_pick=1` in its `variables` block
        // (this mirrors the in-game serialization form).
        // NB: no leading newline — HOI4txt must be the first bytes or the
        // header is not stripped and the tape misparses silently.
        let text = "HOI4txt\nstates={\n    2={\n        owner=\"ITA\"\n        variables={\n            random={ 284725575 2631284997 }\n            tac_pick=1\n        }\n    }\n    3={\n        owner=\"ITA\"\n        variables={\n            tac_pick=0\n        }\n    }\n    445={\n        owner=\"FRA\"\n        flags={\n            tac_hot={ value=1 date=\"1936.1.1.12\" }\n        }\n    }\n}\n";
        let save = SaveParser::parse_save_from_str(text).unwrap();
        assert_eq!(save.picked_states, vec![2]);
    }

    #[test]
    fn root_level_templates_and_id_resolution() {
        let save = SaveParser::parse_save_from_str(SYNTH_SAVE).unwrap();
        // Root template was parsed.
        assert_eq!(save.templates.len(), 1);
        assert_eq!(save.templates[0].name, "Infanterie-Division");
        assert_eq!(save.templates[0].id, Some(7));

        let ger = &save.countries["GER"];
        assert_eq!(ger.divisions.len(), 3);
        let div = &ger.divisions[0];
        assert_eq!(div.id, 100);
        assert_eq!(div.name, "1. Infanterie-Division");
        // Resolved via division_template_id = 7.
        assert_eq!(div.template_name, "Infanterie-Division");
        assert_eq!(div.battalions.len(), 2); // infantry x2, light_armor x1
        assert_eq!(div.support_companies.len(), 1);
        let inf = div
            .battalions
            .iter()
            .find(|b| b.token == "infantry")
            .unwrap();
        assert_eq!(inf.count, 2);
        assert_eq!(inf.unit_type, tactical_core::UnitType::Infantry);
        let armor = div
            .battalions
            .iter()
            .find(|b| b.token == "light_armor")
            .unwrap();
        assert_eq!(armor.unit_type, tactical_core::UnitType::LightArmor);
    }

    #[test]
    fn country_level_template_fallback_by_name() {
        let save = SaveParser::parse_save_from_str(SYNTH_SAVE).unwrap();
        let ger = &save.countries["GER"];
        let div = ger.divisions.iter().find(|d| d.id == 101).unwrap();
        assert_eq!(div.template_name, "Reserve-Division");
        assert_eq!(div.battalions.len(), 1);
        assert_eq!(
            div.battalions[0].unit_type,
            tactical_core::UnitType::Cavalry
        );
    }

    #[test]
    fn division_equipment_id_references_resolve_via_registry() {
        // 1.19.2 real-save form: divisions hold
        // `equipment = { equipment = { id = { id = N type = 70 } amount = M } }`,
        // resolved through the root `equipments` block's id → token map.
        let text = r#"
equipments = {
    infantry_equipment_1 = { id = { id = 1326 type = 70 } }
    support_equipment_1 = { id = { id = 1327 type = 70 } }
}
countries = {
    ITA = {
        units = {
            division = {
                id = { id = 1118 type = 51 }
                location = 13250
                organization = 0.9
                strength = 0.95
                equipment = {
                    equipment = { id = { id = 1326 type = 70 } amount = 910 }
                    equipment = { id = { id = 1327 type = 70 } amount = 30 }
                    equipment = { id = { id = 9999 type = 70 } amount = 5 }
                }
            }
        }
    }
}
"#;
        let save = SaveParser::parse_save_from_str(text).unwrap();
        let div = &save.countries["ITA"].divisions[0];
        assert_eq!(div.equipment.get("infantry_equipment_1"), Some(&910.0));
        assert_eq!(div.equipment.get("support_equipment_1"), Some(&30.0));
        // Unknown registry ids are ignored gracefully (§11.3).
        assert_eq!(div.equipment.len(), 2);
    }

    #[test]
    fn nameless_templates_do_not_poison_id_resolution() {
        // Regression guard (1.19.2 real saves): army-HQ templates carry
        // `localization_key=` instead of `name=` — 438 of 698 in a 1936
        // save. An empty template name must NOT become a by_name[""] entry
        // that silently captures every unresolved division (CC.NN.
        // divisions resolved to hq_infantry×2 + hq_support_company).
        let text = r#"
division_templates = {
    division_template = {
        id = { id = 9 type = 52 }
        localization_key = "ARMY_HQ_TEMPLATE_NAME"
        regiments = { hq_infantry = { x = 0 y = 0 } hq_infantry = { x = 0 y = 1 } }
    }
    division_template = {
        id = { id = 7 type = 47 }
        name = "Camicie Nere"
        regiments = { militia = { x = 0 y = 0 } militia = { x = 0 y = 1 } militia = { x = 1 y = 0 } }
        support = { engineer = { x = 0 y = 0 } }
    }
}
countries = {
    ITA = {
        units = {
            division = {
                id = { id = 1118 type = 51 }
                division_template_id = { id = 7 type = 47 }
                location = 13250
                organization = 0.9
                strength = 0.95
            }
        }
    }
}
"#;
        let save = SaveParser::parse_save_from_str(text).unwrap();
        let div = &save.countries["ITA"].divisions[0];
        // The id reference wins even though the division has no name field:
        assert_eq!(div.template_name, "Camicie Nere");
        assert_eq!(div.battalions.len(), 1);
        assert_eq!(div.battalions[0].token, "militia");
        assert_eq!(div.battalions[0].count, 3);
        assert_eq!(div.support_companies.len(), 1);
        assert_eq!(div.support_companies[0].token, "engineer");
    }

    #[test]
    fn division_fields_are_extracted() {
        let save = SaveParser::parse_save_from_str(SYNTH_SAVE).unwrap();
        let div = &save.countries["GER"].divisions[0];
        assert_eq!(div.location, Some(6334));
        assert!((div.organization - 0.85).abs() < 1e-4);
        assert!((div.strength - 0.90).abs() < 1e-4);
        assert!((div.experience - 0.35).abs() < 1e-4);
        assert!((div.entrenchment - 2.0).abs() < 1e-4);
        assert_eq!(div.supply_status, Some(0.75));
        assert_eq!(div.equipment.get("infantry_equipment_1"), Some(&180.0));
        assert_eq!(div.equipment.get("support_equipment"), Some(&30.0));
        // Division without optional fields gets defaults / None.
        let div3 = save.countries["GER"]
            .divisions
            .iter()
            .find(|d| d.id == 102)
            .unwrap();
        assert_eq!(div3.supply_status, None);
        assert!((div3.organization - 1.0).abs() < 1e-6);
    }

    #[test]
    fn technologies_and_ideas_are_extracted() {
        let save = SaveParser::parse_save_from_str(SYNTH_SAVE).unwrap();
        let ger = &save.countries["GER"];
        assert!(ger.technologies.contains(&"infantry_weapons".to_string()));
        assert!(ger.technologies.contains(&"mobile_warfare".to_string()));
        assert_eq!(ger.active_ideas, vec!["war_economy".to_string()]);
    }

    #[test]
    fn bare_ideas_list_and_dynamic_modifiers_are_extracted() {
        // 1.19.2 real-save shapes: national
        // spirits as a bare token list under `ideas`, dynamic modifiers as
        // `dynamic_modifier = { modifier = { modifier="NAME" value={ floats }
        // enabled=yes } … }`.
        let text = r#"
countries = {
    ITA = {
        ideas = { ITA_vittoria_mutilata ITA_king_victor_emanuel }
        dynamic_modifier = {
            modifier = {
                modifier = "ITA_regio_esercito_dynamic_modifier"
                value = {
                    0.1 0.1 -0.1 -0.1 0.15 0 0 0
                }
                enabled = yes
            }
            modifier = {
                modifier = "ITA_disabled_one"
                value = { 0.5 0.5 }
                enabled = no
            }
        }
    }
    JAP = {
        active_ideas = {
            war_economy = { level = 1 }
        }
        ideas = { JAP_army_faction_tier_2_equal_navy }
    }
}
"#;
        let save = SaveParser::parse_save_from_str(text).unwrap();
        let ita = &save.countries["ITA"];
        assert_eq!(
            ita.active_ideas,
            vec![
                "ITA_king_victor_emanuel".to_string(),
                "ITA_vittoria_mutilata".to_string()
            ]
        );
        // Only the enabled entry is collected, values in file order.
        assert_eq!(ita.dynamic_modifiers.len(), 1);
        let (name, values) = &ita.dynamic_modifiers[0];
        assert_eq!(name, "ITA_regio_esercito_dynamic_modifier");
        assert_eq!(values.len(), 8);
        assert!((values[2] + 0.1).abs() < 1e-6);
        assert!((values[4] - 0.15).abs() < 1e-6);
        // Both idea keys merge into one list.
        let jap = &save.countries["JAP"];
        assert!(jap.active_ideas.contains(&"war_economy".to_string()));
        assert!(jap
            .active_ideas
            .contains(&"JAP_army_faction_tier_2_equal_navy".to_string()));
    }

    #[test]
    fn character_manager_leader_traits_are_extracted_with_expire_filter() {
        // 1.19.2 real-save shape:
        // `character_manager.historical.character` blocks; a `country_leader`
        // role carries a bare-token `traits` list and an `expire` end date.
        // Save date 1936.1.1.13 → the 1935-expired role is skipped, the
        // 1965 one is active.
        let text = "HOI4txt\ndate=\"1936.1.1.13\"\n".to_owned()
            + r#"
character_manager = {
    historical = {
        character = {
            id = { id = 496 type = 4713 }
            token = "ITA_benito_mussolini"
            country = "ITA"
            portraits = { civilian = { large = "GFX_x" } }
            country_leaders = {
                country_leader = {
                    ideology = fascism_ideology
                    traits = { il_duce }
                    expire = "1965.1.1.1"
                }
            }
            variables = { random = { 1 2 } }
        }
        character = {
            id = { id = 497 type = 4713 }
            country = "ITA"
            country_leaders = {
                country_leader = {
                    ideology = fascism_ideology
                    traits = { outdated_trait }
                    expire = "1935.6.1.1"
                }
            }
        }
        character = {
            id = { id = 500 type = 4713 }
            country = "ETH"
            country_leaders = {
                country_leader = {
                    ideology = despotism
                    expire = "1950.1.1.1"
                }
            }
        }
        character = {
            id = { id = 600 type = 4713 }
            country = "ITA"
            corps_commander = { skill = 2 }
        }
    }
}
countries = {
    ITA = { }
    ETH = { }
}
"#;
        let save = SaveParser::parse_save_from_str(&text).unwrap();
        // Active role's traits collected; the expired role is filtered out.
        assert_eq!(
            save.countries["ITA"].leader_traits,
            vec!["il_duce".to_string()]
        );
        // Role without traits → nothing collected.
        assert!(save.countries["ETH"].leader_traits.is_empty());
    }

    #[test]
    fn appointed_advisors_resolve_idea_tokens_by_slot() {
        // 1.19.2 real-save shapes:
        // the country block's `appointed_advisors` anonymous entries carry
        // (slot, character id); the character's `advisors` role blocks carry
        // (slot, idea_token). Same-slot role wins; no same-slot role falls
        // back to the first role block; unknown ids skip silently.
        let text = "HOI4txt\ndate=\"1936.1.1.13\"\n".to_owned()
            + r#"
character_manager = {
    historical = {
        character = {
            id = { id = 4432 type = 73 }
            token = "ITA_mario_roatta"
            country = "ITA"
            advisors = {
                advisor = {
                    slot = "high_command"
                    idea_token = "roatta_high_command"
                }
                advisor = {
                    slot = "political_advisor"
                    idea_token = "mario_roatta_political_advisor"
                }
            }
        }
        character = {
            id = { id = 2434 type = 73 }
            token = "ITA_some_chief"
            country = "ITA"
            advisors = {
                advisor = {
                    slot = "army_chief"
                    idea_token = "some_chief_token"
                }
            }
        }
    }
}
countries = {
    ITA = {
        appointed_advisors = {
            {
                slot = "political_advisor"
                character = { id = 4432 type = 73 }
            }
            {
                slot = "high_command"
                character = { id = 2434 type = 73 }
            }
            {
                slot = "army_chief"
                character = { id = 9999 type = 73 }
            }
        }
    }
    ETH = { }
}
"#;
        let save = SaveParser::parse_save_from_str(&text).unwrap();
        let ita = &save.countries["ITA"];
        // Slot match: character 4432's political_advisor role (NOT its
        // first block). Fallback: character 2434 has no high_command role →
        // its first (only) block's token. Unknown id 9999 skipped.
        assert_eq!(
            ita.active_advisors,
            vec![
                "mario_roatta_political_advisor".to_string(),
                "some_chief_token".to_string()
            ]
        );
        assert!(save.countries["ETH"].active_advisors.is_empty());
    }

    #[test]
    fn unknown_subunit_token_falls_back_to_infantry() {
        let text = r#"
countries = {
    GER = {
        division_template = {
            "Odd" = {
                regiments = {
                    warpack = { x = 0 y = 0 }
                    warpack = { x = 1 y = 0 }
                }
            }
        }
        units = {
            division = {
                id = 1
                division_template = "Odd"
            }
        }
    }
}
"#;
        let save = SaveParser::parse_save_from_str(text).unwrap();
        let div = &save.countries["GER"].divisions[0];
        assert_eq!(div.battalions.len(), 1);
        assert_eq!(div.battalions[0].count, 2);
        assert_eq!(
            div.battalions[0].unit_type,
            tactical_core::UnitType::Infantry
        );
        // Original token retained as the log-friendly marker.
        assert_eq!(div.battalions[0].token, "warpack");
    }

    #[test]
    fn find_divisions_in_province_filters_by_tag_and_location() {
        let save = SaveParser::parse_save_from_str(SYNTH_SAVE).unwrap();
        let found = find_divisions_in_province(&save, "GER", 6334);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|d| d.location == Some(6334)));
        assert!(find_divisions_in_province(&save, "GER", 1).is_empty());
        assert!(find_divisions_in_province(&save, "FRA", 6334).is_empty());
    }

    #[test]
    fn parse_save_reads_file_from_disk() {
        let path =
            std::env::temp_dir().join(format!("tactical_save_test_{}.hoi4", std::process::id()));
        let content = format!("HOI4txt\r\n{SYNTH_SAVE}");
        std::fs::write(&path, &content).unwrap();
        let save = SaveParser::parse_save(&path).unwrap();
        assert!(save.countries.contains_key("GER"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_is_io_error() {
        let path = std::path::Path::new("definitely/not/a/real/save.hoi4");
        let err = SaveParser::parse_save(path).unwrap_err();
        assert!(matches!(err, SaveError::Io { .. }));
    }

    #[test]
    fn theatres_and_unit_leaders_are_extracted() {
        // 1.19.2 real-save shapes: armies
        // as `theatres.theatre.orders_group` blocks (`member = { unit =
        // {…} }`, optional `leader` id object), the army group as a nested
        // `field_marshal_group` whose bare child `orders_group = { id = N
        // … }` one-liners may precede its own id, and the commanders as
        // `character_manager.historical.character.corps_commander` /
        // `field_marshal` blocks (leader instance ids are type 4713;
        // `navy_leader` blocks share the id space but must be skipped).
        let text = r#"HOI4txt
character_manager = {
    historical = {
        character = {
            id = { id = 100 type = 73 }
            country = "GER"
            corps_commander = {
                id = { id = 500 type = 4713 }
                name = "GER_general"
                skill = 4
                traits = { panzer_leader infantry_officer }
                attack_skill = 4
                defense_skill = 3
            }
        }
        character = {
            id = { id = 101 type = 73 }
            country = "GER"
            field_marshal = {
                id = { id = 600 type = 4713 }
                skill = 5
                traits = { unyielding_defender }
                attack_skill = 5
                defense_skill = 2
            }
        }
        character = {
            id = { id = 102 type = 73 }
            country = "GER"
            navy_leader = {
                id = { id = 700 type = 4713 }
                attack_skill = 9
                defense_skill = 9
            }
        }
    }
}
countries = {
    GER = {
        theatres = {
            theatre = {
                id = { id = 1 type = 67 }
                orders_group = {
                    id = { id = 42 type = 53 }
                    name = "1. Armee"
                    member = { unit = { id = 1001 type = 51 } }
                    member = { unit = { id = 1002 type = 51 } }
                    leader = { id = 500 type = 4713 }
                    field_marshal_group = no
                }
                orders_group = {
                    id = { id = 43 type = 53 }
                    member = { unit = { id = 1003 type = 51 } }
                    field_marshal_group = no
                }
                field_marshal_group = {
                    orders_group = { id = 42 type = 53 }
                    orders_group = { id = 43 type = 53 }
                    id = { id = 44 type = 53 }
                    leader = { id = 600 type = 4713 }
                    field_marshal_group = yes
                }
                unit = { id = 1004 type = 51 }
            }
        }
    }
}
"#;
        let save = SaveParser::parse_save_from_str(text).unwrap();
        // Leaders: general + FM keyed by leader instance id; the navy
        // leader block is skipped by key.
        assert_eq!(save.leaders.len(), 2);
        let general = &save.leaders[&500];
        assert!(!general.is_field_marshal);
        assert!((general.attack_skill - 4.0).abs() < 1e-6);
        assert!((general.defense_skill - 3.0).abs() < 1e-6);
        assert_eq!(
            general.traits,
            vec!["panzer_leader".to_string(), "infantry_officer".to_string()]
        );
        let marshal = &save.leaders[&600];
        assert!(marshal.is_field_marshal);
        assert!((marshal.attack_skill - 5.0).abs() < 1e-6);
        assert!((marshal.defense_skill - 2.0).abs() < 1e-6);
        assert_eq!(marshal.traits, vec!["unyielding_defender".to_string()]);
        // Armies: two orders_groups plus the FM group. The unattached
        // `unit = {…}` line is not an army.
        assert_eq!(save.armies.len(), 3);
        let army = save.armies.iter().find(|a| a.id == 42).unwrap();
        assert!(!army.is_fm_group);
        assert_eq!(army.members, vec![1001, 1002]);
        assert_eq!(army.leader, Some(500));
        assert!(army.child_armies.is_empty());
        // The leaderless army keeps its members; leader is None (§11.3).
        let leaderless = save.armies.iter().find(|a| a.id == 43).unwrap();
        assert_eq!(leaderless.leader, None);
        assert_eq!(leaderless.members, vec![1003]);
        // The FM group: child refs read even though they precede its id.
        let group = save.armies.iter().find(|a| a.id == 44).unwrap();
        assert!(group.is_fm_group);
        assert_eq!(group.child_armies, vec![42, 43]);
        assert_eq!(group.leader, Some(600));
        assert!(group.members.is_empty());
    }
}
