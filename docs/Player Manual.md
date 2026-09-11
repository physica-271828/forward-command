# Forward Command — Player Manual

> Beta · personal hobby project · updates whenever, no schedule · largely vibe-coded.
> Feedback: QQ group (@owner / admins). For bug reports, prepare what §6 FAQ first question asks for (version + symptoms + reproduction steps; attach a diagnostic bundle for live-mode issues).
>
> This is the English version of the Chinese player manual (`玩家说明书.md`).
> Where the two differ, the Chinese original governs.

---

## 1. What this is

A mod that lets you personally ~~micromanage~~ command a single battle in *Hearts of Iron IV* (HOI4). It comes as two halves:

- **The HOI4 mod "前敌指挥 / Forward Command"** — provides the in-game entry point (a decision), writes the start signal into `game.log`, and wakes the external program.
- **The external program (`forward-command.exe`)** — a standalone 3D hex-grid wargame app: it reads HOI4's log and your save, rebuilds the contested province as a tactical map, and injects the battle results back into the campaign. You command battalion by battalion, and the outcome is written back to HOI4.

In one sentence: **the mod reads the game's save file and logs, and sends battle results back only through the game's own console** — it does not modify the game itself and never touches the network.

**GitHub downloads** [GitHub](https://github.com/physica-271828/forward-command/releases)

**Workshop subscription** [Steam Workshop](https://steamcommunity.com/sharedfiles/filedetails/?id=3790483700)

---

## 2. Installation

### 2.1 Requirements

- Genuine HOI4 **1.19.\*** (non-Steam copies: paths/versions are incompatible and not supported for now).
- **Windows + Steam** (currently the only supported platform).
- **Full DLC recommended**; with missing DLC, some equipment/template data may not line up (reports welcome).
- This mod is currently **incompatible with any mod that changes game data**; loading other mods alongside it is not guaranteed to work.
- HOI4 must use **text saves**: `save_as_binary=no` (see §2.2 step 4 — the program can fix this in one click).
- Get the zip (**GitHub Releases or the QQ group file share**).

### 2.2 Install · Channel A: local (do NOT subscribe to the Workshop item on this channel)

1. Unpack the zip anywhere (Desktop, D: drive — the whole folder is portable).
2. Double-click `install-mod.bat` (it installs the mod into your HOI4 user directory; success or failure stays on screen).
3. Open the HOI4 launcher and enable **Forward Command** in your playset.
4. Save-format check:
   - the installer **warns on the spot** if your settings use binary saves;
   - run `forward-command.exe` — first launch shows a **language-selection page** first (pre-selected from your system language; changeable later in Settings), then opens the **Settings** page, which detects the save format and offers a **one-click fix** (restart HOI4 afterwards; only saves made after that are text saves).
5. On the Settings page, confirm the three paths (HOI4 install dir / save dir / game.log) — they are auto-detected in the normal case; click **Save**.

### 2.3 Install · Channel B: Steam Workshop subscription

1. Open the "Workshop subscription" link at the top of this manual and subscribe.
2. Get the matching zip, then follow the rest of §2.2 except step 2 — do **not** run `install-mod.bat`.

### 2.4 Updating & uninstalling

- **Local channel update**: unpack the new zip over the old folder (or into a new one) and double-click `install-mod.bat` again. Your preferences live in `settings.json` next to the exe — keep that file when replacing the folder.
- **Workshop channel update**: the launcher updates the mod automatically; you still replace `forward-command.exe` from the zip by hand.
- **Version notice**: when the mod and the program versions do not match (for example the mod auto-updated but your exe is still old), an in-game popup tells you which half to update — program too old: replace `forward-command.exe` (no need to reinstall the mod); mod too old: run `install-mod.bat` again.
- **Switching channels**:
  - Local → Workshop: run `install-mod.ps1 -Uninstall` first, then subscribe;
  - Workshop → local: unsubscribe in Steam first, then run `install-mod.bat`.
- **Uninstall**:
  - If you subscribed: unsubscribe and delete the folder;
  - If you use the local channel: run `install-mod.ps1 -Uninstall` (PowerShell) and remove the mod from your launcher playset.

---

## 3. Playing

### 3.1 Starting a battle

1. Run `forward-command.exe` and click **Start live listen** in the main menu (the window may be closed — the program minimizes to the system tray and keeps listening; closing is not quitting).
2. In HOI4, open the **decisions** panel, find the **"Forward Command"** category, and click the **"Enter Tactical Battle in <state>"** decision.
3. Alternatively, after opening the panel, look for the decision icon on the map and click it in the state you want to fight in.
4. The tactical battle window opens automatically. A few special cases:
   - no battle in the chosen state → an in-game popup says so; just pick another state;
   - several battles in one state → the program shows a **battle picker** (map preview; click the one you want);
   - a state you picked earlier the same day **cannot be picked again for several game hours** (game-engine limitation) — use another state or wait until the next day.

### 3.2 Deployment phase

- Your battalions start in the **Order of Battle (OOB)** panel on the right:
  - **Attach support companies**: divisional support companies can be attached to individual battalions to strengthen them;
  - **Place one by one**: click an OOB row, then click a hex inside the deployment zone; deployed battalions can be repositioned by clicking elsewhere;
  - **Sector deploy**: click **[Deploy N to sector]** on the division header, then **drag a rectangle** on the map — the whole division is placed automatically;
  - **Auto deploy**: the **[Auto Deploy]** button on the right panel hands all remaining battalions to the AI (manually placed ones stay put);
  - **Recall**: click **[Recall All]** or the division's **[Recall N]** to return deployed battalions to the OOB.
- When every battalion is in place, **[Begin Battle]** lights up — click it to start the battle.

### 3.3 Combat controls

Turn-based: **1 turn = 10 minutes, 6 turns = 1 strategic hour**.

- **Select**: left-click your battalion (click empty ground or an enemy to deselect).
- **Multi-select & groups**: left-drag to box-select, Shift-click to add; Ctrl+number to group, then press the number to recall the whole group; C toggles between manual control and handing back to the division AI (player-controlled battalions carry a cyan outline).
- **Move**: right-click the destination — the path ribbon and estimated turns appear; hold Shift while right-clicking to append waypoints in sequence for multi-leg moves.
- **Attack**: right-click a visible enemy. Adjacent means an **assault**; direct-fire units (tanks, anti-tank guns) fire **direct fire** even at range; artillery fires a **precision shot**.
- **Radial buttons** (appear when a battalion is selected; unavailable ones are hidden):
  - **H Hold**: infantry-type units take up cover (+25% defense), costs one turn;
  - **E / L Emplace / Limber**: towed guns **must be emplaced to fire** and limbered to move — one turn each;
  - **F Fire mission**: artillery area barrage (once per turn);
  - **R Retreat**: disengage while adjacent to the enemy (at the cost of organization).
- **Cancel an order**: right-click your own battalion.
- **Division orders**: select the division's **HQ** → button bar on top: **Advance** (the whole division goes on the offensive) / **Seize** (pick a hex to capture and hold) / **Destroy** (hunt down one enemy battalion). The division AI then runs the division — **you can still override it by giving battalions direct orders**.
- **Allied (friendly) nations**: in multi-nation games, allied divisions are deployed and fought by their own AI — you cannot command their battalions directly; allied base plates are coloured per nation; you can still select an allied HQ to issue division-level orders.
- **Camera**: WASD / arrow keys to pan, wheel to zoom, Q/E rotate, R/F pitch, middle-drag the map.
- **End turn**: **[End Turn]** on the right panel — your movement orders execute first, then the enemy AI acts.
- **Automation panel**: three checkboxes next to End Turn — **auto-skip combat-free turns / auto-close battle reports / auto-sync every hour** (each with a dwell-time slider); check all three together with division orders for fully automatic spectating.

### 3.4 Battle reports & victory

- The highlighted areas on the map (cities / key points) are the objectives: capture them all, or destroy the enemy, to win.
- A battle report pops up whenever a fight resolves; the **Details** button next to each entry shows the full damage formula for that exchange.
- The battle log window (**[Battle Log]** on the right) works the same — click any line to expand its details.
- **Encirclement & surrender**: a fully encircled force (no way out, no relief) keeps losing organization until it surrenders; if a pocket contains a live HQ it is not judged encircled — destroy that HQ first when you are the one encircling, and protect yours when you are the one surrounded.

### 3.5 Sync

- Every 6 turns (1 strategic hour), **[Sync to HOI4]** lights up: click it to write that hour's results back to HOI4 — **game time then advances one hour** — and keep fighting.
  - While a tactical battle is running, **the contested state is frozen in HOI4** (a "tactical battle in progress" modifier), so damage is never resolved twice; the rest of the world keeps running.
- When the battle is over, click **[Apply & Exit]**: the final results are written back, the window closes, and the game shows a "tactical battle finished" popup.
- To stop midway: Esc → quit (it asks first if you have unsynced results); or use **[End Tactical Battle]** in the sync dialog (already-synced parts are kept).
- **Escape hatch**: if the program hangs, crashes or misbehaves, click **"Force Exit Tactical Mode"** in the HOI4 decision — it clears the battlefield state immediately (unsynced results are lost).

### 3.6 How results are written back (important)

- Default **writeback (organization + strength, with per-hex precision)**: the defender's losses land exactly on the hex attacked; attacker losses go back by source province, so divisions from the same province that did not fight may take a little spillover — that is an HOI4 command-mechanism limitation; a single specific division cannot be targeted.
- Want no effect at all: set **"Damage writeback"** to **off** in the Settings page (sandbox mode — fight for fun).

---

## 4. Settings

Main menu → Settings:

- **Paths + language**: auto-detected; language switches take effect immediately (中文 / EN).
- **Damage writeback**: whether results are written back — see §3.6.
- **Keep manual control**: whether a completed manual order is handed back to the division AI (default: handed back); while kept, right-click a battalion to toggle its manual state.
- **MSAA / shadow / render scale / FPS cap / menu FPS / idle frame-drop**: performance.
  - For stutter, first lower the **render scale** (100 → 85 → 70 → 50; trades image quality for performance);
  - then drop shadow to Low/Off and MSAA to 2×/Off;
  - enabling idle frame-drop is recommended: the battle window drops to ~10 fps when untouched (giving the GPU back to HOI4) and snaps back the moment you act.
- **Export diagnostic bundle**: for bug reports — one click packs logs + saves into a zip next to the exe; send it to the group owner. The bundle contains your machine's paths and saves; open it first if you are unsure.
- **About**: version number and build date, including a **disclaimer** entry.
- Render settings can also be changed mid-battle: Esc → Settings; renderer items apply instantly.

---

## 5. Known limitations (during beta)

- Windows only; single-player only; **Ironman unavailable** (text saves are required); **multiplayer disabled**.
- Equipment / template data may misalign (specific division setups welcome; include a save if you can).
- How often a state can be entered for a tactical battle per day is limited by the game engine (see §3.1).
- The program types into the HOI4 console via **synthesized keystrokes**: antivirus software may block or flag it (see FAQ).

---

## 6. Feedback & FAQ

**Q: What should I include in a bug report?**
A: ① a detailed description of the bug, including what you did before and after and what you expected; ② the Settings-page **diagnostic bundle** (tick "include saves"); ③ a recording if you have one. Send it to the group owner.

**Q: Antivirus flags or blocks it?**
A: The program has to type into the HOI4 console (the only channel for writing results back) — that is exactly the kind of behavior antivirus heuristics watch for. It **does not connect to the network, has no telemetry, and its source code is public on GitHub** (see the README or your release page) — feel free to whitelist it:
Windows Defender: Settings → Privacy & security → Windows Security → Virus & threat protection → under "Virus & threat protection", Manage settings → Exclusions → Add folder (pick your unpacked folder).

**Q: The game stutters?**
A: Lower the render scale first (Settings page), then shadow/MSAA, then enable idle frame-drop. A discrete GPU is recommended; 8 GB+ VRAM suggested.

**Q: After syncing, HOI4 does not respond / shows "sync not confirmed"?**
A: Make sure HOI4 is still running in the foreground, not sitting in the main menu and not paused (`pause_on_lost_focus` should be set to no, or the game freezes time when it loses focus). Retry or cancel per the popup — already-injected damage is never lost.

**Q: The battle stays frozen?**
A: The program is not running. Start `forward-command.exe` and enable live listen (tray menu) **before** using the decisions; if the program dies mid-battle, the mod releases the frozen battle after about two game days and tells you.

**Q: Sync looks stuck (5–15 s, window unresponsive)?**
A: That is the save check plus the injection wait — let it finish. Killing the process can leave a game-side freeze that has to be cleared with **Force Exit Tactical Mode**.

**Q: No decisions appear?**
A: Check HOI4 1.19.\*, non-Ironman, text saves, the mod enabled in the current playset, and (local channel) that `install-mod.bat` actually succeeded.

**Q: Damage or equipment numbers look wrong?**
A: Check DLC completeness first, then report with a diagnostic bundle (Settings → *Export diagnostic bundle*).

**Q: Why can't I read my save?**
A: It must be a text save. The detection line on the Settings page must be green; after switching `save_as_binary=no`, restart HOI4 and **save a new game once**.

**Q: Can I support with a tip?**
A: Yes, entirely voluntary — use the wish-pool link at the end of this manual. Tips do not affect mod content, updates, or how feedback is handled.

---

## 7. Wishlist / planned

(**Wish pool — [drop a coin / support](https://ifdian.net/a/forward-command)**)

- Trenches, fort terrain;
- Air close-support mechanics;
- Naval support and amphibious battles;
- Support for other major mods, e.g. Kaiserreich.
