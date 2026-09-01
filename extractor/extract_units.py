"""Extract unit templates from HOI4 battalion definition files."""
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
UNITS_DIR = os.path.join(GAME_DIR, "common", "units")
OUT_FILE = os.path.join(OUT_DIR, "unit_templates.json")

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


TERRAIN_NAMES = {
    "forest", "hills", "mountain", "plains", "urban", "jungle", "marsh",
    "desert", "river", "amphibious", "fort"
}


def extract_subunit(name, data):
    unit = {}
    unit["max_strength"] = data.get("max_strength", 0)
    unit["max_organisation"] = data.get("max_organisation", 0)
    unit["default_morale"] = data.get("default_morale", 0)
    unit["combat_width"] = data.get("combat_width", 0)
    unit["manpower"] = data.get("manpower", 0)
    unit["training_time"] = data.get("training_time", 0)
    unit["supply_consumption"] = data.get("supply_consumption", 0)
    unit["weight"] = data.get("weight", 0)

    group = data.get("group", None)
    unit["group"] = group

    raw_type = data.get("type", None)
    types = []
    if isinstance(raw_type, list):
        types = [t for t in raw_type if isinstance(t, str)]
    elif isinstance(raw_type, str):
        types = [raw_type]
    unit["types"] = types

    reg = data.get("regimental", None)
    if reg is not None:
        unit["regimental"] = reg

    need = data.get("need", {})
    if isinstance(need, dict):
        unit["needs"] = {k: v for k, v in need.items() if isinstance(v, (int, float))}
    else:
        unit["needs"] = {}

    essential = data.get("essential", None)
    if essential:
        unit["essential"] = essential

    terrain_mods = {}
    for terrain in TERRAIN_NAMES:
        terrain_data = data.get(terrain, None)
        if isinstance(terrain_data, dict):
            mods = {}
            for stat in ("attack", "defense", "defence", "movement"):
                val = terrain_data.get(stat)
                if isinstance(val, (int, float)):
                    if stat == "defence":
                        mods["defense"] = float(val)
                    else:
                        mods[stat] = float(val)
            if mods:
                terrain_mods[terrain] = mods
    unit["terrain_modifiers"] = terrain_mods

    abbrev = data.get("abbreviation", None)
    if abbrev:
        unit["abbreviation"] = abbrev

    categories_raw = data.get("categories", None)
    if isinstance(categories_raw, list):
        unit["categories"] = categories_raw

    return unit


def parse_file(filepath):
    with open(filepath, "r", encoding="utf-8-sig") as f:
        text = f.read()
    tokens = tokenize(text)
    idx = 0
    while idx < len(tokens):
        if tokens[idx] == "sub_units":
            idx += 2
            sub_units, idx = parse_val(tokens, idx)
            return sub_units if isinstance(sub_units, dict) else {}
        idx += 1
    return {}


def collect_unit_files(base_dir):
    files = []
    for f in os.listdir(base_dir):
        fpath = os.path.join(base_dir, f)
        if os.path.isfile(fpath) and f.endswith(".txt"):
            files.append(fpath)
    return files


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    files = collect_unit_files(UNITS_DIR)
    all_units = {}
    support_units = {}
    errors = []

    for fpath in files:
        try:
            data = parse_file(fpath)
            if not isinstance(data, dict):
                continue
            for name, sub_data in data.items():
                if not isinstance(sub_data, dict):
                    continue
                entry = extract_subunit(name, sub_data)
                is_support = (sub_data.get("regimental") == False or
                              sub_data.get("combat_width") == 0 or
                              "support" in (sub_data.get("group") or ""))
                if is_support:
                    support_units[name] = entry
                else:
                    all_units[name] = entry
        except Exception as e:
            errors.append((os.path.basename(fpath), str(e)))

    combined = {
        "line_battalions": all_units,
        "support_companies": support_units,
    }

    with open(OUT_FILE, "w", encoding="utf-8") as f:
        json.dump(combined, f, indent=2, ensure_ascii=False)

    print(f"Extracted {len(all_units)} line battalions + {len(support_units)} support companies")
    print(f"From {len(files)} files")
    print(f"Output: {OUT_FILE}")
    if errors:
        print(f"\nErrors ({len(errors)}):")
        for fn, err in errors:
            print(f"  {fn}: {err}")


if __name__ == "__main__":
    main()
