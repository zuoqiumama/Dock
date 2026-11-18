# DSH → FeatherDock Agent Bridge

把 DeepSeek Harness 的真实会话生命周期发布到 FeatherDock，替代手动 `send-agent-event.ps1` 测试事件。

## 状态映射

| DSH 权威事件 | FeatherDock |
| --- | --- |
| `turn/start` | `Running` |
| `approval/asked` | `Waiting` |
| `ask_user_question` 工具调用 | `Waiting` |
| `approval/decided` / 问题结果 | `Running` |
| `turn/end: completed` | `Done` |
| `turn/end: error / max-tokens` | `Error` |
| `turn/end: blocked` | `Waiting` |
| 取消、停止或会话销毁 | `Idle` / 删除会话 |

标题来自首个有意义的会话标题或公开用户文本，锁定后不被后续宿主通知覆盖；工作目录来自 Session header / 工作目录事件。适配器不读取或转发推理块、请求头、工具参数和原始模型响应；详情仅保留有界公开摘要。

## 工作方式

- 作为独立 DSH Host 插件运行，使用 `agent/created`、`agent/disposed`、`session/event`，不扫描窗口、不读取界面文字。
- 默认写入 `%LOCALAPPDATA%\FeatherDock\agent-sessions\`，与 Dock 现有文件事件通道一致。
- 每个顶层主会话独立哈希文件名、单调 `seq`、进程级 `instance`、真实 Host `pid`，同目录临时文件原子替换。
- 内部 `origin=subagent` / `delegationDepth` 子 Agent 按 `parentSession` 聚合到主会话，不单独占行或覆盖主标题；用户 fork 的独立会话不按 `parentSession` 合并。孤儿与循环谱系保持隐藏。
- 启动恢复保留旧 Idle 的真实最后事件时间，不把历史会话刷新成“刚刚”；Dock 默认只显示最近 30 分钟空闲历史，可在 Dock 设置中调整。
- 没有轮询、子进程或额外 npm 依赖。插件卸载只清理仍属于自己实例的文件，不会误删热重载后新实例的状态。
- 启动时最多投影最近 4096 条事件，用于长会话标题和当前状态恢复；历史 Done 不会作为新提醒重放，真实 Waiting 会保留。
- Dock 尚未运行时，当前状态仍保存在会话目录；Dock 启动后会读取它。

## 测试

使用 DSH 随附或普通 Node.js 24+：

```powershell
node --test tests/state.test.mjs tests/transport.test.mjs
```

## 安装

在 DSH 插件管理器中选择“添加插件”→“安装第三方插件”，输入本目录的绝对路径并启用。也可使用 Desktop CLI：

```powershell
$env:ELECTRON_RUN_AS_NODE = '1'
& 'C:\Path\To\DeepSeek Harness.exe' `
  'C:\Path\To\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\cli.js' `
  plugin --profile desktop add 'C:\Path\To\dsh-featherdock'
```

安装后发送一轮真实对话，Dock 的 dsh 会话卡应自动显示 Running；审批或提问时显示 Waiting，完成后显示 Done。此插件只支持 DSH，不会把 Codex 或 Antigravity 的窗口状态冒充为已接入任务状态。
