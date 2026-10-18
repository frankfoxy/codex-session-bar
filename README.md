# Codex Session Bar

A compact Windows companion bar for Codex Desktop, with live session status, token usage, TPS, SVG quota batteries, and a Busy-first recent-chat switcher.

Windows 上的 Codex 桌面伴随工具：在标题栏空白处显示真实会话状态、Token、TPS 和账户额度，并支持最近会话切换。这是独立项目，使用本机 Codex 数据和桌面接口。

运行要求：Windows 10/11、Python 3.10+、Node.js（可从 PATH 找到），以及已登录的 Codex Desktop。额度查询需要可用的 Codex CLI；会话切换与图标同步依赖桌面自带的 `codex-app-tools` 插件和当前唯一的桌面连接。

首次使用，在项目目录运行 `python -m pip install --no-cache-dir -r requirements.txt`，然后双击 `Launch.cmd`。启动器优先使用现有计划任务，其次使用 PATH 中的 `pythonw.exe` 或 `pyw.exe`。可用 `CODEX_SESSION_BAR_PYTHON` 显式指定 Python 路径。

可选运行 `powershell -NoProfile -ExecutionPolicy Bypass -File .\Enable-Autostart.ps1` 安装当前用户的 Windows 任务 `Codex Session Bar`：登录自动启动，独立于 Codex 的进程生命周期。Codex 关闭时后台等待，重新打开后自动显示；启动时页面尚未加载会继续重试。异常退出最多自动重启三次。主动点击「×」会退出本次运行，之后可在任务计划程序启动该任务。取消自启动可删除同名任务。

- 默认英文、单行布局，固定高度 40px，宽度随标题和数据收缩；Working 用橙色标记，Token 简写为 `27.2M`。拖动中间的六点手柄移动，图钉切换置顶，「×」退出。详情用独立浮层显示，展开/关闭不改变悬浮条高度。
- 显示输出 TPS（`t/s`）：最近两次已保存用量的输出 Token 增量除以采样时间差，包含两次采样间工具和网络等待，并非瞬时模型解码速度；当前轮次不足两个采样时显示 `— t/s`。悬停可查看计算口径。
- SVG 电池按真实剩余额度比例填充：75% 起绿色、50% 起蓝色、25% 起橙色、更低为红色；右侧圆环表示当前额度窗口距离重置的时间比例。主条显示最多两个额度窗口及百分比，悬停查看全部窗口、重置时间与说明；缺失额度显示未知。
- 点击图标打开与现有 Claude session-bar 相同的 90 个 emoji：彩色圆点/方块优先，然后按状态、硬件、软件等分组。选择或清除会更新所绑定会话的真实标题。
- 点击会话名称打开最近 10 个本地根会话（排除子代理和归档），从最近记录中把 Busy 优先排列，橙色圆点和底色突出显示；右侧显示 Busy / Done / Waiting / Stopped / Failed / Ready。实时运行状态来自桌面 `list_threads`，结束状态结合最后一轮记录，列表打开时每 2 秒刷新。点击任意一行通过 `navigate_to_codex_page` 切换 Codex 页面并恢复自动跟随；名称只负责点击，拖动使用中间的独立六点手柄。
- 默认「跟随」当前正在查看的 Codex 聊天：标题、图标、状态和 Token 自动切换。通过 Windows UI Automation 只读获取当前可见主窗口的页面标题，精确匹配唯一的本机会话；不读消息正文，不依据后台流活动日志或任务活跃时间猜测。切换时先清空旧数据，并关闭旧图标选择器，避免改错会话。
- 默认自动跟随，开关放在下拉详情面板的「Follow current chat」。详情面板手动选择会话会关闭跟随；重新勾选恢复自动跟踪。
- 默认放在 Codex 标题栏内部的空白处：菜单文字右侧、最小化/最大化/关闭按钮左侧。独立观察线程只读标题栏控件矩形，空白区随实际菜单宽度更新；窗口过窄、无法完整放下时移到菜单栏下方，避免覆盖按钮和文字。「Dock in title bar」恢复默认停靠，升级时自动迁移旧位置。
- 点击向下箭头选择绑定会话、查看 Token 明细和全部额度窗口。
- 每 80ms 只读检查主窗口位置和可见状态：移动/调整大小时保持相对位置；最小化、隐藏、切到其他虚拟桌面或关闭主窗口时一起隐藏，恢复时重新显示。手动拖动会保存相对偏移，仍然随主窗口移动。窗口联动与会话跟随开关独立。
- 位置、置顶和跟随偏好会保存。离开会话页、标题重复或无法确认会话时禁用图标操作，不沿用上一个聊天的数据；可切为固定模式手动选择。没有本地数据的会话明确提示不可用。
- 会话视图每 0.1 秒增量检测，识别切换后立即唤醒数据读取；状态和 Token 每 0.5 秒刷新。快速连续切换时丢弃已过期的读取结果，只显示当前选中的会话。
- 状态来自本机历史数据库；Token 来自 JSONL 的有界尾部及增量读取。额度查询与改名各自独立运行，网络延迟不会阻塞会话跟随。读取失败时明确提示，不展示模拟值。
- Token「会话累计」包括重复发送的上下文；「最近调用」是最近一次模型请求，不能当作整轮消耗。缓存输入包含在总输入中。Token 在 Codex 保存用量事件后刷新，并非逐字流式估计。
- 额度来自官方 `account/rateLimits/read`，每 60 秒刷新，可手动刷新；账户共享，非某个会话独享。刷新失败保留数据并标「*」，悬停说明数据状态。缺失值不当作零。
- 内置状态库只读打开；改名通过桌面自带 `codex-app-tools` 的 `set_thread_title`，同时通知 App 刷新侧栏。独立 `app-server` 的 `thread/name/set` 仅改持久记录，不能用于这里的实时同步。桌面连接失败明确报错，不降级成只保存名称。
- 不提交提示词、不恢复/中断任务、不直接修改 Codex 数据库。双击启动时自动连接当前唯一的 Codex 桌面实例；多个实例无法确定目标时提示从 Codex 内重启。
- 运行日志、诊断和截图只在当前工作区 `tmp/codex-session-bar/`；偏好保存在本工具目录的 `settings.json`。

默认使用项目根目录的 `tmp/codex-session-bar/` 保存运行内容；放在工作区 `tools/codex-session-bar` 时使用该工作区根目录的 `tmp`。也可用 `CODEX_SESSION_BAR_WORKSPACE` 指定已有工作区。会话 ID 由本机环境、偏好或最近本地会话动态确定，仓库不包含账户凭据、个人会话配置或使用记录。

诊断：`python session_bar.py --probe`。界面验证：`python session_bar.py --test-ui`。再次启动会显示已有悬浮条，避免重复运行。

开发验证：`python -B -m unittest discover -s . -p test_session_bar.py -v`。无桌面交互的界面验证可先设置 `QT_QPA_PLATFORM=offscreen`，再运行 `python -B session_bar.py --test-ui`；验证图片也只写入工作区 `tmp`。只提交源代码、测试和安装文档，`settings.json`、日志、会话数据、缓存和临时文件均由 `.gitignore` 排除。

当前版本适配本机 `state_5.sqlite`、`thread_history_1.sqlite` 和桌面工具接口；Codex 更新可能改变这些内部格式。`--probe` 会重设当前图标为原值，以验证真实桌面同步；单元测试和 `--test-ui` 不会提交提示词或切换用户桌面会话。
