# FeatherDock Agent Adapters (Track B Examples)

FeatherDock separates Agent recognition into two explicit tiers:

- **Track A（仅识别窗口 / Window Only）**: Automatically detects open GUI windows of supported Agents (`Antigravity`, `Codex`, `dsh`) without any adapter setup. Because no task lifecycle events have arrived, these entries show **"仅识别窗口"** ("Window Only") in the hover card instead of guessing task progress.
- **Track B（任务状态已接入 / Task Events Active）**: Receives real task states (`Running`, `Waiting`, `Done`, `Error`, `Idle`, `Closed`) over either:
  1. `FeatherDock.exe --agent-event --json <json>`（或 `--agent ... --session ... --state ...`，`WM_COPYDATA` fast path）, or
  2. Atomic JSON session files under `%LOCALAPPDATA%\FeatherDock\agent-sessions\*.json` (`ReadDirectoryChangesW` watcher).

## Real DSH lifecycle plugin

[`dsh-featherdock/`](dsh-featherdock/) is a native DSH Host plugin, not a window-title scraper. It subscribes to `session/event` and publishes `turn/start`, approvals, questions and `turn/end` through the atomic JSON channel. It is event-driven, has no polling or child processes, and uses stable session IDs, monotonic sequences and the real Host PID.

Install `dsh-featherdock/` through DSH Plugins → Add plugin → Install third-party plugin, or with the Desktop CLI documented in its README. After installation, a real DSH conversation should show Running automatically; approvals/questions show Waiting and completion shows Done. Internal subagents roll up into their top-level conversation instead of creating duplicate rows; user-created forks remain independent. Codex and Antigravity still use their separate adapter paths below until their real hooks are installed.

## Quick Usage

### 1. Push a live event via CLI (`WM_COPYDATA`)

```powershell
powershell -ExecutionPolicy Bypass -File .\send-agent-event.ps1 `
  -Agent codex `
  -Session "codex-main" `
  -Title "Refactor Dock" `
  -Detail "Waiting for shell approval" `
  -State waiting `
  -Seq 42
```

#### Structured CLI Exit Codes

| Exit Code | Meaning |
| --- | --- |
| `0` | Delivered and accepted by running `FeatherDock.exe` (`LRESULT(1)`) |
| `2` | Invalid or missing CLI arguments / JSON payload |
| `3` | No running `FeatherDockWindow` instance found (`FindWindowW`) |
| `4` | `SendMessageTimeoutW` failed/timed out or Dock rejected payload (`LRESULT(0)`) |

### 2. Write a persistent session file (atomic rename)

```powershell
powershell -ExecutionPolicy Bypass -File .\write-session-file.ps1 `
  -Agent antigravity `
  -Session "ag-task-1" `
  -Title "PR Review" `
  -Detail "Running cargo check" `
  -State running `
  -Seq 10
```

> Always write to a `.tmp` file in the same directory and atomically rename/replace `<session>.json` so `SessionDirWatcher` never reads half-written JSON.

### 3. Codex hook / notify wrapper example

See `codex-notify-adapter.ps1` for an example translating hook JSON (`agent-turn-complete`, approval prompt, running task, or exit) into monotonically sequenced events (`instance` + `seq`), falling back cleanly to atomic JSON files if Dock isn't open yet.

## Event Fields & Ordering Guarantees

Every event accepts optional ordering & lifecycle fields:

- `agent`: `"antigravity" | "codex" | "dsh"` (normalized)
- `session_id`: Unique session key within the agent
- `state`: `"running" | "waiting" | "done" | "error" | "idle" | "closed"`
- `instance`: Optional writer/process instance id; prevents stale events from an older instance or restarted worker from overwriting newer state
- `seq`: Strictly increasing per-instance sequence number (`u64`); breaks ties within the same Unix second (`updated_at`)
- `pid` / `hwnd`: Process/window hints validated by FeatherDock (`IsWindow`, `GetWindowThreadProcessId`, process creation time, and executable/AUMID identity); exited PIDs trigger immediate cleanup and tombstone protection against stale JSON resurrection
- `deep_link`: Optional allowlisted URI scheme (`antigravity://`, `codex://`, `dsh://`, `vscode://`, `cursor://`)
