"""Extract terrain modifiers from HOI4 terrain definition file."""
import argparse
import json
import os
import sys

_parser = argparse.ArgumentParser(description=__doc__)
_parser.add_argument("--hoi4-dir", default=os.environ.get("HOI4_DIR", r"D:\Steam\steamapps\common\Hearts of Iron IV"),
                     help="HOI4 install directory (or set HOI4_DIR env var)")
_parser.add_argument("--out", default=r"D:\OpenCode\Forward Command\forward-command\data",
                     help="output directory for the extracted JSON tables")
_args = _parser.parse_args()
GAME_DIR = _args.hoi4_dir
OUT_DIR = _args.out
TERRAIN_FILE = os.path.join(GAME_DIR, "common", "terrain", "00_terrain.txt")
OUT_FILE = os.path.join(OUT_DIR, "terrain_mods.json")

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


def extract_category(name, data):
    result = {}
    result["movement_cost"] = data.get("movement_cost", 0)
    result["combat_width"] = data.get("combat_width", 0)
    result["combat_support_width"] = data.get("combat_support_width", 0)

    units_mod = data.get("units", {})
    if isinstance(units_mod, dict):
        attack = units_mod.get("attack", 0)
        defense = units_mod.get("defence", 0)
        movement = units_mod.get("movement", 0)
    else:
        attack = 0
        defense = 0
        movement = 0

    result["attack_mod"] = float(attack) if isinstance(attack, (int, float)) else 0
    result["defense_mod"] = float(defense) if isinstance(defense, (int, float)) else 0

    if movement:
        result["movement_mod"] = float(movement) if isinstance(movement, (int, float)) else 0

    enemy_air = data.get("enemy_army_bonus_air_superiority_factor", None)
    if enemy_air is not None and isinstance(enemy_air, (int, float)):
        result["enemy_air_sup_bonus"] = float(enemy_air)

    attrition = data.get("attrition", None)
    if attrition is not None and isinstance(attrition, (int, float)):
        result["attrition"] = float(attrition)

    truck_attr = data.get("truck_attrition_factor", None)
    if truck_attr is not None and isinstance(truck_attr, (int, float)):
        result["truck_attrition_factor"] = float(truck_attr)

    supply_flow = data.get("supply_flow_penalty_factor", None)
    if supply_flow is not None and isinstance(supply_flow, (int, float)):
        result["supply_flow_penalty_factor"] = float(supply_flow)

    sickness = data.get("sickness_chance", None)
    if sickness is not None and isinstance(sickness, (int, float)):
        result["sickness_chance"] = float(sickness)

    return result


def main():
    os.makedirs(OUT_DIR, exist_ok=True)

    with open(TERRAIN_FILE, "r", encoding="utf-8-sig") as f:
        text = f.read()

    tokens = tokenize(text)
    idx = 0
    categories = {}
    terrain_graphics = {}

    while idx < len(tokens):
        if tokens[idx] == "categories":
            idx += 2
            cats_block, idx = parse_val(tokens, idx)
            if isinstance(cats_block, dict):
                categories = cats_block
        elif tokens[idx] == "terrain":
            idx += 2
            terr_block, idx = parse_val(tokens, idx)
            if isinstance(terr_block, dict):
                terrain_graphics = terr_block
        else:
            idx += 1

    result = {}
    for name, data in categories.items():
        if not isinstance(data, dict):
            continue
        result[name] = extract_category(name, data)

    with open(OUT_FILE, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2)

    print(f"Extracted {len(result)} terrain categories")
    print(f"Output: {OUT_FILE}")


if __name__ == "__main__":
    main()
