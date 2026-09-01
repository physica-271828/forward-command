**English** | [简体中文](README_CN.md)

# Forward Command / 前敌指挥

An adaptive tactical-battle companion for **Hearts of Iron IV** — fight the
battles yourself instead of watching the dice roll.

> Personal hobby project, released as-is. No update schedule.

---

## What it does

When two armies clash on the HOI4 strategic map, Forward Command lets you take
personal command of the tactical battle:

- A companion mod detects the battle and signals the external program through
  `game.log`.
- The program parses your save (division composition, equipment, org/strength,
  tech, doctrines) and generates a hex-grid tactical map of the contested
  province — terrain, rivers, cities, attack axes.
- You fight the battle turn by turn in a 3D tactical view: deployment,
  movement, assaults, artillery fire missions, division-level orders, flag
  capture, encirclement, fog of war — against a three-layer AI with 16 tactic
  cards drawn from HOI4's own combat tactics.
- On sync, the results are written back into HOI4 as organization/strength
  damage on the real divisions, and the strategic clock advances. You can keep
  fighting across multiple strategic hours or end the battle and let HOI4
  resume.

English and 简体中文 UI, switchable in settings.

## How it works

Three channels connect the program and the game (see `DESIGN.md` for the full
protocol):

| Channel | Direction | Content |
|---------|-----------|---------|
| `game.log` | mod → program | Trigger signals (`tac_start`, heartbeat, …) as JSON lines |
| Save file | game → program | Division composition, equipment, org/str, tech (Clausewitz text format) |
| Console injection | program → mod | Damage values and sync markers via `set_var` + scripted effects (`run tac_inject.txt`, Windows `SendInput`) |

The injection channel automates the HOI4 console with synthesized keystrokes —
that is the whole trick, and exactly why the source is public: you can read
every line of it. The program never touches the network, never reads anything
outside the HOI4 directories and its own folder, and does nothing beyond the
interactions documented in `docs/免责声明.md`.

## Requirements

- **Windows** (the console-injection channel is Win32-only)
- **Hearts of Iron IV 1.19.\***, single-player, **text saves** required
  (`save_as_binary=no` in `settings.txt` — the installer warns you)
- Full DLC recommended; missing DLC may shift equipment/template data
- Ironman and multiplayer are not supported

## Installation (players)

Pick **ONE** mod channel — never enable both (two same-name mods break the
game):

- **Workshop**: subscribe on Steam — then SKIP `install-mod.bat`; the launcher
  installs and updates the mod automatically.
- **Local** (not subscribed): continue below and run the installer.

1. Download and unpack the latest release zip.
2. Double-click `install-mod.bat` — it installs the companion mod into your
   HOI4 user directory (and cleans up any legacy copy).
3. Enable **Forward Command** in the HOI4 launcher and restart the game.
4. Run `forward-command.exe`, leave it listening, and play HOI4. When a battle
   starts, pick it from the in-game decision to take tactical command.

The full player manual (Chinese): `docs/玩家说明书.md`.

## Building from source

```
cargo build --release --workspace
```

- Rust stable, `x86_64-pc-windows-gnu` toolchain (a GNU linker environment
  such as WinLibs must be on `PATH` for builds and tests)
- The workspace's `.cargo/config.toml` template may need adjusting for your
  machine (linker paths, registry mirror)
- The JSON tables under `data/` are pre-extracted from the game files; the
  Python scripts under `extractor/` regenerate them from a local HOI4 install
  if needed
- `cargo test --workspace` runs the unit-test suite (a few tests require a
  local HOI4 installation and are ignored by default)

## Documentation

- `DESIGN.md` — full design specification (architecture, protocol, map
  generation, combat model, AI, time model, UI, mod, settings)
- `docs/玩家说明书.md` — player manual (Chinese)
- `docs/免责声明.md` — disclaimer (Chinese; mechanics disclosure, antivirus
  guidance, risk statement)
- `HOI4_UNITS.md` — HOI4 land-unit inventory → internal unit mapping

## Third-party assets

- UI icons: **Twemoji**, [CC-BY 4.0](https://creativecommons.org/licenses/by/4.0/)
  — see `assets/ATTRIBUTION.md`
- CJK fallback font: **Noto Sans SC**, SIL Open Font License — see
  `assets/fonts/LICENSE-OFL.txt`
- Game data extracted from the user's own HOI4 installation remains the
  property of Paradox Interactive.

## AI-Assisted Development

This project is designed and directed by the author, with the code,
documentation, and writing produced in collaboration with AI assistants —
principally **Kimi K3** (Moonshot AI) and **DeepSeek V4** (DeepSeek). The
author does not claim a line-by-line review of the Rust code: every feature is
verified functionally, through playtesting and the project's automated test
suite. The source is published precisely so that those who can read it may
audit it themselves.

## Disclaimer

This is an unofficial fan project and is **not affiliated with or endorsed by
Paradox Interactive**. Hearts of Iron IV and all related assets are the
property of Paradox Interactive AB. The program modifies game state through
console automation; although cleanup paths are tested, **back up saves you
care about**. Use at your own risk — see `docs/免责声明.md` for the full text.

## Support

Forward Command is free and will stay free — no paid content, ever. If it
brought you some fun, voluntary tips toward the development costs (AI
assistant subscription, electricity) are welcome:

[爱发电 Afdian](https://ifdian.net/a/forward-command)

Tips are unrelated to downloads, updates, and content, and do not affect how
feedback is handled.

Feedback & discussion: QQ group 960 830 355 (Chinese — beta builds and
chatter) or
[GitHub Issues](https://github.com/physica-271828/forward-command/issues)
(any language).

## License

Copyright (C) 2026 physica.

This program is free software: you can redistribute it and/or modify it under
the terms of the **GNU General Public License v3.0** as published by the Free
Software Foundation — see `LICENSE`. Third-party assets listed above remain
under their own licenses.
