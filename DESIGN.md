<!-- Public mirror of the internal design doc. IDs/dates/process narrative scrubbed; section numbers are load-bearing (cited by code doc comments) — do not renumber. -->
# Forward Command — Adaptive Tactical Wargame System

## 1. Overview

### 1.1 Vision

Battalion-level tactical command layer for HOI4. An external Rust program reads HOI4 save data, renders a hex-grid wargame of the contested province, and injects tactical combat results back via console commands.

### 1.2 Prior Art

CK3 "Voices of Court" — external `.exe` reads `debug.log` and injects console commands via Windows `SendInput`. Same architecture, proven viable.

### 1.3 Architecture at a Glance (revised)

```
HOI4 Process                          External Program (Rust)
══════════                            ══════════════════════
Mod writes game.log ───────────────▶  log_listener detects trigger
                                       ↓
                                      save_parser reads .hoi4
                                       ↓
                                      map_generator → hex grid
                                       ↓
                                      Bevy/egui tactical UI
                                       ↓
Console receives ◀──────────────────  injector (SendInput)
  set_var / d_tac_* effects / event / run
```

### 1.4 Data Channels (revised)

| Channel | Direction | Content | Format |
|---------|-----------|---------|--------|
| **game.log** | Mod → External | Trigger signals, abort, heartbeat | JSON Lines |
| **Save file** | Passive → External | Division composition, equipment, org/str, tech, doctrines | Clausewitz text |
| **Console injection** | External → Mod | Damage values, sync markers, battle-end | `set_var` + scripted effects (`d_tac_*`) + `event` + `run` |

---

## 2. Technology Stack

### 2.1 External Program (revised)

| Component | Choice | Purpose |
|-----------|--------|---------|
| Language | **Rust** | Performance, single `.exe`, no runtime |
| ECS/Engine | **Bevy 0.15** | 3D mesh rendering of board/units (`tactical3d-render`, with bevy_egui 0.31); the old 2D line used Bevy 0.14 sprite batching |
| UI Panels | **egui** | Info panel, command buttons, sync status |
| Save Parsing | **jomini** crate | Parse Clausewitz text `.hoi4` saves |
| Log Listening | `notify` crate | File system listener for `game.log` |
| Injection | Win32 FFI (`SetForegroundWindow`, `SendInput`) | Console command automation |

### 2.2 Workspace Structure (revised)

One Rust workspace in the monorepo: `forward-command/`. The older 2D
workspace `tactical/` (Bevy 0.14, 9 crates incl. `tactical-ui`/`tactical-bin`)
was removed and survives only in git history.

```
forward-command/                  # v0.2.0 — current line: 3D renderer + live loop
├── Cargo.toml                    # Workspace root (11 members)
├── settings.json                 # Runtime-generated; menu Settings page persists
│                                 #   hoi4_dir / saves_dir / log_path / language /
│                                 #   msaa / shadow / max_fps / low_power (§12.1)
├── crates/
│   ├── tactical-core/            # Hex math, grid, terrain, units, pathfinding, LoS, RNG,
│   │                             #   flag-capture board, command/HQ (zero external deps)
│   ├── tactical-combat/          # Combat resolution
│   ├── tactical-sync/            # Battle lifecycle state machine, injection batches
│   ├── tactical-map/             # provinces.bmp + definition.csv → bitmap-scale hex grid
│   ├── tactical-save/            # jomini-based .hoi4 save parser → tactical units
│   ├── tactical-ai/              # Tactic-driven enemy AI (planner + doctrine cards)
│   ├── tactical-inject/          # Win32 console injection (SendInput)
│   ├── tactical-listen/           # notify-based game.log tailer, JSON Lines parsing
│   ├── tactical-locale/          # HOI4-style en/zh localization (§15), zero deps
│   ├── tactical3d-render/        # Bevy 0.15 + bevy_egui 0.31 3D renderer & UI
│   └── tactical3d-bin/           # Binary `forward-command` (menu/tray, demo, live,
│                                 #   headless, preview, battle scripts)
├── data/                         # 6 pre-extracted JSON tables
│   ├── equipment_stats.json      # Pre-extracted equipment attributes
│   ├── unit_templates.json       # Pre-extracted battalion stats
│   ├── terrain_mods.json         # Pre-extracted terrain modifiers
│   ├── combat_tactics.json       # Pre-extracted tactic definitions
│   ├── doctrine_bonuses.json     # Pre-extracted doctrine tree bonuses
│   └── country_colors.json       # 304 tags → [r,g,b] (unit base plates)
├── extractor/                    # 6 Python scripts for data pre-extraction
│   ├── extract_equipment.py
│   ├── extract_units.py
│   ├── extract_terrain.py
│   ├── extract_tactics.py
│   ├── extract_doctrines.py
│   └── extract_country_colors.py
└── hoi4-mod/forward-command/      # The HOI4 mod component (see §2.3)
```

The 3D workspace generates its meshes/colors at runtime; `assets/` carries
only fonts and UI icons. `data/battles/*.json` battle scripts follow the schema in
`tactical3d-bin/src/script.rs`.

### 2.3 Mod File Structure (revised)

Actual files under `forward-command/hoi4-mod/forward-command/`:

```
forward-command/
├── descriptor.mod                         # still references thumbnail.png
│                                          #   (file not yet provided)
├── common/
│   ├── decisions/
│   │   └── tac_entry.txt                  # Tactical entry decision (clicked from
│   │                                      #   the decision panel; HOI4 decisions
│   │                                      #   cannot be hotkey-bound)
│   ├── scripted_effects/
│   │   ├── d_tac_apply_damage.txt         # d_ prefix → console command
│   │   ├── d_tac_sync_hourly.txt          # Hourly sync receiver
│   │   └── d_tac_end_battle.txt           # Battle termination
│   └── on_actions/
│       └── tac_on_actions.txt             # Hourly heartbeat on_action source
├── events/
│   └── tac_sync_events.txt                # tac_sync.1, tac_abort.1, tac_complete.99
└── localisation/
    ├── english/
    │   └── tac_l_english.yml
    └── simp_chinese/
        └── tac_l_simp_chinese.yml
```

Planned but not yet implemented: `interface/tac_overlay.gui` (scripted GUI
status overlay) and `interface/mapicons.gui` (battle bubble → tactical entry)
(not yet implemented).

---

## 3. Communication Protocol

### 3.1 Channel 1: game.log (Mod → External, JSON Lines) (revised)

Every message is one line of valid JSON. The external program listens to `game.log` for new lines matching `{"type":"tac_...",...}`.

```
{"type":"tac_start",  "ts":"1940.5.13.15","province":3560,  "tag":"GER","leader_id":3,      "attack_dirs":["E","NE"],"is_player_attacker":true}
{"type":"tac_abort",  "ts":"1939.9.1.09","tag":"GER"}
{"type":"tac_heartbeat","ts":"1939.9.1.10","tag":"GER","hour":3}
{"type":"tac_state",  "ts":"1939.9.1.10","tag":"GER","hour":3,"phase":"active"}
{"type":"tac_enemy_tactic","ts":"1939.9.1.10","tag":"GER","enemy_tactic":"blitz"}
```

Current mod status: `tac_start` keeps the
`"province":0` placeholder BY DESIGN — the external program infers the
contested province from the save (both sides' in-combat divisions sit in
it; `scenario::infer_battle_province`) — and ships constant
`"is_player_attacker":true` (no vanilla-verified attack/defend trigger
yet; the plumbing is wired, the truth value is not yet resolved).
`attack_dirs:["W","NW"]`/`leader_id:0` remain placeholders. The mod now
SENDS `tac_enemy_tactic` (fixed `"default"` card) in the same tick right
after `tac_start`; the listener looks ahead in the same poll batch, then
grace-polls ~1 s. `tac_heartbeat`/`tac_state` still carry `"hour":0` —
1.19 has no scriptable current-hour value — and the listener substitutes
the real game hour from the log line's own `[yyyy.mm.dd.hh]` prefix
(`tactical-listen::game_hour_prefix`).

#### Message Catalog

| Type | Fields | Triggers |
|------|--------|----------|
| `tac_start` | `ts`, `province`, `tag`, `leader_id`, `attack_dirs`, `is_player_attacker` | Player picks a battle state in the attack/defend state-target decision (§10.1); `province` stays 0 in the log — the external program resolves the real one from the post-click snapshot's `tac_pick=1` state; `is_player_attacker` is real per decision |
| `tac_refresh` | `ts` (prefix only) | (REMOVED — daily-pulse lag made the refresh loop structural; listener keeps parsing it for old logs) |
| `tac_abort` | `ts`, `tag` | Player uses "Force Exit Tactical Mode" decision |
| `tac_heartbeat` | `ts`, `tag`, `hour` | `on_hourly` during tactical mode (proves mod alive) |
| `tac_state` | `ts`, `tag`, `hour`, `phase` | After each sync, mod reports current strategic state |
| `tac_enemy_tactic` | `ts`, `tag`, `enemy_tactic` | Sent by the mod in the same tick right after `tac_start` (fixed `"default"`; real HOI4 tactic-token mapping is iteration 2) |
| `tac_complete` | `ts`, `result` | Battle completed on the strategic map (sent by the mod; currently ignored by the listener) |
| `tac_damage_applied` | `keep`, `enemy_keep`, `ts` | Mod acknowledgement after `d_tac_apply_damage` (sent by the mod; currently ignored by the listener) |
| `tac_battle_ended` | `ts`, `result` | Battle-end notification (sent by the mod; currently ignored by the listener) |

### 3.2 Channel 3: Console Injection (External → Mod) (revised)

External program writes a batch file to `tac_inject.txt` in the HOI4 **user directory** (the console `run` only resolves relative paths there — absolute `%TEMP%` paths fail with "Couldn't find file"), then posts `run tac_inject.txt` to the console via **background PostMessage** (no foreground steal, no SendInput; the legacy foreground path remains as the fallback). A battle instance uses a per-process name, `tac_inject_<pid>.txt` — two concurrent battles clobbered one shared file; the pid names leave one file per battle behind, so the menu sweeps stale ones (mtime older than 24h) at startup.

**Hourly sync batch:**
```
eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }
eval_effect damage_units = { province = 13251 limit = { tag = ITA } org_damage = 0.100 str_damage = 0.020 ratio = yes army = yes }
eval_effect ITA = { d_tac_sync_hourly = yes }
```

**Battle end batch:**
```
eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 1.000 ratio = yes army = yes }   # only when a flag collapse is pending (§6.11)
eval_effect damage_units = { province = 13237 limit = { tag = ETH } org_damage = 0.120 str_damage = 0.050 ratio = yes army = yes }
eval_effect ITA = { d_tac_end_battle = yes }
```

**Early-exit batch:** the battle window's End Tactic
/ Esc-abandon exits ride the same two-phase shape, except phase 2 is the
ABORT situation event (shared cleanup + popup + the uniform `tac_abort`
"tactical mode off" log token — an early exit resolves nothing, so never
`d_tac_end_battle`):
```
eval_effect ITA = { country_event = { id = tac_abort.1 hours = 0 } }
```
End Tactic carries the partial hour (phase 1 shaped as at battle end);
abandon sends an empty phase 1 (§11.3: unsynced damage is lost, no clock
advance).

Batch notes:

- **Damage channel = `damage_units` eval_effect one-liners** (org + str):
  `damage_units` is documented in the
  shipped `documentation/effects_documentation.md:3417` and used by vanilla
  (BBA/NSB/ITA ×3). Probed semantics: no location = no-op; `province=` is
  the finest filter (no per-division targeting exists; the
  `template` field is a compile-wired but match-dead PDS stub);
  `ratio = yes` removes percent-of-MAX points; `limit` is a
  country-scope check on the unit's owner (`tag = X` / `has_war_with = ROOT`
  verified; division variables are invisible to it). Console-set variables
  stay unreadable — all values are literal in the line.
- Batch shape (hourly sync, `writeback` setting §12.1): mode `org_str`
  (default) = defender `damage_units` lines at the contested province
  (province-exact — every division in an attacked province defends it) plus
  attacker lines per source province (org exact; str diluted across
  every division of that tag there so the province total stays exact);
  each line's `limit` filters ONE owning country, and damage points and
  maxima pools are both tracked PER TAG (per province for attackers), so
  a side fighting with several tags (allies) writes each tag its own line
  at its own ratio — a tag that took no damage gets no line at all;
  mode `off` = no damage lines at all. (The third mode `org_only` was
  removed.) Damage ratio bases are
  pool-matched to the battalion-scale numerator: org divides by the ASSEMBLED battalion org pool (Σ
  battalion max_org, HQ excluded) — HOI4's division org is a subunit MEAN,
  and dividing battalion points by division-mean sums under-counted the
  base by the battalions-per-division factor (~6-8× writeback inflation);
  str divides by division strength sums, which already equal the battalion
  sums. `org_damage`/`str_damage` fields with ratios below 0.0005 are
  omitted. The §6.11 collapse line is
  a `damage_units` `org_damage = 1.000` at the contested province keyed on
  the defender tags (one line each). Scripted-effect calls (`d_tac_end_battle` etc.) are
  scope-pinned (`eval_effect ITA = { d_tac_end_battle = yes }`) because
  the console runs on the player's currently-selected scope;
  `damage_units` with `province=`+`limit` is scope-robust by construction.
  The literal bucket effects (`z_tac_org_buckets.txt`) are
  RETIRED (deleted).
- Whole-army `set_unit_organization` channels are RETIRED: the `org_only` per-side keep lines and the
  `d_tac_collapse_*` mod effects applied to EVERY `tac_in_battle`-tagged
  division, and division tagging can only be country-wide — a
  decisive hour clamped the enemy keep to zero and zeroed an entire
  national army, ending even unrelated unfrozen battles. Every damage
  line now rides `damage_units` province+tag; the tagging machinery is
  removed from the mod. (`set_unit_organization = x` multiplies current org, 42→21→10 — nothing
  ships on it anymore.)
- Sync ack = `eval_effect <tag> = { d_tac_sync_hourly = yes }`: the scope-pinned scripted effect fires the hidden
  `tac_sync.1` from script, so the hourly `tac_state` receipt logs WITHOUT
  a window. A console-fired `event tac_sync.1 <tag>` ignores the event's
  `hidden = yes` and popped an empty one-option window every sync, each
  unanswered copy lingering ~14 days as an "event timed out" notification.
- `pause_in_hours 1` (§8.4) advances the strategic clock exactly one game
  hour per sync batch — a single command that switches to gamespeed 5 by
  itself and auto-pauses. It is always the batch's LAST
  line, and every sync/end-phase-1 batch carries it (end: only with
  unsynced turns). During the tactical battle the contested state is
  frozen (`tac_freeze`, §8.4), so the advancing hour deals no vanilla
  combat damage — no double accounting.

**Injection procedure (background PostMessage — no foreground steal):**
```rust
fn inject(cmds: &[&str]) {
    // 1. Write the batch file into the HOI4 USER DIR (the console `run`
    //    resolves relative paths there; absolute paths fail).
    //    `tac_inject.txt` is the injector default — a battle instance
    //    writes `tac_inject_<pid>.txt` so concurrent battles never
    //    clobber one shared file. A TRAILING newline is mandatory —
    //    HOI4's run-file parser drops the final unterminated line.
    std::fs::write(user_dir.join("tac_inject.txt"), cmds.join("\n") + "\n").unwrap();

    // 2. Post the console toggle + a ping line, then READ game.log for the
    //    marker (the console toggle is stateless and its input focus can be
    //    lost; desync would otherwise type into the map)
    post_key(hoi4_hwnd, VK_OEM_3);            // ` (console toggle, posted)
    post_text(hoi4_hwnd, "eval_effect log = \"TAC_PING_<pid>_<ms>\"");
    post_key(hoi4_hwnd, VK_RETURN);
    if !wait_for_marker_in_game_log() { recover_console_and_retry_once(); }

    // 3. Type the run command (bare file name — `run tac_inject_<pid>.txt`
    //    for a battle instance) and execute, then close
    post_text(hoi4_hwnd, "run tac_inject.txt");
    post_key(hoi4_hwnd, VK_RETURN);
    post_key(hoi4_hwnd, VK_OEM_3);            // console close
    // No SetForegroundWindow, no SendInput — the user's focus never moves.
    // Failure falls back once to the legacy foreground SendInput path.
}
```

---

## 4. Tactical Map Generation

### 4.1 Inputs (revised)

| Source | Data |
|--------|------|
| `provinces.bmp` | Pixel color → province ID mapping |
| `definition.csv` | Province ID → type (land/sea/lake), terrain, RGB color |
| `adjacencies.csv` | River crossings, strait data (four adjacency types parsed; the generator currently consumes only River) |
| `rivers.bmp` | River strokes (index < 254 = river) |
| `history/states/*.txt` | Victory points (province id → level) |
| `map/unitstacks.txt` | Unit stack positions per province (index-0 stack → VP city anchor) |
| `localisation/<lang>/victory_points_l_<lang>.yml` | VP names (`VICTORY_POINTS_<pid>`) for floating city labels (the UI language's file, English as fallback) |
| `MAP_SCALE_PIXEL_TO_KM = 7.114` | Scale factor from `defines.lua` |
| Attack directions | Live: derived from the attacking divisions' source provinces (save `land_combat` attacker locations); fallback `tac_start.attack_dirs` / script `dirs:` |

### 4.2 Algorithm (per province, at runtime; revised)

```
1. Parse provinces.bmp → 2D array of province IDs.
2. Find all pixels matching the target province ID.
3. Compute bounding box: (min_x, min_y, max_x, max_y).
3b. Multi-province stitching: fold a staging strip
   (ORIGIN_STRIP_PX = 2 px) of the attacker's origin provinces' territory
   into the bounding box. The inter-province river becomes a real obstacle.
   Live battles: the save names the source provinces (the
   attacking divisions' locations), so each listed province contributes its
   FULL shared border with the battle province — no 60°-sector clip and no
   minimum border length (a 1–2 px sliver is the only way in; the attack
   must stage there). The strip's inner edge IS the real province border,
   for the border's full length. Script battles (dirs only) and the live
   fallback (no listed source borders the province) keep the sector
   heuristic: a land neighbour qualifies when ≥3 of its border pixels fall
   in an attack direction's sector, and its strip is clipped to those
   pixels (the old single-centroid rule mis-aimed Viipuri 9206, whose SE
   border's centroid lands in E; full-border strips of giant neighbours
   ballooned Ioannina 3914 to a 4,619-hex zone).
   The attacker's deployable
   ground is the STRIP hexes only (centre pixel within 2 px of the shared
   border) — gifting every origin pixel inside the bbox ballooned the zone
   when the origin neighbours are giant provinces (the strip
   is proportionally large on tiny provinces by construction: 1 px ≈ 58
   hexes at bitmap scale, so a 55-px strip ≈ 3,200 hexes of staging ground).
3c. Shoreline margin: the bbox is expanded by
   SHORE_MARGIN_PX = 1 px on every side (clamped to the bitmap) BEFORE grid
   sizing, so the sea/lake a coastal province borders — and a ring of
   neighbouring land — is sampled into the map. Without it the grid
   hard-cuts at the province's own extent and a straight coastline shows no
   water at all. Margin cells are out-of-province and flagged
   `out_of_bounds` (§6.14): impassable Terrain::Water (sea/lake
   pixel) or PASSABLE neighbour-terrain backdrop (land pixel — enterable
   ground with the §6.14 dwell-leaving rule); 1 px ≈ 7 hex columns on x /
   ≈8 rows on y at bitmap scale.
    Rendering (keyed on
    `out_of_bounds`; fog states added; palette reworked): the 3D board mesh and the
    minimap draw every sampled hex, so the board edge and foreign islands
    never read as holes. IN-BOUNDS Water stays a low deep-blue prism.
    OUT-OF-BOUNDS cells — land AND water alike —
    read as pseudo-transparent DISTANCE HAZE: the pixel's own terrain
    colour blended 0.55 toward the camera clear colour (0.53, 0.66, 0.82 —
    there is no ground plane under the board, so the sky blend IS the
    transparency; board.rs backdrop_color_of / SKY_HAZE) / a fixed
    rgb(58,58,64) minimap dot. Fog of war is an OVERCAST shadow, not darkness:
    Visible keeps full colour; Revealed dims ×(0.78, 0.81, 0.87); Hidden
    ×(0.48, 0.51, 0.58) — then each is muted 30–35% toward its own
    luminance, so the terrain hue survives faintly under a soft grey-day
    light, slightly cool-leaning. The hazed backdrop takes the same
    treatment, so its dark sits a touch lighter and bluer than the
    battlefield dark and the province border stays readable in the dark
    (界外迷雾 vs 战场暗幕).
    River bands ride the same shadow multipliers. The deployment zone has
    NO colour wash (the thick border mesh alone
    carries zone readability), and mountain rock props take their hex's
    elevation-band colour.
4. Compute grid dimensions: width = px_w × 7.114, height = px_h × 8.2145
   (§4.3 revised: uniform bitmap scale, NO cos(lat) term;
   8.2145 = 7.114 × 2/√3 compensates the pointy-top row packing so the
   rendered board matches the bitmap silhouette).
5. Compute target hex columns = ceil(width / 1.0), rows similarly.
6. Cap at MAX_GRID_WIDTH=512 × MAX_GRID_HEIGHT=512 (covers 98% of the 10,028 battleable provinces;
   the remaining polar/desert mega-provinces still get squeezed).
   Provinces in states with `impassable = yes` are excluded from battle
   at state level (21 states / 126 provinces).
6b. Reject provinces that produce fewer than MIN_PROVINCE_HEXES=20
   in-province hexes (too small to fight).
7. Down-sample occupancy: each hex cell → is center pixel inside the battle
   province OR an origin strip? (per-cell pixel province id retained).
8. Generate internal terrain (per-hex base from the pixel's OWN province):
   - Base terrain from definition.csv.
   - Seeded pseudo-random variation (forest → 70% forest + 20% clearing + 10% rough).
   - Village scatter (Terrain::Village, seeded, ~0.5–5% by terrain).
   - Coastal/border water: sea/lake province pixels inside the bbox (incl.
     the shoreline margin ring) → Terrain::Water (impassable).
   - River hexes from rivers.bmp: 3×3-centroid smoothing → centreline →
     hex-line rasterization (1 hex wide, continuous).
     Border river hexes just outside the province stay passable.
     River hexes are passable shallows, not blockers: movement cost ×3,
     a unit standing in a river hex takes ×2 damage, and they are not
     deployable (combat rules in §6.6; only generation is covered here).
   - Urban hexes from victory_points: hex count N = int((VP + 2) × U),
     with U = 1.2 when the province's own definition.csv terrain is urban,
     else 1.0. Anchored at the province's index-0 unit-stack position from
     map/unitstacks.txt (falls back to the province centroid when no stack
     data exists), filled nearest-first, skipping River/Water. An urban
     province without any VP entry still gets one small town as VP=1.
     Each VP city carries a floating name label (VICTORY_POINTS_<pid> from
     the UI language's victory_points yml — simp_chinese for a Chinese
     session, english otherwise/fallback; rendered as plain
     white text with a black outline).
    - River edges along edges matching adjacencies.csv data (cosmetic strips).
 8b. Per-hex ELEVATION noise: a deterministic
    smooth value-noise field (tactical-core noise.rs, two octaves, ~8-hex
    base wavelength, keyed by the elevation seed) modulates the terrain
    base elevation — Mountain ±1.75 (levels 0–4 with true peaks and valley
    floors), Hills ±0.75 (0–2), everything else flat (plains 0,
    marsh/river/water −1). ONE continuous field across the whole grid
    (battle province + stitching strip + shoreline margin), so ridge lines
    never break at province borders. Battle paths seed it with the BATTLE
    seed (same seed → identical relief, the determinism contract; live
    battles get fresh relief per battle); menu backdrops and debug fall
    back to the province id (stable per province). Later overlays re-stamp
    their own bases: rivers → −1, VP urban → 0. The field drives LOS
    (ridge rule, §6.6) and the per-hex 3D relief (every prism renders at
    its own elevation) — single source of truth for headless, AI and the
    renderer. Mountain hexes also carry their elevation in COLOUR
    five discrete contour bands (terrain.rs
    `banded_color` — L0 dark valley floor up to L4 near-white peak, hard
    band edges matching the blocky prisms), so the colour IS the height on
    both the board and the minimap.
 9. Determine frontlines from attack_dirs (dynamic frontline placement):
   - Stitched: attacker deploys on origin-province soil, defender in the
     battle province's central third (MIN_ZONE_DISTANCE no-man's land).
     The defender zone is biased toward the attack: its band is slid by a
     quarter of its span toward the attack direction, the central third
     being only the baseline.
   - Fallback (no origin found): attacker strips along attack-direction
     edges, defender in map center. Both are anchored to the IN-PROVINCE
     hex extent, not the grid edges (the shoreline margin ring
     pads the grid, so grid-edge strips would land in the water/backdrop
     ring and come out empty on island battles).
10. Render background + hex overlay.
```

### 4.3 Map Scale (revised — replaces the Mercator correction)

```
hex columns = pixel_width  × 7.114   # MAP_SCALE_PIXEL_TO_KM (defines.lua)
hex rows    = pixel_height × 8.2145  # = 7.114 × 2/√3: pointy-top rows are
                                     # spaced 1.5·size vs √3·size columns, so
                                     # a pixel row needs 2/√3 more hex rows
```

The HOI4 bitmap IS the geographic standard — the game renders
`provinces.bmp` 1:1 (unitstacks world coordinates are bitmap pixels), and
as a mod we take the game map as-is. No
latitude / cos(latitude) correction anywhere: the grid samples the bitmap
at a uniform density on both axes, so the rendered board matches the
in-game province silhouette at any latitude.

History (why there is no correction to correct): the earlier code
applied cos(lat) to the width with latitude decoded pole-to-pole plus a
9.768 km/px Y scale — both disproved by VP-city measurements (least-squares fit over 95 cities, tools/map_projection_probe.py:
X is pure equirectangular at 15.6 px/deg ≈ 7.116 km/px, Y spans only
~74°N…48°S with latitude spacing growing northward, and the Americas sit
~11.6° north of the old world). HOI4's projection is itself distorted, so
no true-ground-scale model could ever match the in-game look anyway; at
Viipuri (61°N) the old model squeezed the board to 0.32× of its in-game
E-W extent (aspect flipped 1.77 → 0.57). The old notes on the
pole-to-pole model were removed with this revision.

---

## 5. Division Data Pipeline

### 5.1 Data Flow (revised)

```
Save File (.hoi4)              Pre-extracted JSON            External Program
═══════════════                ═════════════════             ════════════════
division_template               equipment_stats.json
  regiments → types+counts  ─┐                              ┌─ Battalion base
division.equipment → counts ─┤  merge ───────────────────▶  │   attributes
technologies → tech_level  ──┘   [1]                        ├─ Per-battalion
active_ideas → modifiers         [1]                        │   stats
doctrine → bonuses               [2]                        └─ equipment_ratio
leader → traits+skills             [3]
division → org, str, exp
```

Pipeline status:

- [1] `technologies` drive the doctrine-table filter; `active_ideas`
  (with dynamic modifiers, country-leader traits and advisors) feed the
  national org and combat modifiers. The
  percentage bonuses techs grant to equipment CATEGORIES remain
  unmodelled (no tech-modifier table is extracted) — the researched tech
  level shows only through which equipment variant the division holds.
- [2] Doctrine factors are wired: assembly passes the country's
  researched `DoctrineTable` (category-flattened approximation per
  `DoctrineTable::factors`).
- [3] Unit leaders (army generals / field marshals) are parsed and
  applied: save chain `theatres → orders_group /
  field_marshal_group → leader id → character corps_commander /
  field_marshal`; +0.025 attack/defense per skill point, a field
  marshal's regular bonuses at ×0.5
  (`FIELD_MARSHAL_ARMY_BONUS_RATIO`); trait combat factors via the
  `unit_leader_traits` table section (FM-only blocks at full, regular
  trait blocks halved for a FM; terrain/skill-grant trait keys not
  modelled).

### 5.2 Save Fields Read (revised)

| Section | Fields Extracted |
|---------|-----------------|
| `division_template` | `regiments` (subunit type + x/y), `support`; `need` is **not parsed** — equipment requirements come from `unit_templates.json` `needs` instead |
| `division` | `division_template` name + `division_template_id`, `location` (province), `organization`, `strength`, `experience`, `equipment` (counts), `entrenchment`, `supply_status`; auto-name token pair `division_name={type,name_order}` + the template's `division_names_group` (resolved live against `common/units/names_divisions/*.txt` at assembly — `%d` decimal / `%s` Roman, `ordered[N]` else `fallback_name`; player `override` renames win; `type` is always 0 in 1.19.2) |
| `technologies` | List of researched tech tokens (parsed; doctrine progress tokens are mixed into this same list) |
| `active_ideas` | Active national spirit tokens (parsed; no consumer yet) |
| `doctrine` | Doctrine progress appears as tokens inside `technologies`; consumed via `DoctrineTable::researched` |
| `leader` | Unit leaders: `theatres` army/army-group membership + `leader` ids → `character` corps_commander/field_marshal skills+traits (parsed); the political country-leader record is only used for its traits |
| `country.modifiers` | Accumulated modifier map (**not parsed**) |

### 5.3 Stat Calculation (revised)

All game-data-derived stats are pre-calculated into battalion base attributes on tactical launch. Only effects generated within the wargame become tactical buffs.

Status: doctrine factors, national combat modifiers
(`army_attack_factor` / `army_defence_factor` / `breakthrough_factor`),
division experience (Green −25% … Veteran +75%, attack side only) and unit
leader bonuses (±2.5% per skill point, FM halved; per-type trait factors)
are all wired into the assembly below. Tech-category percentage bonuses
remain unmodelled (resolve to 1.0 — no tech-modifier table is extracted).

```
# HOI4 equipment stats are
# BATTALION-LEVEL values at full complement — normalize piece counts by the
# per-battalion need (fill ratio). The old ×count form was 100× the
# HOI4/script scale (issue family).
battalion.soft_attack = Σ(equipment_stats[e].soft_attack × fill[e] × tech_modifier[e])
                      × (1 + doctrine_attack + country_attack + leader_attack
                         + leader_type_attack[group])
                      × (1 - (1 - equipment_ratio) × degradation_factor)
                      × experience_factor                         # attack side only
                      # fill[e] = allocated pieces / per-battalion need
                      # (100 rifles → 1.0); allocation comes from template
                      # `needs` shares, surplus capped at available stock;
                      # degradation_factor defaults to 0.5.

battalion.max_org = Σ(unit_template[s].max_organisation × battalion_count[s]) / battalion_count
                  × (1 + doctrine_org_factor) × (1 + country_org_factor) + country_org_flat

battalion.org     = max_org × current_org_ratio_from_save     # ratio applies to
                                                              # current org only

battalion.max_strength = Σ(unit_template[s].max_strength × battalion_count[s])
battalion.strength     = max_strength × current_strength_ratio_from_save

battalion.armor = max(equipment_stats[*].armor) × tech_armor_modifier
battalion.piercing = weighted_avg(equipment_stats[*].piercing, equipment_count)
battalion.hardness = weighted_avg(equipment_stats[*].hardness, equipment_count)
battalion.speed = battalion-type speed table (tactical-core/src/unit.rs,
                  HOI4-aligned; §6.2 abolished AP). Equipment min-speed is
                  deliberately NOT applied (speed comes from the chassis
                  table only; the equipment-speed statistic is not computed).
```

**Tactical buffs** (generated in-wargame only): entrenchment, terrain cover, encirclement attrition, supply degradation, support company adjacency buffs, commander effects after promotion in battle. (The flank damage bonus was retired, §6.4.)

### 5.4 Pre-Extracted Data Tables (revised)

#### equipment_stats.json
Complete extraction from `common/units/equipment/*.txt`, `dlc/*/common/units/equipment/*.txt` (276 entries). Each entry carries a plural `categories` list plus `group` and `active` fields:

```json
{
  "infantry_equipment_0": {
    "archetype": "infantry_equipment",
    "year": 1918,
    "soft_attack": 3.0, "hard_attack": 0.5,
    "defense": 20.0, "breakthrough": 2.0,
    "armor": 0.0, "piercing": 1.0,
    "hardness": 0.0, "max_speed": 4.0,
    "reliability": 0.9, "supply_use": 0.06,
    "build_cost": 0.43, "resources": {"steel": 2},
    "categories": ["infantry"], "group": "archetype", "active": true
  },
  "artillery_equipment_1": { "archetype": "artillery_equipment", ... }
}
```

#### unit_templates.json
From `common/units/*.txt` battalion definitions. Top level is split into
`line_battalions` (68 entries) and `support_companies` (89 entries) — note
that HOI4's `artillery_brigade`, `anti_tank_brigade`, `anti_air_brigade`,
`rocket_artillery_brigade`, `motorized_rocket_brigade`, etc. are filed under
the `support_companies` key in this table:

```json
{
  "line_battalions": {
    "infantry": {
      "max_strength": 25, "max_organisation": 60,
      "default_morale": 0.3, "combat_width": 2,
      "manpower": 1000, "training_time": 90,
      "supply_consumption": 0.06, "weight": 0.5,
      "group": "infantry", "types": ["infantry"],
      "needs": {"infantry_equipment": 100},
      "terrain_modifiers": { ... },
      "abbreviation": "INF", "categories": ["category_front_line", ...]
    }
  },
  "support_companies": {
    "artillery_brigade": { "max_strength": 0.6, "max_organisation": 40, ... }
  }
}
```

#### terrain_mods.json
From `common/terrain/00_terrain.txt` (14 entries). Every entry has
`movement_cost`, `combat_width`, `combat_support_width`, `attack_mod` and
`defense_mod`; rough terrain additionally carries `enemy_air_sup_bonus`,
`truck_attrition_factor` and `supply_flow_penalty_factor`, and the harsh
terrains (mountain, jungle, marsh, desert) add `attrition` (jungle/marsh/
desert also `sickness_chance`):

```json
{
  "forest": {
    "movement_cost": 1.5, "combat_width": 60,
    "combat_support_width": 30,
    "attack_mod": -0.15, "defense_mod": 0.0,
    "enemy_air_sup_bonus": -0.1,
    "truck_attrition_factor": 0.2,
    "supply_flow_penalty_factor": 0.08
  },
  "urban": { "attack_mod": -0.30, "movement_cost": 1.2, "combat_width": 80, ... }
}
```

#### combat_tactics.json
From `common/combat_tactics.txt`. Top level is `{"phases": […], "tactics": {…}}`
— 5 phase names and 55 tactics. Per-tactic condition fields are `trigger`,
`active` and `is_attacker`:

```json
{
  "phases": ["close_combat", "tactical_withdrawal", "seize_bridge",
             "hold_bridge", "street_fighting"],
  "tactics": {
    "basic_attack": {
      "attacker_damage": 0.05, "defender_damage": 0.0,
      "speed_mod": 0.0, "combat_width_mod": 0.0,
      "counters": [], "countered_by": ["counterattack"],
      "phase_change": null,
      "active": true, "is_attacker": true,
      "trigger": {"is_attacker": true, "phase": false}
    }
  }
}
```

#### doctrine_bonuses.json
Recursively extracted from `common/doctrines/**/*.txt` (116 doctrine
tree keys — the HOI4 1.15 system: `new_mobile_warfare`,
`superior_firepower`, …, plus the folder stubs `land`/`naval`/`air`/
`special_forces`). Each tree is `{path, nodes}`, where `nodes` maps a
doctrine tech token to its bonuses including per-battalion-category
`category_modifiers`:

```json
{
  "superior_firepower": {
    "path": "folders\\doctrine_folders.txt",
    "nodes": {
      "superior_firepower": {
        "xp_cost": 100.0,
        "enable_tactics": ["tactic_barrage"],
        "category_modifiers": {
          "line_artillery": {"soft_attack": 0.1},
          "support_battalions": {"max_organisation": 10.0}
        },
        "additional_brigade_column_size": 1.0,
        "planning_decay_rate_factor": -0.1
      }
    }
  }
}
```

#### country_colors.json
From `common/countries/colors.txt` (extract_country_colors.py): 304 country
tags mapped to `[r, g, b]`. Used for unit base-plate colors and deploy-zone
border accents:

```json
{
  "GER": […], "FRA": […], "SOV": […], ...
}
```

### 5.5 Variable Tagging (for Injection Targeting) (revised)

In `tac_start` decision's `complete_effect`:

```
# For each division in the battle:
every_country_division = {
    limit = {
        OWNER = { tag = ROOT }
        # Design calls for a battle-province check here (unit_leader scope +
        # province check). Current mod status: no province check — every
        # player division country-wide gets tac_in_battle = 1
        # (not yet implemented; see §10.1 roadmap).
    }
    set_variable = { tac_in_battle = 1 }
}
```

Console injection then targets (matches the design — the `tac_in_battle = 1`
limit is implemented as written):
```
damage_units = {
    limit = { check_variable = { tac_in_battle = 1 } }
    org_damage = var:tac_org_dmg
    str_damage = var:tac_str_dmg
}
```

---

## 6. Tactical Combat System

### 6.1 Unit Attributes (revised)

All per-battalion values after save-based calculation:

| Attribute | Source |
|-----------|--------|
| `soft_attack` | Equipment × tech × doctrine |
| `hard_attack` | Same |
| `defense` | Same |
| `breakthrough` | Same |
| `max_org` | Unit template × org ratio from save |
| `max_strength` | Unit template × str ratio from save |
| `speed_kmh` | Battalion-type table below (HOI4-aligned); drives continuous movement (§6.2) |
| `range` | Battalion-type table below |
| `sight` | Battalion-type table below |
| `armor` | Max equipment armor × tech |
| `piercing` | Weighted avg equipment piercing |
| `hardness` | Weighted avg equipment hardness |

Battalion classes: a battalion class = **weapon
type ⊕ chassis ⊕ token flags**, mirroring HOI4's own `type = { motorized,
artillery }` multi-labels. Rules attach to attribute flags, not to the
UnitType enum (which stays as the save-mapping key and model family).

- **Attributes**: `Infantry` (Hold stance, sight 2 — includes motorized /
  mechanized infantry, who dismount to fight), `Cavalry` (no Hold),
  `Motorized` / `Mechanized` (motor mobility class), `Armored` (direct fire
  range 2, sight 1), `Artillery` (fire missions: precise or area), `Rocket` (area-fire only,
  min range 3, no assault; same 0.3 precision factor as tube
  artillery, no saturation doubling), `AT` (direct fire range 2),
  `AA` (umbrella radius 3 placeholder), `Towed` (must emplace — a LIMBERED
  towed unit deals no damage in any form: no assault, no counter, no fire
  support), `Recon`
  (sight 4), `Support`; `Amphibious` / `Flame` are filed now and get rules
  in later passes.
- **Chassis** (road speed + emplacement): None (foot) · Towed (horse,
  3 km/h, emplace) · TruckTowed (12 km/h, emplace) · Wheeled (self-
  propelled wheel, 12) · Halftrack (10) · armored Light / Medium / Heavy /
  SuperHeavy / Modern (12/10/8/6/12).
- Weapon-type defaults: Infantry / Marine / Mountaineer / Paratrooper /
  Engineer range 1, sight 2, speed 4 · Cavalry / Bicycle 1/2/6 · Motorized /
  Mechanized infantry 1/2 (chassis speed) · Recon 1/4 · armor 2/1 ·
  Artillery 9/1 · AT 2/2 · AA 1/2 (+umbrella 3) · Rocket 6 (min 3)/1.
- Examples: `medium_sp_artillery_brigade` = Artillery + Medium chassis
  (10 km/h, no emplacement); `mot_artillery_brigade` = Artillery +
  TruckTowed (12 km/h, emplaces); `motorized_rocket_brigade` (Katyusha) =
  Rocket + Wheeled (12 km/h, area-fire only, no emplacement);
  `rocket_artillery_brigade` (Nebelwerfer) = Rocket + Towed (3 km/h,
  emplaces).

Mobility classes: **leg** (infantry, cavalry, bicycle, support companies) vs **motor** (anything with a chassis, incl. towed gun wagons). Motor units suffer a larger terrain debuff: the terrain's excess-over-1km cost is ×1.5 (§6.6).

### 6.2 Movement Orders & Time (replaces the old AP model)

- 1 hex = 1 km; 1 tactical turn = 10 minutes (6 turns per strategic hour).
- There are **no action points**. Movement uses standing **move orders**: the player/AI picks a destination, the path is auto-computed (A*), drawn on the map as route arrows, and the unit marches at its own speed.
- At the end of each side's turn, every unit of that side with an order advances `speed_kmh / 6` effective kilometres along its path (fractional progress persists inside the order — 4 km/h infantry covers one plains hex every 2 turns).
- Per-hex travel time = effective km ÷ speed. Effective km = terrain cost (mobility-class adjusted) + river-edge surcharge (+2, crossed edges only). (ZOC delay disabled — §6.5.)
- **Contact stop**: stepping into a hex adjacent to an enemy consumes the order and halts the unit (assault is a separate decision next turn). The halt is **symmetric**: adjacent enemies with marching orders of their own stop too.
- **Interception**: if the *next* path hex itself is enemy-held (fog-hidden, or the enemy marched onto the route), the unit halts in front of it and **both** orders are spent — the simple ambush rule; a red "Intercepted!" popup marks both units.
- **Fog-limited pathing**: player-issued orders and the ETA display are computed against the player's view — enemies hidden by fog neither block routes nor project ZOC. Execution stays omniscient (contact/interception above), so nothing leaks but ambushes happen.
- One battalion per hex: paths route **around** occupied hexes (friendly or enemy) but may legally *end* on a friendly-occupied hex (the occupant may march away first). If the next hex is still friendly-occupied at execution, the unit waits (progress kept), pops an orange "Congested", and retries with a detour (≤1.5× remaining cost) next turn.
- **One action per turn** per unit (`acted` flag): assault, fire support, emplace, or limber each consume the turn's action. A unit that acted does not march at the end of that turn.
- Assault consumes the unit's turn and cancels its move order. Fire support consumes the turn's action but not the order (a firing unit simply does not march that turn — towed guns are emplaced anyway and cannot march).
- Selected-unit ETA: remaining travel time of the standing order, displayed in whole turns (§9.2).

| Action | Cost |
|--------|------|
| Issue/change move order | free (marches from the end of the turn) |
| Assault adjacent enemy | the turn's action |
| Fire Support | the turn's action (towed guns: emplaced first) |
| Emplace / Limber (towed guns) | the turn's action each way (≈1 turn to deploy, ≈1 turn to pack up) |
| Take cover / stand up (Hold) | the turn's action to take / free to leave (drops on move or attack) |

### 6.3 Combat Resolution (hit-step × numbers-squared model; rework — supersedes the Lanchester-denominator form)

The HOI4 dice model was retired long ago: its division-scale
constants (attacks in the hundreds × 0.05 per die) left battalion-scale
engagements (attacks 4–14) dealing ~0.05 damage per action. Its successor
— a deterministic Lanchester square law A²/(D+40)×6 — was retired in turn: it squared the EQUIPMENT-quality gap
along with the numbers (a 2× stat edge dealt 4×), needed the arbitrary
softening constant C=40 (and still divide-by-zeroed on breakthrough-0
units), and turned the breakthrough≪defense gap into a 7.3×
attack-is-suicidal inversion. The replacement keeps **numbers squared but
quality linear** (classical Lanchester strength = q×N²) and turns defense
into the **vanilla hit step**, resolved in a **unified end-of-turn fire
phase** (预令统一结算) with no jitter.

#### Damage Formula

```
q = (soft_attack×(1−h) + hard_attack×h×piercing tier) × precision_factor
    # the VANILLA continuous hardness mix (replaces the h<0.5 binary
    # switch); the piercing tier (1.0/0.8/0.65/0.5 — vanilla's own
    # PIERCING_THRESHOLDS table) multiplies the HARD component only
P = q × g²            # g = strength ratio (equipment fill included, §5.3)
    # AIMED-fire regime (assault / direct fire / non-artillery counter)
    — AREA fire is LINEAR instead: P = q × g — indirect artillery fire missions AND counter-battery
      replies: "shells are shells, however distributed" (the Deitchman
      area-fire law); at full strength the two forms coincide
    — pooled (concentration): P = Σ(q_i·g_i) × Σg_i
      # fire conservation: two half-strength battalions = one full one;
      # combined-arms pairs cross-multiply (quality × numbers synergy);
      # aimed-fire only — fire missions never pool
D = defense × (1.25 if Holding) × (1 + 0.10 × entrench layers)
    × (1 + battalion terrain DEFENSE adjuster)   # v3.3 — vanilla per-class
    # data (mountaineers +10% mountain, engineers +25% forest/river…);
    # the global terrain defense column is retired
    — or breakthrough instead of defense for a unit hit while attacking
hit = hit_base + (hit_saturated − hit_base) × P/(P+D)
    # the soft-step form of vanilla's 10%/40% hit chances
    # (00_defines.lua BASE_CHANCE_TO_AVOID_HIT = 90 / …_AT_NO_DEF = 60):
    # same asymptotes, no cliff at battalion-scale numbers, no division
    # by D alone (breakthrough 0 simply takes the saturated rate)
org_damage = damage_scale × P × hit × ∏linear modifiers   # deterministic

Linear modifiers (NEVER inside P): battalion terrain ATTACK adjuster
(v3.3 — vanilla per-class data, strike-role form: the firer applies its
own adjuster against the target's hex; line infantry has zero adjusters
everywhere, specialists/vehicle classes carry the terrain game), direct-fire
falloff beyond range 1 (×0.6), cover (per-terrain; NEGATIVE on Desert/River
= exposed — river's −50% carries the retired ×2 ford rule), command aura
±10%, melee elevation / indirect crest (§6.6).

Hard cap: one org hit ≤ max_org × 0.40.
Shock:    one fire phase's AGGREGATE delivered org damage on a target
          (a group strike's per-attacker shares sum on the target first)
          ≥ max_org × 0.25 → Shocked (see below). With the bounded hit
          step, both fuse and shock are only reachable by CONCENTRATED
          strikes — "massed fire suppresses" is their clean semantics.
str_damage = org_damage × λ × (max_str/max_org)
          # λ = break_str_loss (0.12) normally — a full break (max_org
          # cumulative) costs 12% of max strength for EVERY class, the
          # vanilla division-scale break cost (the per-hit dice ratio
          # 0.68 only works against HOI4's division-scale org/str pool
          # geometry). λ = broken_str_loss (0.68 — the vanilla dice
          # ratio) when the target's org is ALREADY 0 at the start of
          # the volley: no organization
          # left to absorb the fire, so it lands on manpower/equipment;
          # judged per volley at volley start, never mid-volley (a unit
          # broken by the first shares of a group volley still converts
          # the whole volley at 0.12).
Defaults: hit_base = 0.10, hit_saturated = 0.40 (locked vanilla
          constants), damage_scale = 1.0 (K₂ — an infantry duel breaks in
          ~10 strategic hours; slower tempo lets
          clear front lines form), break_str_loss = 0.12,
          broken_str_loss = 0.68,
          random_spread = 0 (deterministic — the jitter mechanism is kept
          for an optional resolution-fog mode; the fog of war, not the
          resolution, carries uncertainty),
          terrain_modifier_scale = 1.0 (v3.3 — global dial on
          every battalion terrain adjuster; 0 switches terrain identity off).
          Balance levers: damage_scale (tempo) + the precision-factor
          table (relative class power) + break_str_loss/broken_str_loss
          (strength attrition) + terrain_modifier_scale (terrain identity
          strength); h0/h1 stay locked.
```

**Precision factors** (per attribute flag; first tier wins: rocket > tube >
AT > AA > armor > default): infantry family / recon / engineer 1.0, armor
0.5, armored car 0.6 (currently maps to Recon → 1.0), AT/TD 0.8, AA 0.7,
tube & rocket artillery 0.3 (equalized — a rocket salvo's per-hex
punch equals a tube strike; delivery is the difference). They translate
HOI4's division-scale attack ratings into battalion-level effective fire
(area weapons saturate, direct weapons track) and sit INSIDE q (linear).

**Baselines** (org / max strength, HOI4 subunit values): infantry 60/25,
cavalry 70/25, motorized 60/25, mechanized 60/30, all tanks 10/2, armored
car 20/5, engineer/recon/support 20/2. Tube/rocket artillery, AT and AA are
*companies* in HOI4 (org 0) — promoted here: towed 30/4, towed rocket 25/3,
SP rocket 20/3. Attack/defense stats come from the equipment chain (1939
medium tank: soft 19 / hard 14 / def 5 / brk 36 / armor 60 / pier 61).

> The promoted baselines now apply on BOTH
> paths — the live/save mapper (`tactical-save/src/units.rs`) substitutes
> `UnitType::base_org()/base_strength()` whenever the HOI4 template org is
> 0, matching the `BattalionUnit::new` demo defaults; save org/str ratios
> still scale the current values only.

#### Unified Fire Phase (预令统一结算)

The acting side only REGISTERS orders during its turn (move order XOR one
attack order per unit; re-registering swaps freely). At end of turn, after
the movement phase, every attack order resolves **together**: all damage
and counter-fire is computed first and applied simultaneously — mutual
destruction is possible, and a dying unit still shoots back.

> The LATER order
> supersedes the earlier one in both directions — registering an attack
> clears the march, issuing a move order cancels the registered attack
> (logged), and taking cover (Hold) cancels it too. Right-clicking the
> unit itself ("stand by") still cancels both at once.

- **Concentration (numbers squared)**: attackers pooling on one target
  merge into ONE strike with P = Σ(q_i·g_i) × Σg_i — 2 full battalions ≈
  ×4 firepower PLUS deeper hit-step saturation (each point of P past D
  hits at up to 4×). Each attacker is credited in proportion to its
  contribution q_i·g_i. Fire missions stay separate.
- **Counter-fire**: the defender's firepower split LINEARLY across every
  attacker inside the defender's own range (P/n — fire conservation;
  vanilla distributes its attacks linearly too. The retired ÷n² form made
  a surrounded defender's output evaporate — an 81:1 melee exchange).
  Each share evaluates its own hit step against the caught attacker's
  breakthrough. Only the primary target of a fire mission replies;
  retreating units never counter; a rifle battalion shelled from 9 hexes
  cannot reply. **Counter-battery gate**: no radar-less
  counter-battery — indirect artillery replies only inside its direct-lay
  self-defense circle (`counter_direct_lay_range` = 2, guns over open
  sights) and rocket launchers never counter (unguided saturation has no
  direct-lay mode). Every surviving counter is therefore AIMED fire
  (square law) by construction; fire missions stay AREA (linear P = q·g).
  A LIMBERED towed unit never replies either (limbered guns
  deal no damage in any form).
- **Annihilation**: strength ≤ 0 eliminates the battalion outright (no
  untargetable zombie holding its hex).
- **Target lost**: an order whose target died / left the envelope fizzles
  with an orange floater.
- **Assault occupation**: a broken defender retreats; the attacker moves in.
- **Shocked**: the DELIVERED org damage on one target aggregated across
  the whole fire phase — a concentrated group strike's per-attacker
  shares sum on the target before the test — reaching 25% of max org
  (the 40% per-hit cap means only serious hits trigger) marks the unit
  "S": it may NOT register attack
  orders while shocked; movement / Hold / emplacement stay legal and
  in-flight orders are NOT cancelled (shock suppresses attacks only,
  by design). The shock persists until the
  END OF THE NEXT turn-end after the one that inflicted it — a
  counter-fire shock rides through the whole enemy turn, and a shock
  suffered on the enemy's turn blocks orders for the owner's full turn
  (`CombatEngine::expire_shocks`).
- **Battle reports**: after each fire phase a click-through modal
  walks every engagement — camera focuses the hex, the window lists each
  attacker with its attack type (assault / direct fire / barrage), org/str
  damage dealt, counter-fire taken, and the outcome (BROKEN / SURRENDERS /
  ANNIHILATED). [Continue] advances, Esc skips the rest; map input, End Turn
  and the AI turn are frozen while a report is open. **Presentation**: the playback zoom is pinned so the defender and its
  3-hex ring always read clearly (`REPORT_CAM_DISTANCE`, the player's zoom
  is restored on drain), the consequence of a repel / annihilation is
  deferred until THAT report is confirmed — the unit keeps its pre-combat
  look as a "report ghost" while the modal shows the engagement, and only
  [Continue] slides it to its retreated hex / removes it (Esc releases all
  remaining ghosts at once) — and damage floaters paint on the shared panel
  background layer so they slide under the info panels
  instead of climbing over the report window. **Camera glide**: every focus change is a smooth pan+zoom GLIDE
  (`CameraGlide`, smoothstep easing, duration scaled to the pan/zoom delta
  and clamped 0.35–1.2 s) — from the pre-tour view to the first report,
  between reports, and back to the pre-tour view when the queue drains.
  The report's combat animation and its window open only when the glide
  LANDS (the arrival pulse sets `tour.focused`; the window and the autoplay
  driver's [Continue] both gate on it), and the tour teardown waits for the
  return glide. While a glide plays, user camera input and stale one-shot
  camera requests are dropped. **Engagement detail**:
  every report lane carries a「细节 / Details」button, and every
  exchange/counter line in the standalone battle-log window is clickable —
  both open the SAME non-modal engagement-detail window showing the full
  formula chain of the exchange with the resolved numbers plugged in:
  q composition (soft/hard × hardness × piercing tier × class precision) →
  firepower P (lone q·g², or q·g for area fire; the volley pool
  Σ(q·g)×Σg with the member's share; counter-fire ÷n) → D (base × Hold × entrenchment × terrain
  adjuster, defense or breakthrough) → the 10%/40% hit step → the linear
  modifier stack (command aura, terrain aptness, direct-fire falloff,
  melee elevation / crest, cover — neutral ×1.00 rows greyed) → jitter /
  hard cap / pool share / area weight → delivered org, and the org→str
  conversion (0.12 normal / 0.68 already-broken). The numbers are captured
  at resolution time into a fixed-size Copy `HitBreakdown` (tactical-core
  `damage.rs` `*_explained` variants; the plain functions are thin
  wrappers, so the panel can never drift from the resolution and the AI
  estimate path is untouched). Capture is pure bookkeeping — no RNG draws,
  deterministic battles stay bit-identical. Enemy-turn engagements get the
  same detail; fizzled (target-lost) lanes carry no chain and no button.
  **Formatting**: the window title carries only
  the hex — the direction lines (`■ kind — A → B` / `■ Counter-fire — B →
  A`, 18 pt strong, PARCHMENT for the player's acting side / GOLD for the
  enemy) are the first-level headers, with ▪ 15 pt section headers and
  indented rows beneath. Chain rows write each segment as "name value" (×
  is ONLY the operator between segments) and OMIT neutral segments — an
  all-neutral D collapses to a single grey "base" row, and the
  breakthrough side never lists the Hold/entrenchment channels it does not
  have. The linear modifier stack keeps one "name ×value" row per factor
  with neutral rows greyed.

#### Fire Mission Rules

- **精确射击 (Precision fire) vs 覆盖射击 (Area fire)**: the mode is chosen by HOW the mission is
  issued, never by a sight check.
  - **精确射击**: the player selects a deployed (or self-propelled) gun and
    RIGHT-clicks a VISIBLE enemy unit inside the envelope → 100% damage on
    that unit's hex (the picker never reveals hidden enemies — a clickable
    enemy IS visible). Solid arc; the §6.6 crest factor applies.
    The AI mirrors the rule: a mission is precise iff its aim hex holds an
    enemy unit currently visible to the acting side (its own fog view).
  - **覆盖射击**: the F radial button + a left-click on ANY in-range hex
    → the barrage LANDS REGARDLESS of who is in the zone ("点击任意射程内地点" — a friends-only zone still bleeds); every
    TARGETABLE unit in the hex + 6 neighbours (friends included — the
    F-barrage cannot tell friend from foe) takes ITS OWN strike weighted by
    its hex: the aim hex 4/10, each of the 6 neighbours 1/10
    (`CombatParams.area_center_share` / `area_neighbor_share`) — per-victim:
    each victim's own hex decides its terrain/cover/crest. Only a zone
    with NO targetable unit at all fizzles target-lost. Dashed arc + the
    whole 7-hex blast zone outlined in the same dim hue (the outline IS
    the friendly-fire warning). Intel-goal fire is always area.
  - Rockets: only 覆盖射击 — a right-click also resolves as an
    area salvo; a launcher can never single out a target. The salvo
    strikes EVERY unit in the zone at FULL tube-artillery strength (no
    weighting, no dilution), friends included.
- **Pre-order arc telegraphing**: the
  fire-mission pre-order arc shows its resolution quality BEFORE End Turn
  — the interior colour names the §6.6 crest bucket at the aim hex (exposed
  crest ×1.5 = magenta-red, neutral = bright red, defilade ×0.5 = amber),
  and an AREA (weighted 4/10-1/10 zone; rockets always) mission runs
  dashed and dimmed, with
  its whole 7-hex blast zone (target + 6 neighbours) outlined in the same
  dim hue. Assault and direct-fire lanes keep the plain bright red (their
  geometry rules differ). F-picking shows NO enemy markers (highlighting visible enemies would mislead — they are
  precision-strike targets for the right-click path, not the F-barrage).
- **Rockets**: always area — the salvo
  strikes EVERY unit in the 7-hex zone at full tube-artillery strength (same
  0.3 precision factor; no dilution, no doubling), friends included
  (friendly fire; the AI refuses aim hexes whose blast contains friends).
  Per-hex battle reports mark friendly-fire lanes. **Reload**:
  after a salvo the launcher reloads for 3 turns (fire end of turn N → next
  on turn N+3; `rocket_fire_cooldown_turns`); the counter shows "W" and the
  hover card "RELOADING (Nt)" while hot. Minimum range 3 (was 2 —
  at 2 the splash still reached hexes adjacent to the launcher); the
  selection ring shows an orange dashed minimum-range circle.
  NB: fire missions are NOT re-validated at resolution — a friendly unit
  moving into the blast zone after registration gets hit (kept as a
  realistic battlefield accident; the AI only
  avoids friends at PLANNING time).
- **Emplacement (towed guns)**: towed artillery / AT / AA must be emplaced
  before firing; (un)limbering consumes the turn.

#### Support Companies (attachments; revised)

Support companies are NOT map units: they attach to a battalion (one-shot
stat bonuses baked in: AT +4 hard/+10 piercing, artillery +5 soft, AA +1
soft, recon +1 sight; ongoing: hospital ×0.75 str damage taken, maintenance
+0.5 str/full turn). The host dies → attachments die. In live mode the
division's companies attach round-robin across its battalions.
Engineer, logistics and military-police companies attach the same
way but have NO tactical effect yet (planned — not yet implemented).
Signal companies got their effect (§6.13): the company rides
the division HQ and extends its command aura radius (3 → 6 km).

### 6.4 Encirclement (progressive attrition model; redesigned)

The flanking damage bonus is **retired** — a multi-directional attack is already rewarded by the Lanchester concentration itself (each attacker resolves its own strike), so the old ×1.25/×0.90 flank terms double-counted the pincer. Encirclement is now a pure attrition/surrender model with two levels. Both require an **isolated** target: an adjacent combat-effective enemy must be present (impassable terrain alone never encircles — the old rule bled units standing in water corridors with zero contact), and no combat-effective friend may be adjacent (a contiguous friendly line cannot be pocketed one battalion at a time).

| Level | Condition | Effects |
|-------|-----------|---------|
| **Partial encirclement** | ≤2 free adjacent edges, OR an opposite adjacent enemy pair | -2.5% max org/turn |
| **Full encirclement** | 0 free edges | -5% max org/turn. Cannot retreat — org at 0 = surrender instead of retreat. Equipment captured (planned — not yet implemented) |

The pace follows the 10-minute turn (§8.1): a static partial pocket collapses in ~40 turns (≈7 strategic hours), a full pocket in ~20 (≈3.3 h) — encirclement is a siege clock, not a death sentence. The HQ command aura (+2% max org/turn, §6.13) visibly sustains a pocket. Board status icons (§9): blue **↓** = partial attrition, blue **↓↓** = full attrition, red **↑** = HQ-aura recovery.

### 6.5 Zone of Control (ZOC; revised)

- Each battalion projects ZOC to its 6 adjacent hexes.
- **ZOC movement delay is DISABLED**: entering ZOC and ZOC→ZOC movement cost no extra effective km — `zoc_entry_ap_cost` / `zoc_to_zoc_ap_cost` default to 0 in `params.rs`. The mechanism is kept (surcharges still read by `step_km`); restore +1 / +2 to re-enable. Enemy-adjacent friction is instead carried by contact stop, interception, and the unified fire phase.
- **Contact stop**: a marching unit that steps into an enemy-adjacent hex halts and its move order is consumed (§6.2).
- ZOC extends only into passable neighbor hexes. Since the terrain pass, the only impassable terrain is open Water — mountains are passable (3 km) and rivers are fordable (§6.6), so ZOC crosses them freely.

### 6.6 Terrain Effects (revised — cover is the only uniform combat layer; the Attack/Defense modifier columns are retired)

| Terrain | Move Cost | Sight Mod | Cover |
|---------|-----------|-----------|-------|
| Plains | 1.0 | 2 | 0% |
| Forest | 1.5 | 2 | +15% |
| Hills | 1.5 | 3 | +20% |
| Mountain| 3.0 | 4 | +40% |
| Urban | 1.2 | 1 | +50% |
| Jungle | 2.0 | 2 | +20% |
| Marsh | 2.5 | 2 | +10% |
| Desert | 1.2 | 3 | **−10%** |
| River | 3.0 | 2 | **−50%** |
| Clearing | 1.0 | 2 | 0% |
| Village | 1.0 | 2 | +30% |
| Water | impassable | 3 | — |

- **v3.3 shape**: cover — damage taken by the OCCUPANT of the hex, negative = exposed — is the only uniform terrain combat layer. The old per-terrain attack/defense modifier columns were linear duplicates of this channel (both read the target's hex), and vanilla terrain carries no defense bonus anyway. Per-terrain rationale: Forest 15% (trees conceal but splinter under shellfire); Hills 20% (slopes; the melee elevation rule adds the dynamic part); Mountain 40% (rock and relief — assaults belong to specialists); Urban 50% (dense masonry, hardest on the map; vehicles canalized); Jungle 20% (dense vegetation, no hard cover); Marsh 10% (open wetland); Desert −10% (exposed, fire superiority tells); River −50% (caught mid-ford in open water); Village 30% (masonry farmsteads, the classic Eastern-Front strongpoint — above forest/hills, below dense urban; capped there because villages are seeded scatter on real provinces).
- **Battalion terrain adjusters**: the unit-class terrain game rides on the per-battalion modifiers from the HOI4 unit files (`unit_templates.json`, values verbatim, §5.4), baked into `BattalionUnit::terrain_adj` at assembly — per save TOKEN on the live path (rangers keep their forest identity even mapped to Infantry), per canonical template key on script/preset/demo paths. Strike-role semantics (§6.3): the firer's `attack` adjuster multiplies its damage as a standalone linear factor against the target's hex (floored at 0); the absorber's `defense` adjuster multiplies its D on its own hex. Line infantry/militia/paratroopers define zero adjusters in vanilla (the baseline); specialists carry bonuses (mountaineers +35%/+10% mountain, marines +30% river/marsh attack, rangers forest/jungle, engineers +25% defense on rough ground/river); vehicle and towed classes carry penalties (medium armor −40% urban, mech −30% jungle, towed guns −20% forest, …). `terrain_modifier_scale` (CombatParams, default 1.0) is the global dial. The `amphibious`/`fort` keys are dropped (no counterpart hex) and the `movement` key is not consumed (mobility classes already price terrain).
- Move Cost is the leg-class value; **motor-class units pay 1 + (cost − 1) × 1.5** (the excess over the 1 km baseline is multiplied, §6.1 mobility classes).
- **River hexes are fords, not roads** (revised rule): crossable and may be held, but never deployable; a unit standing in a river hex is exposed via the −50% cover above — the old ×2 damage special case is retired (one number per terrain).
- River *edges* (from `adjacencies.csv`) instead levy a flat **+2 km surcharge** on the crossing step only.
- **Water** is the only fully impassable terrain (map border / coastal flavour from the province bitmap) — no combat rules.
- **Line of sight**: terrain no longer hard-blocks LOS except **Urban** (buildings; street fighting). Mountain relief is carried by the per-hex elevation field (§4.2 step 8b): an intermediate hex blocks only when it stands **strictly higher than BOTH endpoints** (ridge rule — peaks see over saddles, valleys are shut in, equal elevation never blocks). Sight range from the table above stays the HARD CAP (§6.1): LOS only shortens it, never extends it.
- **Concealment**: Forest/Jungle do not blind their OCCUPANT (sight 2, plains-level — the old in-forest sight 1 is gone; Marsh likewise opens to 2: open wetland). Instead they CONCEAL: an observer's sight counts **−1 against a target hex** on Forest/Jungle (infantry 2→1 hex, recon 4→3), floored at adjacency — a sight-1 unit still spots the treeline next to it, so point-blank ambushes are seen. The rule lives in the fog update (`FogOfWar::update`), so it is symmetric across both sides' views. Urban keeps sight 1 + its hard LoS block (buildings).
- **Crest rules (plan 2)**: the slope's bite lives in the hex relief (with the terrain modifier columns retired in v3.3, this is unchanged and now carries all of it):
  - **Melee height gain**: at distance ≤ 1 each level of elevation advantage multiplies the strike by ×(1 ± 0.15), clamped at ±0.45 (3 levels) — the crest occupant dominates the assault from below and its counter-fire runs downhill the same way (symmetric).
  - **Indirect-fire crest factor**: a shell flies a high arc — the ridge never blocks it (no LOS check on fire missions); instead the TARGET'S OWN STEP (the hex on the gun-target line immediately before the target) decides: step higher → reverse slope (defilade, ×0.5 — the shoulder throttles the impact angle, Korean-war reverse-slope defence); step lower → exposed crest (×1.5 — the ridge line is the death line); level → neutral. **Urban targets are always neutral** (a VP city is flat ground for fire support — the valley-town siege must grind on assault, not be throttled).
  - **Direct-fire line**: a flat trajectory (tanks, AT — range > 1, not indirect) cannot shoot over a ridge — an intermediate hex strictly higher than both endpoints makes the shot impossible (reported as `target_lost`; the AI never picks such targets, and indirect fire never goes through this check).
  - The river bed (elevation −1) reads as defilade against bank-side guns — a ford target halves remote fire (netting the −50% exposure to ×0.75), while a melee assault from the bank fires DOWN at ×1.15 on top of the exposure (×1.725): the crossing stays dangerous up close, the far bank stays partly sheltered from long range.

### 6.7 Supply (planned — not yet implemented; revised)

The mechanic below is designed but NOT wired up: the save parser reads
`division.supply_status` into the model, and no logic crate consumes it —
there is no supply buff/debuff, no encirclement supply reduction, and no
out-of-supply penalty in the tactical battle today.

- Strategic-level supply status (from `division.supply_status` in save) provides a global buff/debuff to all battalions of that division.
- Encirclement progressively reduces effective supply:
  - Partial encirclement: -20% supply effectiveness per turn (caps at -60%).
  - Full encirclement: -40% per turn (caps at -100%).
- Zero supply: -25% attack, -35% defense (matching HOI4 out-of-supply penalties).

### 6.8 Abilities & Interactions (revised)

#### Support Companies

Support companies are **attachments**, not map units — a company
attaches to a battalion (same hex, no model of its own) and rides with it
until the host dies (§6.3 Support Companies). Current effects:

| Company | Effect |
|---------|--------|
| Anti-Tank | +4 hard attack, +10 piercing (baked in at attach) |
| Artillery | +5 soft attack (baked in) |
| Anti-Air | +1 soft attack (baked in) |
| Recon | +1 sight range (baked in) |
| Field Hospital | ×0.75 strength damage taken (ongoing) |
| Maintenance | +0.5 strength regen per full turn (ongoing) |
| Signal | rides the division HQ; command aura radius +3 km (3→6) (§6.13) |
| Engineer / Logistics / Military Police | attach the same way; no tactical effect yet (planned — not yet implemented) |

#### Retreat

- **Manual retreat** (player command — the radial menu's "R" button, offered
  only while in contact with an adjacent enemy; otherwise a plain move order
  is the right tool): a fighting withdrawal. Takes a -20% org penalty and
  loses all entrenchment, then auto-withdraws one hex per turn toward the
  own map edge — the player does NOT pick the path.
- **Involuntary retreat** (org reaches 0): unit routs automatically, stepping
  one hex per turn toward its own edge (Attacker → its entry edges, §4.2
  step 9; Defender → the far map edge). Path auto-selected hex-by-hex
  (nearest safe step, respecting stacking and impassable terrain).
- **Pursuit**: retreating units remain valid attack targets — they take damage normally, never counter, and are annihilated when strength reaches 0. (The older "only flanking/fresh attackers can annihilate" rule was dropped with the Lanchester rework, .)
- Units that successfully retreat to a deployment-zone edge hex are removed from the tactical map. "Deployment-zone edge" is implemented literally — the DEFENDER's routs aim at the EASTERN RIM of its own deployment zone (the reachable province boundary; the map edge can lie outside the battle province on stitched maps) and walk the BFS shortest path to it (one hex per turn), so a rout trapped against the province boundary or in a terrain dead-end still exits instead of freezing forever (the old greedy single-step descent froze on score plateaus and in pockets).

#### Hold (Take Cover)

- H = **take cover** (hunker down / use local cover), an infantry-attribute
  action: taking cover **costs the turn's action** and grants **+25% defense**
  (single level, no stacking).
- The stance drops the moment the unit **moves** or **resolves an attack** —
  there is no assault restriction and no move-cost penalty while holding
  (the old "no assault / double move cost" side effects are removed).
  Standing back up (H again) is free.
- Entrenchment layers are a SEPARATE value brought in from the HOI4 save's
  division entrenchment: the save value maps to 0–3 layers (a value ≤ 1 is
  treated as a fraction and scaled ×3; larger values are rounded), each
  layer granting +10% defense. (This is a tactical rescale, not HOI4's own
  numbers — strategic digging is `DIG_IN_FACTOR = 0.02` per level up to
  `UNIT_DIGIN_CAP = 5`; we compress that into the 3-layer cap.)
  Battle-time digging as a deployable map element is a future item.

#### Equipment Loss

- The external program does NOT compute equipment losses — there is no
  loss-factor conversion on our side. At sync time the injection only
  reports per-division **org and strength damage ratios** (`tac_org_dmg` /
  `tac_str_dmg`, §8.2).
- The `damage_units` effect in HOI4 handles equipment deduction automatically from the reported strength damage; any recovery/loss split is HOI4-side.

### 6.9 Stacking (revised)

- **1 battalion per hex** — no exceptions.
- Support companies are NOT units on the map: they attach to a battalion and share its hex (§6.8), so they never affect stacking.

### 6.10 Engagement

- **Manual trigger only.** Adjacent hostile battalions do NOT auto-engage.
- Combat occurs only when a player or AI explicitly issues an Assault or Fire Support command.
- Units can stand adjacent without fighting (tactical maneuver, feint).

### 6.11 Victory Conditions (revised)

Two paths end a battle: the annihilation path (legacy) and the
**flag-capture path**.

- **Annihilation victory** (unchanged): all enemy battalions are off the
  map — eliminated, surrendered (§6.4), or retreated past their edge.
- **Defender victory** (unchanged): all attacker battalions eliminated /
  surrendered / retreated.
- Headquarters units (§6.13) are NOT fighting units — a side with only
  HQs left is beaten.
- Mutual annihilation = no winner — but it is a TERMINAL outcome (the
  battle ends as a draw).
- **Flag-capture victory**: the battle's objective conclusion —
  real sieges end when key points fall (Warsaw capitulated after the
  26 Sep storm took the southern forts, not when every battalion died),
  and the annihilation path alone never concludes a city fight (headless
  Warsaw traces: a hidden city garrison froze the battle for 300+ turns,
  and the post-capture mop-up stalls behind a packed attacker blob).

**Flag zones** — where the flags are:

- City battles: ONE flag = the VP-city urban cluster (map-derived —
  Warsaw 32 hexes, Berlin 62, Stalingrad 38).
- Field battles: THREE flags (trigger model A). The
  positions come from, in order:
  1. the battle script's explicit `flags:` field (data/battles/*.json —
     per-battle historical anchors; the schema grows an optional
     `flags: [{q, r}, …]` per side or battle level);
  2. the fallback: three well-separated anchors sampled from the
     defender deployment zone's interior core (the ~40% of
     hexes with the largest margin to the nearest non-zone cell — the
     attacker zone, the province border / out-of-province ring, water and
     the map rim all count; the old deepest-from-attacker metric parked
     anchors on the province's far border and clipped their clusters into
     edge half-discs).
- A flag ZONE is the hex set within `flag_cluster_radius` = 2 of its
  anchor (≈19 hexes), clipped to the defender's deployment zone and
  passable terrain. Field clusters stay small.
- Large cities ship ONE flag in v1; multiple flags inside one city is a
  possible v2 (e.g. Berlin centre + Reichstag).

**Capture progress** — per flag, a counter 0..`flag_progress_cap` = 12
(12 turns of dominance ≈ 2 strategic hours, DESIGN §8.1):

- Attacker:defender UNIT-COUNT RATIO inside the zone > 2:1 → +1 per turn
  (control ratio, not hex occupancy — the defender
  feeds units into the zone to press the ratio, which is exactly the
  AI's counteraction hook).
- Ratio < 1:2 → −1 per turn.
- In between → contested, unchanged.
- Progress is battle state: checkpoint restore / rollback keeps it.

**Victory trigger** (checked at the end of each turn):

- City battle: the single flag at full progress.
- Field battle: ALL three flags full AT THE SAME TIME (a flag that decays
  back below full blocks the trigger — the attacker must hold the whole
  front of flags, the A model).
- On trigger the ATTACKER WINS **immediately** —
  the original design had the defender collapse into the §6.8 rout flow
  and the battle end only when every routed unit Withdrew/was destroyed;
  that mop-up stalled behind packed attacker blobs, exactly the
  never-converging tail the mechanic was created to kill. The defender's
  org is zeroed — **strength is NEVER touched** (a surrender, not a
  massacre: org 0 + full str is the exact payload of the surrender
  injection) — and there is no tactical rout flow: the battle declares
  victory this turn. In live mode the collapse injects org-zeroing damage
  to the defender's divisions only (`d_tac_collapse`: org ratio 1, str 0)
  — the HOI4 strategic layer resolves the retreat / province outcome
  itself (no province flip from the tactical layer).

**Rules and guards**:

- Flags matter only when they exist: a battle with no flag zones (no VP
  city, no script `flags:`, no fallback anchors) keeps the annihilation
  path as its only conclusion.
- Flag zones are ANCHORS for every defender doctrine (§7.3): withdrawal
  paths never cede a flag while it can still be contested; delay screens
  wrap around them; urban defense anchors by construction (the city IS
  the flag).
- The attacker must physically HOLD the zone (units inside, ratio
  pressed) — surrounding without entry earns no progress (the 8-25 Sep
  state: encircled but not taken).
- The player gets a capture-progress bar plus a "flag falling" warning
  banner — NO dialog, how to react is the player's call. On the MAP every flag zone carries a permanent highlight: translucent AMBER fill plates
  (deeper than the panel gold — gold washed out on plains/urban) that
  leave terrain and fog readable and never cover pieces (they ride the
  ground under the unit models), plus a thick opaque outline per flag
  in panel gold whose emissive sine-pulses on a 4 s period
  (0.15×↔1.8×) — the motion is what draws the eye. The deployment zone
  borders share the same band width and pulse emphasis, in their side
  colors.

### 6.12 Turn Order

- **Attacker moves first** each tactical turn (10 min).
- Attacker completes all actions → Defender completes all actions.
- Turn counter increments after both sides act.

### 6.13 Division Headquarters & Chain of Command

Every division fields exactly one **HQ unit**, synthesized at roster build
time (save mapper, battle scripts and presets alike — never parsed from
save data). The HQ fights with basic self-defense fire only and is fragile
(org 20, strength 3, soft attack 3, defense 6), but it keeps its division
effective:

- **Command aura** (radius `hq_aura_radius` = 3 hexes, same side AND same
  division only): battalions in command get ±10% attack/defense — a LINEAR
  post-step modifier in the §6.3 model, so the effect honestly is
  ±10% — and regenerate `hq_org_regen_frac` = 2% of max org per full turn
  **out of contact only** (an adjacent enemy means the
  unit is fighting, not regrouping — the old unconditional 5% kept
  frontline units' org pinned at full while they took counter-fire, and a
  Warsaw headless attacker ran 300 turns at ~zero net loss).
- **HQ destroyed** (annihilated — strength 0; retreat and surrender do NOT
  count): every surviving same-division battalion immediately loses
  `hq_death_org_frac` = 20% of its max org (standard Break/Surrender rules
  apply at org 0) and the aura is gone for good. The collapse IS reported
  to HOI4 through the sync loop; the HQ's own casualties are NOT (no HOI4
  division maps to it).
- **Chassis follows the template**: any tank battalion → the HQ rides an
  armored car (never a tank), motorized/mechanized majority
  → trucks, otherwise foot.
- **Signal company** (the first real `SupportKind::Signal` effect; it replaces the battalion-relay design): the company rides ON
  THE HQ — signal attachments found on battalions are routed to the
  division HQ at roster build time — and extends the aura radius by
  `hq_signal_radius_bonus` = 3 hexes (3 → 6 km).
- HQs don't count for victory checks (§6.11). The AI does not prioritize
  HQs as targets; its own HQs shadow the division centroid inside the
  aura leash and flee adjacent enemies (§7.3).

**Visualization** (Steel Division style; selection-scoped):
selecting the HQ draws its aura ring (radius reflects the signal bonus)
plus every division lane; selecting a plain battalion draws only its own
lane and no ring. Lanes are thin translucent dark-gold — solid while in
command, DASHED (thinner, shorter period, more transparent) when out of
reach. The OOB division header shows HQ org%, command coverage and the
aura radius; HQ unit labels render in gold.

---

### 6.14 Out-of-Bounds Leaving

The shoreline margin (§4.2 step 3c) made the world beyond the
province visible; this rule makes it *meaningful*. Every out-of-province
cell is flagged `out_of_bounds` (the margin ring, enclaves, foreign
islands):

- **Water stays impassable** (`Terrain::Water`, sea/lake pixels — 海洋格
  不可通行). **Backdrop land is passable at its own terrain cost** (界外陆地格行动力消耗与界内一致) — the ring is real ground a
  unit can march through.
- **Dwell = desertion.** At every full-turn end (`apply_oob_leaving`,
  after the retreat steps) a unit standing on an out-of-bounds hex accrues
  one dwell turn (`oob_turns`); ending a turn back in bounds resets the
  count — passing through the ring is free, only consecutive linger
  counts. At `oob_leaving_turns` = 6 full turns (= 1 strategic hour, §8.1)
  the unit **leaves the battle**: `UnitState::LeftBattle`, org 0, strength
  FROZEN (it slipped away intact — not annihilated), removed from the
  board (OFFBOARD), uncommandable, ignored by the AI, resolved for victory
  (§6.11). Active, Retreating and Withdrawn units all dwell — so remnants
  parked on a map-edge column slowly dissolve instead of clogging the
  corridor forever. The wiped org ratio rides the sync damage channel
  (`record_damage(side, org_frac, 0.0)`); the strength is never reported
  as a loss.
- **Pathfinding soft-avoids the ring**: stepping into an out-of-bounds hex
  pays `oob_step_penalty_km` = 40 effective km on top of terrain (execution
  itself is unchanged — §6.2 hours), so planned routes (AI approaches,
  player standing orders) detour around the ring instead of wandering off
  the map; 40 km makes even a full 7-hex ring shortcut uneconomical. The
  AI never *plans* through out-of-bounds ground.
- **Unless it is losing.** Rout BFS ignores costs, and its preferred exit
  is the out-of-bounds ring itself (劣势避战主动撤出边界):
  a broken unit flees INTO the ring and holds there while the dwell
  counter walks it out — escapable prey for `oob_leaving_turns` turns
  (Retreating units stay targetable, never counter), then gone for good.
  The §6.8 deployment-edge Withdrawn remains the fallback for water-locked
  maps where no ring land is reachable.
- **UI honesty**: the hex hover card shows a prominent red IMPASSABLE
  line on water and a gold OUT-OF-BOUNDS warning (with the dwell limit) on
  backdrop land; a unit's hover card shows the dwell countdown
  (`battle.hover.oob_countdown`) once `oob_turns > 0`; every departure is
  battle-logged (`log.oob.left`, Door icon). Backdrop land renders in the
  same grey-washed style as before (keyed on `out_of_bounds` now, not
  passability) — passable but visually "not the battlefield".

---

## 7. AI Engine

### 7.1 Three-Layer Decision Architecture

```
Layer 1: Strategic Objective Selection
  └─ Evaluate global situation (force ratio, terrain, objectives)
  └─ Select high-level goal (push_center / hold / flank_left / flank_right / delay)

Layer 2: Tactical Unit Assignment
  └─ Assign each battalion to a role (assault_unit / support_fire / hold_position / reserve)
  └─ Match battalion types to roles (tanks → assault, artillery → support, etc.)

Layer 3: Action Execution
  └─ Per assigned battalion, select specific hex target and command
  └─ Evaluate: is target reachable? is it in sight/range? is the risk acceptable?
```

### 7.2 Tactic → Strategic Objective Mapping (revised)

The card library grew from 9 to **16**: every one of
the 55 vanilla HOI4 tactic tokens single-maps onto a card — phase-variant
rolls (close_combat / tw_* / bridge / street-fighting) fold into their
parent card's behavior, masterful variants fold into their base. Cards are
NOT side-locked: any card may be assigned to either side (headless
`atk_tactic=`/`def_tactic=`, scripts, debug form); the side column below is
the HOI4 bloodline only. `CombatTactic::from_str` is the single mapping
point (`tactic.rs`); unknown tokens fall back to `Default`.
Vanilla attacker-only rolls (`cc_withdraw` / `sf_storm` `sf_barrage`
`sf_armor_supported_assault` `sf_mouse_holing`) never fold onto the
defender card of the same family — the live enemy AI plays the attacked
side's real roll, and a garrison/rearward card would paralyze it.

| Card | HOI4 tokens (bloodline) | Side | Primary Objective | Behavior |
|------|-------------------------|:---:|------|-----------|
| `blitz` | `blitz` `masterful_blitz` `breakthrough` | atk | **Deep penetration** | Tank units rush forward on narrow front, bypass enemy strongpoints. Motorized follows to widen breach. Aggressive push. |
| `assault` | `assault` `planned_attack` `relentless_assault` `unexpected_thrust` `barrage` `cc_attack` `cc_defend` `cc_withdraw` `tw_attack` `tw_chase` `tw_intercept` `sf_storm` `sf_barrage` `sf_armor_supported_assault` `sf_mouse_holing` | atk | **Push center** | Standard assault: artillery fire-preparation concentrates on one hex, the infantry line advances in step, assaults on contact. The preparation hex is the assault pool's breach point — each battery ranks every in-range enemy by its would-be pool volley (the pool math over every adjacent assault-capable friend, planning-order-independent) so fire and melee converge on the same hex (the unified fire phase resolves both pools the same end-of-turn: stacked org collapse, not sequential softening); the globally weakest hex remains the no-pool fallback. |
| `encirclement` | `encirclement` | atk | **Pincer movement** | Tank units swing to extreme flanks. Infantry pins center. Coordinated 2-direction encirclement attempt. |
| `mass_charge` | `banzai_charge` `grand_banzai_charge` `human_wave_tactics` `infantry_charge` `shock` `cc_storm` | atk | **Full frontal assault** | All infantry advances simultaneously — an unconditional 1-hex push per turn toward the nearest enemy (or the objective when blind). High casualty tolerance. |
| `infiltration_assault` | (custom card, no vanilla token) | atk | **Exploit gaps** | Recon units probe far flanks. Infantry concentrates on weakly-held hexes. Avoid frontal assaults on strong positions. |
| `seize_bridge` (RiverAssault) | `seize_bridge` `attacker_sb_*` `attacker_hb_*` | atk | **Push center** | River crossing: artillery pounds the ford, infantry forces the crossing accepting the ×2 river damage, then holds the far bank. |
| `elastic_defense` | `elastic_defense` | def | **Delay & preserve** | Fall back 1 hex when attacked. Counter-attack only isolated enemy units. Prioritize unit survival. |
| `delay` | `delay` `masterful_delay` | def | **Delay** | Mobile delay: keeps the enemy in a 2–3 hex contact band with constant fire and stepwise resistance — never breaks contact (vs tactical_withdrawal's full rearward shift). |
| `counterattack` | `counterattack` `backhand_blow` | def | **Delay** | Counter-offensive: holds the line like Default, but strikes hard at an isolated OR beaten (<50% org) target and keeps the ground — unlike elastic, which strikes and falls back. |
| `ambush` | `ambush` `cc_local_strong_point` | def | **Hold** | Ambush: never moves, lurks in cover (deployment weights cover ×3), strikes only an enemy that steps adjacent; no cover-shuffling under shelling. |
| `hold_bridge` (RiverDefense) | `hold_bridge` `defender_sb_*` `defender_hb_*` | def | **Delay** | River line: holds the bank at all costs — no retreat ever, half-forded enemies struck (deployment hugs the bank, shield bonus doubled). |
| `urban_defense` | `urban_defense` `sf_defense` `sf_fortify` `sf_ambush` | def | **Hold** | Street fighting: the WHOLE line garrisons the city, never leaves it, fights enemies that enter. |
| `overwhelming_fire` | `overwhelming_fire` | def | **Attrition warfare** | Concentrate all artillery on single weakest hex. Infantry holds line. Cycle damaged units to rear. |
| `guerrilla_tactics` | `guerrilla_tactics` | def | **Hit and run** | Alternates across turns: strikes when adjacent, otherwise maneuvers while staying out of contact. Never end turn adjacent to enemy. |
| `tactical_withdrawal` | `tactical_withdrawal` `tw_defend` `tw_evade` | def | **Systematic retreat** | All units fall back 1 hex per turn toward rear. A single rearguard unit holds the front. Hold at narrow terrain. |
| `default` | `basic_attack` `basic_defend` + unknown | both | **Hold & engage nearest (def) / plain advance (atk)** | Defender: hold, engage the closest enemy, rotate damaged units. Attacker: the planner lifts the no-doctrine card onto the plain-advance posture (`TacticalAi::new`) — march on the objective, assault on contact — so a generic vanilla attack roll never parks at the deployment. |

The counter hints on the Tactic Card follow `combat_tactics.json`:
counterattack counters assault; ambush counters shock; delay is countered
by shock; river defense counters river assault; etc. (`tactic.rs
counter_hint`). Masterful variants currently fold into their base card —
a future "leader skill → AI aggression" parameter may separate them.

### 7.3 AI Constraint Rules (revised)

- Damaged units (<30% org) withdrawn from frontline whenever possible.
- Outnumbered 3:1 locally → refuse assault, go defensive.
- Artillery stays 1-2 hexes behind frontline.
- **Defender fire bases**: a DEFENDING towed gun with no enemy in its envelope emplaces IN PLACE instead of creeping after far-away visible enemies — on a sparse long front the creep goal drifted every turn, the re-issued order reset the march hours, and the battery never emplaced nor fired all battle (Warsaw trace: guns 20+ hexes from the nearest visible enemy). The enemy comes to a defender; only the ATTACKER creeps its guns forward (§7.3) and limbers to follow the advance.
- **Blind bombardment**: an emplaced attacker battery with no visible target shells its intel goal (the besieged city / the enemy deployment zone) when in range — a siege must bleed, and before this the Warsaw garrison hid behind fog + urban LoS while NOTHING fired (300-turn staring match). The attacker gun also stays emplaced instead of limbering away while the intel zone is in range.
- **Storm rule**: the besieger may assault an urban garrison beaten below 40% org or locally outnumbered ≥3:1 despite the hopeless-trade math AND the local-odds gate — a broken garrison's packed neighbors no longer protect it (the 26 Sep Warsaw general assault after days of bombardment).
- **Blind-march gate**: the attacker's hold-role blind march keys on "no COMBAT-EFFECTIVE enemy visible" — a RETREATING unit in view previously filled the enemy list and parked the whole force at the city edge forever.
- **Intel tie-break toward the zone centre**: among equally-near intel-ring hexes the blind march prefers the one closest to the deployment-zone centroid — the old (q, r) tie-break drifted the sweep toward the zone's NW corner and a besieging force wheeled PAST the city (Warsaw: front parked at q16-18 for 150 turns while the city sat at q30-35).
- **Flag anchoring**: every defender doctrine treats the battle's flag zones (§6.11) as anchors — fall-back paths keep a flag contested as long as possible (ceding a holdable flag is a lost battle; an in-zone defender at ≥1/3 capture progress holds instead of falling back — tactical_withdrawal exempt as the deliberate abandon card), delay screens wrap the zone, urban defense anchors by construction (the city IS the flag). The ATTACKER's objective layer treats flag anchors as primary blind-march / fire goals when flags exist — the siege doctrine already converges on the city; field flags generalize it to open ground.
- **Flag-defense response tiers**: the defender AI scales its reaction to the per-flag capture progress — <1/3: normal doctrine; 1/3–2/3: reinforce the threatened flag (second-line units reroute, reusing the screen_threat primitive); >2/3: nearby reserve/line units counterattack the flag zone to press the control ratio; a flag falls → the attacker wins at once (§6.11: org zeroed, no mop-up). Threat ordering across several flags is by progress (highest first), never a flat split — the highest-progress flag claims its progress-proportional share of the nearest out-of-contact line units first.
- Support companies are attachments riding with their host battalion, so they need no positioning rule. (The old "stay near the unit they buff" follow logic remains in the AI code only for legacy map-unit companies, which neither the live/save path nor the demo scenarios produce anymore.)
- Units that already acted this turn → Hold.
- **Fog-limited planning**: the AI plans against its own fog view — hidden enemies neither block its routes nor draw its fire. Execution stays omniscient (contact/interception are physical).
- **Blind-man's objective** (revised): with no enemy visible, the side marches on pre-battle intel — the enemy DEPLOYMENT ZONE: each unit aims at its NEAREST zone hex (never its own hex), so a long stitched front advances along its full width. A single centroid point collapses a big front into a blob (Warsaw trace: the origin strip's centroid sat 25 hexes off the border). A defending AI holds. Without intel a fog-limited planner freezes in place.
- **Hold-role blind march**: hold-role units (blitz shoulders, overwhelming-fire line, etc.) on an ATTACKING side also blind-march on the intel when NO enemy is visible — a parked shoulder line seals the vanguard in with friendly-packed hexes and pathing deadlocks (tanks stuck at the frontier, Warsaw trace). With the enemy in sight the §7.2 doctrine stands (hold; assault-role units and artillery carry the fight).
- **Expected-value fire targeting**: artillery ranks targets by estimated org damage (the shared §6.3 formula — hit step, precision factor, piercing tier in q) — batteries no longer waste missions on tanks they cannot scratch.
- **Hopeless-trade refusal**: an assault is refused when expected counter-damage exceeds 1.5× expected damage dealt, unless the strike is expected to break the target outright. The dealt estimate is pool-aware — the unit evaluates the volley it JOINS (itself plus every adjacent assault-capable friend, P = Σ(q·g)×Σg through the shared core `strike_group_org_estimate`, the counter total stays ~constant split across the pool) — and target selection prefers the most-joinable victim, so independent planners converge on the same pool instead of fragmenting.
- **Elastic reinforcement**: elastic-defense units with no contact but an enemy within 3 hexes step toward it, stopping at distance 2 (second line, ready to counter-punch).
- **Counterattack reserve closes up**: a counterattack battalion out of contact with an enemy within 8 hexes approaches to standoff 2 (the same screen_threat primitive, longer reach) — the deployment's reserve echelon actually reaches the line before the strike window opens, so the counter-punch starts from the reserve, not from a static backfield.
- **Order persistence**: a re-affirmed same-destination move order keeps the WHOLE standing order — path and invested travel hours (the planner's fresh recomputation is advisory only; `refresh_move_order` heals the standing path each turn under anti-oscillation rules). Objective assignment is hysteretic too: a unit keeps its standing destination while it stays inside the division order's objective area and legal, so per-turn reservation churn cannot flap destinations. Any flip/reset pins units slower than 1 hex/turn.
- **Terrain-aware valuation**: the AI's org-damage estimate mirrors the strike() terrain column for the target's hex — defense modifier in the denominator, attack modifier and cover as linear terms, river ×2 — so it loves hitting units caught mid-ford (both assault and artillery target selection) and the counter-attack mirror refuses assaults from inside a ford.
- **River discipline**: retreats and cover-seeking never step INTO a river hex (ties broken by defensive terrain); damaged-unit withdrawals and artillery break-contact steps pay a ford penalty equal to one hex of distance.
- **Screen, never cross**: elastic reinforcement forms its second line on the threat's standoff ring on its OWN side of any river — shielded ring hexes first, any river-fording route rejected outright; when the shielded ring is already manned by the front line the backfield holds (manned-ring rule).
- **Terrain deployment scoring** (revised): the AI deployment front line picks hexes by score — frontage (LINEAR distance to the inter-zone boundary, not the enemy centroid — the centroid anchor collapses a long strip's front into a point) + defensive terrain + a large "river shield" bonus when a river runs between the hex and the enemy — ending back-to-the-river deployments. The defender lays a band ~3 hexes off the boundary (not the razor edge) and garnisons the VP city by QUOTA: a third of the line battalions (militia/remnant types, weakest first, at most ⅓ of any one division — a whole weak division must not be gutted) deploy inside the city nearest-first around its centroid.
- **Division sectors**: pass 1 partitions the front band (the zone's own edge + the profile standoff, ±1.5) into contiguous ARC sectors — the band's PCA principal axis gives the arc coordinate — one sector per division with line troops, width proportional to its line-unit count, tiled in roster order; each division places its battalions inside its own sector (plus a 2-arc slack for terrain) with the k-th unit aiming at the k-th equal slice of the sector (slot tie-break) — the old cohesion pull is gone: the sector IS the cohesion, and divisions hold contiguous line segments instead of blobs/columns (Warsaw 3544 trace: 46./10./19. ID stacked into one 3-wide column; the first unit's (q,r) tie-break parked whole formations at one end of the front). `ai_deploy_impl`'s `sector_divisions` lets callers deploying division-by-division (allied contingents, headless splits) pass the shared order so every call lands in ITS slice.
- **Support band**: AT/AA/artillery deploy in a support band BEHIND the line, anchored to the placed line's own distance to the enemy zone (`front_of`) — never to the division centroid (a centroid-relative ring let guns land ON the razor edge or chain into deep columns). Standoff per class: AT = line + 2 (the infantry's second line — the classic PAK siting covering approach lanes), AA = line + 3 (divisional flak protects the guns/command positions; the umbrella radius 3 still covers the line), artillery = line + clamp(range/2, 1, 4). **AA (attack range 1) is NOT a pass-1 line unit** — a flak battery never holds the razor edge; the city-garrison quota still takes it into the city (flak over the town). Support units stay inside their own division's sector (the sector partition counts ALL deployable units, so an army-level AT/AA group with no line troops owns a slice instead of falling back to the global front centroid) and spread along the band by the crowd penalty + slot tie-break; a degenerate zone (band beyond the zone) falls back to the sector's nearest ground rather than leaving guns OFFBOARD. **The DEFENDER's slot is instead anchored to its own line's arc CENTRE** (the likeliest contact axis), the k-th gun offsetting ±2.5-hex steps around it, sector-bounded — the defender is a fire base whose stand IS its coverage, and on wide sparse fronts the even spread parked guns a screen's width off the actual engagement (arena pz_at: the howitzer sat 19 hexes from the panzer lane and never fired). The ATTACKER keeps the even spread: its guns creep toward the nearest enemy and pair with their local line segment — clustering converged every tube on one flapping creep goal and they never settled (arena arty_inf: attacker fire 111→0).
- **Reverse-slope siting + crest observation posts** (plan 2): the deployment is elevation-aware, mirroring the §6.6 crest rules. (a) A hex whose OWN STEP toward the enemy (the first intermediate on the enemy-centroid line) stands strictly higher is DEFILADED from indirect fire (×0.5) — the line and the guns prefer it (REVERSE_SLOPE_W ≈ 45, the "照抄河盾" term of the plan): the defender holds the military crest's reverse slope while the attacker's jump-off and the support band sit behind ridges too; the DEEP_W penalty still caps the pull. (b) The DEFENDER posts a THIN observation screen — 1 post per 8 line units, weakest first (OBSERVER_QUOTA_DIV) — re-placed onto CREST hexes (elevation ≥ 1 and no sampled intermediate hex toward the enemy stands higher): the ridge rule gives the post unobstructed sight over the approach (peaks see over saddles), and since the side's fog view is shared the post extends the whole side's spotting — the "峰线薄观察哨" of the plan. Posts stay in their division's sector, hug the front, and accept the exposed-crest ×1.5 as the historical observer's trade; no crest near the front → the unit keeps its line spot. Both predicates reuse the coarse enemy-centroid line sampling of the river-shield rule.
- **Tactic profiles**: the deployment echoes the §7.2 card — each card's profile sets the echelon split (a front rank at the band plus reserve bands BEHIND it: elastic 65/35 with armor in reserve, counterattack 60/40, tactical_withdrawal 35/35/30 fallback lines, delay 50/50, overwhelming_fire 85/15, assault/default 75/25 waves, mass_charge 100% on the razor) and the horizontal shape (blitz CenterFocus: only the middle ~70% of the band is used — the axis concentration; encirclement TwoWings; ambush/guerrilla a deeper band + loose spacing). The distance penalty steepens beyond the band (+1.5 hexes, −DEEP_W=12/hex, waived for loose cards) so a river shield (+80) or city bonus can still draw the line a few hexes deeper but bare terrain cannot (an attacker's deep urban hex must never beat the razor edge).
- **HQ handling** (§6.13): HQs never attack, shadow their division's living-members centroid at standoff 2, stay inside the aura leash and flee adjacent enemies; AI deployment parks them behind the line. Target selection does not prioritize enemy HQs.

### 7.4 Division Orders

The player commands each battalion by hand (§9.3) — manageable for a demo
roster, tedious for a full scripted battle (1939_warsaw: ~75 attacker
battalions). **Division orders** let the player command one division with a
single intent; the division AI (the same `TacticalAi` that runs the enemy)
fills in the battalion-level orders, and the player's manual commands
always win.

**Order set** (issued by selecting the division HQ → the horizontal
square-button bar above it — deliberately distinct from the battalion
letter ring):

| Order | Target | Behavior | End |
|-------|--------|----------|-----|
| **推进** Advance | none | The attack-side doctrine: push, assault on contact, artillery support; blind march on the pre-battle intel / flag zones when nothing is visible (索敌行军). | Cancel / no units |
| **占领** Seize | one hex (map pick) | March straight on the hex (fighting whatever blocks the path); artillery prefers the point's defenders; on occupying the hex → "已占领" report and the order flips to the **hold-back phase** — the division defends the point with the elastic-defense card (occupy terrain, emplace guns, do not advance) until cancelled. An enemy re-taking the hex flips it back to the maneuver phase (目标被夺回). | Cancel / no units |
| **歼敌** Engage | one visible enemy battalion (map pick) | Pursue the target through its last known position (updated only while visible — fog-honest), assault the target itself when adjacent (never traded for a weaker neighbour), fire missions weight it. | Target eliminated / routed / retreated |

**Lifecycle**: an order persists until its goal is reached (Seize), the
target is gone (Engage), the division has no combat-effective units left
(无兵力 — all eliminated/surrendered/withdrawn), or the player cancels it
(the HQ bar's [取消], or the OOB division header's ✕ — the OOB cancel works
even after the HQ fell). At the START of every player turn (and immediately
on issue) the game plans each ordered division once: per-division slice of
`plan_turn_div_order` against the PLAYER's fog view, registered as standing
orders through the same executor as the enemy AI. Division orders are
per-division by design — no cross-division coordination (hex congestion is
resolved by the existing §6.2 wait/detour rule); the flag-tier response is
skipped (an explicit player command outranks the flag doctrine, though the
flag zones still anchor the blind march).

**Manual override (微操排他)**: any player command on a battalion
(move / attack / Hold / emplace / retreat) sets `manual_override`; the
division AI leaves that unit alone until the player's command completes —
a march ends when it arrives or is consumed (contact stop), an attack
resolves in the fire phase (1-turn protection; re-issue to continue), Hold
and an emplaced gun are open-ended (until stand by), and right-clicking
the unit (stand by) releases it immediately. `refresh_turn` clears the
override when the command is done.

**Division sensor radius** (`div_sensor_radius` = 10): a division-order plan reacts only to enemies within 10 hexes of
its own battalions — an Advance pushes and searches its OWN front instead
of being pulled across the map by enemies another division detected.
Applies to ALL three order kinds (no front/rear direction split). Far
enemies stay pathfinding obstacles (their hexes still block routes); they
simply never become targets.

**Objective-area spread**: MOVEMENT goals
under a Seize/Engage order aim at the objective AREA — the point plus its
ring-1/ring-2 hexes (ring 1 for an Engage pursuit) — not the single goal
hex: a whole division routing to one hex queued into a single corridor and
stacked on one city corner. Each battalion picks the nearest passable,
unoccupied, unreserved hex of the area (river hexes are never chosen
destinations); the point itself keeps a 3-hex preference so the nearest
battalion marches ONTO it — someone must occupy the hex to declare it
seized. Fire missions and blind bombardment still converge on the exact
point (`order_fire_goal`). In the Seize hold-back phase the division's
towed guns emplace in place as a fire base (the elastic-defense card)
instead of creeping after the point — the guns defend the seizure.

**Fight-through**: a commanded division
fights whatever stands in its way — an adjacent combat-effective enemy is
assaultable despite the trade/odds doctrine gates. A refused assault left
commanded battalions pinned: every step of a march stayed inside the
§6.5 contact ring (the order is consumed at the first step) and the
enemy's hex blocked the route, so a 贴脸 battalion neither fought nor
advanced. Applies to all three order kinds (the Engage quarry itself
included); the whole-side AI keeps the conservative doctrine (`order` is
None there) — fight-through is the player's explicit command, not new
attacker policy.

**Determinism**: per-division planners derive their seeds from the battle
seed ⊕ division hash ⊕ turn — replays (checkpoints) are
deterministic, tie-breaks vary across turns. Orders live in `GameController`
and are restored by checkpoint snapshots.

### 7.5 Allied AI — Passive Friendlies

In a multi-nation force the player's side may be planned by SEVERAL
commanders: the player commands her own divisions while each allied AI
nation runs its own `TacticalAi` over a SLICE of the side (its national
divisions — per-nation planners with no cross-nation coordination; the
Narvik Allies coordinated poorly, and that is the flavor). For each allied
planner the rest of the side — player-commanded units and the other allied
nations' slices — are **passive friendlies**
(`plan_turn_flags(..., passive_friendlies)` in `tactical-ai/src/planner.rs`):

- **Occupancy**: passive friendlies join the planner's occupancy view
  (`ctx.all`), so they block pathing, destination booking, and ZOC tables
  like any unit — §6.9 one-battalion-per-hex holds across command
  boundaries.
- **Statistics**: they count as nearby friends wherever the planner asks
  "how many friends are near X" — the §7.3 local 3:1 odds gate, the storm
  rule's adjacent-friend count, the §7.1 global force-ratio downgrade (a
  weak allied slice propped up by a strong player force does not refuse
  the battle), rocket blast-safety (an allied battalion in the blast is a
  friendly casualty all the same), and the own–enemy axis for flanking
  waypoints.
- **No command**: they never receive actions — every assignment site (the
  main planning loop, the flag-defense pool, the rearguard pick, role
  assignment, attachment/HQ anchors) iterates the planner's own slice
  only, and no returned `AiAction` ever carries a passive unit's id.

The whole-side enemy AI and the headless evaluator pass `None` (no passive
friendlies, behavior unchanged).

**Split-command pipeline (P1):**

- **Script declaration** (`tactical3d-bin/src/script.rs`): a side may carry
  a `divisions:` block — `{name, tag?, control: player|ai, tactic?}` per
  division. Absent block = every division player-commanded under the side
  tag (all 18 legacy scripts). Absent tactic = side default (attacker →
  `assault`, defender → `elastic_defense`). Validated: known division
  names only, no duplicates, tactic tokens from the whitelist.
- **Contingents**: the player side's `ai` divisions group by resolved tag
  into `AllyContingent`s (first appearance order; the first division of a
  tag carries the contingent's tactic — one nation, one planner, one
  card). `GameController.allies` + `division_tags` hold them; the single
  commandability predicate is `GameController::commands(unit)` =
  `side == player_side && !allied` — with no contingents it degenerates to
  the old side check, so unscripted battles are byte-identical.
- **Deployment**: allied divisions wait OFFBOARD (they never block the
  Begin Battle button). The player may drag a deployment SUGGESTION
  rectangle per allied division (the sector flow storing
  `allied_sectors` instead of deploying — suggestion + auto combined); at BeginBattle each nation deploys its
  divisions via `ai_deploy` (suggestion rect ∩ zone ∩ deployable, else the
  full zone), accumulating placed hexes into `pre_used` so later nations
  avoid crowded sectors.
- **Turn planning**: `plan_allied_nations` runs at every player-turn start
  right after `plan_division_orders` (same once-per-turn latch and
  battle-tour gate): per contingent, its slice (divisions minus any under
  a player-issued division order), the PLAYER's fog view, the enemy
  deployment zone as blind-march intel, a per-nation deterministic seed
  (FNV over tag ^ turn, the division-order mix), actions applied silently
  (trailing `EndTurn` dropped — the player's turn belongs to the player).
- **Player orders to allied HQs** (B-scheme): selecting an allied
  HQ opens the division command bar (with an ally badge); Advance/Seize/
  Engage issue as normal — the ordered division is suspended from its
  nation's slice and planned by the division-order path until the order ends or
  is cancelled. Allied battalions are selectable for info only (no radial
  ring, no right-click orders, no hand deployment).
- **Side-granularity systems are untouched**: fog is shared across the
  side (allied units spot for the player), flag capture and victory count
  the whole side, command auras stay per-division. UI: OOB groups the
  player side into own divisions + per-contingent sections (raw tag
  labels); stored suggestion rects render as a steel-blue overlay during
  Deployment.
- First battle: `1940_narvik` — the player commands the French CEFS
  contingent (11 battalions); ENG (24th Guards Bde + 51st Fd Regt), POL
  (SBSP) and NOR (6. Divisjon) divisions are allied AI (`assault`).

**P2:**

- **Menu nation selector** (Debug Battle → Script file): the script's
  player-side `divisions:` block is loaded in the menu and offered as one
  player/allied-AI radio row per resolved country tag. The selection is
  serialized as repeatable `div_control=TAG:player|ai` CLI args
  (`Scenario.div_control`, rides along with `file=` like `side=`/`seed=`),
  applied by `assemble` onto the player side's `ScriptSide` BEFORE the tag
  table and contingent grouping (`script::apply_control_overrides`), so the
  command split, the colors and the OOB stay consistent. Empty override =
  the script file is the single source of truth (all legacy runs).
- **Per-tag base-plate colors**: `BattalionUnit.tag` (country tag, stamped
  at assembly from the division→tag table; empty for non-script battles),
  a `TagColors` render resource (tag → HOI4 country color, filled by the
  bin crate from `country_colors.json`), and a mesh cache keyed
  `(ModelFamily, Side, GunState, tag)` — each nation's plates get their own
  country color, side color for untagged units.
- **Headless split evaluation**: `--headless` now honors the script's
  command split — the player side runs one driver per command slice (the
  player proxy + one per allied contingent), each planning its own slice
  with the rest of the side as passive friendlies (the interactive
  `plan_allied_nations` in miniature), deployed per division through its
  own tactic card with `pre_used` accumulation; the side marches + fires
  once per turn like the interactive loop. Narvik battery (10 seeds × 288,
  release): split-command attacker **10/10 flag-capture wins T173–256**
  (mean ~200) — the whole-side baseline (all `div_control=…:player`) is
  9/10 wins T194–269 (mean ~230, one known stall class), so the
  uncoordinated per-nation planners cost nothing in balance.

P3 (live-mode multi-tag interface, symmetric damage writeback) is pending.
Live interim behavior: NO
allied-AI split — every co-belligerent division on the player's side
assembles under the player's direct command (whole-side single planner),
each battalion keeping its owning country's national modifiers and
base-plate color; the damage write-back splits one `damage_units` line
per participating tag (§3.2), so allied losses book to the allied
country's own divisions.

---

## 8. Time System

### 8.1 Tactical ↔ Strategic Conversion

```
1 tactical turn  = 10 minutes of battle time
6 tactical turns = 1 strategic hour
1 sync point     = every 6 turns → console injection
```

Battle clock display: live battles start from the save's `date`
header and show absolute in-game time ("YYYY-MM-DD HH:MM", Gregorian,
leap-aware — start + elapsed battle minutes); demo/script battles keep the
elapsed-only "HH:MM" clock.

### 8.2 Sync Loop

```
Player plays turns 1-6 → clicks [Sync]
  → inject damage for hour 1
  → player continues turns 7-12 → clicks [Sync]
  → inject damage for hour 2
  → ... until battle resolves
```

**Post-sync battle-alive check:** injected org
damage can rout every division of a side before the tactical battle
resolves — vanilla forces 0-org divisions out of combat (engine rule;
in-combat divisions never regenerate org), so the mirrored HOI4 battle
ends early and the tactical one would grind on against ghosts. Because
vanilla damage is frozen (§8.4) and injected damage only lands at syncs,
the HOI4-side ending can only happen **at a sync boundary** — so after
every sync's clock receipt the battle child injects `savegame tac_check`
and scans the fresh save (tactical-sync `savecheck`): the contested
province's `land_combat` still listed → fight on; gone → resolve the
tactical battle with the strategic winner (a defender-tag division
HOLDING the province — its record carries no `retreat=yes`, the only
rout marker; marching divisions show `movement_progress`/`path` but
still hold — means the attack was repelled; defenders all routing or
gone means the province fell — a rout is a slow move, not a teleport,
and 1.19.2 saves carry no per-province controller field).
Unreadable/incomplete saves fail open ("still alive"). The tactical
session records the strategic winner (`resolve_externally`) and the
normal Apply&Exit two-phase end batch rides unchanged. Cost: one forced
save + a dumb text scan per sync (~5-10 s added to the receipt wait).

**Desync guard:** the whole post-sync pipeline
stands on one assumption — the `tac_check` snapshot is the same strategic
world this battle was assembled from. If the player loads an earlier or
wrong save, switches the played country, or unpauses the game mid-battle,
that assumption breaks silently: the alive check would misjudge endings
and the damage lines would book against the wrong world. Two probes guard
every hourly sync, each checked BEFORE the alive check (tactical-sync
`desync.rs`): the clock receipt must read EXACTLY one game hour after the
session's last confirmed clock hour (never a fresh pre-batch
probe — a drifted clock would validate its own drift; a manual unpause
runs the clock ahead; a loaded earlier save runs it backward; Clausewitz
hours run 1..24) and the snapshot's root `player="TAG"` must still be the
battle's country. A
mismatch suspends the flow behind a modal: END BATTLE queues the abort
cleanup batch (unfreeze + flags, no damage — a wrong-world target turns
it into a no-op, a same-save reload still needs it) and CONTINUE
UNSYNCED disables the sync pipeline for the rest of the battle (hours
seal locally, every exit then sends the same cleanup-only batch).
Unparsable clocks / missing tags fail open — the guard never bricks a
battle it cannot read. Restarting HOI4 between battles (nothing live) is
normal usage and unaffected.

**Mid-battle division roster:** the `land_combat`
unit-id lists are not static across a long battle — divisions rout out
one by one under the injected org grind, and reinforcements join
mid-battle (a new attack direction, a defender marching into the
contested province). The tactical board tracks both flows or it drifts
from the HOI4 truth the writeback books against (a division gone HOI4-side
would keep fighting — and counting for victory — as a ghost; a HOI4-side
reinforcement would fight with no mirror). So the session carries a
**roster** (`tactical-sync/src/roster.rs`): one entry per participating
division (`id → {side, tag, name, battalion ids, org pool, max str,
province}`), seeded at assembly, plus a last-seen-province watch table
covering EVERY division in the save (a joiner is by definition not yet a
participant) and the contested province's neighbour→direction table. The
same `tac_check` snapshot that feeds the battle-alive check is then
parsed, and while the HOI4 battle is alive the roster diffs against the
fresh unit-id lists:

* **Departures** (routed out / manually retreated HOI4-side): every
  battalion of the division marches off — `UnitState::LeftBattle` +
  OFFBOARD, the §6.14 terminal state, so victory bookkeeping, combat and
  AI scans need no special-casing (the two roster-closed victory holes:
  an early-routed division that "already arrived" no longer counts; a
  reinforcing division that held now fights).
* **Joiners** (reinforcements): assembled from the fresh save through the
  normal division→battalions pipeline and placed at the map edge in their
  approach direction (no mid-battle deployment phase — a
  battle-start zone may be a combat zone by now). The approach bearing is
  the attacker's current source province, or the defender's last-seen
  province from the previous sync; unknown bearings fall back to the
  side's deployment zone, then any free deployable hex.
* **Damage-pool bases** (the damage-pool framework) re-derive from the
  updated roster every sync, so the next hour's writeback books against
  the real OOB.

Every change is announced (per-division battle-log line + summary notice
flash, both localized; raw join/leave lines in fc_inject.log), and the
sync checkpoint is re-taken with the roster change applied — the roster
IS the HOI4 ground truth and must not rewind (a Restart re-diffs and
self-heals at the next sync). The headless `--livebattle` path does not
run roster maintenance (self-play tooling without the injection drain).

### 8.3 Phase Flow (revised)

The `tactical-sync` state machine has **9 phases** — the 8 below plus
**ENDED**, the terminal state. One `BattleSession` is exactly one battle,
so the old "loop back to WAITING_FOR_TRIGGER" is represented as ENDED;
the menu's live listen then starts a fresh session for the next battle.

```
WAITING_FOR_TRIGGER
    │ (tac_start detected in game.log)
    ▼
LAUNCHING
    │ (parse save, generate map, init units)
    ▼
DEPLOYMENT
    │ (player adjusts positions, clicks "Begin Battle")
    ▼
TACTICAL_ACTIVE
    │ (player plays turns, AI responds)
    │ every 6 turns:
    ├── READY_TO_SYNC → player clicks [Sync]
    │   ├── SYNCHRONIZING → inject → TACTICAL_ACTIVE
    │   │   (post-sync save check: HOI4-side battle already over →
    │   │    READY_TO_END with the strategic winner, §8.2)
    └── battle resolves:
        ├── READY_TO_END → player clicks [Apply & Exit]
        │   ├── INJECTING_FINAL → inject final results → ENDED
        └── (or player aborts → tac_abort written to log → ENDED;
            abort is legal from any phase, unsynced damage is lost)
```

### 8.4 Strategic Clock Advance via Console

Each sync point moves the strategic clock forward exactly one hour (§8.1).
Vanilla HOI4 has no direct time-set/jump command — the only time-related
console commands (verified against the game's own
`documentation/console_commands_documentation.md`) are `time` (read-only),
`fast_forward <days>` (day granularity — unsuitable for the hourly sync),
`gamespeed <0-5>`, and `pause_in_hours <X>`. The implemented mechanism is
the single command (it switches to gamespeed 5 by itself,
runs exactly one game hour — console message "Game will pause after 1
hours" — then auto-pauses):

```
pause_in_hours 1
```

While the clock runs, the battle state is FROZEN: the mod's dynamic
modifier `tac_freeze` (`army_attack_factor = -10` + the air factor pair,
`attacker_modifier = yes` so it reads for everyone engaging there — the
vanilla `unplanned_offensive` construction) keeps both sides' vanilla
combat damage at zero. The tactical engine is the sole damage channel;
the frozen vanilla battle stays alive, so pinning, reinforcement and
attack progress follow vanilla rules, and the rest of the world (AI
moves, build-up) simulates normally during the advancing hour. The freeze
is applied by the entry decision's `complete_effect`
(`FROM = { d_tac_freeze_state = yes }` — the picked state holds the
contested province) and lifted by every cleanup path
(`d_tac_clear_tactical` runs the `d_tac_unfreeze_all` sweep) plus,
explicitly, by the end batch's phase 2. Save-game division cards are
unaffected: a state combat modifier is battle context, not serialized
unit data.

Batch shape (§3.2): every sync batch ends with `pause_in_hours 1` as its
LAST line — the batch executes while the game is paused, and any line
after it would run immediately, before the hour elapses. The end
injection is TWO-PHASE:

1. Phase 1: collapse + the final partial hour's damage lines, then
   `pause_in_hours 1` — appended only when unsynced full turns remain; a
   battle ending exactly on a synced boundary already had its clock hour,
   so no extra one is burned. During the advancing hour the org-zeroed
   divisions disintegrate under vanilla rules while the freeze zeroes
   combat damage.
2. Phase 2 (sent after the receipt, or after its timeout — a stuck freeze
   must never linger): `d_tac_unfreeze_all` + `d_tac_end_battle` (untag
   both sides, clear the mode flag, completion popup).

Receipt: the external program probes game.log with
`eval_effect log = "TAC_CLOCK_<pid>_<n>"` lines (same console chain as
the ping probe, §3.2) and compares the newest probe line's
`[yyyy.mm.dd.hh]` prefix against the pre-injection one. At most one hour
can elapse per batch, so any change — day rollover included, via the full
date compare — confirms the advance. The wait caps at ~30 s. On timeout
the two batch kinds diverge: a FINAL batch keeps the old
log-and-continue (phase 2's unfreeze must never wait on a dialog), while
an HOURLY sync suspends behind the sync-stall dialog — the player is
asked to check HOI4 (alive, not stuck in a menu or on pause: a
menu/pause halt blocks the simulation while console effects still
execute, so the batch lands but the clock cannot move) and chooses Retry (clock-command only — the damage lines already
landed, and a prefix that moved on its own counts as success without
re-sending: a stale `pause_in_hours` timer catches up once the menu
closes) or Cancel (log-and-continue; the strategic clock lags one hour,
harmless and non-compounding). The post-sync follow-up (the battle-alive check + the roster diff, §8.2) runs only after the
dialog resolves. Injection failures themselves stay retry-once for final
phase 1, §3.2.

Caveats:

- Console commands are disabled in ironman by design (already a
  non-goal, §16).
- HOI4 must not auto-pause on focus loss while the tactical window is in
  the foreground (`pause_on_lost_focus=no` in settings.txt) or game
  hours will not elapse.

---

## 9. UI Design

### 9.1 Screen Layout (revised)

```
┌──────────────────────────────────────────────────────────┐
│ [Minimap]          ┄ notice bar ┄        │  Forward Command │
│                    (phase hints /        │  Turn 12 │ 🕐  │
│                     event flashes)       │  Hour 3 │ Att. │
│        TACTICAL HEX MAP                  │  TACTICAL ACT. │
│        (center, largest area)            │  Sedan          │
│      ⬡ ⬡ ⬡ ⬡ ⬡ ⬡ ⬡                     │ ─────────────── │
│     ⬡ ⬡ ⬡ ♜ ⬡ ⬡ ⬡ ⬡ ⬡  ◤hover card◢   │ Enemy tactic    │
│      ⬡ ⬡ ♞ ⬡ ⬡ ⬡ ⬡     (unit/terrain, │  name + descr.  │
│                           exact bars)    │ ─────────────── │
│                                          │ [End Turn ⏎]    │  ← fixed slot,
│                                          │ ─────────────── │    content follows
│                                          │ [Minimap]       │    the phase
│                                          │ [Reset View]    │
│                                          │ [Order of Batt.]│
│                                          │ [Battle Log]    │
│                                          │  Esc — menu     │
└──────────────────────────────────────────┴─────────────────┘
 Esc menu (modal): Continue / Settings / Reset Battle… / Exit Game
```

The left info panel and bottom unit list are GONE: unit & terrain details
moved to the cursor-rest hover card, the unit list became the Order of
Battle window, the tactic card merged into the right panel, and the panel
log became a standalone window.

### 9.2 Panel Specifications (revised)

| Panel | Position | Type | Closable | Content |
|-------|----------|------|:---:|---------|
| **Tactical Map** | Center | Bevy canvas | No | Hex terrain grid, unit sprites, fog overlay |
| **Command Panel** | Right side | egui panel | No | STATIC: turn/hour/phase, location, battle-objective section (above enemy tactic — side-dependent: capture the objectives vs hold them, a city battle names the city itself from the location label; the annihilation path is always listed as the alternative conclusion, and the sole one when the battle has no flags), enemy tactic section, fixed main-action slot ([Begin Battle] / [End Turn] / [Sync] / [Apply & Exit] by phase — position never moves; Begin Battle greys out until every player-commanded battalion is deployed), [Minimap] toggle, [Reset View], [Order of Battle], [Battle Log] |
| **Hover card** | At cursor | egui area | auto | Replaces the Info Panel: resting the cursor 0.3s on a unit / 0.8s on bare terrain pops the full stat sheet; Org/Str bars show exact `current/max` on bar hover; the move-order ETA lives here (only here) |
| **Minimap** | Top-left corner | egui window | Yes | Zoomed-out hex grid overview, colored dots for unit positions (fog-filtered), white quad marking the main camera's visible ground footprint |
| **Esc menu** | Center modal | egui window | Esc | Continue / Settings (in-battle render & performance knobs — MSAA, shadows, render scale, frame cap, idle saver; hot-applied and persisted to settings.json) / Reset Battle… (merged Restart + To-Last-Sync with a confirmation naming the actual target) / Exit Game (live mode confirms an unsynced battle) |
| **Battle log** | Floating | egui window | Yes | Full scrollable history (200 lines); opened from the right panel's [Battle Log] (below [Order of Battle]) and auto-opened on battle end only — sync completion has its own top-anchored modal (damage numbers stay on model floaters) |
| **Order of Battle** | Floating | egui window | Yes | Division → battalion tree (player side only): side-color dot, name, mini org/str bars; click = select + camera pan; destroyed battalions stay greyed/struck-through |
| **Notice bar** | Top-center | egui area | auto | Phase-driven pinned hints (deploy / sync / resolved) + timed event flashes (turn handover, battle start); template for future ops hints |
| **Unit labels** | Above units | egui painter | No | Name + org/str mini bars + status letters **H/E/S/W** (holding, emplaced, shocked, waiting on rocket reload — left corner) and a red **R** retreat marker (right corner). Name text is white (own) / warm orange-red (enemy); the HQ reads differently within its side — gold (own) / brightest red (enemy) |

### 9.3 Interaction Model (revised)

The mouse IS the command interface; the right panel carries no command
buttons anymore (§9.2).

1. **Select unit (non-sticky)**: left-click a friendly unit on the hex map or
   in the Order of Battle window (which also pans the camera to it).
   Left-clicking empty ground, an enemy, or anything else releases the
   selection. Enemies are never selectable.
2. **Move**: right-click the destination hex → standing move order; the
   route ribbon (black edge, red core) grows out from the unit and the
   hover card shows the ETA in turns (the panel is static now).
3. **Attack**: right-click a visible enemy — the order splits by weapon:
    - Indirect artillery (tube / rocket): always a fire mission (精确射击 on
      a right-clicked visible enemy, 覆盖射击 via the F button), never an assault.
   - Direct-fire units: adjacent target → assault; in-range target (armor /
     AT guns, range 2) → direct fire (fire support without counter-attack).
   - Failures pop an orange reason floater on the target hex (Out of range /
     Too close / Emplace first / Holding stance / Already acted).
4. **Stand by**: right-click the selected unit itself → cancels the unit's
   standing orders — move AND attack. Stance, emplacement and the
   acted flag are untouched.
5. **Radial menu**: selecting a unit pops an arc of circular buttons above
   it — **H** = Hold stance (any INFANTRY-attribute unit, including
   motorized/mechanized infantry; cavalry and vehicle crews excluded),
   **E/L** = Emplace / Limber (towed guns, same slot), **F** = fire-mission
   picking (indirect artillery only, one barrage per turn), **R** = Retreat —
   a disengagement move, only available while in contact (adjacent enemy);
   use a plain move order to reposition otherwise.
   Unavailable buttons are HIDDEN, not greyed: an acted unit shows no arc
   at all and its model renders at 35% opacity (the "spent for this turn"
   signal, refreshed solid at the next turn). Placeholder capital
   letters until proper icons are drawn.
6. **Fire-mission picking**: after F, left-click a target hex inside the
   fire envelope (visible enemies inside it are marked) → one barrage; the
   unit is spent for the turn. Right-click or Esc cancels without spending
   the action.
7. **Camera**: WASD/arrows pan, wheel zooms, Q/E orbits, R/F tilts the pitch
   (toward horizontal / toward top-down), middle-drag grabs the map, and
   right-DRAG (>6 px) orbits — a right-CLICK below the threshold is a
   command, not a camera move.
8. **End Turn**: [End Turn] → own standing orders march out, then the AI
   executes its turn.
9. **Sync**: after every 6th turn, [Sync] becomes active → click to inject
   damage into HOI4.
10. **Hover info card**: resting the cursor over the map pops a
    card — 0.3s over a unit, 0.8s over bare terrain. Moving the cursor
    dismisses it, but moving INTO the card keeps it (for reading the exact
    Org/Str values on the bars, HOI4 battle-bubble style). Enemy cards
    follow fog rules: visible enemies only, none during deployment.
11. **Esc menu**: Esc priority ladder — close the topmost modal
    (battle-report tour, sync prompt, confirmations, the settings window;
    while ANY modal is open all map input freezes) → cancel an attachment
    re-assignment pick → cancel fire-mission picking → release
    the selection → open the modal menu. Continue / Settings (in-battle
    render options) / Reset Battle… / Exit Game.
12. **Deployment (rework)**: the player's battalions start OFF
    the board, listed in the Order of Battle window (undeployed). Click an
    OOB row → a ghost follows the cursor → click a valid zone hex to place;
    right-click / Esc cancels. [Auto Deploy] hands every remaining unit to
    the AI deployment planner (hand-placed hexes are protected). **Sector
    deployment**: each division's OOB header carries a
    [Deploy N to sector] button — left-drag a rectangle on the map; the
    preview highlights ONLY the legal hexes (rectangle ∩ player zone ∩
    deployable ∩ unoccupied, cyan) and, throttled to ~5 Hz, ghost-previes
    the division's battalions at the positions the planner would give them;
    release commits (other divisions and hand-placed hexes untouched;
    absurd rectangles > 4096 hexes are rejected). **Recall**:
    [Recall All] (right panel) or per-division [Recall N] (OOB) return
    battalions to the OOB queue behind a confirmation dialog — hand
    positions are lost. The OOB button wears a red counter badge for units
    still waiting. [Begin Battle] is rejected while any unit is
    undeployed. Drag-adjust after placement works as before.
    Enemy units are not on the map yet (§11.1).
13. **Range rings on selection**: selecting a ranged unit
    draws its max-range ring on the board; rocket artillery also shows its
    minimum-range dead-zone circle.
14. **F8 fog reveal (debug)**: toggles a render-only no-fog view for
    inspecting AI deployment and fog leaks; never touches game logic.

### 9.4 Window Style (revised)

- Normal decorated OS window (Bevy `Window` default), 1440×900 default size,
  resizable; FIFO vsync for even frame pacing.
- Borderless + always-on-top is an OPEN issue: planned as a
  release configuration or `--borderless` switch
  (`decorations: false`, `window_level: AlwaysOnTop`).
- Menu↔battle runs as separate windows/processes: the main menu stays
  resident while a battle child process runs (winit allows one EventLoop per
  process). The menu is a **resident tray app**: the Shell_NotifyIconW icon appears at startup
  and lives until the program exits; closing the window (X) hides it to the
  tray instead of quitting; the Exit Game button asks
  minimize-to-tray / quit / cancel; the tray popup carries
  Show menu / toggle live listen / Exit game; while a battle holds the
  screen the menu window hides to the tray (no taskbar residue); tray
  unavailable → fallback to taskbar minimize and direct X/Exit quit.
  The menu's About page shows the version + build date (the
  exe's own mtime, rendered as a local date — survives zip distribution)
  and the author/credits lines, so beta bug reports can name their build;
  a prominent ⚠ button opens the disclaimer terms (same substance as the
  packaged docs/免责声明.md, plain in-game wording, localized).
- Dual-screen: drag to secondary monitor.
- Single-screen: Alt+Tab between HOI4 and tactical program.

---

## 10. Mod Component Specification

### 10.1 Decisions: `tactical_command_attack` / `tactical_command_defend` (revised)

Entry is the vanilla targeted-decision targeting flow: the
state-target entry decision sits as a ROW in the (pictured + described)
decisions-tab category (`on_map_mode = map_and_decisions_view`); clicking
the row enters target selection — map icons appear over the candidate
states — ESC/right-click dismisses the reminder, clicking a state fires.
(The opener/closer NORMAL decisions were retired: a normal
decision's taken-state re-evaluates only on the daily pulse —
`days_re_enable = 0` included — so same-day re-opening was
impossible, while a TARGETED decision re-fires same-day; at most a
per-target cooldown lingers.) `visible = NOT
tac_mode_active` hides the row while a battle runs; every reset path
(`tac_abort.1` / `tac_quiet.1` / `d_tac_end_battle` → shared
`d_tac_clear_tactical`) clears the flag, and the injected reset batches end
with `reloadinterface` so the list refresh is immediate; the
cleanup does NOT re-arm a taken state's icon — a taken state-targeted decision leaves a
PER-TARGET take record (`active_targeted_decision{… target=<state id>
state=completed days=1 }`, save-verified) that is ENGINE-LOCKED until the
daily pulse (~a few in-game hours; same-day re-pick of the same state
then works — accepted behavior). Every scripted channel was exhausted
and is dead: `remove_decision_on_cooldown` (ordinary-decision cooldowns only), `remove_targeted_decision`
(literal ids AND `var:` refs — the `id` keyword stores the state's
INTERNAL id, which mismatches the record's map-id key; state-scope refs
like `THIS` are silently ignored), the vanilla remove+activate pair
(remove alone is a demotion of AVAILABLE records — a red herring —
and activate is blocked on completed records by "Cannot activate if
active in interface"), `remove_decision`, `days_re_enable = 0`. The id-capture + guarded
var pair + clear in the cleanup are no-ops on the record yet flip the
daily-pulse outcome — with them a completed record restores to
available after a few hours (the accepted behavior); without them it
decays to re_enable_cooldown and stays gone into the next day — the machinery is kept as an opaque engine side
channel. There is
NO F12 hotkey. The
decisions live inside a declared category
(`common/decisions/categories/tac_categories.txt` →
`forward_command_decisions`); flag gates live in `available`/`visible` (the
`allowed` trigger set rejects `has_country_flag`, 1.19.2 error.log; `visible`
is the true show/hide gate, `available` only greys). **HOI4 re-evaluates
decision visibility/candidates only on the daily pulse** — any external→injection→decision-gating loop
structurally lags a day, so candidate gating must be script-computable.

**Battle selection:** the player picks
WHICH battle to fight via a single **state-target decision**
(`tactical_command_entry`, vanilla CHI/AFG mechanism) whose candidate pool is
script-computed frontier geography in `target_trigger` FROM scope — EITHER
frontier kind, OR'd:

```
FROM = { OR = {
    AND = { controller = { has_war_with = ROOT }            # enemy state
            any_neighbor_state = { is_controlled_by = ROOT } }
    AND = { is_controlled_by = ROOT                          # your state
            any_neighbor_state = { controller = { has_war_with = ROOT } } }
} }
```

(The attack/defend decision split was reverted — territory
is NOT side, defensive battles happen on enemy soil too, and both candidate
sets displayed on the map simultaneously. `is_attacker`/`is_defender` exist
but are COMBATANT-scope (combat_tactics.txt evaluation only), unusable in
decisions — the side comes from the save's `land_combat` tags.)

The decision stays visible whenever not `tac_mode_active` (`is_at_war` is NOT
a valid trigger — peacetime simply shows zero map-icon
candidates); firing sets `tac_mode_active`, hiding it (one tactical battle at
a time). Its name carries the target state — "Enter Tactical Battle in
§Y[FROM.GetName]§!" / 在§Y[FROM.GetName]§!进入战术战斗 (vanilla
state-target name precedent: YUN_core_state). Candidates are a SUPERSET
(quiet frontier states are pickable): the external
validation on `tac_start` reports "the picked state has no running battle"
instead of launching (no silent fallback).

`complete_effect` runs with FROM = the picked state and writes
`FROM = { set_variable = { tac_pick = 1 } }` FIRST, then the round-5 payload
unchanged (both-sides division tagging, `tac_start` + `tac_enemy_tactic`
bare-JSON log lines, `set_country_flag = tac_mode_active`). The external
program then snapshots (`savegame tac_snap`), finds the `tac_pick=1` state
(state variables serialize), collects the player `land_combat`s inside
it and spawns `--livebattle province=<location>` (scenario locks the
combat by `location`; `province=0`/no-pick = legacy largest-battle
inference). ONE combat → straight in; SEVERAL → the external picker (`PickResolution::Multiple`; sorted largest-first, province id as
tiebreak — no silent largest pick). The menu's picker is a GRAPHICAL map
dialog: the picked state's provinces.bmp/definition.csv
crop — SOLID state outline, SOLID GRAY province outlines, battle provinces FILLED by the
player's side (dark red attacking / dark green defending — the HOI4
battle-bubble pairing), VP-name labels at the centroids, hover emphasis,
pixel-exact click — with a plain list as fallback when the map files are
unreadable; the map is built on the worker thread. CLI `--live` keeps a
numbered stdin prompt. Cancel fires `event tac_abort.1 <tag>` from the menu
and drops the menu back to the tray; a QUIET pick (no running battle in the
state) fires `event tac_quiet.1 <tag>` — an in-game popup that explains and
resets. Both batches END with `reloadinterface`:
console-fired effects do NOT refresh the decisions UI (`visible` re-evals
per interface frame — a decision CLICK refreshes, a console `event` does
not), so the interface reload is what actually hides the map icons NOW. The
interface rebuild does NOT close the console (the
"Interface Reloaded" line sits in the still-open console) — the injector
ALWAYS sends its closing toggle; skipping it left the console open
and desynced the NEXT injection (its opening toggle closed it, the typed
`run` went nowhere, the snapshot timed out, the picker never popped).
Both share the `d_tac_clear_tactical`
scripted effect
(untag both sides + `tac_pick` zeroed + `tac_mode_active` cleared → ALL map
decisions re-arm); `tac_abort.1` and `d_tac_end_battle` call it too. Battle
windows steal the foreground on startup (`window.rs bring_to_foreground`) so the child does not open buried behind HOI4. Cleanup rides
`d_tac_end_battle` and `tac_abort.1`.

```clojure
# common/decisions/tac_entry.txt (skeleton — payload elided)

tactical_command_entry = {
    state_target = yes
    on_map_mode = map_and_decisions_view   # category ROW = the opener: click → targeting (ESC dismisses)
    days_re_enable = 0                     # (normal-decision cooldowns are daily-pulse-bound anyway)
    target_trigger = { FROM = { OR = {
        AND = { controller = { has_war_with = ROOT }
                any_neighbor_state = { is_controlled_by = ROOT } }
        AND = { is_controlled_by = ROOT
                any_neighbor_state = { controller = { has_war_with = ROOT } } }
    } } }
    visible/available = { NOT = { has_country_flag = tac_mode_active } }   # is_at_war invalid
    complete_effect = { FROM = { set_variable = { tac_pick = 1 } } … set tac_mode_active }
}
# tactical_force_exit lives in the SAME file (§10.4); the category carries a
# picture + description (GFX_decision_cat_picture_generic_border_conflicts).
```

### 10.2 Console Commands (`common/scripted_effects/`) (revised)

The external program's hourly sync batch (§3.2) writes **literal
`damage_units` eval_effect lines** — defender lines at the contested
province, attacker lines per source province, ONE PER participating
country tag of each side (the `limit = { tag = … }` filter reaches only
that country's divisions) — then fires the sync ack
through `d_tac_sync_hourly` (scope-pinned like every scripted-effect call).
The literal bucket effects
(`z_tac_org_buckets.txt`, `d_tac_org_own/enemy_0NN`) are RETIRED: values now
ride the eval_effect line itself, so the mod ships no per-value effects at
all. (Earlier the batch fired `event tac_sync.1 <tag>` directly — a
console-fired event ignores `hidden = yes` and popped an empty window every
sync; routing the ack through the scripted effect keeps `tac_sync.1`
hidden as intended.)

Console reality: scripted effects run as
`effect <name>`, events as `event <id> <tag>`; console-set vars are
UNREADABLE in effect arguments; **the console executes
on the player's currently-selected scope** — scripted-effect calls
therefore ride pinned as `eval_effect <player_tag> = { d_tac_… = yes }`;
`damage_units` with `province=`+country-`limit` is scope-robust by
construction — all damage/collapse lines ride it (§3.2). The §6.11
collapse is `org_damage = 1.000` at the contested province (province-
scoped; the `d_tac_collapse_*` effects below were deleted
from the mod — they zeroed every `tac_in_battle`-tagged division of a
country, and tagging can only be country-wide).

```
# d_tac_sync_hourly.txt
d_tac_sync_hourly = {
    country_event = { id = tac_sync.1 hours = 0 }
}

# d_tac_end_battle.txt — cleanup + completion log; the cleanup payload
# lives in the shared d_tac_clear_tactical (serving
# tac_abort.1 / tac_quiet.1 / d_tac_end_battle alike).
d_tac_end_battle = {
    d_tac_clear_tactical = yes
    country_event = { id = tac_complete.99 hours = 0 }
    log = "{\"type\":\"tac_battle_ended\"}"
}
```

`d_tac_clear_tactical` zeroes the
picked-state marker everywhere, clears the mode flag, and purges the
ordinary-decision cooldowns. The state-target entry's
per-target TAKE record is NOT re-armed — it is engine-locked until the
daily pulse (~a few in-game hours; same-day re-pick of the same state
then works — accepted behavior), and NO scripted form accelerates it. The `tac_picked_state` capture +
`has_variable`-guarded `remove_targeted_decision`/`activate_targeted_decision`
var: pair + the clear are no-ops on the record (the `id` keyword stores
the state's INTERNAL id — mismatching the record's map-id key) yet are
KEPT: their presence flips the daily-pulse decay from the dead-end
re_enable_cooldown (icon gone, still gone next day — the original
symptom) to a restore-to-available (icon back after a few hours — the
accepted behavior).

### 10.3 Events (revised)

Refreshed verbatim-ish from `events/tac_sync_events.txt`: namespaces are
declared with `add_namespace` (one per event-id prefix), the two
player-visible situation popups carry `title`/`desc`/`option` (only the sync ack stays `hidden = yes`), variables cleared
with `set_variable = 0` (`remove_variable` does not exist), the abort
cleanup runs the shared `d_tac_clear_tactical` — and ALL events carry
`is_triggered_only = yes` (without it they are RANDOM-FIRE eligible at
the daily pulse).

```
# events/tac_sync_events.txt
add_namespace = tac_sync
add_namespace = tac_complete
add_namespace = tac_abort
add_namespace = tac_quiet

country_event = {   # Hourly sync acknowledgement
    id = tac_sync.1
    hidden = yes
    is_triggered_only = yes
    immediate = {
        log = "{\"type\":\"tac_state\",\"hour\":0,\"phase\":\"synced\"}"
    }
}

country_event = {   # Battle complete (fired by d_tac_end_battle) — popup
    id = tac_complete.99
    title = tac_complete.99.t
    desc = tac_complete.99.d
    is_triggered_only = yes
    immediate = {
        log = "{\"type\":\"tac_complete\"}"
    }
    option = { name = tac_complete.99.o }
}

country_event = {   # Abort (Force Exit decision / picker cancel) — popup
    id = tac_abort.1
    title = tac_abort.1.t
    desc = tac_abort.1.d
    is_triggered_only = yes
    immediate = {
        d_tac_clear_tactical = yes
        log = "{\"type\":\"tac_abort\"}"
    }
    option = { name = tac_abort.1.o }
}

country_event = {   # Quiet-state notify — popup
    id = tac_quiet.1
    title = tac_quiet.1.t
    desc = tac_quiet.1.d
    is_triggered_only = yes
    immediate = {
        d_tac_clear_tactical = yes
        log = "{\"type\":\"tac_abort\"}"
    }
    option = { name = tac_quiet.1.o }
}
```

### 10.4 Abort Decision (revised)

The abort decision lives in the SAME file as the entry decision
(`common/decisions/tac_entry.txt`), not its own file. The flag gate sits in
`available` (not `allowed` — restricted trigger set, see §10.1). On the
external side, the live battle window tails game.log itself
(`battle::watch_tac_abort`) and jumps the session to `Ended` on receipt —
HOI4-side Force Exit wraps the tactical window up immediately.

```
tactical_force_exit = {
    icon = generic_research
    cost = 0
    fire_only_once = no

    available = { has_country_flag = tac_mode_active }

    complete_effect = {
        country_event = { id = tac_abort.1 hours = 0 }
    }
}
```

### 10.5 Heartbeat (revised)

```
# In common/on_actions/tac_on_actions.txt: on_daily
# (on_hourly DOES NOT EXIST in 1.19.2 — vanilla grep finds zero hourly
# hooks; the pulse family is on_daily[_TAG] / on_monthly[_TAG]).
# Only fires if the tac_mode_active flag is set.
if = {
    limit = { has_country_flag = tac_mode_active }
    log = "{\"type\":\"tac_heartbeat\",\"hour\":0}"
}
```

`hour` stays `0` — 1.19 exposes no scriptable current-hour value (vanilla
grep). The listener substitutes the real game hour from the log line's
`[yyyy.mm.dd.hh]` prefix (`tactical-listen::game_hour_prefix`, §3.1). The
heartbeat is liveness-only, so the daily pulse suffices.

### 10.6 Message Coverage Notes (revised)

- `tac_enemy_tactic`: a fixed `"default"` card in the
  same tick right after `tac_start`. The listener consumes it via
  same-batch lookahead + ~1 s grace-poll; the real HOI4 combat-tactic
  token mapping (16 cards, §7.2) is iteration 2.
- `is_player_attacker`: constant `true` until a vanilla-verified
  attack/defend trigger is found.
- Division tagging (`tac_in_battle`) was REMOVED:
  it could only ever be country-wide (no province/in-combat filter —
  `is_in_combat` is unverified), and every consumer moved to
  province-scoped `damage_units` lines (§3.2).
- Strength writeback is an engine gap (no per-division strength knob);
  org carries the precise damage model (§3.2).
- All mod-side `log` lines are single JSON objects (one per line); the
  listener skips non-JSON lines (§3.1).

---

## 11. User Experience

### 11.1 Player Journey (revised)

1. Normal HOI4 gameplay — nothing changes.
2. During a land battle involving player divisions, the "Engage Tactical
   Command" decision becomes available; the player clicks it in the
   decisions tab (no hotkey).
3. The mod tags the player's divisions (`tac_in_battle=1`) and writes a
   `tac_start` JSON line to `game.log` (currently placeholder values, §10.1).
   No auto-save exists — the external program parses the newest regular
   `.hoi4` save it finds.
4. The external program's main menu (Start Live Listen / Debug Battle /
   Settings / About / Exit Game) is already running with Live Listen on; it detects
   `tac_start` and spawns the battle window (normal decorated window,
   1440×900) as a child process.
5. Deployment: the player's battalions start OFF the
   board, in the Order of Battle window; the player places each by clicking
   its OOB row then a valid zone hex (ghost preview), drags a sector per
   division ([Deploy N to sector]), or hits [Auto Deploy] to hand
   the rest to the AI planner (hand-placed hexes survive). The enemy is NOT
   on the map yet. [Begin Battle] refuses to start while any battalion is
   undeployed. (MVP limit: in live mode the player is always the attacker.)
6. Player clicks [Begin Battle] — only NOW does the enemy AI deploy, inside
   its own zone, out of the player's sight.
7. Player plays tactical turns (Move/Assault/Hold/Fire Support/Retreat
   commands, §9.3).
8. Every 6 turns, [Sync to HOI4] becomes active.
9. Player clicks [Sync] → brief flash (HOI4 console opens/closes, ~0.5s) →
   summary modal → tactical game resumes.
10. Battle resolves → [Apply & Exit] appears → clicks → final injection →
    battle window closes, back to the still-resident menu.
11. Back in HOI4, strategic battle updated.

### 11.2 Setup (revised)

1. Install the mod component with `install-mod.ps1` (copies
   `hoi4-mod/forward-command` into the Paradox mod folder and writes the
   launcher `.mod` descriptor; `-Uninstall` removes it), then enable
   "Forward Command" in the HOI4 launcher. (No Steam Workshop
   distribution yet.)
2. In HOI4: `save_as_binary=no` in settings — the installer WARNS when
   `settings.txt` has `save_as_binary=yes`; the menu Settings page probes the
   value live and offers a one-click fixer ("Set save_as_binary=no", keeps a
   `settings.txt.bak` backup; restart HOI4 for the change to stick).
3. Place the release build (`forward-command.exe` + `data/` tables +
   `settings.json`) in any folder.
4. Run the `.exe` before or after launching HOI4; paths (HOI4 install /
   saves dir / game.log) auto-detect, overridable in the menu Settings page.
5. Play.

### 11.3 Error Recovery

- External program crashes → player uses "Force Exit Tactical Mode" decision in HOI4.
- Any unsynced tactical results are lost.
- Player can re-enter tactical mode immediately (re-parse save for fresh state).

---

## 12. Configuration (revised)

### 12.1 `settings.json` (written by the menu Settings page)

Runtime-generated by the main-menu Settings page; lives in the
workspace root. Every field falls back to its default (auto-detected path
or built-in value) when missing/blank, so a fresh install "just works".

```json
{
  "hoi4_dir": "D:/Steam/steamapps/common/Hearts of Iron IV",
  "saves_dir": "C:/Users/<user>/Documents/Paradox Interactive/Hearts of Iron IV/save games",
  "log_path": "C:/Users/<user>/Documents/Paradox Interactive/Hearts of Iron IV/logs/game.log",
  "language": "zh",            // "en" | "zh" (§15); blank/unknown → zh (CN closed-beta default)
  "writeback": "org_str",      // org_str | off — damage write-back mode
                               //   no per-division script knob
                               //   exists. org_str = org+str at
                               //   province precision (defender contested-exact; attacker
                               //   str diluted across each source province); off = no
                               //   writeback. The third mode org_only was removed
                               //   its retired token
                               //   maps to the default. Menu Settings
                               //   page shows player-facing labels + per-mode details
                               //   (en/zh; no dev-log internals).
  "msaa": 2,                   // 4 | 2 | 1 (= off); default 2. The 3D camera
                               //   always carries an FXAA pass — the far-zoom orbit moiré is
                               //   covered at a fraction of the MSAA delta.
  "shadow": 1,                 // 0=off | 1=low | 2=high; default low
                               //   (RTS-zoom casters are only minis+props — High ≈ Low visually).
                               //   tiers:
                               //   low  = 1024 map + ONE cascade + Hardware2x2 filtering
                               //   high = 2048 map + two cascades + Gaussian (9-tap) filtering
  "shadow_map": 2048,          // LEGACY key; no longer migrates to a level
  "max_fps": 60,               // 30|60|90|120|144, 0 = uncapped; the Vulkan
                               //   present path does not pace, so this pads full-speed frames
  "render_scale": 100,         // 100|85|70|50: below 100 the 3D scene
                               //   renders into a smaller offscreen target and upscales to the
                               //   window — the weak-GPU lever (cuts pixels + MSAA buffers by
                               //   scale²; cursor/raycast coords convert via picking::scaled_cursor)
  "low_power": true            // idle frame-saver (battle windows drop to ~10 fps when idle)
}
```

Settings keys live in `tactical3d-bin/src/settings.rs` (defaults + loaders).
The menu Settings page hot-applies msaa/shadow/max_fps/low_power to the
running menu; battle windows expose the same render knobs
mid-battle — Esc → Settings: a `BattleSettings` resource in
tactical3d-render/src/settings.rs is hot-applied by the render crate
(render_scale flips build/tear down the offscreen path at runtime) and
persisted back to settings.json by the bin crate, so both surfaces always
agree (render_scale stays battle-window-only — the menu backdrop is never
scaled).
Battle windows also drop to the idle ~10 fps cadence when visible but
UNFOCUSED for >1.5 s (like the minimize guard — the player is
in HOI4 between syncs and full-cost frames are waste).
The msaa/shadow/max_fps rendering consequences carry render-gate and
Windows no-idle-frame trap caveats.

Auto-detection: `hoi4_dir` via known install locations (validated by
`map/provinces.bmp`), `saves_dir` via `%USERPROFILE%/Documents/...`,
`log_path` via the Paradox logs directory.

The Settings page also carries the one-click **diagnostic bundle** export:
a single zip next to the exe (%TEMP%
fallback when unwritable) containing `diag-info.txt` (version, paths,
included/missing manifest), `crash.log`, `settings.json`, a `game.log`
tail (512 KiB), an `fc_inject.log` tail (8 MiB), and — while its checkbox
is on (default) — the newest `.hoi4` save. The zip writer is hand-rolled
in `tactical3d-bin/src/diag.rs` (deflate via the already-present flate2,
UTF-8 entry names, tmp+rename); the button tooltip warns the bundle
contains local paths and the save.

### 12.2 Combat tuning — hardcoded in `CombatParams`

There is NO tuning file: all combat/movement parameters are hardcoded
defaults in `tactical-core/src/params.rs` (`CombatParams::default()`).
Current values:

| Parameter | Value | Parameter | Value |
|-----------|-------|-----------|-------|
| `hit_base` | 0.10 (vanilla defended hit rate, locked) | `partial_encircle_org_attrition` | 0.025 |
| `hit_saturated` | 0.40 (vanilla saturated hit rate, locked) | `full_encircle_org_attrition` | 0.05 |
| `damage_scale` | 1.0 (K₂ — global lethality; balance lever) | `zoc_entry_ap_cost` | 0.0 (zeroed, mechanism kept) |
| `break_str_loss` | 0.12 (a full break costs 12% max str, §6.3) | `zoc_to_zoc_ap_cost` | 0.0 (zeroed, mechanism kept) |
| `broken_str_loss` | 0.68 (org-0 target str rate, §6.3) | | |
| `org_cap_ratio` | 0.40 | `friendly_path_penalty_km` | 0.5 (lane spreading) |
| `shock_threshold_ratio` | 0.25 | `friendly_dest_penalty_km` | 2.0 (lane spreading) |
| `direct_fire_falloff` | 0.60 | `hold_defense_bonus` | 0.25 |
| `random_spread` | 0.0 (deterministic since v3.2, mechanism kept) | | |
| `rocket_fire_cooldown_turns` | 3 | `entrench_max_layers` | 3 |
| `turns_per_strategic_hour` | 6 | `entrench_defense_per_layer` | 0.10 |
| `area_center_share` | 0.40 (覆盖射击弹着格权重) | `area_neighbor_share` | 0.10 (邻格各 1/10) |
| `hq_aura_radius` | 3 | | |
| `melee_elevation_gain` | 0.15/level (≤1 hex) | `melee_elevation_cap` | 0.45 (3 levels) |
| `exposed_crest_mult` | 1.5 (indirect, step lower) | `defilade_mult` | 0.5 (indirect, step higher) |
| `hq_org_regen_frac` | 0.02 (out of contact) | `hq_death_org_frac` | 0.20 |
| `hq_combat_bonus` | 0.10 | `hq_signal_radius_bonus` | 3 |
| `flag_progress_cap` | 12 | `flag_capture_ratio` | 2.0 |
| `flag_decay_ratio` | 0.5 | `flag_cluster_radius` | 2 |
| `field_flag_count` | 3 | `flag_deep_core_fraction` | 0.40 |
| `div_sensor_radius` | 10 (§7.4) | `oob_leaving_turns` | 6 |

Grid caps (`MAX_GRID_WIDTH/HEIGHT` = 512, `HEX_SCALE_KM` = 1.0) live in
`tactical-map`, not in `CombatParams`.

### 12.3 `config.json` (REMOVED)

`config.json` was deleted: no code read it, its tuning keys had already
moved into `CombatParams` (some, e.g. `base_avoid_chance`, had no code
counterpart at all) and its path keys had moved to `settings.json`.
Rationale: while combat balance is iterated via code
edits + recompile in playtesting, a half-wired runtime JSON only adds
drift risk — §12.2's `CombatParams` table is the single source of truth.
If user-facing tuning is wanted later, it belongs on the menu Settings
page, not in a resurrected file.

---

## 13. Development Roadmap (revised)

Test counts (`grep -rc "#\[test\]"` over
`forward-command/crates`, summed): **320 `#[test]` functions total** —
`cargo test --workspace` reports 316 passed + 4 ignored (three
tactical-map tests require a local HOI4 install; run with `-- --ignored`,
plus the tray lifecycle smoke). The phases below itemize only a subset;
the remainder lives in tactical-listen / tactical3d-bin /
tactical3d-render (mesh_build, board, locale).

### Phase 0: Foundation Tooling (Rust workspace) ✅ COMPLETE
- [x] `tactical-core`: Hex coordinate math, pathfinding (A* on hex grid), line-of-sight, range calculations, pre-order movement, fog, encirclement, unit traits, params, chain of command — 62 tests
- [x] `tactical-save`: jomini-based save parser, division extraction, stat reverse-calculation — 31 tests
- [x] `tactical-map`: `provinces.bmp` parser, bounding box, occupancy mask, hex grid generation, rivers/VP/city overlays — 33 tests (3 ignored: need local HOI4)
- [x] `tactical-ai`: 3-layer decision AI, 16 tactic cards, terrain-aware deployment — 46 tests
- [x] `tactical-inject`: Win32 FFI (SetForegroundWindow, SendInput) — 6 tests
- [x] `extractor/`: Six Python scripts → 6 JSON tables (equipment, units, terrain, tactics, doctrines, country colors)
- [x] Environment: stable-gnu + WinLibs POSIX UCRT + rust-lld linker

### Phase 1: Tactical Engine Core ✅ COMPLETE
- [x] `tactical-combat`: Combat resolution (assault, fire support, armor-piercing, counter-attack, elimination; Lanchester rework; command aura + HQ collapse) — 27 tests
- [x] Fog of war: per-unit sight range, stale intel tracking (in `tactical-core`)
- [x] Encirclement detection: partial/full encirclement + progressive attrition (in `tactical-core`; redesign, §6.4)
- [x] `tactical-sync`: Battle lifecycle state machine (9 phases incl. Ended, turn counting, sync triggering, victory detection) — 37 tests

### Phase 2: UI ✅ COMPLETE (2D; superseded)
- [x] Bevy hex grid renderer (10 terrain colors, fog darkening, Gizmos-based) — in `tactical-ui`
- [x] Camera: 2D orthographic, mouse-wheel zoom (0.5-3.0), WASD/arrow pan
- [x] Unit sprite rendering: filled (attacker) / outlined (defender), selection ring, low-org warning
- [x] Valid-target overlays: green (move), red (assault), blue (support)
- [x] egui command panel: 5 buttons + unit quick-stats (Org/Str bars) + Sync/End Turn
- [x] egui info panel: battle info + detailed selected-unit stats
- [x] egui unit list: bottom strip with type icons, clickable
- [x] Click-to-select: screen→hex coordinate conversion + unit detection
- [x] Debug test tool: CLI scenario builder (province select, ASCII terrain/deployment map, unit stats) — `tactical-debug`

The 2D `tactical-ui` above lived in the legacy `tactical/` workspace (removed). The current `forward-command` workspace shipped the 3D renderer
(`tactical3d-render`) plus the gameplay/UI overhaul (pre-order movement, radial command menu, hover cards, static command panel,
support-company attachment, main menu, battle-report tour, true-scale maps
with rivers/cities). See §9 for the current UI.

### Phase 3: HOI4 Mod — IN PROGRESS
- [x] `tac_entry.txt` decision (clicked in the decisions tab — no hotkey)
- [x] `game.log` JSON Lines output in `complete_effect` — **placeholder values** (`province=0`, `leader_id=0`, hardcoded `attack_dirs`)
- [x] Variable tagging (`tac_in_battle=1`) per division
- [x] `d_tac_apply_damage`, `d_tac_sync_hourly`, `d_tac_end_battle`, `d_tac_collapse` (§6.11) scripted effects
- [x] `tac_sync.1`, `tac_complete.99`, `tac_abort.1` events
- [x] "Force Exit Tactical Mode" abort decision
- [x] Heartbeat via `on_hourly` — placeholder `hour=0`
- [x] **Player battle selection**: ONE state-target entry decision with script-computed frontier candidates (both geographies OR'd in `target_trigger` FROM scope) — no injection round-trip (the refresh loop removed: decision re-evaluation is daily-pulse only); post-click snapshot resolves `tac_pick=1` → real contested province; quiet-state pick fires the in-game `tac_quiet.1` popup + reset instead of launching; side resolved from the save's `land_combat` (attack/defend split reverted — territory ≠ side); hidden from the decisions list via `on_map_mode = map_only` (map-icon over candidate states only) and renamed with the target state (`[FROM.GetName]`); multi-battle picked states go through the external picker (menu dialog / CLI numbered prompt, `PickResolution::Multiple`) instead of silently taking the largest
- [ ] Real `tac_start` context values: ~~province id~~ (via pick resolution); leader id + attack dirs still placeholders
- [x] `save_as_binary=no` check: `install-mod.ps1` warns on `save_as_binary=yes`; menu Settings page probes it live with a one-click fixer (`force_text_saves` in tactical3d-bin/settings.rs)
- [ ] Integration testing (full loop: trigger → launch → play → sync → apply → resume) — blocked on the placeholder values above

**Scheduling:** the mod placeholder wiring
(real `tac_start` context, heartbeat hour, province-checked
`tac_in_battle` tagging, `tac_enemy_tactic`) MUST land before the first
full live-loop test. All other designed-but-unimplemented items —
§6.4 partial-encirclement movement restriction, §6.4 full-encirclement
equipment capture, §6.7 supply, and the engineer/logistics/
military-police attachment effects (§6.8) — are explicitly deferred to
AFTER that first full live-loop test. (The signal-company effect shipped with §6.13.)

### Phase 4: Polish
- [x] Support companies — shipped as battalion ATTACHMENTS, not independent map units
- [x] Division headquarters & chain of command (§6.13)
- [x] Flag-capture victory (siege conclusion, §6.11/§7.3): flag zones, control-ratio progress, collapse + retreat flow (zone-rim BFS, §6.8), flag-aware AI, progress-bar UI, `d_tac_collapse` injection
- [x] Doctrine/tech integration — doctrine factors + researched techs applied in the save→unit pipeline (§5)
- [ ] Allied support request system
- [~] Allied (friendly-nation) AI in tactical battles — **P1 + P2 (§7.5)** — scripted battles (per-nation planners, division-granularity control split, player may issue division orders to allied HQs, deployment suggestions); P2: menu nation selector (`div_control=` overrides), per-tag base-plate colors, headless split evaluation (Narvik battery 10/10); live interim: NO allied-AI split — allied divisions assemble under the player's direct command with per-tag damage write-back lines (§7.5); remaining: P3 live-mode interface (multi-tag `tac_start`, war-relation roster split, allied-AI takeover slices, optional command-mode setting)
- [ ] Advanced stances + movement modes
- [ ] Balance testing
- [ ] Installer / auto-config utility (mod half covered by `install-mod.ps1`)
- [ ] Runtime data extraction — the exe reads the HOI4 install directly, replacing the pre-extracted `data/` tables (also removes `data/`+`extractor/` from the public release)
- [ ] Open-source release — GitHub public repo + Actions-built release exe + GPL-3.0 (scheduled after the first full live-loop test)

---

## 14. Technical Risks (revised)

| Risk | Mitigation |
|------|------------|
| `save_as_binary=no` not set | `install-mod.ps1` inspects `settings.txt` and warns when binary saves are on; the player toggles the setting manually (no auto-fixer exists) |
| Console injection timing | `SetForegroundWindow` + generous sleep (50-150ms); test across hardware; `--dry` injector mode writes batch files without `SendInput` |
| jomini save parsing edge cases | jomini is the ONLY parser (no regex fallback exists); mitigated by the text-save requirement, table validation, and HOI4-install smoke tests (`--ignored`) |
| External program crash | Manual abort via HOI4 decision; re-enter tactical mode for fresh state; the main menu stays resident while a battle child process runs |
| Multiplayer incompatibility | MP is UNSUPPORTED by design (single-player feature); note: the mod has no `is_ai`/MP guard — players are simply told not to use it in MP |
| Windows-only SendInput | External program targets Windows; no Linux support initially |
| `provinces.bmp` loading | Load once and cache on program startup (13k provinces indexed in memory) |
| High-latitude distortion | None — the bitmap is used verbatim at a uniform scale (§4.3 revised); grid shape matches the in-game silhouette at any latitude |

---

## 15. Localization

The UI ships English + Simplified Chinese, switchable on the Settings page,
in the HOI4 key-value localisation idiom so players can customize strings or
add languages without recompiling.

### 15.1 String tables

- **Crate:** `tactical-locale` (zero dependencies, hand-rolled parser).
- **File layout:** `localisation/<language>/*.yml` using the HOI4 modding
  format — `l_english:` / `l_simp_chinese:` header, `key:0 "text"` lines,
  `#` comments, `\"`/`\n` escapes. The version number is accepted but
  ignored; load order (embedded → external files, alphabetical, later wins)
  decides overrides. Shipping files are split by domain:
  `00-core` (menu/settings/splash/titles), `10-battle-ui` (battle egui),
  `20-battle-log` (log/notice/floater templates), `30-names` (enum display
  names: `unit_type.*`, `terrain.*`, `side.*`, `state.*`, `phase.*`,
  `attr.*`, `tactic.<id>.name|desc|hint`, `attack_kind.*`, `outcome.*`;
  plus generated-name labels: `unit_abbrev.*` for the battalion
  counter names + special subunit tokens, `support_abbrev.*` for support
  tags).
- **Embedded + override:** both languages are compiled in via `include_str!`
  (the exe runs standalone); files in the external `localisation/` dir
  override them key-by-key — that is the user-customization / third-language
  entry point. Missing keys fall back active language → English → the raw
  key itself (HOI4-style visible miss marker).
- **Lookup:** `Locale::tr(key)` for literals; `Locale::trf(key, &[(name, val)])`
  substitutes `{name}` placeholders — word order differs across languages,
  so positional `format!` is not used for translated text. Enum names derive
  keys from the variant's Debug name via `camel_to_snake` (e.g.
  `MotRocketArtillery` → `unit_type.mot_rocket_artillery`); the old Rust
  `name()`/`Display` impls stay as the English fallback for tests/CLI.
- **Generated battalion names:** save-assembled counter names
  ("1. Inf" / "1. 步兵"), support-attachment tags and the synthesized HQ
  label are baked at assembly time from a `UnitNaming` table
  (tactical-save — plain data, the crate stays locale-free); tactical3d-bin
  overlays the session language from `unit_abbrev.*` / `support_abbrev.*`
  (English file = the legacy abbreviations, byte-identical to before).
  Script/demo/preset battles keep their hand-written historical names.
- **HOI4's own names:** VP city names are read from the
  install's matching localisation file — `victory_points_l_simp_chinese.yml`
  for a Chinese session, the English file otherwise/as fallback — so the
  floating label and the multi-battle picker show the same language the
  player sees in HOI4.
- **Switching:** `settings.json` field `language` (`"en"`/`"zh"`). The
  Settings page radio applies instantly (the `LocaleRes` resource is rebuilt
  and the menu re-renders next frame) and persists immediately; battle child
  processes (`--battle`/`--livebattle`/`--demo`) read it at startup.
- **Invariants (unit tests):** the embedded en/zh files must define the same
  key set with non-empty values; every enum variant must resolve to a real
  key. A missing translation is a build-time failure, not on-screen tofu.

### 15.2 Fonts and icons

- **CJK font chain** (egui `Proportional`/`Monospace` families, appended
  after the built-ins, installed in BOTH languages — HOI4 saves carry
  arbitrary-language division names): system `msyh.ttc` (Microsoft YaHei,
  TTC face 0) → bundled `assets/fonts/NotoSansSC-Subset.otf` (~1.6 MB,
  SIL OFL; ASCII + CJK punctuation + GB2312 level-1 hanzi + every char used
  in the localisation files; regenerate with `tools/make_font_subset.py`)
  → system `simhei.ttf`/`simsun.ttc`. Unreadable candidates skip silently.
- **Icons, not emoji fonts:** egui rasterizes glyphs as alpha masks, so
  color emoji fonts cannot work (and `egui-twemoji` was rejected — it
  embeds all ~3700 Twemoji into the exe for the ~26 needed). Instead ~26
  vendored Twemoji PNGs (`assets/icons/`, CC-BY 4.0, see ATTRIBUTION.md)
  are compiled in and uploaded as egui textures; `IconSet` composes
  icon+text buttons/labels (egui text cannot embed images). Battle-log
  lines carry an optional leading icon (`LogLine{icon, text}`).
- **Limitations (deliberate):** already-written battle-log lines are not
  retro-translated on a mid-battle language switch; dynamic names from
  saves/scripts (division names, hand-written script rosters) pass through
  untranslated — VP city names and generated battalion names DO follow the
  UI language (above);
  CLI/stdout (`--live`, `--headless`, `--debug` stdin) stays English;
  scenario/script internal error strings stay English; the single-letter
  stance glyphs (H/E/S/W/R/L/F) are icons, not words, and are not
  translated.

---

## 16. Design Decisions Summary (renumbered from §15; revised)

| Decision | Choice |
|----------|--------|
| Architecture | External Rust program (tactical engine) + minimal HOI4 mod |
| Tech stack | Rust + Bevy (render) + egui (UI) + jomini (save parse) |
| Data source | Save file (division composition) + game.log (triggers) |
| Data ingress | Background PostMessage → `run tac_inject_<pid>.txt` (per-process batch in the HOI4 user dir; injector default name `tac_inject.txt`): literal `eval_effect damage_units = {…}` lines + scope-pinned scripted-effect calls (incl. the `d_tac_sync_hourly` ack — no direct `event` calls: console-fired events ignore `hidden = yes`); legacy SendInput path = fallback |
| Data egress | JSON Lines in `game.log` via mod `log` effect |
| Injection targeting | `tac_in_battle=1` variable on divisions, set at tactical entry |
| Province grid | Minimum bounding rectangle + occupancy mask → hex grid |
| Hex scale | 1km/hex, bitmap scale capped at 512×512 |
| Multi-direction attack | Dynamic frontlines: attacker zones on direction edges, defender in center |
| Unit stats | All game data pre-calculated into base stats at launch; only wargame-internal effects = tactical buffs |
| Equipment loss | None computed on our side — injection reports org/str damage ratios only; `damage_units` deducts equipment HOI4-side (§6.8) |
| Supply | Strategic supply baseline + progressive encirclement penalty |
| Deployment | Player battalions start OFF the board in the OOB: place each via OOB row → zone hex click, or [Auto Deploy] to the AI planner; OOB button badge counts the remainder; [Begin Battle] refused while any is undeployed; drag-adjust after placement; enemy AI deploys only at [Begin Battle], out of sight |
| Engagement | Manual trigger only (no auto-engagement on adjacency) |
| Stacking | 1 battalion per hex |
| Support companies | NOT map units — attached to a host battalion, stat bonuses baked in, ride/die with the host |
| Assault | Close combat + move into eliminated enemy hex |
| Fire Support | Ranged; precise fire on a right-clicked visible enemy, area fire (4/10-1/10 weighted zone) via the F button |
| Hold | Take cover: +25% def, costs the turn, drops on move/attack; entrenchment layers from strategic value |
| Retreat | Manual: radial **R** button, contact-only disengagement (-20% org, clears entrenchment, auto-withdraws one hex/turn). Involuntary (org=0): disordered rout to own edge; retreating units stay targetable (pursuit) |
| Turn order | Attacker first |
| Time | 6 tactical turns (10min each) = 1 strategic hour |
| Sync | Every 6 turns → console injection of damage values; strategic clock +1h via `pause_in_hours 1` under the state freeze (§8.4) |
| Combat formulas | Hit-step × numbers-squared model + precision factors + unified end-of-turn fire phase (superseded the Lanchester-denominator form); vanilla hit rates 10%/40% as a soft step, deterministic |
| AI | 3-layer decision tree; tactic card → strategic objective → unit assignment → action execution |
| Fog of war | Per-unit sight range, stale intel for previously-observed hexes |
| Advance elements (MVP) | Fortifications + air support = global buffs, not full implementation |
| UI | 3D scene + static command panel + cursor hover card + radial command menu; floating Minimap / Game Log / Order-of-Battle windows; battle-report and sync modals |
| Window | Normal decorated 1440×900 window; borderless + topmost planned (open) |
| Recovery | Manual "Force Exit" decision; no auto-recovery |
| Multi-division | Show all friendly divisions; player commands own only |
| Project structure | One Rust workspace `forward-command/` — tactical-core, -combat, -sync, -map, -save, -ai, -inject, -listen, -locale, tactical3d-render, tactical3d-bin (legacy `tactical/` 2D workspace removed) |
| Localization | HOI4-style key-value files (embedded + external override), en/zh, Settings-page switch; CJK font chain + vendored Twemoji icon textures (§15) |
| Data pre-extraction | 6 JSON tables: equipment, units, terrain, tactics, doctrines, country colors |
| Mod role | Trigger detection + variable tagging + scripted effects/events hosting injection + hourly heartbeat |
| Time limit | None — battle plays to resolution |

---

## 17. Reference Files (renumbered from §16; revised)

| File | Purpose |
|------|---------|
| `map/provinces.bmp` | Province pixel map (13,413 provinces) |
| `map/definition.csv` | Province ID → terrain/RGB/coastal/continent mapping |
| `map/adjacencies.csv` | River/strait connections between provinces |
| `map/rivers.bmp` | River pixel overlay — river hexes on the tactical grid |
| `map/unitstacks.txt` | Index-0 unit-stack positions — anchor VP city placement |
| `map/default.map` | Master map file index — NOT read by the external program |
| `history/states/*.txt` | `victory_points = { <pid> <level> }` blocks → VP levels per province |
| `localisation/<lang>/victory_points_l_<lang>.yml` | VP display names for the floating city label (UI language, English fallback) |
| `common/units/*.txt` | Battalion template definitions (attributes, equipment needs) |
| `common/units/equipment/*.txt` | Equipment archetype definitions (attack/defense/armor stats) |
| `common/terrain/00_terrain.txt` | Terrain categories with combat modifiers |
| `common/combat_tactics.txt` | Tactic definitions (effects, counters, conditions) |
| `common/defines/00_defines.lua` | `MAP_SCALE_PIXEL_TO_KM`, all combat parameters |
| `common/doctrines/*.txt` | Doctrine tree with stat bonuses |
| `common/technologies/*.txt` | Technology tree with equipment stat upgrades |
| `common/countries/colors.txt` | Country map colors → unit base plates / deploy-zone borders |
| `documentation/effects_documentation.md` | Modding effects API reference |
| `documentation/triggers_documentation.md` | Modding triggers API reference |
| `documentation/console_commands_documentation.md` | Console command reference |
| `documentation/dynamic_variables_documentation.md` | Dynamic variable reference |
| `%USERPROFILE%/Documents/Paradox Interactive/Hearts of Iron IV/save games/` | Save file directory |
| `%USERPROFILE%/Documents/Paradox Interactive/Hearts of Iron IV/logs/game.log` | Log file |

