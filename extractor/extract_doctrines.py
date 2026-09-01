"""Extract doctrine bonuses from HOI4 doctrine files."""
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
DOCTRINES_DIR = os.path.join(GAME_DIR, "common", "doctrines")
OUT_FILE = os.path.join(OUT_DIR, "doctrine_bonuses.json")

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


COMBAT_STATS = {
    "soft_attack", "hard_attack", "defense", "breakthrough",
    "armor_value", "ap_attack", "hardness", "maximum_speed",
    "reliability", "max_organisation", "default_morale", "max_strength",
    "org_loss_when_moving", "land_reinforce_rate", "supply_consumption",
    "planning_speed", "army_speed_factor", "max_planning", "initiative",
    "entrenchment", "recon_factor", "weight", "toughness",
    "suppression", "recovery_rate", "training_time",
    "org_loss_at_low_org_factor", "additional_brigade_column_size",
}


def extract_modifiers(data, depth=0):
    mods = {}

    for key, val in data.items():
        if key == "enable_tactic":
            if isinstance(val, str):
                mods.setdefault("enable_tactics", []).append(val)
            elif isinstance(val, list):
                mods.setdefault("enable_tactics", []).extend(v for v in val if isinstance(v, str))
        elif key in COMBAT_STATS and isinstance(val, (int, float)):
            mods[key + "_factor"] = float(val)
        elif isinstance(val, dict):
            if any(k in val for k in COMBAT_STATS | {"attack", "defence", "movement"}):
                for sub_key, sub_val in val.items():
                    if sub_key in COMBAT_STATS and isinstance(sub_val, (int, float)):
                        mods.setdefault(sub_key + "_factor", 0)
            elif key == "rewards":
                for reward_name, reward_data in val.items():
                    if isinstance(reward_data, dict):
                        reward_mods = extract_modifiers(reward_data, depth + 1)
                        mods.setdefault("rewards", {})[reward_name] = reward_mods
            elif key == "milestones":
                if isinstance(val, list):
                    milestones = []
                    for i, ms in enumerate(val):
                        if isinstance(ms, dict):
                            ms_mods = extract_modifiers(ms, depth + 1)
                            milestones.append(ms_mods)
                    mods["milestones"] = milestones

    if "enable_tactics" in mods:
        seen = set()
        unique = []
        for t in mods["enable_tactics"]:
            if t not in seen:
                seen.add(t)
                unique.append(t)
        mods["enable_tactics"] = unique

    return mods


def is_doctrine_name(name):
    if not isinstance(name, str):
        return False
    if not name:
        return False
    if name[0] not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_":
        return False
    if name in ("folder", "folders", "track", "tracks"):
        return False
    return True


def parse_doctrine_file(filepath):
    with open(filepath, "r", encoding="utf-8-sig") as f:
        text = f.read()

    tokens = tokenize(text)
    idx = 0
    result = {}
    while idx < len(tokens):
        if isinstance(tokens[idx], str) and idx + 1 < len(tokens) and tokens[idx + 1] == "=":
            key = tokens[idx]
            idx += 2
            val, idx = parse_val(tokens, idx)
            if is_doctrine_name(key):
                result[key] = val
        elif isinstance(tokens[idx], str) and idx + 1 < len(tokens) and tokens[idx + 1] == "{":
            key = tokens[idx]
            idx += 1
            val, idx = parse_val(tokens, idx)
            if is_doctrine_name(key):
                result[key] = val
        else:
            idx += 1
    return result


def collect_doctrine_files(base_dir):
    files = []
    for root, dirs, filenames in os.walk(base_dir):
        for f in filenames:
            if f.endswith(".txt"):
                files.append(os.path.join(root, f))
    return files


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    files = collect_doctrine_files(DOCTRINES_DIR)
    result = {}
    errors = []

    for fpath in files:
        try:
            data = parse_doctrine_file(fpath)
            relpath = os.path.relpath(fpath, DOCTRINES_DIR)
            for name, node_data in data.items():
                if not isinstance(node_data, dict):
                    continue
                if name in ("folder", "folders", "track"):
                    continue
                node = {}

                for sub_key, sub_val in node_data.items():
                    if sub_key == "enable_tactic":
                        if isinstance(sub_val, str):
                            node.setdefault("enable_tactics", []).append(sub_val)
                        elif isinstance(sub_val, list):
                            node.setdefault("enable_tactics", []).extend(
                                v for v in sub_val if isinstance(v, str))
                    elif isinstance(sub_val, (int, float)):
                        node[sub_key] = float(sub_val)
                    elif isinstance(sub_val, dict) and sub_key in ("category_all_infantry",
                                                                   "category_light_infantry",
                                                                   "category_all_armor",
                                                                   "category_tanks",
                                                                   "category_army",
                                                                   "category_front_line",
                                                                   "category_support_battalions",
                                                                   "category_infantry",
                                                                   "category_line_artillery",
                                                                   "category_artillery",
                                                                   "category_cavalry",
                                                                   "category_vehicle_infantry",
                                                                   "category_regimental_support_artillery",
                                                                   "category_motorized",
                                                                   "category_mechanized",
                                                                   "category_special_forces",
                                                                   "category_marines",
                                                                   "category_regimental_support_battalions",
                                                                   "category_divisional_support_battalions",
                                                                   "category_infantry_and_bicycle",
                                                                   "category_marines_and_amphibious",
                                                                   "category_special_forces_leg_infantry"):
                        target = sub_key.replace("category_", "")
                        node.setdefault("category_modifiers", {})
                        for mod_key, mod_val in sub_val.items():
                            if isinstance(mod_val, (int, float)):
                                node["category_modifiers"].setdefault(target, {})[mod_key] = float(mod_val)
                    elif sub_key == "milestones":
                        if isinstance(sub_val, list):
                            milestones = []
                            for ms in sub_val:
                                if isinstance(ms, dict):
                                    ms_mods = {}
                                    for mk, mv in ms.items():
                                        if mk == "enable_tactic":
                                            if isinstance(mv, str):
                                                ms_mods.setdefault("enable_tactics", []).append(mv)
                                            elif isinstance(mv, list):
                                                ms_mods.setdefault("enable_tactics", []).extend(
                                                    v for v in mv if isinstance(v, str))
                                        elif isinstance(mv, (int, float)):
                                            ms_mods[mk] = float(mv)
                                        elif isinstance(mv, dict) and mk.startswith("category_"):
                                            target = mk.replace("category_", "")
                                            ms_mods.setdefault("category_modifiers", {})
                                            for modk, modv in mv.items():
                                                if isinstance(modv, (int, float)):
                                                    ms_mods["category_modifiers"].setdefault(target, {})[modk] = float(modv)
                                    milestones.append(ms_mods)
                            node["milestones"] = milestones
                    elif sub_key == "rewards":
                        if isinstance(sub_val, dict):
                            rewards = {}
                            for reward_name, reward_data in sub_val.items():
                                if isinstance(reward_data, dict):
                                    rmods = {}
                                    for rk, rv in reward_data.items():
                                        if rk == "enable_tactic":
                                            if isinstance(rv, str):
                                                rmods.setdefault("enable_tactics", []).append(rv)
                                            elif isinstance(rv, list):
                                                rmods.setdefault("enable_tactics", []).extend(
                                                    v for v in rv if isinstance(v, str))
                                        elif isinstance(rv, (int, float)):
                                            rmods[rk] = float(rv)
                                        elif isinstance(rv, dict) and rk.startswith("category_"):
                                            target = rk.replace("category_", "")
                                            rmods.setdefault("category_modifiers", {})
                                            for modk, modv in rv.items():
                                                if isinstance(modv, (int, float)):
                                                    rmods["category_modifiers"].setdefault(target, {})[modk] = float(modv)
                                    rewards[reward_name] = rmods
                            node["rewards"] = rewards

                if node:
                    if name not in result:
                        result[name] = {
                            "path": relpath,
                            "nodes": {},
                        }
                    result[name]["nodes"][name] = node

        except Exception as e:
            errors.append((os.path.basename(fpath), str(e)))

    with open(OUT_FILE, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2)

    print(f"Extracted {len(result)} doctrine trees from {len(files)} files")
    print(f"Output: {OUT_FILE}")
    if errors:
        print(f"\nErrors ({len(errors)}):")
        for fn, err in errors:
            print(f"  {fn}: {err}")


if __name__ == "__main__":
    main()
