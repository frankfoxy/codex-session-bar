# Codex Session Bar

**Codex 桌面端的轻量会话悬浮条：一眼看进度，一键切会话。**

把当前会话、Token 用量、生成速度和剩余额度放进标题栏空白处。多任务运行时，随时查看哪些会话还在忙，快速回到想看的聊天。

**Rust 原生 Windows 版** · 运行不需要 Python 或 Qt · 中文使用微软雅黑，支持多屏 DPI 缩放。

![悬浮条位于 Codex 标题栏的菜单与窗口按钮之间](preview-titlebar.png)

| 实时掌握进度 | 快速切换会话 | 自定义会话图标 |
| :--- | :--- | :--- |
| 状态、耗时、Token、TPS、额度 | 最近 10 个会话，Busy 优先 | 完整 Emoji、分类、搜索和随机选择 |

## 01 · 进度与额度，一条看全

![单行状态悬浮条](preview-bar.png)

- **正在忙几个？** 橙色 `Busy (5)` 显示运行中的会话数量。
- **用了多少、跑得多快？** 同行显示当前会话累计 Token、输出 `t/s` 和本轮耗时。
- **还能用多少？** 矢量电池展示剩余额度，旁边圆环提示距离重置的进度。
- **随窗口走。** 自动跟随当前聊天和 Codex 窗口的移动、隐藏与恢复；支持拖动，始终置顶。

## 02 · 多个会话，点开就切

点击悬浮条上的**会话名称**，展开 Recent chats。

![最近会话列表与运行中会话的汇总指标](preview-chats.png)

- **Busy 优先显示**，用橙色状态和底色区分；右端显示完成、等待等状态。
- **一行看完指标**：运行中会话显示速度、累计 Token 和持续时间；顶部汇总全部已扫描 Busy 会话的速度与 Token。
- **后台预加载**，点击即可显示已有列表；点击任意条目切到对应会话。
- **选择时不跳动**：列表打开期间固定条目顺序，状态和数值继续更新；关闭后再打开，才按最新状态排序。

需要持续盯住一个会话时，在详情里点击 **Choose a fixed chat…**。彩色 Emoji 和状态帮助区分会话，支持搜索、滚轮浏览及方向键选择；确认后关闭自动跟随，固定显示选定会话。

![深色固定会话选择面板，带彩色 Emoji、Busy 状态和选中标记](preview-fixed-chats.png)

## 03 · 给会话一个容易认的图标

点击悬浮条左端图标，即可展开完整选择面板，修改当前会话的图标。

![完整 Emoji 面板，最左边骰子按钮可随机选择](preview-icons.png?v=rust)

**首屏是“默认集合”**：保留原来的 90 个常用图标及排列顺序。下拉选择 **All emoji** 可浏览 Unicode Emoji 18.0 全集的 3,972 个图标与组件，也可选择其他分类。支持英文名称或 Emoji 搜索，包含肤色、国旗和组合 Emoji。

点击最左边 **🎲** 随机换成不同的图标，也可一键清除。修改会同步到 Codex 的会话标题；切换聊天时，悬浮条上的图标一起变化。新 Emoji 的显示取决于系统字体。

> 图片为界面演示，标题栏使用“xxx 会话名称”和示例速率，不包含个人聊天记录。

## 快速开始

运行需要 **Windows 10/11、Node.js、已登录的 Codex Desktop**，以及桌面自带的 App Tools 插件。无需安装 Python、Qt 或额外的 Codex CLI。

以下是从源码构建的步骤，需要已安装 **Rust MSVC 工具链和 Visual Studio C++ Build Tools**。已构建的程序可直接运行。

**1. 获取项目**

```powershell
git clone https://github.com/frankfoxy/codex-session-bar.git
cd codex-session-bar
```

**2. 构建原生程序**

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\Build.ps1
```

**3. 启动**

打开 Codex，然后双击 **Launch.cmd** 或 **bin\codex-session-bar.exe**。默认英文、单行显示，自动跟随当前聊天。首次启动停留在首页时，也能查看额度、Busy 汇总和最近会话。

### 可选：登录后自动启动

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\Enable-Autostart.ps1
```

Codex 关闭时工具在后台等待，重新打开后自动出现。安装时也会安装“打开会话条”技能。取消自启动可在 Windows 任务计划程序中删除 `Codex Session Bar` 任务。

### 收起后怎么打开？

- 点击 **×** 只收起到 Windows 系统托盘，后台继续运行。
- 点击托盘里的 **Codex Session Bar** 图标，或右键选择 **Open session bar**，即可恢复。
- 在 Codex 对话里说 **“打开会话条”**，或使用 **`$codex-session-bar`**；安装技能后可恢复隐藏的工具条，也可重新启动已退出的工具条。新技能可能需要在新的对话中调用。
- 需要完全退出时，在托盘菜单或详情面板选择 **Quit session bar**。

## 日常操作

| 点击位置 | 操作 |
| :--- | :--- |
| 左端图标 | 更换或清除当前会话图标 |
| 会话名称 | 打开最近会话列表，点击条目切换 |
| 中间六点手柄 | 拖动悬浮条 |
| 向下箭头 | 查看详情、开关 Follow current chat、恢复标题栏停靠 |
| `×` | 收起到系统托盘 |
| 托盘图标 | 打开悬浮条；右键菜单可完全退出 |

## 数字怎么看

| 指标 | 含义 |
| :--- | :--- |
| `27.2M` / `TOK` | 会话累计 Token，包含重复发送的上下文；不是费用金额 |
| `t/s` | 最近保存样本的输出 Token 速度，包含采样间的工具和网络等待 |
| `Busy (N)` / `Total` | 全部已扫描的本地 Busy 主会话，可能多于列表中的 10 条 |
| 电池百分比 | 账户共享的剩余额度 |
| `—` / `*` | 数据暂不可用 / 数据不完整或沿用缓存，悬停可查看说明 |

本项目是独立的 Windows 伴随工具。Codex 更新可能影响兼容性；出现无法识别当前聊天或连接失败时，会明确提示。

[开发与故障排查](DEVELOPMENT.md) · [参与贡献](CONTRIBUTING.md)

Emoji 数据来自 [Unicode 官方 Emoji 数据](https://unicode.org/Public/emoji/latest/)，许可见 [UNICODE-LICENSE.txt](UNICODE-LICENSE.txt)。
