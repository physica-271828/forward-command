"""Extract national org/combat modifiers from HOI4 definition files.

Sources:
- common/dynamic_modifiers/*.txt -> the ORDERED modifier key list of every
  dynamic modifier. A country's save block carries the current values as a
  bare float array (`value = { 0.1 0.1 -0.1 ... }`) in definition order, so
  the runtime resolves values by index.
- common/ideas/*.txt -> per-idea MODIFIER_KEYS values from each idea's
  `modifier = { ... }` block.
- common/country_leader/*.txt -> per-trait MODIFIER_KEYS values.
  Trait definitions sit under `leader_traits = { ... }`; modifier keys may
  appear at the trait's top level OR inside a nested `hidden_modifier = {
  ... }` block — both are scanned.
- common/unit_leader/*.txt -> per-trait modifier values of GENERAL and
  FIELD-MARSHAL traits. Every numeric entry of the
  trait's own `modifier = { ... }` block is summed under its plain key;
  entries of its `field_marshal_modifier = { ... }` block are summed under
  an "fm:"-prefixed key — the runtime applies a field marshal's regular
  modifiers at x0.5 (FIELD_MARSHAL_ARMY_BONUS_RATIO) but his FM-only
  modifiers at full, so the provenance must survive the merge into one map.

Output: data/modifiers.json
    {"dynamic_modifiers": {name: [keys...]},
     "ideas": {token: {key: value, ...}},
     "leader_traits": {token: {key: value, ...}},
     "unit_leader_traits": {token: {key: value, "fm:key": value, ...}}}

Self-check: the key count of ITA_regio_esercito_dynamic_modifier must equal
the 1.19.2 save's serialized value-array length (45) — a mismatch means the
structural-key exclusion set no longer matches HOI4's serializer.
"""
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
DYN_DIR = os.path.join(GAME_DIR, "common", "dynamic_modifiers")
IDEAS_DIR = os.path.join(GAME_DIR, "common", "ideas")
LEADER_DIR = os.path.join(GAME_DIR, "common", "country_leader")
UNIT_LEADER_DIR = os.path.join(GAME_DIR, "common", "unit_leader")
OUT_FILE = os.path.join(OUT_DIR, "modifiers.json")

# Modifier keys carried into the JSON (org and combat keys).
# Vanilla spellings verified against common/ideas + documentation: the
# defense key is British (`army_defence_factor`); `breakthrough_factor` is
# the army-scope breakthrough key (army_spirits.txt).
MODIFIER_KEYS = frozenset({
    "army_org_factor", "army_org",
    "army_attack_factor", "army_defence_factor", "breakthrough_factor",
})

# Keys of a dynamic-modifier definition that are NOT serialized into the
# save's value array (verified against tac_snap.hoi4: every save-carried
# country modifier's array length matches the definition key count minus
# these — custom_modifier_tooltip/remove_trigger included).
STRUCTURAL_KEYS = frozenset({
    "enable", "icon", "picture", "remove_trigger", "custom_modifier_tooltip",
})

# Ground-truth probe (1.19.2 tac_snap.hoi4): value array = 45 entries.
SELF_CHECK = {"ITA_regio_esercito_dynamic_modifier": 45}


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
        if c in "{}=":
            tokens.append(c)
            i += 1
            continue
        if c == '"':
            j = text.index('"', i + 1)
            tokens.append(text[i:j + 1])
            i = j + 1
            continue
        if c.isdigit() or (c == '-' and i + 1 < len(text) and text[i + 1].isdigit()):
            j = i + 1
            dots = 0
            while j < len(text) and (text[j].isdigit() or text[j] == '.'):
                if text[j] == '.':
                    dots += 1
                j += 1
            val = text[i:j]
            # Dates ("1937.1.1") are opaque strings, not numbers.
            tokens.append(float(val) if dots == 1 else (int(val) if dots == 0 else val))
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


def parse_val(tokens, pos):
    """Parse one value; objects become ("PAIRS", [(key, value), ...]) to keep order."""
    if pos >= len(tokens):
        return None, pos
    t = tokens[pos]
    if t == "{":
        pos += 1
        pairs = []
        while pos < len(tokens) and tokens[pos] != "}":
            if isinstance(tokens[pos], str) and not tokens[pos].startswith('"'):
                key = tokens[pos]
                pos += 1
                if pos < len(tokens) and tokens[pos] == "=":
                    pos += 1
                    val, pos = parse_val(tokens, pos)
                    pairs.append((key, val))
                else:
                    pairs.append((key, None))
            else:
                pairs.append((None, tokens[pos]))
                pos += 1
        if pos < len(tokens):
            pos += 1
        return ("PAIRS", pairs), pos
    if isinstance(t, str) and t.startswith('"'):
        return t.strip('"'), pos + 1
    if isinstance(t, (int, float)):
        return t, pos + 1
    return t, pos + 1


def parse_top_level(filepath):
    """Yield (name, ("PAIRS", pairs)) for every top-level `name = { ... }`."""
    with open(filepath, "r", encoding="utf-8-sig") as f:
        tokens = tokenize(f.read())
    idx = 0
    while idx < len(tokens):
        if isinstance(tokens[idx], str) and not tokens[idx].startswith('"'):
            key = tokens[idx]
            if idx + 2 < len(tokens) and tokens[idx + 1] == "=" and tokens[idx + 2] == "{":
                val, idx = parse_val(tokens, idx + 2)
                yield key, val
                continue
        idx += 1


def extract_dynamic_modifiers():
    """name -> ordered modifier key list (structural keys dropped)."""
    table = {}
    for fn in sorted(os.listdir(DYN_DIR)):
        if not fn.endswith(".txt"):
            continue
        for name, val in parse_top_level(os.path.join(DYN_DIR, fn)):
            if not (isinstance(val, tuple) and val[0] == "PAIRS"):
                continue
            keys = [k for k, _v in val[1] if k and k not in STRUCTURAL_KEYS]
            table[name] = keys
    return table


def walk_ideas(pairs, table):
    """Collect idea tokens anywhere in the `ideas` tree (categories nest).

    Any `NAME = { ... modifier = { ... } ... }` block is an idea definition;
    modifier keys are read from its top-level modifier block(s) (case-insensitive
    — vanilla has `army_org_Factor` in GER.txt). Non-numeric values (variable
    references) cannot be resolved here and are skipped.
    """
    for key, val in pairs:
        if not (isinstance(val, tuple) and val[0] == "PAIRS"):
            continue
        body = val[1]
        if key:
            entry = {}
            for bk, bv in body:
                if bk and bk.lower() == "modifier" and isinstance(bv, tuple) and bv[0] == "PAIRS":
                    for mk, mv in bv[1]:
                        if mk is None or not isinstance(mv, (int, float)):
                            continue
                        mk = mk.lower()
                        if mk in MODIFIER_KEYS:
                            entry[mk] = entry.get(mk, 0) + float(mv)
            if entry:
                table[key] = entry
                continue  # an idea body never nests another idea
        walk_ideas(body, table)


def extract_ideas():
    """idea token -> {"army_org_factor": f, "army_org": v} (zero keys omitted)."""
    table = {}
    for fn in sorted(os.listdir(IDEAS_DIR)):
        if not fn.endswith(".txt"):
            continue
        for name, val in parse_top_level(os.path.join(IDEAS_DIR, fn)):
            if name != "ideas" or not (isinstance(val, tuple) and val[0] == "PAIRS"):
                continue
            walk_ideas(val[1], table)
    return table


def collect_modifiers(pairs, entry):
    """Sum numeric MODIFIER_KEYS entries of a PAIRS list into entry."""
    for key, val in pairs:
        if key is None or not isinstance(val, (int, float)):
            continue
        key = key.lower()
        if key in MODIFIER_KEYS:
            entry[key] = entry.get(key, 0) + float(val)


def extract_leader_traits():
    """trait token -> modifier values (org and combat keys).

    Traits sit under the `leader_traits = { ... }` top-level block; modifier
    keys may appear at the trait's top level or inside a nested
    `hidden_modifier = { ... }` block — both levels are summed. Non-modifier
    keys (random, ai_will_do, custom_modifier_tooltip, ...) never match the
    key set, so no exclusion list is needed.
    """
    table = {}
    for fn in sorted(os.listdir(LEADER_DIR)):
        if not fn.endswith(".txt"):
            continue
        for name, val in parse_top_level(os.path.join(LEADER_DIR, fn)):
            if name != "leader_traits" or not (isinstance(val, tuple) and val[0] == "PAIRS"):
                continue
            for tkey, tval in val[1]:
                if not tkey or not (isinstance(tval, tuple) and tval[0] == "PAIRS"):
                    continue
                entry = {}
                collect_modifiers(tval[1], entry)
                for bk, bv in tval[1]:
                    if bk and bk.lower() == "hidden_modifier" and isinstance(bv, tuple) and bv[0] == "PAIRS":
                        collect_modifiers(bv[1], entry)
                if entry:
                    table[tkey] = entry
    return table


def extract_unit_leader_traits():
    """unit-leader trait token -> modifier values.

    General/field-marshal traits sit under the `leader_traits = { ... }`
    top-level block of common/unit_leader/*.txt (same wrapper key as the
    country-leader file, different directory). Only the trait block's OWN
    `modifier` / `field_marshal_modifier` children are read — the `modifier`
    blocks nested deeper under `new_commander_weight`/`ai_will_do` are AI
    weight logic, not unit modifiers. All NUMERIC entries are kept (no key
    whitelist: the runtime picks the combat-relevant ones); terrain
    sub-blocks (`forest = { attack = 0.1 }`) are objects, not numerics, and
    skip themselves. `field_marshal_modifier` entries are written under an
    "fm:"-prefixed key so the runtime can apply them at full strength while
    halving the same holder's regular `modifier` entries
    (FIELD_MARSHAL_ARMY_BONUS_RATIO = 0.5).
    """
    table = {}
    for fn in sorted(os.listdir(UNIT_LEADER_DIR)):
        if not fn.endswith(".txt"):
            continue
        for name, val in parse_top_level(os.path.join(UNIT_LEADER_DIR, fn)):
            if name != "leader_traits" or not (isinstance(val, tuple) and val[0] == "PAIRS"):
                continue
            for tkey, tval in val[1]:
                if not tkey or not (isinstance(tval, tuple) and tval[0] == "PAIRS"):
                    continue
                entry = {}
                for bk, bv in tval[1]:
                    if bk is None or not (isinstance(bv, tuple) and bv[0] == "PAIRS"):
                        continue
                    blk = bk.lower()
                    if blk == "modifier":
                        prefix = ""
                    elif blk == "field_marshal_modifier":
                        prefix = "fm:"
                    else:
                        continue
                    for mk, mv in bv[1]:
                        if mk is None or not isinstance(mv, (int, float)):
                            continue
                        key = prefix + mk.lower()
                        entry[key] = entry.get(key, 0) + float(mv)
                if entry:
                    table[tkey] = entry
    return table


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    dynamic = extract_dynamic_modifiers()
    ideas = extract_ideas()
    leader_traits = extract_leader_traits()
    unit_leader_traits = extract_unit_leader_traits()

    for name, expected in SELF_CHECK.items():
        got = len(dynamic.get(name, []))
        if got != expected:
            print(f"SELF-CHECK FAILED: {name} has {got} modifier keys, "
                  f"expected {expected} (1.19.2 save value-array length) — "
                  f"review STRUCTURAL_KEYS", file=sys.stderr)
            sys.exit(1)
        print(f"self-check ok: {name} -> {got} keys")

    with open(OUT_FILE, "w", encoding="utf-8") as f:
        json.dump({"dynamic_modifiers": dynamic, "ideas": ideas,
                   "leader_traits": leader_traits,
                   "unit_leader_traits": unit_leader_traits}, f,
                  indent=2, ensure_ascii=False)
    print(f"Extracted {len(dynamic)} dynamic modifiers, {len(ideas)} ideas with modifiers, "
          f"{len(leader_traits)} leader traits with modifiers, "
          f"{len(unit_leader_traits)} unit-leader traits with modifiers")
    print(f"Output: {OUT_FILE}")


if __name__ == "__main__":
    main()
