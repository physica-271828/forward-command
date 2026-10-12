# 更新日志

> 仅记录正式版更新内容；群内 beta 迭代版本不入此志。

## v0.3.3（2026-10-13）

**新功能**
- 进战机制重做：新增「扫描可微操的战斗」决议——点击后程序扫描当前存档，把所有可指挥的战斗逐州点亮到地图上；边境战争（如诺门坎）、远征军与盟友领土作战、志愿军、登陆战、空降战现在都可以进入战术战斗；同一州存在多场战斗时，选择器会区分「陆战」与「边境战争」
- 扫描体验改进：点击扫描立即暂停游戏并当场给出回执（程序未运行会立刻提示，不再无声失败）；每 12 小时自动复查一次
- 声测定位与反炮兵：敌方间瞄火炮开火会暴露发射格标记（持续 3 回合）；选中炮兵按 F 可对标记区域覆盖反炮；AI 也会进行反炮兵作战
- RTS 模式（测试）：一键全自动连打，带专属速度滑条（0.5×–4.0×），顶栏提示全程静默
- 师行动进度记忆：师改指目标后，若重新指回原方向，已积累的行军进度保留
- 新增「全体推进」按钮：一键命令全师向前推进
- 控制区迟滞：邻近近战的格子按双方组织度×兵力比例减速
- 战略映射扩展：师的补给率与燃油率现在会影响战术数值（组织度上限、攻防、速度等），兵牌面板新增两行显示
- 部署区规则改进一批：登陆战（攻方占据全部滩头、守方纵深全域部署）、室内战混合部署区、多来源方向组合部署、按来源方向部署与走廊倾向
- 支援连效果扩展与修正：工兵（地形/堑壕）、后勤（围困损耗减半）、宪兵（不投降）与 8 个具名支援连的特殊效果实装；集团军 HQ 师正常装配参战
- 整点同步期间窗口不再冻结，显示「同步中」旋转指示
- 俄语界面（第三语言）：程序与 mod 全量俄语本地化，地名按界面语言显示；俄语系统首次启动自动预选
- 战斗日志落盘：最近一场战斗的日志自动保存到文件，诊断包自动携带
- 关于页新增鸣谢/特别鸣谢名单

**BUG 修复**
- 修复科技与国家精神加成一直未实际生效的问题（存档读取路径错误）
- 修复按真实坦克设计计算的装甲单位防御/突破数值恒为 0 的问题
- 修复未满员营连装备核算：一丝装备不再全功能上阵，缺机动装备会正确降速
- 修复战斗结束后进战图标在地图上复活/残留的问题
- 修复关键点占领值衰减与回落规则的两处错误
- 修复 AI 炮兵/骑兵/HQ 空闲时错误获得据守加成的问题
- 修复溃军朝进攻方方向撤退的问题
- 修复守军全灭后 AI 炮兵反复盲轰空地的问题
- 修复盟军部署方向错误与敌侧友邦兵牌配色错误
- 修复「文档」目录为本地化名称或被重定向（如土耳其语、OneDrive）时安装器与注入失败的问题
- 存档目录填错一层时，设置页会显示黄色警告
- 修复 HOI4 侧战斗已结束而战术层探不到结局的问题
- 修复移动改道进度超投、HQ 崩溃误伤同名友军师、缝合地图河流标错岸、Shift 连续下令 panic 等问题
- 外部静态审查累计修复约 40 处内部一致性与稳定性问题

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

## v0.3.3 (2026-10-13)

**New features**

- Battle-entry overhaul: a new "Scan for commandable battles" decision — clicking it makes the program scan the current save and light up every battle you can command as a per-state map icon; border wars (e.g. Khalkhin Gol), expeditionary / allied-territory fighting, volunteers, naval invasions and paradrops can now all enter tactical battles; when one state holds several battles, the picker distinguishes "Land battle" from "Border war";
- Scan experience: scanning pauses the game immediately and acknowledges right away (if the program is not running you are told on the spot instead of failing silently); an automatic re-check runs every 12 hours;
- Sound ranging & counter-battery: enemy indirect-fire artillery reveals its firing hex with a marker (lasts 3 turns); select artillery and press F to barrage the marked area; the AI conducts counter-battery fire as well;
- RTS mode (beta): one-click fully automatic continuous play with a dedicated speed slider (0.5×–4.0×); top-bar banners stay silent;
- Division movement-progress memory: after retargeting a division, moving it back toward the original direction keeps the progress already made;
- New "Advance All" button: orders the whole division forward with one click;
- Zone-of-control drag: hexes adjacent to close combat slow movement in proportion to both sides' organization × strength;
- Strategic mapping extensions: a division's supply and fuel rates now feed into tactical stats (organization cap, attack/defense, speed, etc.), shown as two new lines on the counter panel;
- A batch of deployment-zone improvements: naval invasions (attackers hold every beachhead, defenders deploy in depth), mixed deployment zones for interior battles, combined multi-origin deployment, deploy-by-origin with corridor preference;
- Support-company effects expanded and corrected: engineers (terrain/entrenchment), logistics (halved encirclement attrition), military police (no surrender) and special rules for 8 named support companies; army-group HQ divisions are now properly assembled into battle;
- The window no longer freezes during hourly syncs — a "synchronizing" spinner is shown instead;
- Russian UI (third language): full Russian localization for the program and the mod, with place names following the UI language; Russian systems get it pre-selected on first launch;
- Battle log on disk: the most recent battle's log is saved to a file and automatically included in diagnostic bundles;
- Credits / special-thanks list added to the About page.

**Bug fixes**

- Technology and national-spirit bonuses were never actually applied (wrong save-file read paths) — fixed;
- Armoured units computed from real tank designs always had 0 defense/breakthrough — fixed;
- Understrength battalions' equipment accounting: a sliver of equipment no longer fields a fully functional unit, and missing motorized equipment now slows units correctly;
- Battle-entry icons no longer reappear or linger on the map after a battle ends;
- Two fixes to the objective capture-value decay/fallback rules;
- AI artillery/cavalry/HQs no longer wrongly gain the Hold bonus while idle;
- Routed units no longer retreat toward the attacker's side;
- AI artillery no longer keeps blindly shelling empty ground after the defenders are wiped out;
- Allied deployment directions and hostile-side ally counter colors fixed;
- Installer and injection no longer fail when the Documents folder has a localized name or is redirected (e.g. Turkish, OneDrive);
- The settings page now shows a yellow warning when the saves directory is one level off;
- Fixed the tactical layer failing to notice that a battle had already ended on the HOI4 side;
- Fixed movement over-investment on retargeting, HQ collapses hurting same-named friendly divisions, stitched maps marking rivers on the wrong bank, a Shift-chained-orders panic, and more;
- Roughly 40 internal consistency and stability fixes from external static reviews.

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
