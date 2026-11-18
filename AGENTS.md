# FeatherDock Agent 总文档

本文件是项目唯一的**需求、产品约束与协作规则入口**。后续任务只使用下面三个固定文档：

| 文件 | 只记录什么 | 更新时机 |
| --- | --- | --- |
| `AGENTS.md` | 已确认需求、长期技术约束、协作规则 | 需求或约束变化时 |
| `PLAN.md` | 尚未完成的下一步计划 | 计划调整、任务完成或新增时 |
| `REVIEW.md` | 最近一次审查/验证结论及必要历史摘要 | 每次审查、构建或桌面验收后 |

- 不再新增 `HANDOFF`、`DESIGN-日期`、`IMPLEMENTATION-日期`、`REVIEW-日期` 等分散入口。
- `README.md` 面向使用者；`examples/` 只保留可运行示例和协议用法；`outputs/`、`.dsh/tmp/` 只放日志、截图等原始证据，不放新的 Agent 报告。
- 完成事项从 `PLAN.md` 移除，结果写入 `REVIEW.md`；不要在本文件堆积每次执行日志。
- 旧交接、设计、调研和审查文档已于 2026-11-14 合并到这三个文件。

## 1. 产品定位与不可回退的方向

FeatherDock 是 Windows 10+ 的轻量原生 Dock：

- Rust + Win32 + DirectComposition + Direct2D + D3D11；不引入 Electron、WebView 或常驻后台扫描服务。
- 静止时事件驱动，不用轮询制造动画、窗口状态或任务状态；弹层关闭后停止非必要工作。
- 保留「折纸收纳」品牌资源与「暖瓷玻璃」默认主题。配置键继续使用 `theme = "glass"`，其余 `compact`、`solid`、`macos`、`contrast` 不得失效。
- 原生菜单、输入框和设置窗口优先保证可读性与键盘行为，不为透明效果牺牲对比度。
- 全屏游戏免打扰是硬约束。不得为了截图或验收强行唤出 Dock、抢占前台或修改用户的隐藏设置。
- 保留任务栏与桌面图标的异常恢复能力，以及 Shell 图标提取的助手进程隔离。

## 2. 已具备能力，避免重复立项

当前工作区已经包含：

- 主条宽度预算、更多应用、窄屏自适应、弹层边界约束和单显示器选择。
- 后台桌面扫描、图标缓存、异步结果通知、固定项拖拽排序和保存失败回滚。
- 拼音/首字母搜索、别名、最近使用、运行窗口独立可达和键盘导航。
- 文件夹 Stack、DWM 窗口预览、分页控制中心、五种主题、布局预览/保存/取消恢复。
- WindowsApps/快捷方式/AppUserModelID 等应用身份匹配，以及全屏边缘 1 物理像素容差。
- Agentic Dock Phase 1 的主体实现：Agent 分区、状态聚合、会话卡、事件协议和生命周期保护。

历史调研中的「全部应用」「视觉文件夹面板」「完整 UIA provider」「每屏独立 Dock」「媒体控制」仍属于后续增量，不要误写为现有能力。

## 3. Agentic Dock Phase 1 需求基线

### 3.1 分区与布局

主条按以下顺序组织：

```text
[Start] [Drawer] [常规固定应用] | [普通运行窗口] ‖ [Agent 区] [Overflow?] [系统托盘] [Control]
```

- 首批 Agent 为 `antigravity`、`codex`、`dsh`；同一 Agent 的固定项和运行窗口聚合成一个槽位。
- Agent 区为空时隐藏；普通项与 Agent 固定项各自内部排序，不跨区拖拽。
- 溢出时优先保留 `Waiting`、`Error`、`Running` 等活跃 Agent，再保留普通运行项。
- 系统托盘按钮按需唤出 Explorer 的原生「隐藏的图标」面板。任务栏完全隐藏时保持透明和自动隐藏，点击只打开托盘面板。遵守全屏免打扰。
- 非标准可执行文件名必须在 `featherdock.toml` 显式声明，例如 DeepSeek Harness：

```toml
[[item]]
label = "DeepSeek Harness"
path = "C:\\Path\\To\\DeepSeek Harness.exe"
kind = "agent"
agent = "dsh"
```

### 3.2 状态与视觉

状态优先级固定为：

```text
Waiting > Error > Running > Done > Idle
```

- `Idle`：静止图标，只保留正常窗口运行指示。
- `Running`：低亮度薄荷流光；仅在主条可见且未启用减少动画时运行。
- `Waiting`：琥珀状态点，首次进入且目标不在前台时最多提醒一次。
- `Error`：柔红状态点；`Done`：未读完成点，精确查看对应会话后消除。
- 会话角标、会话卡内容与主条聚合状态必须来自同一数据模型，不猜测未接入的任务进度。

### 3.3 双轨数据源

- **Track A：仅识别窗口。** 通过 WinEvent/窗口枚举识别 Agent 窗口；明确标注「仅识别窗口」，不得伪造 Running/Waiting。
- **Track B：任务事件。** 使用 `FeatherDock.exe --agent-event` + `WM_COPYDATA`，或 `%LOCALAPPDATA%\FeatherDock\agent-sessions\*.json` 原子文件事件。
- 两种通道共用 `agent`、`session_id`、`state`、`title`、`detail`、`cwd`、`instance`、`seq`、`pid`、`hwnd`、`deep_link`、`updated_at` 字段。
- 文件写入必须同目录临时文件后原子重命名；监听从内核目录事件唤醒。
- `deep_link` 只允许 `antigravity://`、`codex://`、`dsh://`、`vscode://`、`cursor://`，事件不得触发任意 shell 命令。

完整示例见 `examples/agent-adapters/README.md`。

### 3.4 生命周期、顺序与可达性

- JSON 解析必须有总大小和嵌套深度限制；畸形输入只拒绝当前事件。
- 同实例使用单调递增 `seq`；旧事件不得覆盖新状态或重新打开已关闭会话。
- 绑定 HWND 的会话必须精确匹配焦点，不能用同 Agent 的另一个窗口消警。
- 窗口销毁、进程退出和文件重载共享 Tombstone 规则，旧 JSON 不得复活假运行状态。
- 接收和激活外部 HWND 时都校验窗口、PID/进程实例与 Agent 身份；失败时回退到可信窗口。
- 会话卡按工作区和 DPI 计算可见行数；超出可见范围的项目必须能通过滚动或「查看全部」到达。
- 指针离开 Dock 与卡片后，经短暂宽限关闭卡片；不为离开检测增加常驻轮询。

## 4. 后续产品方向（只保留确认过的优先级）

Phase 1 收尾后，优先顺序为：

1. 完整「全部应用」入口、实用的文件夹面板、清楚且可恢复的设置。
2. 键盘完整可达、搜索空态/历史管理、应用显隐例外、配色与布局密度解耦。
3. 麦克风/媒体控制、Dock 应用组、场景配置、鼠标所在屏唤出。

Phase 2/3 的会话管理、控制中心 Agent 总览和跨 Agent 工作流，详见 `PLAN.md` 的待规划项。每屏独立 Dock、四边布局、任意脚本插件、动态壁纸引擎、通用消息数/托盘镜像暂不扩张。

## 5. 关键技术约束与模块地图

| 模块 | 责任与约束 |
| --- | --- |
| `src/main.rs` | 消息循环、窗口/DPI、弹层协调、GPU 恢复；先释放旧窗口合成目标再重建 |
| `src/agent_state.rs` | Agent 识别、状态聚合、JSON、顺序、焦点、生命周期与 Tombstone |
| `src/agent_popup.rs` | 会话卡布局、滚动、离开关闭、点击激活 |
| `src/config.rs` / `src/settings.rs` | 配置兼容；后台只读，迁移和维护写回走明确入口 |
| `src/dock.rs` / `src/render.rs` | 布局预算、命中包络、Agent 视觉和溢出优先级 |
| `src/drawer.rs` / `src/drawer_input.rs` | 抽屉交互；跨嵌套消息循环使用弱生命周期身份和拥有参数的动作 |
| `src/graphics.rs` / `src/backdrop.rs` | GPU、图标批次、背景材质；图标 source/代际必须匹配 |
| `src/windows_list.rs` | WinEvent 窗口分组、激活、全屏/最大化判定 |
| `src/taskbar.rs` / `src/watchdog.rs` | 任务栏策略与异常恢复；生命周期修改需同时检查 |
| `src/native_tray.rs` | 按需在独立线程触发 Explorer 原生托盘面板；触发前复核前台与全屏状态 |

额外红线：

- 菜单、分类编辑和异步扫描使用稳定 entry key；菜单打开期间合并刷新，销毁后的旧实例不得执行动作。
- 命中区域由实际绘制基线、底部距离、跳动和放大包络推导。
- 不提交用户配置、缓存、个人路径、日志、构建产物或本机工具链。
- `THIRD_PARTY_NOTICES.md` 随发布说明保留。

## 6. 构建、发布与桌面执行

### 6.1 固定命令

```powershell
.\build.cmd -Verify -Stage  # 格式、严格 Clippy、测试、release；只暂存
.\build.cmd                 # 发布根目录 FeatherDock.exe
.\FeatherDock.exe --quit    # 正常退出
```

- 唯一运行/分发入口是根目录 `FeatherDock.exe`；`target/` 与 `outputs/pending/` 不是日常入口。
- 最终 EXE 必须为 Medium 完整性；保留 `build.ps1` 的设置和检查步骤。
- `-Stage` 与 `-Run` 不组合；自定义 `CARGO_TARGET_DIR` 前先核对解析后的清理范围。
- 遇到受限执行器中的 `0xc0000142`，使用 `scripts/quiet-run.py` 和可执行宿主，不反复启动 Rust 工具链。
- 不按映像名强杀 FeatherDock；主程序与 watchdog 是同一个 EXE。异常恢复才使用 `--restore-system`。

### 6.2 Agent 执行环境

- **真实桌面收尾统一在交互桌面执行。** 当前 DSH 交互会话可枚举真实窗口并投递 `WM_COPYDATA`，优先用于发布、启动、截图、UIA 和 IPC 验收。
- Antigravity 新会话从 **Dock 项目**进入，不以 `outside-of-project` 作为本项目会话；项目权限和命令模式在项目上下文维护。
- Codex 的全局 `danger-full-access` 不等于 Node/Computer-use runner 也处于交互桌面。看到 `CodexSandboxOffline` 或 `EnumWindows=0` 时，把 GUI 验收交给 DSH，不重复尝试跨桌面窗口消息。
- 全屏游戏处于前台时只记录「视觉验收待执行」，不得移动鼠标、切换焦点或改变系统设置来制造截图。

### 6.3 每次变更的基本流程

1. 先读最新用户要求、`git status --short`、本文件、`PLAN.md`、`REVIEW.md`。
2. 保留工作区已有修改；新增源码和脚本不能只靠 `git commit -am` 检查。
3. 更新需求/计划/审查三个文件中的对应项，避免代码与文档脱节。
4. 至少运行 `build.cmd -Verify -Stage`；真实 GUI、性能或兼容结论必须有对应实测证据。
5. 未经用户明确要求，不创建 Git commit 或 push。
