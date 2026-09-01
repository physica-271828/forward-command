[English](README.md) | **简体中文**

# 前敌指挥 / Forward Command

一个 Hearts of Iron IV 的战术战斗外置程序：战略地图上的师级交战，不再看骰子，
变成玩家亲自指挥的六角格战术战斗。

> 个人兴趣作品，更新随缘，无时间表。

---

## 这是什么

两军在 HOI4 战略地图上交火时，前敌指挥让玩家亲自接管这场战术战斗。整个流程分为四步：

1. 伴随 mod 侦测到交战，通过 `game.log` 向外部程序发出信号。
2. 外部程序读取当前存档，解析出参战师的编制、装备、组织度、兵力、科技与学说，再为交战省份生成一张六角格战术地图，地形、河流、城市和进攻轴线都来自真实省份数据。
3. 玩家在 3D 战术界面中逐回合指挥：部署兵力、机动接敌、发起突击、呼叫炮兵火力、下达师级命令、夺旗与合围。战场笼罩在战争迷雾中，对手是一套三层 AI，会使用取自 HOI4 本体战斗战术的 16 种战术卡。
4. 战斗同步时，战果按组织度和兵力伤害写回 HOI4 中的真实师，战略时钟随之推进。玩家可以连续打多个战略小时，也可以随时结束战斗，把局势交还给战略地图。

界面语言支持简体中文与英文，可在设置页随时切换。

## 工作原理

程序与游戏之间有三条数据通道（完整协议见 `DESIGN.md`）：

| 通道 | 方向 | 内容 |
|------|------|------|
| `game.log` | mod → 程序 | 触发信号（`tac_start`、心跳等），JSON 行格式 |
| 存档文件 | 游戏 → 程序 | 师编制、装备、组织度/兵力、科技（Clausewitz 文本格式） |
| 控制台注入 | 程序 → mod | 伤害数值与同步标记，经 `set_var` + 脚本效果（`run tac_inject.txt`，Windows `SendInput`） |

所谓注入，就是用合成按键替玩家在 HOI4 控制台里敲命令，原理就这么简单。
源码全部公开，欢迎逐行审查：程序**不联网**，不读取 HOI4 目录和自身文件夹以外
的任何位置，除了 `docs/免责声明.md` 里声明的交互之外什么也不做。

## 运行环境

- **Windows**（控制台注入通道仅支持 Win32）
- **Hearts of Iron IV 1.19.\***，单人模式，**必须是文本存档**
  （`settings.txt` 中 `save_as_binary=no`，安装器会检查并警告）
- 建议全 DLC；DLC 缺失可能导致装备/编制数据错位
- 铁人与多人模式不支持

## 安装（玩家）

mod 渠道**二选一，切勿同时启用**（两个同名 mod 同时加载会导致冲突）：

- **已订阅创意工坊版**：跳过 `install-mod.bat`，mod 由启动器自动安装与更新。
- **未订阅（本地版）**：按下面步骤运行安装脚本。

1. 下载并解压最新发布 zip。
2. 双击 `install-mod.bat`，把伴随 mod 装进 HOI4 用户目录（并清理旧版残留）。
3. 在 HOI4 启动器启用 **Forward Command**，重启游戏。
4. 运行 `forward-command.exe` 保持监听，正常玩 HOI4。战斗打响后，通过游戏内
   决议选择接管战术指挥。

完整玩家说明书：`docs/玩家说明书.md`。

## 从源码构建

```
cargo build --release --workspace
```

- Rust stable，`x86_64-pc-windows-gnu` 工具链（构建与测试需要 GNU 链接器
  环境，如 WinLibs 加入 `PATH`）
- 工作区的 `.cargo/config.toml` 模板可能需要按本机环境调整（链接器路径、
  注册表镜像）
- `data/` 下的 JSON 表是从游戏文件预提取的；如需重新生成，`extractor/`
  下的 Python 脚本可从本机 HOI4 安装重新提取
- `cargo test --workspace` 运行单元测试套件（少数测试需要本机装有 HOI4，
  默认忽略）

## 文档

- `DESIGN.md`——完整设计规格（架构、协议、地图生成、战斗模型、AI、时间
  模型、UI、mod、设置项）
- `docs/玩家说明书.md`——玩家手册（中文）
- `docs/免责声明.md`——免责声明（机制告知、杀毒软件指引、风险声明）
- `HOI4_UNITS.md`——HOI4 陆军单位清单 → 内部单位映射

## 第三方资产

- UI 图标：**Twemoji**，[CC-BY 4.0](https://creativecommons.org/licenses/by/4.0/)
  ——见 `assets/ATTRIBUTION.md`
- CJK 回退字体：**Noto Sans SC**，SIL 开放字体许可——见
  `assets/fonts/LICENSE-OFL.txt`
- 从使用者本机 HOI4 安装提取的游戏数据，版权归 Paradox Interactive 所有。

## AI 协作声明

本项目由作者主导设计，代码、文档与文案主要依托 AI 助手协作完成，以
**Kimi K3**（Moonshot AI）与 **DeepSeek V4**（DeepSeek）为主。作者未对
Rust 代码逐行审查：全部功能均经实机运行逐项验证，代码正确性由项目的
自动化测试套件保障，源码公开以供有能力者自行审查。

## 免责声明

本项目为非官方爱好者作品，**与 Paradox Interactive 无任何关联，未获其认可或
背书**。Hearts of Iron IV 及其全部素材归 Paradox Interactive AB 所有。程序通过
控制台自动化修改游戏状态；虽然清理路径已经过测试，但是**重要存档请先备份**。使用
风险自担，完整文本见 `docs/免责声明.md`。

## 支持

本作品免费且永远免费，没有任何付费内容。如果它给你带来了快乐，欢迎自愿
投币支持开发成本（AI 助手订阅、电费等）：

[爱发电](https://ifdian.net/a/forward-command)

打赏与下载、更新、内容完全无关，不影响任何反馈的处理。

反馈与交流：QQ 群 960 830 355（中文，最新测试版与讨论），或
[GitHub Issues](https://github.com/physica-271828/forward-command/issues)
（任何语言均可）。

## 许可

Copyright (C) 2026 physica

本程序为自由软件：可按 **GNU 通用公共许可 v3.0**（GPL-3.0）的条款再分发
和/或修改，全文见 `LICENSE`。上述第三方资产保留其原有许可。
