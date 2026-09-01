"""Extract country map colors from HOI4 common/countries/colors.txt.

Output: country_colors.json  { "TAG": [r, g, b], ... } with 0-255 ints.
HSV entries are converted to RGB (Clausewitz hsv is 0..1 floats).
The `color` (map) entry is used; the brighter `color_ui` variant is ignored.
"""
import argparse
import json
import os
import re

_parser = argparse.ArgumentParser(description=__doc__)
_parser.add_argument("--hoi4-dir", default=os.environ.get("HOI4_DIR", r"D:\Steam\steamapps\common\Hearts of Iron IV"),
                     help="HOI4 install directory (or set HOI4_DIR env var)")
_parser.add_argument("--out", default=r"D:\OpenCode\Forward Command\forward-command\data",
                     help="output directory for the extracted JSON tables")
_args = _parser.parse_args()
GAME_DIR = _args.hoi4_dir
OUT_DIR = _args.out
COLORS_FILE = os.path.join(GAME_DIR, "common", "countries", "colors.txt")
OUT_FILE = os.path.join(OUT_DIR, "country_colors.json")

# Top-level tag block start, e.g. "GER = {" (comments already stripped).
TAG_RE = re.compile(r"^([A-Z0-9_]+)\s*=\s*\{", re.MULTILINE)
# color = rgb { r g b } / color = hsv { h s v } (color_ui excluded: the word
# "color" here is not followed by "_", because we require \s*= right after).
COLOR_RE = re.compile(r"\bcolor\s*=\s*(rgb|hsv)\s*\{\s*([0-9.\s]+?)\s*\}", re.IGNORECASE)


def hsv_to_rgb(h, s, v):
    """HSV (0..1 floats) → RGB 0-255 ints."""
    i = int(h * 6.0) % 6
    f = h * 6.0 - int(h * 6.0)
    p = v * (1.0 - s)
    q = v * (1.0 - f * s)
    t = v * (1.0 - (1.0 - f) * s)
    r, g, b = [(v, t, p), (q, v, p), (p, v, t), (p, q, v), (t, p, v), (v, p, q)][i]
    return [round(r * 255), round(g * 255), round(b * 255)]


def main():
    os.makedirs(OUT_DIR, exist_ok=True)

    with open(COLORS_FILE, "r", encoding="utf-8-sig") as f:
        text = f.read()
    # Strip comments (# to end of line).
    text = re.sub(r"#.*", "", text)

    result = {}
    tags = list(TAG_RE.finditer(text))
    for i, m in enumerate(tags):
        tag = m.group(1)
        # Search for the first color entry inside this block (up to the next tag).
        end = tags[i + 1].start() if i + 1 < len(tags) else len(text)
        block = text[m.end():end]
        cm = COLOR_RE.search(block)
        if not cm:
            continue
        mode = cm.group(1).lower()
        vals = [float(x) for x in cm.group(2).split()]
        if len(vals) < 3:
            continue
        if mode == "rgb":
            result[tag] = [round(vals[0]), round(vals[1]), round(vals[2])]
        else:
            result[tag] = hsv_to_rgb(vals[0], vals[1], vals[2])

    with open(OUT_FILE, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, sort_keys=True)

    print(f"Extracted {len(result)} country colors")
    print(f"Output: {OUT_FILE}")


if __name__ == "__main__":
    main()
