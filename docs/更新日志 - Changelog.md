# 更新日志

> 仅记录正式版更新内容；群内 beta 迭代版本不入此志。

## v0.3.2（2026-09-11）

**新功能**
- 地图路网：按省份真实地形生成公路 / 铁路 / 桥梁 / 村庄，野地行军整体放慢；
- 合围重做：类似围棋规则判定集团被包围情况，集团内有指挥部则判定为未被包围；
- 盟军 AI：盟国的师现在均由盟军 AI 指挥，自行部署与作战，玩家可以选中盟军指挥部下达师级命令；
- 版本握手：mod 与程序版本不匹配时游戏内弹窗提示更新
- 多选与编组：左键拉框选取、Shift 点击加选、Ctrl+数字编组；C 键整集手操 ⇄ 归还切换
- Shift 连续下令：按住 Shift 右键依次追加移动路径点
- 新增「保持指挥」选项：可以选择手操命令结束后是否自动将指挥权交给师AI；保持指挥状态下，通过右键点击棋子切换手操状态；
- 河流宽度分档：按原版地图的河流大小分类，生成 1~3 格宽河流
- 玩家控制的营兵牌加青色描边提示
- 首次启动新增语言选择页（按系统语言预选）

**BUG 修复**
- 修复占领旗点组织度清零未回写到游戏的问题； 
- 修复指挥部兵力与师兵力未挂钩的问题； 
- 师指挥部现在不再能主动发起进攻； 
- 修复先被击溃再被包围的营不投降的问题； 
- 修复围困造成组织度伤害未回写到游戏的问题；
- 修复 AI 炮兵覆盖射击误伤友军：弹着圈内有友军不再开火；占领目标点后不再持续炮击点位；
- 修复切换师命令后，本回合已行动的手操营被 AI 误加固守姿态的问题；
- 修复改手操指令后，行军进度无条件清零的问题；现在同目的地重下指令/改临近格不再丢失已走进度；
- 修复点固守（H）后棋子状态没有立刻结算的问题；

## v0.3.1（2026-09-07）

**新功能**
- 战斗自动化面板：「结束回合」旁三个局内勾选——自动跳过无战斗回合 / 自动关闭战报 / 整点自动同步（各带停留时间滑动条）；全勾 + 师级命令 = 全自动观战
- 新开档弹出一次使用须知；决议分类常驻外部程序提醒

**BUG 修复**
- 坦克/坦歼/自行火炮等装甲单位实战攻击为 0：现按存档中的真实坦克设计（底盘+模块+升级等级）计算数值；坦歼/自行火炮类营空编制上阵一并修复
- 未运行外部程序时点决议导致战斗永久冻结：新增看门狗，最迟两个游戏日内自动解除冻结并弹窗指引

## v0.3.0（2026-09-04）

初始正式版发布。

---

# Changelog (English)

> Formal releases only; in-group beta iterations are not listed here.

## v0.3.2 (2026-09-11)

**New features**

- Map road network: highways / railways / bridges / villages generated from each province's real terrain; cross-country movement is slower overall;
- Encirclement rework: a whole group is judged as encircled with go-like rules; a group containing an HQ counts as not encircled;
- Allied AI: allied divisions are now commanded by their own AI, deploying and fighting on their own; you can select an allied HQ to issue division-level orders;
- Version handshake: when the mod and the program versions do not match, an in-game notice tells you to update;
- Multi-select & groups: left-drag to box-select, Shift-click to add, Ctrl+number to group; C toggles the whole group between manual control and handing it back;
- Shift-chained orders: hold Shift while right-clicking to append movement waypoints in sequence;
- New "keep manual control" option: choose whether a completed manual order is handed back to the division AI; while kept, right-click a battalion to toggle its manual state;
- River width tiers: rivers are generated 1–3 hexes wide based on the river's size on the vanilla map;
- Player-controlled battalions get a cyan outline marker;
- First launch now offers a language-selection page (pre-selected from the system language).

**Bug fixes**

- Capturing the objective now actually writes the defender's organization zeroing back to the game;
- HQ strength is now tied to the division's strength;
- Division HQs can no longer initiate attacks;
- Battalions broken first and encircled afterwards now surrender;
- Encirclement attrition damage now writes back to the game;
- AI artillery barrages no longer hit friendly units inside the impact ring; the AI also stops shelling an objective it has already taken;
- After switching division orders, manually controlled battalions that already acted are no longer wrongly set to Hold;
- Re-issuing a manual order no longer resets movement progress; same destination / adjacent-hex changes keep the progress already made;
- Pressing Hold (H) now resolves immediately on the counter.

## v0.3.1 (2026-09-07)

**New features**

- Battle automation panel: three in-game checkboxes next to End Turn — auto-skip combat-free turns / auto-close battle reports / auto-sync every hour (each with a dwell-time slider); all three checked + division orders = fully automatic spectating;
- A one-time usage notice when starting a new game; a persistent reminder for the external program in the decision category.

**Bug fixes**

- Armoured units (tanks / tank destroyers / self-propelled artillery) dealt 0 damage in real battles: stats are now computed from the save's actual tank designs (chassis + modules + upgrade level); tank destroyers / SPGs with empty equipment templates are fixed as well;
- Triggering a decision without the external program running froze the battle forever: a watchdog now releases the freeze within two game days at the latest and shows guidance.

## v0.3.0 (2026-09-04)

Initial formal release.
