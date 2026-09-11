# Forward Command（前敌指挥）免责声明

**使用本软件即表示你已阅读、理解并同意以下全部条款。**

## 一、性质声明

1. 本软件（含 HOI4 mod「前敌指挥」与外部程序，以下合称"本软件"）是**个人兴趣作品**，**免费发布**，与 Paradox Interactive AB 及其任何关联公司**无任何关系**，不是官方产品，未获得 Paradox 的认可、授权或支持。
2. 《Hearts of Iron IV》及其全部素材、商标、数据的版权归 Paradox Interactive AB 所有。
3. 本软件按 **GPL-3.0** 开源发布、免费提供（见仓库 `LICENSE`）：复制、修改与再分发遵循该许可证。

## 二、工作原理告知（重要）

1. 本软件运行时会：
   - 读取 HOI4 的 **game.log** 日志文件与你的**存档文件**；
   - 在 HOI4 用户目录下写入一个临时指令文件，并**模拟键盘输入**（PostMessage/SendInput）唤起游戏控制台执行它，以此把战斗结果写回游戏（伤害回写、交战州的冻结与解冻、弹出提示）。
2. 除上述声明的交互外，本软件**不做任何其他行为**：不联网、不收集或上传任何数据、不含遥测、不修改 HOI4 程序本体、不读取与本软件无关的文件。
3. 上述"模拟键盘输入"属于杀毒软件的敏感行为类别，**可能被杀毒软件拦截或误报**；你需要自行将本软件加入白名单才能正常使用。是否信任并加白名单由你自己决定。
4. 本软件源代码已在 [GitHub 公开](https://github.com/physica-271828/forward-command/releases)，供任何人查验无恶意行为。

## 三、风险自担

1. 本软件会向游戏内注入战斗结果（部队组织度/强度伤害、州级冻结与解冻修正）。注入机制虽已经过"清理无残留"验证，但**无法保证在所有环境下都万无一失**。
2. **强烈建议**：在使用本软件进行重要战局前，备份你的存档。
3. 因使用本软件导致的**存档损坏、游戏异常、系统问题或其他任何损失**，由使用者自行承担，作者不承担任何责任。
4. 本软件与杀毒软件、其他 mod、其他第三方工具的兼容性**不作任何保证**。

## 四、适用范围

- 仅支持 **Windows**；仅支持**单人游戏**；
- 需要**正版 HOI4 1.19.\*** 且存档为**文本格式**（`save_as_binary=no`）；
- **铁人模式不可用**（铁人禁止控制台，机制上无法实现）；
- **多人游戏不可用**（注入机制在多人环境下没有意义）；
- **建议全 DLC**：DLC 不全时，部分装备/编制数据可能错位（本软件的数据表提取自使用者本机的游戏文件，该等数据的版权属 Paradox）。

## 五、第三方资产

- 界面图标：**Twemoji**（CC-BY 4.0 许可）；
- 中文字体：**Noto Sans CJK**（SIL Open Font License）；
- 上述许可文本随软件包附带（`assets/ATTRIBUTION.md`、`assets/fonts/LICENSE-OFL.txt`）。

## 六、内测特别声明

1. 当前版本为**内测测试版**，可能含有缺陷；功能、数值、行为在后续版本中可能变更，恕不另行通知。
2. **更新随缘，无时间表**：作者不承诺任何更新频率、修复期限或新功能。
3. 内测反馈（bug 报告、建议）完全自愿；反馈内容会被整理进公开/内部台账用于改进软件。

## 七、无担保

本软件按"**现状**"（AS IS）提供，不附带任何明示或默示的担保，包括但不限于对适销性、特定用途适用性、不侵权的担保。在法律允许的最大范围内，作者不对使用本软件产生的任何直接、间接、附带、特殊或后果性损害负责。

---

*Forward Command · 个人兴趣作品 · 与 Paradox Interactive 无关 · 免费软件 · GPL-3.0 开源*

---

# Forward Command — Disclaimer (English)

> English translation of the Chinese disclaimer above. Where the two differ, the Chinese text governs.

**By using this software you confirm that you have read, understood and accepted all of the terms below.**

## 1. Nature of this software

1. This software (the HOI4 mod "前敌指挥 / Forward Command" and the external program, jointly "the software") is a **personal hobby project**, **released free of charge**, and has **no relationship whatsoever** with Paradox Interactive AB or any of its affiliates: it is not an official product and has not been endorsed, authorised or supported by Paradox.
2. *Hearts of Iron IV* and all of its assets, trademarks and data are the property of Paradox Interactive AB.
3. This software is released as open source under the **GPL-3.0** license (see `LICENSE` in the repository): copying, modification and redistribution are governed by that license.

## 2. How it works (important)

1. While running, the software:
   - reads HOI4's **game.log** and your **save files**;
   - writes a temporary command file into the HOI4 user directory and **synthesizes keyboard input** (PostMessage/SendInput) to drive the game console into executing it, writing battle results back into the game (damage write-back, state freeze/unfreeze, popups).
2. Beyond the interactions listed above, the software **does nothing else**: no network access, no data collection or upload, no telemetry, no modifications to the game's own files, no reading of files unrelated to the software.
3. The synthesized keyboard input above falls in the category antivirus software watches for and **may be blocked or falsely flagged**; you must whitelist the software yourself to use it normally. Whether you trust it and whitelist it is your own decision.
4. The source code is [public on GitHub](https://github.com/physica-271828/forward-command/releases) so anyone can verify there is nothing malicious.

## 3. Use at your own risk

1. The software injects battle results into the game (organization / strength damage, state-level freeze and unfreeze modifiers). Although the injection mechanism has passed "no-residue" cleanup verification, it **cannot be guaranteed flawless in every environment**.
2. **Strongly recommended**: back up your saves before playing an important campaign with this software.
3. **Save corruption, game errors, system problems or any other loss** caused by using this software are borne by the user; the author accepts no responsibility.
4. No compatibility guarantees are given with antivirus software, other mods, or other third-party tools.

## 4. Scope

- **Windows only**; **single-player only**;
- requires **genuine HOI4 1.19.\*** with **text-format saves** (`save_as_binary=no`);
- **Ironman is unavailable** (Ironman disables the console — mechanically impossible);
- **Multiplayer is unavailable** (injection has no meaning in a multiplayer environment);
- **Full DLC recommended**: with missing DLC, some equipment/template data may misalign (the software's data tables are extracted from the user's own game files; copyright in that data belongs to Paradox).

## 5. Third-party assets

- UI icons: **Twemoji** (CC-BY 4.0 license);
- CJK font: **Noto Sans CJK** (SIL Open Font License);
- The license texts above ship with the package (`assets/ATTRIBUTION.md`, `assets/fonts/LICENSE-OFL.txt`).

## 6. Beta notice

1. The current version is a **beta build** and may contain defects; features, values and behaviour may change in future versions without notice.
2. **Updates whenever, no schedule**: the author promises no update frequency, no fix deadline and no new features.
3. Beta feedback (bug reports, suggestions) is entirely voluntary; reports may be compiled into public/internal ledgers to improve the software.

## 7. No warranty

The software is provided "**AS IS**", without any express or implied warranty, including but not limited to merchantability, fitness for a particular purpose and non-infringement. To the maximum extent permitted by law, the author is not liable for any direct, indirect, incidental, special or consequential damages arising from the use of this software.

---

*Forward Command · personal hobby project · not affiliated with Paradox Interactive · free software (open source, GPL-3.0)*
