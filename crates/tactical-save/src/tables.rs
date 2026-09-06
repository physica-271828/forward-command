//! Loaders for the pre-extracted JSON data tables (DESIGN.md §5.4):
//! `equipment_stats.json`, `unit_templates.json`, `doctrine_bonuses.json`,
//! `modifiers.json`, plus the tank-designer tables `tank_modules.json` /
//! `tank_upgrades.json` feeding [`design_stats`].
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

use crate::model::EquipmentDesign;
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
// tank_modules.json / tank_upgrades.json (§5.4) — designer stat tables
// ---------------------------------------------------------------------------

/// One combat-stat block of a tank module or upgrade row, keyed by HOI4
/// stat name. Only the stats the tactical side models are extracted; every
/// field defaults to 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct StatBlock {
    #[serde(default)]
    pub soft_attack: f32,
    #[serde(default)]
    pub hard_attack: f32,
    /// Armor-piercing — HOI4's `ap_attack` (the equipment table's
    /// `piercing`).
    #[serde(default)]
    pub ap_attack: f32,
    /// HOI4's `armor_value` (the equipment table's `armor`).
    #[serde(default)]
    pub armor_value: f32,
    #[serde(default)]
    pub hardness: f32,
    #[serde(default)]
    pub defense: f32,
    #[serde(default)]
    pub breakthrough: f32,
}

impl StatBlock {
    fn accumulate(&mut self, other: &StatBlock, times: f32) {
        self.soft_attack += other.soft_attack * times;
        self.hard_attack += other.hard_attack * times;
        self.ap_attack += other.ap_attack * times;
        self.armor_value += other.armor_value * times;
        self.hardness += other.hardness * times;
        self.defense += other.defense * times;
        self.breakthrough += other.breakthrough * times;
    }
}

/// One tank module row from `tank_modules.json`: flat `add_stats` plus the
/// `multiply_stats` zone.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct TankModuleStats {
    #[serde(default)]
    pub add: StatBlock,
    #[serde(default)]
    pub mult: StatBlock,
}

/// Tank module table (`extractor/extract_equipment.py`). Carries every
/// vanilla module token, including stat-less ones, so lookups can tell a
/// known-but-inert module from an unknown (modded) token.
#[derive(Debug, Clone, Default)]
pub struct TankModuleTable {
    modules: HashMap<String, TankModuleStats>,
}

impl TankModuleTable {
    /// Load from a `tank_modules.json` file. A missing or malformed file
    /// degrades to an empty table, never an error — design stat computation
    /// then treats every module as unknown (warns and contributes 0).
    pub fn load(path: impl AsRef<Path>) -> Self {
        let Ok(text) = std::fs::read_to_string(path.as_ref()) else {
            return Self::default();
        };
        Self::from_str(&text).unwrap_or_default()
    }

    /// Parse from an in-memory JSON string.
    pub fn from_str(text: &str) -> Result<Self, SaveError> {
        let modules: HashMap<String, TankModuleStats> =
            serde_json::from_str(text).map_err(|e| json_err("<tank_modules>", e))?;
        Ok(TankModuleTable { modules })
    }

    pub fn get(&self, token: &str) -> Option<&TankModuleStats> {
        self.modules.get(token)
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

/// One tank upgrade row from `tank_upgrades.json`: the PER-LEVEL flat adds.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct TankUpgradeRow {
    #[serde(default)]
    add: StatBlock,
}

/// Tank upgrade table: upgrade token → per-level add block. Only the
/// add_stats (flat per-level) upgrade form is extracted — designer tank
/// upgrades (`tank_nsb_engine_upgrade` / `tank_nsb_armor_upgrade`); the
/// legacy factor-per-level upgrades apply to non-designer fixed equipment.
#[derive(Debug, Clone, Default)]
pub struct TankUpgradeTable {
    upgrades: HashMap<String, TankUpgradeRow>,
}

impl TankUpgradeTable {
    /// Same degrade-silent contract as [`TankModuleTable::load`].
    pub fn load(path: impl AsRef<Path>) -> Self {
        let Ok(text) = std::fs::read_to_string(path.as_ref()) else {
            return Self::default();
        };
        Self::from_str(&text).unwrap_or_default()
    }

    /// Parse from an in-memory JSON string.
    pub fn from_str(text: &str) -> Result<Self, SaveError> {
        let upgrades: HashMap<String, TankUpgradeRow> =
            serde_json::from_str(text).map_err(|e| json_err("<tank_upgrades>", e))?;
        Ok(TankUpgradeTable { upgrades })
    }

    /// Per-level adds of an upgrade token.
    pub fn get(&self, token: &str) -> Option<&StatBlock> {
        self.upgrades.get(token).map(|row| &row.add)
    }

    pub fn len(&self) -> usize {
        self.upgrades.len()
    }

    pub fn is_empty(&self) -> bool {
        self.upgrades.is_empty()
    }
}

/// The module/upgrade tables the design-aware assembly needs, loaded once
/// per battle alongside the equipment table.
#[derive(Debug, Clone, Default)]
pub struct DesignTables {
    pub modules: TankModuleTable,
    pub upgrades: TankUpgradeTable,
}

impl DesignTables {
    /// Load both designer tables from the runtime data dir (each degrades
    /// silently like the modifier table).
    pub fn load(data_dir: &Path) -> Self {
        DesignTables {
            modules: TankModuleTable::load(data_dir.join("tank_modules.json")),
            upgrades: TankUpgradeTable::load(data_dir.join("tank_upgrades.json")),
        }
    }
}

/// Multiply-zone base semantics for designer stats. HOI4 tank modules carry
/// a few `multiply_stats` entries (cast/welded armor +30%, squeezebore
/// +10% ap, wheeled/half-track hardness penalties); the in-game designer
/// applies them to the displayed TOTAL (chassis base + module adds +
/// upgrade levels), so `false` is the calibrated default — flip to `true`
/// (multiply the chassis base alone, then add) if a recalibration against
/// the designer disagrees.
pub const MULTIPLY_ON_CHASSIS_BASE: bool = false;

/// Computed combat stats of one save equipment design (§5.3 designer
/// path): the five fields the battalion assembly takes from the design
/// (attack triple + armor/hardness bases); defense/breakthrough keep the
/// static equipment row values.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DesignStats {
    pub soft_attack: f32,
    pub hard_attack: f32,
    pub piercing: f32,
    pub armor: f32,
    pub hardness: f32,
}

/// Compute a design's combat stats from the save's module picks and upgrade
/// levels (§5.3):
///
/// ```text
/// final[S] = (chassis[S] + Σ module add[S] + Σ upgrade level × add[S])
///            × (1 + Σ module mult[S])           # MULTIPLY_ON_CHASSIS_BASE = false
/// ```
///
/// (`MULTIPLY_ON_CHASSIS_BASE = true` multiplies the chassis base alone,
/// then adds.) Tank chassis rows carry no attack stats — the whole attack
/// triple comes from module adds. Unknown module/upgrade tokens (modded
/// content) push a warning and contribute 0; the design still resolves.
/// Only a design whose chassis token is missing from the equipment table
/// returns `None` — the caller then falls back to the static equipment row
/// path.
pub fn design_stats(
    design: &EquipmentDesign,
    equipment: &EquipmentTable,
    tables: &DesignTables,
    warnings: &mut Vec<String>,
) -> Option<DesignStats> {
    let chassis = equipment.resolve(&design.token)?;
    let label = design.name.as_deref().unwrap_or(&design.token);
    let mut add = StatBlock::default();
    let mut mult = StatBlock::default();
    for (slot, token) in &design.modules {
        match tables.modules.get(token) {
            Some(m) => {
                add.accumulate(&m.add, 1.0);
                mult.accumulate(&m.mult, 1.0);
            }
            None => warnings.push(format!(
                "design \"{label}\" ({}): unknown module {token} in {slot} — counted as 0",
                design.token
            )),
        }
    }
    for (token, level) in &design.upgrades {
        match tables.upgrades.get(token) {
            Some(per_level) => add.accumulate(per_level, *level as f32),
            None => warnings.push(format!(
                "design \"{label}\" ({}): unknown upgrade {token} — counted as 0",
                design.token
            )),
        }
    }
    let combine = |base: f32, a: f32, m: f32| {
        if MULTIPLY_ON_CHASSIS_BASE {
            base * (1.0 + m) + a
        } else {
            (base + a) * (1.0 + m)
        }
    };
    Some(DesignStats {
        soft_attack: combine(chassis.soft_attack, add.soft_attack, mult.soft_attack),
        hard_attack: combine(chassis.hard_attack, add.hard_attack, mult.hard_attack),
        piercing: combine(chassis.piercing, add.ap_attack, mult.ap_attack),
        armor: combine(chassis.armor, add.armor_value, mult.armor_value),
        hardness: combine(chassis.hardness, add.hardness, mult.hardness),
    })
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

    // -----------------------------------------------------------------------
    // design_stats — anchored to the literal vanilla values of
    // common/units/equipment/{tank_chassis.txt, modules/00_tank_modules.txt,
    // upgrades/land_upgrades.txt} (the JSON snippets below carry the
    // extractor-filtered shapes: only the 7 modeled combat stats survive).
    // -----------------------------------------------------------------------

    /// Chassis rows as extracted: designer-era chassis carry armor/hardness/
    /// speed but ZERO attack (the gun is a module pick).
    const DESIGN_EQUIPMENT_JSON: &str = r#"{
        "light_tank_chassis_2": {"archetype": "light_tank_chassis", "year": 1936,
            "soft_attack": 0, "hard_attack": 0, "defense": 0, "breakthrough": 0,
            "armor": 20.0, "piercing": 0, "hardness": 0.8, "max_speed": 6.0},
        "medium_tank_chassis_0": {"archetype": "medium_tank_chassis", "year": 1922,
            "soft_attack": 0, "hard_attack": 0, "defense": 0, "breakthrough": 0,
            "armor": 20.0, "piercing": 0, "hardness": 0.85, "max_speed": 4.0}
    }"#;

    /// Vanilla module values after the extractor's stat-key filter:
    /// tank_heavy_machine_gun 8/2/6, tank_close_support_gun 22/5/10,
    /// additional_machine_guns +1 soft (+2 defense), one-man light turret
    /// +7 breakthrough, medium two-man turret +16, riveted +2/+2,
    /// welded +4/+4 & armor ×1.3, armor_skirts +3 armor, squeezebore ap
    /// ×1.1; engines/bogie contribute nothing to the modeled stats.
    const DESIGN_MODULES_JSON: &str = r#"{
        "tank_heavy_machine_gun": {"add": {"soft_attack": 8, "hard_attack": 2, "ap_attack": 6}, "mult": {}},
        "tank_close_support_gun": {"add": {"soft_attack": 22, "hard_attack": 5, "ap_attack": 10}, "mult": {}},
        "additional_machine_guns": {"add": {"soft_attack": 1, "defense": 2}, "mult": {}},
        "tank_light_one_man_tank_turret": {"add": {"breakthrough": 7}, "mult": {}},
        "tank_medium_two_man_tank_turret": {"add": {"breakthrough": 16}, "mult": {}},
        "tank_riveted_armor": {"add": {"defense": 2, "breakthrough": 2}, "mult": {}},
        "tank_welded_armor": {"add": {"defense": 4, "breakthrough": 4}, "mult": {"armor_value": 0.3}},
        "armor_skirts": {"add": {"armor_value": 3, "breakthrough": 3}, "mult": {}},
        "squeezebore_adaptor": {"add": {}, "mult": {"ap_attack": 0.1}},
        "tank_gasoline_engine": {"add": {}, "mult": {}},
        "tank_diesel_engine": {"add": {}, "mult": {}},
        "tank_bogie_suspension": {"add": {}, "mult": {}}
    }"#;

    /// land_upgrades.txt per-level adds: engine upgrade touches no modeled
    /// stat (speed/cost only — still a known token), armor upgrade gives
    /// +2.5 armor and +1.25 breakthrough per level.
    const DESIGN_UPGRADES_JSON: &str = r#"{
        "tank_nsb_engine_upgrade": {"add": {}},
        "tank_nsb_armor_upgrade": {"add": {"breakthrough": 1.25, "armor_value": 2.5}}
    }"#;

    fn design_fixtures() -> (EquipmentTable, DesignTables) {
        let eq = EquipmentTable::from_str(DESIGN_EQUIPMENT_JSON).unwrap();
        let tables = DesignTables {
            modules: TankModuleTable::from_str(DESIGN_MODULES_JSON).unwrap(),
            upgrades: TankUpgradeTable::from_str(DESIGN_UPGRADES_JSON).unwrap(),
        };
        (eq, tables)
    }

    fn design(
        token: &str,
        modules: &[(&str, &str)],
        upgrades: &[(&str, u32)],
    ) -> EquipmentDesign {
        EquipmentDesign {
            token: token.to_string(),
            name: Some("Test".to_string()),
            is_frame: false,
            modules: modules
                .iter()
                .map(|(s, m)| (s.to_string(), m.to_string()))
                .collect(),
            upgrades: upgrades
                .iter()
                .map(|(t, l)| (t.to_string(), *l))
                .collect(),
        }
    }

    #[test]
    fn design_stats_light_tank_hmg() {
        // The Ha-Go shape: light_tank_chassis_2 + heavy MG + one-man turret
        // + bogie + riveted + gasoline, no upgrade levels.
        let (eq, tables) = design_fixtures();
        let d = design(
            "light_tank_chassis_2",
            &[
                ("main_armament_slot", "tank_heavy_machine_gun"),
                ("engine_type_slot", "tank_gasoline_engine"),
                ("turret_type_slot", "tank_light_one_man_tank_turret"),
                ("suspension_type_slot", "tank_bogie_suspension"),
                ("armor_type_slot", "tank_riveted_armor"),
            ],
            &[("tank_nsb_engine_upgrade", 0), ("tank_nsb_armor_upgrade", 0)],
        );
        let mut warnings = Vec::new();
        let s = design_stats(&d, &eq, &tables, &mut warnings).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert!((s.soft_attack - 8.0).abs() < 1e-6, "soft={}", s.soft_attack);
        assert!((s.hard_attack - 2.0).abs() < 1e-6, "hard={}", s.hard_attack);
        assert!((s.piercing - 6.0).abs() < 1e-6, "pierce={}", s.piercing);
        assert!((s.armor - 20.0).abs() < 1e-6, "armor={}", s.armor);
        assert!((s.hardness - 0.8).abs() < 1e-6, "hardness={}", s.hardness);
    }

    #[test]
    fn design_stats_medium_close_support_with_mgs() {
        // The I-Go shape: medium_tank_chassis_0 + close support gun +
        // additional MGs + two-man turret + diesel, engine upgrade level 4
        // (touches no modeled stat).
        let (eq, tables) = design_fixtures();
        let d = design(
            "medium_tank_chassis_0",
            &[
                ("main_armament_slot", "tank_close_support_gun"),
                ("engine_type_slot", "tank_diesel_engine"),
                ("special_type_slot_1", "additional_machine_guns"),
                ("turret_type_slot", "tank_medium_two_man_tank_turret"),
                ("suspension_type_slot", "tank_bogie_suspension"),
                ("armor_type_slot", "tank_riveted_armor"),
            ],
            &[("tank_nsb_engine_upgrade", 4), ("tank_nsb_armor_upgrade", 0)],
        );
        let mut warnings = Vec::new();
        let s = design_stats(&d, &eq, &tables, &mut warnings).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert!((s.soft_attack - 23.0).abs() < 1e-6, "soft={}", s.soft_attack);
        assert!((s.hard_attack - 5.0).abs() < 1e-6, "hard={}", s.hard_attack);
        assert!((s.piercing - 10.0).abs() < 1e-6, "pierce={}", s.piercing);
        assert!((s.armor - 20.0).abs() < 1e-6, "armor={}", s.armor);
        assert!((s.hardness - 0.85).abs() < 1e-6, "hardness={}", s.hardness);
    }

    #[test]
    fn design_stats_armor_upgrade_levels_add_per_level() {
        // Armor upgrade level 5: chassis 20 + 5 × 2.5 = 32.5.
        let (eq, tables) = design_fixtures();
        let d = design(
            "medium_tank_chassis_0",
            &[("main_armament_slot", "tank_close_support_gun")],
            &[("tank_nsb_armor_upgrade", 5)],
        );
        let mut warnings = Vec::new();
        let s = design_stats(&d, &eq, &tables, &mut warnings).unwrap();
        assert!((s.armor - 32.5).abs() < 1e-6, "armor={}", s.armor);
    }

    #[test]
    fn design_stats_multiply_zones() {
        // Welded armor ×1.3 rides the total (chassis 20 + armor_skirts 3)
        // — (20 + 3) × 1.3 = 29.9. Squeezebore multiplies piercing:
        // (0 + 10) × 1.1 = 11.
        let (eq, tables) = design_fixtures();
        let d = design(
            "medium_tank_chassis_0",
            &[
                ("main_armament_slot", "tank_close_support_gun"),
                ("armor_type_slot", "tank_welded_armor"),
                ("special_type_slot_1", "armor_skirts"),
                ("special_type_slot_2", "squeezebore_adaptor"),
            ],
            &[],
        );
        let mut warnings = Vec::new();
        let s = design_stats(&d, &eq, &tables, &mut warnings).unwrap();
        assert!((s.armor - 29.9).abs() < 1e-4, "armor={}", s.armor);
        assert!((s.piercing - 11.0).abs() < 1e-6, "pierce={}", s.piercing);
        assert!((s.soft_attack - 22.0).abs() < 1e-6, "soft={}", s.soft_attack);
    }

    #[test]
    fn design_stats_degradation_paths() {
        let (eq, tables) = design_fixtures();
        let mut warnings = Vec::new();

        // A design without a modules block (frame / legacy entry) resolves
        // to the chassis row untouched — attack stays 0, no warnings.
        let bare = design("light_tank_chassis_2", &[], &[]);
        let s = design_stats(&bare, &eq, &tables, &mut warnings).unwrap();
        assert!((s.soft_attack - 0.0).abs() < 1e-6);
        assert!((s.armor - 20.0).abs() < 1e-6);
        assert!(warnings.is_empty());

        // Unknown module/upgrade tokens (modded content) warn and count 0;
        // the design still resolves with the known modules summed.
        let modded = design(
            "light_tank_chassis_2",
            &[
                ("main_armament_slot", "tank_heavy_machine_gun"),
                ("special_type_slot_1", "some_mod_module"),
            ],
            &[("some_mod_upgrade", 3)],
        );
        let s = design_stats(&modded, &eq, &tables, &mut warnings).unwrap();
        assert!((s.soft_attack - 8.0).abs() < 1e-6, "soft={}", s.soft_attack);
        assert_eq!(warnings.len(), 2, "warnings: {warnings:?}");
        assert!(warnings[0].contains("some_mod_module") || warnings[1].contains("some_mod_module"));
        assert!(warnings[0].contains("some_mod_upgrade") || warnings[1].contains("some_mod_upgrade"));

        // A chassis token missing from the equipment table is the ONLY
        // unparseable case → None (caller falls back to the static path).
        let ghost = design("plasma_tank_chassis_9", &[], &[]);
        assert!(design_stats(&ghost, &eq, &tables, &mut warnings).is_none());
    }
}
