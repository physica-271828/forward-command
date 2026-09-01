//! Loaders for the pre-extracted JSON data tables (DESIGN.md §5.4):
//! `equipment_stats.json`, `unit_templates.json`, `doctrine_bonuses.json`,
//! `modifiers.json`.
//!
//! The loaders match the REAL shapes found in `data/` (which differ slightly
//! from the illustrative snippets in §5.4):
//! - equipment: flat map `key → stats`; most types exist only as variants
//!   (`infantry_equipment_0..3`) whose `archetype` field names the base type
//!   (`infantry_equipment`). Archetype entries, when present, have
//!   `archetype: null`.
//! - unit templates: `{ "line_battalions": {...}, "support_companies": {...} }`
//!   with equipment requirements under `needs` (plural).
//! - doctrines: `{ tree → { path, nodes: { node → modifiers } } }` where
//!   combat bonuses hide inside `category_modifiers` objects (possibly nested
//!   under `rewards`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tactical_core::{TerrainAdjusters, UnitType};

use crate::SaveError;

fn read_json(path: &Path) -> Result<String, SaveError> {
    std::fs::read_to_string(path).map_err(|e| SaveError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

fn json_err(path: &str, e: serde_json::Error) -> SaveError {
    SaveError::Data {
        path: PathBuf::from(path),
        message: e.to_string(),
    }
}

// ---------------------------------------------------------------------------
// equipment_stats.json (§5.4)
// ---------------------------------------------------------------------------

/// One equipment entry from `equipment_stats.json`.
///
/// All numeric fields default to 0 so partial entries still load. Unknown
/// extra fields (`resources`, `categories`, `group`, ...) are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct EquipmentStats {
    /// Base archetype name (e.g. "infantry_equipment"); `null` on entries that
    /// ARE the archetype (e.g. "artillery_equipment").
    #[serde(default)]
    pub archetype: Option<String>,
    /// Introduction year; `null` in the data for a few entries (treated as 0
    /// when picking the latest variant of an archetype).
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub soft_attack: f32,
    #[serde(default)]
    pub hard_attack: f32,
    #[serde(default)]
    pub defense: f32,
    #[serde(default)]
    pub breakthrough: f32,
    #[serde(default)]
    pub armor: f32,
    #[serde(default)]
    pub piercing: f32,
    #[serde(default)]
    pub hardness: f32,
    #[serde(default)]
    pub max_speed: f32,
    #[serde(default)]
    pub reliability: f32,
    #[serde(default)]
    pub supply_use: f32,
    #[serde(default)]
    pub build_cost: f32,
}

/// Equipment table with exact-key lookup plus archetype resolution.
///
/// Archetype resolution is required in practice: save files and unit
/// templates refer to archetypes (`infantry_equipment`, `light_tank_chassis`,
/// `support_equipment`) but the real JSON only carries variant keys for most
/// of them (`infantry_equipment_0..3`, `light_tank_chassis_0..3`,
/// `support_equipment_1`).
#[derive(Debug, Clone, Default)]
pub struct EquipmentTable {
    entries: HashMap<String, EquipmentStats>,
    /// archetype name → key of the latest-year variant (deterministic:
    /// ties broken by key name).
    archetype_best: HashMap<String, String>,
}

impl EquipmentTable {
    /// Load from a `equipment_stats.json` file (§5.4).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SaveError> {
        let path = path.as_ref();
        let text = read_json(path)?;
        Self::from_str(&text).map_err(|e| match e {
            SaveError::Data { message, .. } => SaveError::Data {
                path: path.to_path_buf(),
                message,
            },
            other => other,
        })
    }

    /// Parse from an in-memory JSON string.
    pub fn from_str(text: &str) -> Result<Self, SaveError> {
        let entries: HashMap<String, EquipmentStats> =
            serde_json::from_str(text).map_err(|e| json_err("<equipment_stats>", e))?;
        let mut table = EquipmentTable {
            entries,
            archetype_best: HashMap::new(),
        };
        table.build_archetype_index();
        Ok(table)
    }

    fn build_archetype_index(&mut self) {
        // Pick, per archetype, the latest-year variant; break ties by key for
        // determinism. Archetype entries (archetype: null) index under their
        // own key so resolve("artillery_equipment") also succeeds via the
        // exact path.
        let mut best: HashMap<String, (i32, String)> = HashMap::new();
        for (key, stats) in &self.entries {
            let archetype = stats.archetype.clone().unwrap_or_else(|| key.clone());
            let year = stats.year.unwrap_or(0);
            let replace = match best.get(&archetype) {
                None => true,
                Some((best_year, best_key)) => {
                    (year, key.as_str()) > (*best_year, best_key.as_str())
                }
            };
            if replace {
                best.insert(archetype, (year, key.clone()));
            }
        }
        self.archetype_best = best
            .into_iter()
            .map(|(arch, (_year, key))| (arch, key))
            .collect();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Exact-key lookup.
    pub fn get(&self, key: &str) -> Option<&EquipmentStats> {
        self.entries.get(key)
    }

    /// Lookup by exact key, falling back to the latest-year variant of the
    /// archetype named `key` (§5.3 equipment resolution).
    pub fn resolve(&self, key: &str) -> Option<&EquipmentStats> {
        if let Some(e) = self.entries.get(key) {
            return Some(e);
        }
        self.archetype_best
            .get(key)
            .and_then(|k| self.entries.get(k))
    }

    /// Archetype name a resolved equipment key belongs to. Division equipment
    /// counts are aggregated under this name before apportioning (§5.3).
    pub fn archetype_of(&self, key: &str) -> Option<String> {
        self.resolve(key)
            .map(|e| e.archetype.clone().unwrap_or_else(|| key.to_string()))
    }
}

// ---------------------------------------------------------------------------
// unit_templates.json (§5.4)
// ---------------------------------------------------------------------------

/// One battalion/support-company template from `unit_templates.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct UnitTemplateStats {
    #[serde(default)]
    pub max_strength: f32,
    #[serde(default)]
    pub max_organisation: f32,
    #[serde(default)]
    pub default_morale: f32,
    #[serde(default)]
    pub combat_width: f32,
    #[serde(default)]
    pub manpower: f32,
    #[serde(default)]
    pub training_time: f32,
    #[serde(default)]
    pub supply_consumption: f32,
    #[serde(default)]
    pub weight: f32,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    /// Equipment requirement per battalion: archetype → count (§5.2 `need`).
    #[serde(default)]
    pub needs: HashMap<String, f32>,
    #[serde(default)]
    pub terrain_modifiers: HashMap<String, HashMap<String, f32>>,
    #[serde(default)]
    pub categories: Vec<String>,
}

/// Battalion stat templates, split the way the JSON file is.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UnitTemplateTable {
    #[serde(default)]
    pub line_battalions: HashMap<String, UnitTemplateStats>,
    #[serde(default)]
    pub support_companies: HashMap<String, UnitTemplateStats>,
}

impl UnitTemplateTable {
    /// Load from a `unit_templates.json` file (§5.4).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SaveError> {
        let path = path.as_ref();
        let text = read_json(path)?;
        serde_json::from_str(&text).map_err(|e| SaveError::Data {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Parse from an in-memory JSON string.
    pub fn from_str(text: &str) -> Result<Self, SaveError> {
        serde_json::from_str(text).map_err(|e| json_err("<unit_templates>", e))
    }

    /// Look up a subunit token: line battalions first, then support companies.
    pub fn get(&self, token: &str) -> Option<&UnitTemplateStats> {
        self.line_battalions
            .get(token)
            .or_else(|| self.support_companies.get(token))
    }

    /// Per-battalion terrain adjusters for a unit type via its canonical
    /// template key (§6.6 v3.3) — the script/preset/demo assembly
    /// path, where no save token exists. The save path resolves per-token
    /// instead (`Subunit::resolve`, units.rs). Zero-filled when the template
    /// is missing (missing table degrades silently, §5.3 fallback rule).
    pub fn terrain_adjusters_for(&self, ut: UnitType) -> TerrainAdjusters {
        self.get(crate::mapping::canonical_template_key(ut))
            .map(|t| t.terrain_adjusters())
            .unwrap_or_default()
    }
}

impl UnitTemplateStats {
    /// Convert the template's HOI4 `terrain_modifiers` block into the
    /// tactical adjuster arrays. `attack`/`defense` stats land on
    /// their arrays; `amphibious`/`fort` terrain keys and the `movement`
    /// stat are dropped by design (see `TerrainAdjusters`).
    pub fn terrain_adjusters(&self) -> TerrainAdjusters {
        let mut adj = TerrainAdjusters::default();
        for (terrain_key, stats) in &self.terrain_modifiers {
            for (stat, value) in stats {
                adj.set_hoi4(terrain_key, stat, *value);
            }
        }
        adj
    }
}

// ---------------------------------------------------------------------------
// doctrine_bonuses.json (§5.4)
// ---------------------------------------------------------------------------

/// Aggregated doctrine combat factors (§5.3 `doctrine_*_modifier`).
///
/// All fields are additive fractions applied as `(1 + factor)` multipliers.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DoctrineFactors {
    /// Applies to soft and hard attack.
    pub attack: f32,
    pub defense: f32,
    pub breakthrough: f32,
    pub organisation: f32,
}

/// Doctrine tree as stored in `doctrine_bonuses.json`. Node payloads are kept
/// as raw JSON because modifier keys vary widely between trees.
#[derive(Debug, Clone, Deserialize)]
struct DoctrineTree {
    #[serde(default)]
    #[allow(dead_code)]
    path: Option<String>,
    #[serde(default)]
    nodes: HashMap<String, serde_json::Value>,
}

/// Doctrine bonus table (§5.4). The real JSON nests combat modifiers inside
/// per-node `category_modifiers` objects (e.g.
/// `"category_modifiers": { "line_artillery": { "soft_attack": 0.1 } }`),
/// sometimes further nested under `rewards`.
#[derive(Debug, Clone, Default)]
pub struct DoctrineTable {
    trees: HashMap<String, DoctrineTree>,
}

impl DoctrineTable {
    /// Load from a `doctrine_bonuses.json` file (§5.4).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SaveError> {
        let path = path.as_ref();
        let text = read_json(path)?;
        Self::from_str(&text).map_err(|e| match e {
            SaveError::Data { message, .. } => SaveError::Data {
                path: path.to_path_buf(),
                message,
            },
            other => other,
        })
    }

    /// Parse from an in-memory JSON string.
    pub fn from_str(text: &str) -> Result<Self, SaveError> {
        let trees: HashMap<String, DoctrineTree> =
            serde_json::from_str(text).map_err(|e| json_err("<doctrine_bonuses>", e))?;
        Ok(DoctrineTable { trees })
    }

    pub fn tree_count(&self) -> usize {
        self.trees.len()
    }

    /// Return a copy containing only the doctrines a country has researched
    /// (§5.1 doctrine progress). A tree is kept whole when the tree key itself
    /// is a researched token; otherwise only nodes whose name matches a token
    /// are kept.
    pub fn researched(&self, tokens: &[String]) -> DoctrineTable {
        let mut trees = HashMap::new();
        for (name, tree) in &self.trees {
            if tokens.iter().any(|t| t == name) {
                trees.insert(name.clone(), tree.clone());
                continue;
            }
            let nodes: HashMap<String, serde_json::Value> = tree
                .nodes
                .iter()
                .filter(|(node, _)| tokens.iter().any(|t| t == *node))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !nodes.is_empty() {
                trees.insert(
                    name.clone(),
                    DoctrineTree {
                        path: tree.path.clone(),
                        nodes,
                    },
                );
            }
        }
        DoctrineTable { trees }
    }

    /// Aggregate combat factors over every node currently in the table
    /// (§5.3). Call [`researched`](Self::researched) first to restrict the
    /// table to the doctrines the country actually has.
    ///
    /// Approximation, driven by the extracted data shape: `soft_attack` and
    /// `hard_attack` fractions sum into `attack`, `defense` into `defense`,
    /// `breakthrough` into `breakthrough`. `max_organisation` values are flat
    /// in HOI4 (e.g. +10 org); values with |v| > 1 are converted to a factor
    /// via v/100, small values are taken as factors directly.
    pub fn factors(&self) -> DoctrineFactors {
        let mut f = DoctrineFactors::default();
        for tree in self.trees.values() {
            for node in tree.nodes.values() {
                collect_category_modifiers(node, 0, &mut f);
            }
        }
        f
    }
}

fn number(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> f32 {
    map.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32
}

fn collect_category_modifiers(v: &serde_json::Value, depth: usize, f: &mut DoctrineFactors) {
    const MAX_DEPTH: usize = 6;
    if depth > MAX_DEPTH {
        return;
    }
    if let serde_json::Value::Object(map) = v {
        for (key, val) in map {
            if key == "category_modifiers" {
                if let Some(cats) = val.as_object() {
                    for (_category, mods) in cats {
                        if let Some(m) = mods.as_object() {
                            f.attack += number(m, "soft_attack") + number(m, "hard_attack");
                            f.defense += number(m, "defense");
                            f.breakthrough += number(m, "breakthrough");
                            let org = number(m, "max_organisation");
                            f.organisation += if org.abs() > 1.0 { org / 100.0 } else { org };
                        }
                    }
                }
            } else {
                collect_category_modifiers(val, depth + 1, f);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// modifiers.json
// ---------------------------------------------------------------------------

/// Modifier values of one static idea (national spirit) token or one
/// country-leader trait token.
/// Missing keys parse as 0.0.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct IdeaModifiers {
    /// `army_org_factor` — additive fraction of division/battalion org.
    #[serde(default)]
    pub army_org_factor: f32,
    /// `army_org` — flat org points.
    #[serde(default)]
    pub army_org: f32,
    /// `army_attack_factor` — additive fraction of attack.
    #[serde(default)]
    pub army_attack_factor: f32,
    /// `army_defence_factor` — additive fraction of defense (British
    /// spelling, as in vanilla files).
    #[serde(default)]
    pub army_defence_factor: f32,
    /// `breakthrough_factor` — additive fraction of breakthrough.
    #[serde(default)]
    pub breakthrough_factor: f32,
}

/// National modifier table extracted from HOI4 `common/dynamic_modifiers`,
/// `common/ideas` and `common/country_leader`
/// (`extractor/extract_modifiers.py`).
///
/// - `dynamic_modifiers`: token → ORDERED modifier key list. A country's save
///   block carries the current values as a bare float array in definition
///   order, so the runtime resolves modifier values by index.
/// - `ideas`: token → org + combat modifier values.
/// - `leader_traits`: country-leader trait token → org + combat modifier
///   values (section absent in older files → empty).
/// - `unit_leader_traits`: unit-leader (general/field-marshal) trait token →
///   raw modifier key/value map (section absent in older files → empty).
///   Entries from the trait's `field_marshal_modifier` block carry the
///   extractor's `fm:` key prefix so the runtime can apply them at full
///   strength while halving the holder's regular `modifier` entries
///   (FIELD_MARSHAL_ARMY_BONUS_RATIO = 0.5).
#[derive(Debug, Clone, Default)]
pub struct ModifierTable {
    dynamic_modifiers: HashMap<String, Vec<String>>,
    ideas: HashMap<String, IdeaModifiers>,
    leader_traits: HashMap<String, IdeaModifiers>,
    unit_leader_traits: HashMap<String, HashMap<String, f32>>,
}

/// On-disk shape of `modifiers.json`.
#[derive(Deserialize)]
struct ModifierFile {
    #[serde(default)]
    dynamic_modifiers: HashMap<String, Vec<String>>,
    #[serde(default)]
    ideas: HashMap<String, IdeaModifiers>,
    #[serde(default)]
    leader_traits: HashMap<String, IdeaModifiers>,
    #[serde(default)]
    unit_leader_traits: HashMap<String, HashMap<String, f32>>,
}

impl ModifierTable {
    /// Load from a `modifiers.json` file. A missing or malformed file
    /// degrades to an empty (neutral) table, never an error — the modifier
    /// channel is a fidelity refinement, not a hard dependency.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let Ok(text) = std::fs::read_to_string(path.as_ref()) else {
            return Self::default();
        };
        Self::from_str(&text).unwrap_or_default()
    }

    /// Parse from an in-memory JSON string.
    pub fn from_str(text: &str) -> Result<Self, SaveError> {
        let file: ModifierFile =
            serde_json::from_str(text).map_err(|e| json_err("<modifiers>", e))?;
        Ok(ModifierTable {
            dynamic_modifiers: file.dynamic_modifiers,
            ideas: file.ideas,
            leader_traits: file.leader_traits,
            unit_leader_traits: file.unit_leader_traits,
        })
    }

    /// Ordered modifier keys of a dynamic modifier definition (value-array
    /// indices in the save).
    pub fn dynamic_keys(&self, name: &str) -> Option<&[String]> {
        self.dynamic_modifiers.get(name).map(Vec::as_slice)
    }

    /// Modifier values of a static idea token.
    pub fn idea(&self, token: &str) -> Option<&IdeaModifiers> {
        self.ideas.get(token)
    }

    /// Modifier values of a country-leader trait token.
    pub fn leader_trait(&self, token: &str) -> Option<&IdeaModifiers> {
        self.leader_traits.get(token)
    }

    /// Raw modifier map of a unit-leader (general/field-marshal) trait token:
    /// `modifier` block entries under their plain key,
    /// `field_marshal_modifier` entries under the extractor's `fm:`-prefixed
    /// key. Unknown tokens (modded content, older table files) return None.
    pub fn unit_leader_trait(&self, token: &str) -> Option<&HashMap<String, f32>> {
        self.unit_leader_traits.get(token)
    }

    pub fn dynamic_len(&self) -> usize {
        self.dynamic_modifiers.len()
    }

    pub fn idea_len(&self) -> usize {
        self.ideas.len()
    }

    pub fn leader_trait_len(&self) -> usize {
        self.leader_traits.len()
    }

    pub fn unit_leader_trait_len(&self) -> usize {
        self.unit_leader_traits.len()
    }
}

// ---------------------------------------------------------------------------
// country_colors.json
// ---------------------------------------------------------------------------

/// Country map colors extracted from HOI4 `common/countries/colors.txt`
/// (`extractor/extract_country_colors.py`): `TAG → [r, g, b]` (0-255).
/// Used to tint unit base plates / deploy-zone borders with the country's
/// HOI4 map color.
#[derive(Debug, Clone, Default)]
pub struct CountryColorTable {
    colors: HashMap<String, [u8; 3]>,
}

impl CountryColorTable {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SaveError> {
        let path = path.as_ref();
        let text = read_json(path)?;
        Self::from_str(&text).map_err(|e| match e {
            SaveError::Data { message, .. } => SaveError::Data {
                path: path.to_path_buf(),
                message,
            },
            other => other,
        })
    }

    pub fn from_str(text: &str) -> Result<Self, SaveError> {
        let colors: HashMap<String, [u8; 3]> =
            serde_json::from_str(text).map_err(|e| json_err("<country_colors>", e))?;
        Ok(CountryColorTable { colors })
    }

    /// Normalized linear-ish RGB (0..1 floats) for a tag, if present.
    pub fn color(&self, tag: &str) -> Option<[f32; 3]> {
        self.colors.get(tag).map(|c| {
            [
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
            ]
        })
    }

    pub fn len(&self) -> usize {
        self.colors.len()
    }
}

/// Shared miniature tables for tests across this crate (mirrors the real
/// §5.4 JSON shapes at small scale).
#[cfg(test)]
pub(crate) mod tests_helpers {
    pub(crate) const MINI_EQUIPMENT_JSON: &str = r#"{
        "infantry_equipment_0": {"archetype": "infantry_equipment", "year": 1918,
            "soft_attack": 3.0, "hard_attack": 0.5, "defense": 20.0, "breakthrough": 2.0,
            "armor": 0.0, "piercing": 1.0, "hardness": 0.0, "max_speed": 4.0},
        "infantry_equipment_1": {"archetype": "infantry_equipment", "year": 1936,
            "soft_attack": 6.0, "hard_attack": 1.0, "defense": 25.0, "breakthrough": 3.0,
            "armor": 0.0, "piercing": 1.0, "hardness": 0.0, "max_speed": 4.0},
        "support_equipment_1": {"archetype": "support_equipment", "year": 1936,
            "soft_attack": 1.0, "hard_attack": 0.0, "defense": 5.0, "breakthrough": 1.0,
            "armor": 2.0, "piercing": 5.0, "hardness": 0.2, "max_speed": 4.0},
        "light_tank_chassis_0": {"archetype": "light_tank_chassis", "year": 1934,
            "soft_attack": 10.0, "hard_attack": 8.0, "defense": 5.0, "breakthrough": 20.0,
            "armor": 10.0, "piercing": 15.0, "hardness": 0.4, "max_speed": 8.0}
    }"#;

    pub(crate) const MINI_UNIT_TEMPLATES_JSON: &str = r#"{
        "line_battalions": {
            "infantry": {"max_strength": 25, "max_organisation": 60,
                "needs": {"infantry_equipment": 100}},
            "light_armor": {"max_strength": 2, "max_organisation": 10,
                "needs": {"light_tank_chassis": 60}}
        },
        "support_companies": {
            "engineer": {"max_strength": 2, "max_organisation": 20,
                "needs": {"infantry_equipment": 10, "support_equipment": 30}},
            "artillery_brigade": {"max_strength": 0.6, "max_organisation": 0,
                "needs": {"infantry_equipment": 12}}
        }
    }"#;
}

#[cfg(test)]
mod tests {
    use super::tests_helpers::*;
    use super::*;

    #[test]
    fn equipment_table_exact_and_archetype_resolution() {
        let table = EquipmentTable::from_str(MINI_EQUIPMENT_JSON).unwrap();
        assert_eq!(table.len(), 4);
        // Exact key.
        let e = table.resolve("infantry_equipment_0").unwrap();
        assert!((e.soft_attack - 3.0).abs() < 1e-6);
        // Archetype key resolves to the latest-year variant (1936, soft 6).
        let e = table.resolve("infantry_equipment").unwrap();
        assert!((e.soft_attack - 6.0).abs() < 1e-6);
        assert_eq!(
            table.archetype_of("infantry_equipment_0").as_deref(),
            Some("infantry_equipment")
        );
        assert!(table.resolve("plasma_gun").is_none());
    }

    #[test]
    fn unit_template_table_loads_both_sections() {
        let table = UnitTemplateTable::from_str(MINI_UNIT_TEMPLATES_JSON).unwrap();
        let inf = table.get("infantry").unwrap();
        assert!((inf.max_organisation - 60.0).abs() < 1e-6);
        assert_eq!(inf.needs.get("infantry_equipment"), Some(&100.0));
        // Support companies are found through the same entry point.
        let eng = table.get("engineer").unwrap();
        assert_eq!(eng.needs.get("support_equipment"), Some(&30.0));
        assert!(table.get("nonexistent").is_none());
    }

    #[test]
    fn doctrine_factors_from_category_modifiers() {
        let json = r#"{
            "superior_firepower": {
                "path": "grand_doctrines\\land_grand_doctrines.txt",
                "nodes": {
                    "superior_firepower": {
                        "xp_cost": 100.0,
                        "category_modifiers": {
                            "line_artillery": {"soft_attack": 0.1},
                            "support_battalions": {"max_organisation": 10.0}
                        }
                    }
                }
            }
        }"#;
        let table = DoctrineTable::from_str(json).unwrap();
        assert_eq!(table.tree_count(), 1);
        let f = table.factors();
        assert!((f.attack - 0.1).abs() < 1e-6);
        // Flat +10 org becomes a 0.10 factor.
        assert!((f.organisation - 0.10).abs() < 1e-6);
    }

    #[test]
    fn doctrine_researched_filters_nodes() {
        let json = r#"{
            "tree_a": {"nodes": {
                "node_a1": {"category_modifiers": {"infantry": {"soft_attack": 0.2}}},
                "node_a2": {"category_modifiers": {"infantry": {"soft_attack": 0.3}}}
            }},
            "tree_b": {"nodes": {
                "node_b1": {"category_modifiers": {"tanks": {"breakthrough": 0.15}}}
            }}
        }"#;
        let table = DoctrineTable::from_str(json).unwrap();
        // Whole tree selected by tree key.
        let t = table.researched(&["tree_a".to_string()]);
        assert!((t.factors().attack - 0.5).abs() < 1e-6);
        // Single node selected by node key.
        let t = table.researched(&["node_b1".to_string()]);
        assert!((t.factors().breakthrough - 0.15).abs() < 1e-6);
        assert!(t.factors().attack.abs() < 1e-6);
        // Nothing researched → no factors.
        let t = table.researched(&[]);
        assert_eq!(t.factors(), DoctrineFactors::default());
    }

    #[test]
    fn malformed_json_is_descriptive_error() {
        let err = EquipmentTable::from_str("{ not json").unwrap_err();
        assert!(matches!(err, SaveError::Data { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("equipment_stats"),
            "error should name the table: {msg}"
        );
    }

    #[test]
    fn modifier_table_loads_and_defaults() {
        // Ordered dynamic keys + per-idea org values + per-trait org values.
        let json = r#"{
            "dynamic_modifiers": {
                "ITA_regio_esercito_dynamic_modifier": [
                    "max_dig_in_factor", "land_doctrine_cost_factor",
                    "army_speed_factor", "army_org_factor"
                ]
            },
            "ideas": {
                "general_staff": {"army_org_factor": 0.05},
                "idea_SIA_military_humiliation": {"army_org": -10.0}
            },
            "leader_traits": {
                "red_army_organizer": {"army_org_factor": 0.12}
            }
        }"#;
        let table = ModifierTable::from_str(json).unwrap();
        assert_eq!(table.dynamic_len(), 1);
        assert_eq!(table.idea_len(), 2);
        assert_eq!(table.leader_trait_len(), 1);
        let keys = table
            .dynamic_keys("ITA_regio_esercito_dynamic_modifier")
            .unwrap();
        assert_eq!(keys[3], "army_org_factor");
        assert!((table.idea("general_staff").unwrap().army_org_factor - 0.05).abs() < 1e-6);
        assert!(
            (table
                .idea("idea_SIA_military_humiliation")
                .unwrap()
                .army_org
                + 10.0)
                .abs()
                < 1e-6
        );
        assert!(
            (table
                .leader_trait("red_army_organizer")
                .unwrap()
                .army_org_factor
                - 0.12)
                .abs()
                < 1e-6
        );
        assert!(table.dynamic_keys("unknown").is_none());
        assert!(table.idea("unknown").is_none());
        assert!(table.leader_trait("unknown").is_none());
        // Missing file → neutral empty table, never an error.
        let missing = ModifierTable::load("definitely/not/a/real/modifiers.json");
        assert_eq!(missing.dynamic_len(), 0);
        // Older table files without the leader_traits section still load.
        let round1 = ModifierTable::from_str(r#"{"dynamic_modifiers": {}, "ideas": {}}"#).unwrap();
        assert_eq!(round1.leader_trait_len(), 0);
        // Malformed file → same neutral fallback (load swallows).
        let bad = ModifierTable::from_str("{ not json");
        assert!(bad.is_err()); // from_str reports; only load() swallows
    }
}
