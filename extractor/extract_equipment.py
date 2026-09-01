"""Extract equipment stats from HOI4 equipment definition files."""
import argparse
import json
import os
import re
import sys

_parser = argparse.ArgumentParser(description=__doc__)
_parser.add_argument("--hoi4-dir", default=os.environ.get("HOI4_DIR", r"D:\Steam\steamapps\common\Hearts of Iron IV"),
                     help="HOI4 install directory (or set HOI4_DIR env var)")
_parser.add_argument("--out", default=r"D:\OpenCode\Forward Command\forward-command\data",
                     help="output directory for the extracted JSON tables")
_args = _parser.parse_args()
GAME_DIR = _args.hoi4_dir
OUT_DIR = _args.out
EQUIP_DIR = os.path.join(GAME_DIR, "common", "units", "equipment")
OUT_FILE = os.path.join(OUT_DIR, "equipment_stats.json")

COMPARISON_OPS = frozenset({">", "<", ">=", "<=", "!=", "=="})


def tokenize(text):
    tokens = []
    i = 0
    while i < len(text):
        c = text[i]
        if c in " \t\r\n":
            i += 1
            continue
        if c == "#":
            j = text.find("\n", i)
            if j == -1:
                break
            i = j + 1
            continue
        if c == "{":
            tokens.append("{")
            i += 1
            continue
        if c == "}":
            tokens.append("}")
            i += 1
            continue
        if c in "=":
            tokens.append("=")
            i += 1
            continue
        if c == '"':
            j = text.index('"', i + 1)
            tokens.append(text[i:j + 1])
            i = j + 1
            continue
        if c.isdigit() or (c == '-' and i + 1 < len(text) and text[i + 1].isdigit()):
            j = i + 1
            is_float = False
            while j < len(text) and (text[j].isdigit() or text[j] == '.'):
                if text[j] == '.':
                    is_float = True
                j += 1
            val = text[i:j]
            tokens.append(float(val) if is_float else int(val))
            i = j
            continue
        if c.isalpha() or c in "_<>!?:":
            j = i
            while j < len(text) and (text[j].isalpha() or text[j].isdigit() or text[j] in "_.-<>!?:@[]"):
                j += 1
            tokens.append(text[i:j])
            i = j
            continue
        i += 1
    return tokens


def is_simple_list(tokens, pos):
    save = pos
    count = 0
    while pos < len(tokens) and tokens[pos] != "}":
        t = tokens[pos]
        if isinstance(t, str) and not t.startswith('"') and not t in ("=", "{", "}"):
            if t in COMPARISON_OPS:
                return False, save
            count += 1
            pos += 1
            continue
        if isinstance(t, (int, float)):
            count += 1
            pos += 1
            continue
        return False, save
    if count == 0:
        return False, save
    if pos < len(tokens) and tokens[pos] == "}":
        return True, save
    return False, save


def parse_val(tokens, pos):
    if pos >= len(tokens):
        return None, pos
    t = tokens[pos]
    if t == "{":
        pos += 1
        if pos < len(tokens) and tokens[pos] == "}":
            return {}, pos + 1
        is_list, _ = is_simple_list(tokens, pos)
        if is_list:
            result = []
            while pos < len(tokens) and tokens[pos] != "}":
                val = tokens[pos]
                if isinstance(val, str) and val.startswith('"'):
                    result.append(val.strip('"'))
                elif isinstance(val, str):
                    result.append(val)
                elif isinstance(val, (int, float)):
                    result.append(val)
                pos += 1
            if pos < len(tokens):
                pos += 1
            return result, pos
        result = {}
        while pos < len(tokens) and tokens[pos] != "}":
            if isinstance(tokens[pos], str):
                key = tokens[pos]
                pos += 1
                if pos < len(tokens) and isinstance(tokens[pos], str) and tokens[pos] in COMPARISON_OPS:
                    op = tokens[pos]
                    pos += 1
                    val, pos = parse_val(tokens, pos)
                    result[f"{key} {op}"] = val
                elif pos < len(tokens) and tokens[pos] == "=":
                    pos += 1
                    val, pos = parse_val(tokens, pos)
                    result[key] = val
                elif pos < len(tokens) and tokens[pos] in ("{",):
                    val, pos = parse_val(tokens, pos)
                    result[key] = val
                elif pos < len(tokens) and isinstance(tokens[pos], (int, float)):
                    result[key] = tokens[pos]
                    pos += 1
                else:
                    result[key] = None
            else:
                pos += 1
        if pos < len(tokens):
            pos += 1
        return result, pos
    elif isinstance(t, str) and t.startswith('"'):
        return t.strip('"'), pos + 1
    elif isinstance(t, (int, float)):
        return t, pos + 1
    elif isinstance(t, str):
        if t in ("yes", "true"):
            return True, pos + 1
        elif t in ("no", "false"):
            return False, pos + 1
        return t, pos + 1
    else:
        return t, pos + 1


def parse_file(filepath):
    with open(filepath, "r", encoding="utf-8-sig") as f:
        text = f.read()
    tokens = tokenize(text)
    idx = 0
    while idx < len(tokens):
        if tokens[idx] == "equipments":
            idx += 2
            equipments_block, idx = parse_val(tokens, idx)
            return equipments_block if isinstance(equipments_block, dict) else {}
        idx += 1
    return {}


def collect_equipment_files(base_dir):
    files = []
    for root, dirs, filenames in os.walk(base_dir):
        for f in filenames:
            if f.endswith(".txt"):
                files.append(os.path.join(root, f))
    return files


STAT_KEYS = [
    "soft_attack", "hard_attack", "defense", "breakthrough",
    "armor_value", "hardness", "maximum_speed", "reliability",
    "ap_attack", "air_attack", "supply_consumption",
    "build_cost_ic", "lend_lease_cost", "fuel_consumption",
]


def resolve_archetypes(equipments):
    archetypes = {}
    for name, data in equipments.items():
        if data.get("is_archetype"):
            archetypes[name] = data

    for name, equipment in equipments.items():
        archetype_name = equipment.get("archetype")
        if not archetype_name:
            if equipment.get("is_archetype"):
                continue
            continue
        archetype = archetypes.get(archetype_name)
        if not archetype:
            continue
        for stat in STAT_KEYS:
            if stat not in equipment:
                if stat in archetype:
                    equipment[stat] = archetype[stat]

        raw_type = equipment.get("type")
        if not raw_type:
            equipment["type"] = archetype.get("type", [])

        if "resources" not in equipment:
            equipment["resources"] = archetype.get("resources", {})

        if "reliability" not in equipment:
            equipment["reliability"] = archetype.get("reliability", 0)

        if "interface_category" not in equipment:
            equipment["interface_category"] = archetype.get("interface_category", None)

        if "can_convert_from" not in equipment:
            equipment["can_convert_from"] = archetype.get("can_convert_from", None)

        if "group_by" not in equipment:
            equipment["group_by"] = archetype.get("group_by", None)

    return equipments


def extract_equipment(equip_dict, name):
    result = {}
    result["archetype"] = equip_dict.get("archetype", None)
    result["year"] = equip_dict.get("year", None)

    result["soft_attack"] = equip_dict.get("soft_attack", 0)
    result["hard_attack"] = equip_dict.get("hard_attack", 0)
    result["defense"] = equip_dict.get("defense", 0)
    result["breakthrough"] = equip_dict.get("breakthrough", 0)
    result["armor"] = equip_dict.get("armor_value", 0)
    result["piercing"] = equip_dict.get("ap_attack", 0)
    result["hardness"] = equip_dict.get("hardness", 0)
    result["max_speed"] = equip_dict.get("maximum_speed", 0)
    result["reliability"] = equip_dict.get("reliability", 0)
    result["supply_use"] = equip_dict.get("supply_consumption", 0)

    build_cost = equip_dict.get("build_cost_ic", 0)
    if isinstance(build_cost, (int, float)):
        result["build_cost"] = float(build_cost)
    else:
        result["build_cost"] = 0

    resources = equip_dict.get("resources", {})
    if isinstance(resources, dict):
        result["resources"] = {k: v for k, v in resources.items() if isinstance(v, (int, float))}
    else:
        result["resources"] = {}

    raw_type = equip_dict.get("type", None)
    categories = []
    if isinstance(raw_type, list):
        categories = [t for t in raw_type if isinstance(t, str)]
    elif isinstance(raw_type, str):
        categories = [raw_type]

    group = equip_dict.get("group_by", None)
    active = equip_dict.get("active", None)
    can_convert = equip_dict.get("can_convert_from", None)

    result["categories"] = categories
    if group:
        result["group"] = group
    if active is not None:
        result["active"] = active
    if can_convert and isinstance(can_convert, dict):
        result["can_convert_from"] = can_convert

    return result


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    files = collect_equipment_files(EQUIP_DIR)
    all_equipment = {}
    errors = []

    for fpath in files:
        try:
            equip_block = parse_file(fpath)
            if not isinstance(equip_block, dict):
                continue
            for name, data in equip_block.items():
                if not isinstance(data, dict):
                    continue
                all_equipment[name] = data
        except Exception as e:
            errors.append((os.path.basename(fpath), str(e)))

    all_equipment = resolve_archetypes(all_equipment)

    result = {}
    for name, data in all_equipment.items():
        if data.get("is_archetype") and data.get("is_buildable") == False:
            continue
        result[name] = extract_equipment(data, name)

    with open(OUT_FILE, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)

    print(f"Extracted {len(result)} equipment entries from {len(files)} files")
    print(f"Output: {OUT_FILE}")
    if errors:
        print(f"\nErrors ({len(errors)}):")
        for fn, err in errors:
            print(f"  {fn}: {err}")


if __name__ == "__main__":
    main()
