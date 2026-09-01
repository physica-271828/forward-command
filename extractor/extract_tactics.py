"""Extract combat tactics from HOI4 combat_tactics.txt."""
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
TACTICS_FILE = os.path.join(GAME_DIR, "common", "combat_tactics.txt")
OUT_FILE = os.path.join(OUT_DIR, "combat_tactics.json")

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


def name_from_tactic_key(key):
    if key.startswith("tactic_"):
        return key[7:]
    return key


def extract_tactic(name, data):
    result = {}

    attacker_dmg = data.get("attacker", 0)
    defender_dmg = data.get("defender", 0)
    if isinstance(attacker_dmg, (int, float)):
        result["attacker_damage"] = float(attacker_dmg)
    else:
        result["attacker_damage"] = 0
    if isinstance(defender_dmg, (int, float)):
        result["defender_damage"] = float(defender_dmg)
    else:
        result["defender_damage"] = 0

    speed = data.get("attacker_movement_speed", 0)
    if isinstance(speed, (int, float)):
        result["speed_mod"] = float(speed)
    else:
        result["speed_mod"] = 0

    cw = data.get("combat_width", 0)
    if isinstance(cw, (int, float)):
        result["combat_width_mod"] = float(cw)
    else:
        result["combat_width_mod"] = 0

    countering = data.get("countered_by", None)
    if isinstance(countering, str):
        result["countered_by"] = [name_from_tactic_key(countering)]
    elif isinstance(countering, list):
        result["countered_by"] = [name_from_tactic_key(x) for x in countering if isinstance(x, str)]
    else:
        result["countered_by"] = []

    phase = data.get("phase", None)
    if isinstance(phase, str):
        result["phase_change"] = phase if phase != "no" else None
    else:
        result["phase_change"] = None

    active = data.get("active", True)
    result["active"] = active

    is_attacker = data.get("is_attacker", None)
    result["is_attacker"] = is_attacker

    trigger = data.get("trigger", None)
    if isinstance(trigger, dict):
        result["trigger"] = trigger

    org_dmg_mod = data.get("attacker_org_damage_modifier", None)
    if org_dmg_mod is not None and isinstance(org_dmg_mod, (int, float)):
        result["attacker_org_damage_mod"] = float(org_dmg_mod)

    defender_org_dmg = data.get("defender_org_damage_modifier", None)
    if defender_org_dmg is not None and isinstance(defender_org_dmg, (int, float)):
        result["defender_org_damage_mod"] = float(defender_org_dmg)

    return result


def main():
    os.makedirs(OUT_DIR, exist_ok=True)

    with open(TACTICS_FILE, "r", encoding="utf-8-sig") as f:
        text = f.read()

    tokens = tokenize(text)
    idx = 0
    phases = []
    all_tactics = {}
    errors = []

    while idx < len(tokens):
        t = tokens[idx]
        if t == "phases":
            idx += 2
            phases_block, idx = parse_val(tokens, idx)
            if isinstance(phases_block, list):
                phases = phases_block
        elif isinstance(t, str) and t.startswith("tactic_"):
            tactic_name = name_from_tactic_key(t)
            pos = idx + 1
            if pos < len(tokens) and tokens[pos] == "=":
                pos += 1
            if pos < len(tokens) and tokens[pos] == "{":
                tactic_data, pos = parse_val(tokens, pos)
                if isinstance(tactic_data, dict):
                    all_tactics[tactic_name] = extract_tactic(tactic_name, tactic_data)
                idx = pos
            else:
                idx = pos
        else:
            idx += 1

    counter_map = {}
    for tactic_name, tactic_data in all_tactics.items():
        for counter_name in tactic_data.get("countered_by", []):
            if counter_name in all_tactics:
                if counter_name not in counter_map:
                    counter_map[counter_name] = []
                if tactic_name not in counter_map[counter_name]:
                    counter_map[counter_name].append(tactic_name)
            elif counter_name not in counter_map:
                counter_map[counter_name] = [tactic_name]
            else:
                if tactic_name not in counter_map[counter_name]:
                    counter_map[counter_name].append(tactic_name)

    for name, data in all_tactics.items():
        if name in counter_map:
            data["counters"] = counter_map[name]
        else:
            data["counters"] = []

    output = {
        "phases": phases,
        "tactics": all_tactics,
    }

    with open(OUT_FILE, "w", encoding="utf-8") as f:
        json.dump(output, f, indent=2)

    print(f"Extracted {len(all_tactics)} combat tactics (plus {len(phases)} phases)")
    print(f"Output: {OUT_FILE}")


if __name__ == "__main__":
    main()
