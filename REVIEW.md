# FeatherDock 审查与验证结果

本文件只保留**最新有效结论**和必要的历史摘要。下一次审查直接更新对应小节，不再新建日期版审查文档。待办统一进入 `PLAN.md`。

## 当前结论 · 2026-11-18

- 修复主条应用窗口数与 Agent 状态角标（[render.rs](src/render.rs) `badge_geometry` / `icon_motion`）在悬浮放大时停留在静态坐标的问题：抽取共享 `icon_motion` 与 `badge_geometry`，使角标右上角随图标 `ic.scale`、`ic.bounce_y`、`ic.presence`（`ENTER_RISE`）及非 `Pill` 样式的按压反馈（`ic.press`）同步移动并按 `sqrt(scale)` 温和缩放，同时增加微弱投影以提升浅色图标上的对比度，并与图标/背板/键帽共享变换计算。
- 延续同日早先完成的 Dock **形态样式与配色解耦**（`DockStyle` × `ThemePreset`），在完整保留原五种主题（`glass`、`compact`、`solid`、`macos`、`contrast`）与默认 `pill` 形态不变的前提下，新增五种非换颜色的 Dock 形态样式：`hairline`（极简顶线）、`islands`（分段浮岛）、`shelf`（底部托台）、`tiles`（独立键帽）与 `fluent`（Win11 流体底座），并在设置窗口「外观」分组同时提供「样式」与「配色」下拉自由组合；同步补齐新样式下的悬停背板、点击按下微缩与分组分隔连续过渡。
- 校验门通过：`cargo fmt --all -- --check`、严格 `cargo clippy --all-targets --locked -- -D warnings`、**259/259 项单元测试**（含 `theme` / `settings` / `dock` / `render` 新增与角标动画 3 项回归）和 `cargo build --release --locked` 均通过。
- 暂存发布与构建修复说明：已修复 `rustup 1.29` 下 `rust-toolchain.toml` 报 `target tuple in channel name` 的问题（拆分 `channel = "stable"` 与 `targets = ["x86_64-pc-windows-gnu"]`，并在 `scripts/dev-env.ps1` 自动探测已安装的 `stable-x86_64-pc-windows-gnu\bin`、`build.ps1` 在无 `WRITE_OWNER` 时校验进程 `>= Medium` 且文件无 `Low/Untrusted` 标签，`.cargo/gnu-linker.cmd` 增加跨工具链自包含链接器回退）。直接执行 `.\build.cmd -Verify -Stage` 已完整通过并退出码为 `0`，最新产物位于 `outputs\pending\FeatherDock.exe`（**1,344,000 bytes**，SHA-256 `83244EA4C3627E0CE75B3524661D8CEA1D1166A19E0CA77476B571C73DFBD3B6`）；因本会话仍处于非交互桌面（`EnumWindows=0`），未替换根目录运行中的 `FeatherDock.exe`，真机交互发布、五种新样式与角标动画的桌面视觉验收保留在 `PLAN.md`。

## 历史结论 · 2026-11-17

- 2026-11-16 独立复审的 5 项问题已全部修复；后续复核发现的稳态 rearm 丢更新窗口及 `Retired None` 跨通道复活路径也已闭合。原问题、修复和证据保留在下节。
- 最终 `build.cmd -Verify` 通过：格式、严格 Clippy、**250/250 项测试**和 release 构建。包版本已提升为 **v0.1.1**，根目录 EXE 已发布并启动，主程序与 watchdog 均从该文件运行。
- 最终发布 EXE 为 **1274 KiB / 1,305,088 bytes**，SHA-256 `6DC97F4F4CA491C5A21852C59134DCD3E7CC33A2A3168C1D1B1E7654906C1790`，完整性为 **Medium**。本发布资产以根目录文件为准，早期暂存哈希不是最终发布哈希。
- 最终根目录程序的真实 file/IPC smoke 全部返回 `0`，`cleanup_file_remaining=false`。发布时前台为普通 DeepSeek Harness 窗口，未进行全屏干预。
- 独立最终复核确认同秒新实例、Waiting 重入、Matched/Retired None 代际清理、启动 arm 后合流、立即 rearm 和取消 drain 路径闭合；稳态 rearm 与 stop-drain 为静态路径确认，未冒充专门动态时序探针。
- 真实 OS 数值 PID 复用、完整桌面视觉项、真实生命周期适配器及兼容/性能验收仍按 `PLAN.md` 保留，不宣称 Phase 1 全部验收完成。

## 2026-11-16 独立复审：问题与修复结果

### 1. 已修复｜同秒重启并复用 PID 时，旧墓碑吞掉新实例首事件

位置：`src/agent_state.rs:354-375`、IPC 门 `:577-585`、file 门 `:767-770`。

```text
旧实例：instance=old, seq=9, ts=100, pid=P, proc_start=A
旧实例退出并留下 DeadLifecycle
新实例：instance=new, seq=1, ts=100, pid=P, proc_start=None
```

`proc_start=None` 在 `same_pid` 判断中落入 `_ => true`；随后时间戳相等、seq 重置为较小值，新事件被墓碑拦截，`watch=None`。IPC 和 file 共用这一入口。独立探针已确认该结果。

修复结果：`DeadLifecycle` 在 PID/start 判断前识别双显式且不同的 `instance`；IPC/file 同秒、同 PID、seq 9→1 均接受并产生监视请求，legacy None 和混合 instance 仍走原规则。

### 2. 已修复｜delayed file 保留路径吞掉重新进入 Waiting 的提醒

位置：`src/agent_state.rs:634-635`、隐藏 file 保留 `:784-792`、bounce 判断 `:856-862`。

```text
file Waiting(seq=100)
→ IPC Running(seq=200)，清除共享 ack
→ delayed file Waiting(seq=100)，保留旧 file 行
→ file Waiting(seq=300)，重新成为可见状态
```

最后一步只检查旧 file 行的 `prev=Waiting`，未考虑中间已经发生的可见 IPC Running，实际得到 `bounces=[]`，后续重复快照也保持静默。本批新增的旧 file 保留分支使该完整序列可触发漏提醒；独立探针已复现。

修复结果：按 event 与旧 file 的版本戳计算应用前的合流可见状态，再判断 Waiting 重入。完整四步中 seq=300 恰好 bounce 一次，重复 300 静默；前台首次 Waiting 后同版本失焦仍不额外 bounce。

### 3. 已修复｜`proc_start=None` 的当前/退休代际退出残留与跨通道复活

位置：`src/agent_state.rs:2401-2408`、`:2562-2571`、`:2588-2597`；原测试 `:3873-3877` 还将残留写成预期。

当前流程先删除匹配的监视项，随后对 `Matched { proc_start: None }` 直接返回 `false`。进程已经退出，该代际不会再产生回调，但 Running/Waiting 会话仍保留；没有后续 file、IPC 或窗口事件时会持续残留。独立探针已确认当前代际完成后会话仍在。

修复结果：`VersionStamp.watch_generation` 与真实 `Watching { generation }` 同步绑定；Matched/Retired None 均按精确 generation 清理，Ignored 和未绑定代际的记录不受影响。新增旧 file/gen1 + 新 IPC/gen2 同秒回归：gen1 只清旧 file，gen2 后双通道归零且不复活。

### 4. 已修复｜watcher 消费通知后读取失败及稳态 rearm 窗口

位置：`src/agent_state.rs:2292-2294`、`:2333-2354`、`:2373-2381`。

目录通知已经消费，而 `metadata/read` 遇到瞬时 `NotFound`、共享锁等错误时，本次仅保留旧快照。线程随后重新进入无限等待；若故障解除后没有第二个目录事件，`latest` 和 Registry 会一直保留旧状态。现有 `:4317-4370` 手工再次调用 loader/publish，绕过了“没有第二通知”的关键条件。

修复结果：每次初始/目录激活在失败后等待可中断的 100ms，再重试恰好一次；持续失败回到无限目录等待，不常驻轮询。通知完成后立即重新 arm，再做 60ms 合流、load/retry；取消路径执行 `CancelIoEx → GetOverlappedResult(true)` drain。生产通知消费 seam 的 fail-once 回归证明无第二事件也能 READY，立即 rearm/drain 由独立静态复核确认。

### 5. 已修复｜启动期间独立 load 与 watcher arm 之间的丢更新窗口

位置：`src/main.rs:1330-1344`、`:1429`；`src/agent_state.rs:2208-2234`、`:2313-2349`。

主流程先加载并应用一次快照，watcher 随后独立加载到 `latest`，但不发送初始 READY，真正目录监视建立得更晚。首次 apply 到首次 arm 之间发生更新时，更新可能只进入 watcher 的 `latest`，也可能被两次读取和 arm 全部错过；没有后续 READY 时，Registry 一直保持首次旧值。

修复结果：main 首载的同一 `SessionSnapshot` 同时初始化 Registry 与 watcher，删除 watcher 第二次预 load 和 `refresh_now` 旁路 writer。首次 `ReadDirectoryChangesW` arm 成功后强制 load+READY；真实临时目录/消息窗口测试在无后续目录事件时收到初始 READY 并合流 post-arm 快照。

## P1 代际与故障矩阵证据

| 项目 | 最终实现 | 回归证据 |
| --- | --- | --- |
| PID 监视代际 | 每次注册分配单调代际；当前、退休、忽略结果分离；退休记录最多 128 项 | 旧 gen1 只清 file/start=A，新 gen2/start=B 的监视和 Running 保留，gen2 自身退出才清理；生产 `ensure_watching` 替换路径另测 |
| 精确进程绑定 | `PidWatchRequest` 包含 PID、key、`SessionSource`、`instance`、seq 和时间；binder 校验后只更新目标通道，且仅新版本允许重绑 | 同 key file=A/None 与 event=B 时仅 event 绑定 B；拒绝跨实例/跨通道回填；legacy 较新版本重绑、同版本 IPC/file 保持 A，旧退出随后清空双通道 |
| 退出清理边界 | 已知 start 必须精确相等；未知 start 不作为通配符；另一通道存活时保留其 ack/read/dead HWND | Matched/Retired/Ignored、None、重复消息、同 start 双通道、单侧 prune 及最终清理均有直接断言 |
| watcher 原子发布 | 任一目录/元数据/文件读取错误让整次加载失败；仅成功快照替换 cache/latest 并 READY | 真实目录中注入单文件读取错误，READY=0、cache/latest/Registry 不变；成功空枚举才清空 |
| delayed file 与 ack/read | 较新 IPC 下保留旧文件通道但不产生监视/提醒副作用；read marker 使用 instance + seq 排序 | 旧 Waiting 不重复 bounce、ack 保留；高 seq 低时间戳及新 instance 的 Done 不错误消未读 |
| 真实进程与消息边界 | owned `ping.exe` 子进程 + 不可见消息窗口，测试结束由 guard 回收 | 实际 `RegisterWaitForSingleObject` → `PostMessageW` → `PeekMessageW` → `complete_and_prune` 全链通过 |
| 动画测试稳定性 | 扫动几何测试直接构造完全缓动姿态，首帧悬停放大不计入扫动位移 | 消除并行测试墙钟负载造成的偶发失败，不改变产品动画代码 |

上述矩阵与 2026-11-16 复审问题的修复结果共同构成当前证据。2026-11-15/16 的非闭环结论已由 2026-11-17 最终修复与复核取代；证据范围仍以各测试和明确标注的静态路径为限。

## 五项 P0 修复与回归

| 项目 | 最终实现 | 回归证据 |
| --- | --- | --- |
| 文件快照与 Tombstone | 空快照只移除 live row，不再清除 `DeadLifecycle`；目录枚举失败保留缓存和已发布快照，只有成功空枚举才确认删除 | 真实临时目录覆盖缺失文件、同版本 JSON 重现、读取失败与确认空目录 |
| Closed 版本门 | Tombstone 与最新版本检查统一前移到所有状态写入和副作用之前；Tombstone 插入拒绝降级 | `Closed@300 → Closed@100 → Running@200` 始终关闭；旧 Closed 不删新 file，新 Closed 清理双通道 |
| file/IPC 版本仲裁 | `file_versions`、`event_versions` 独立保存；合流、prune、proc_start 按通道取戳；file 只有严格新于 event 才可产生状态和提醒 | 覆盖同/不同 instance、seq、updated_at 交错，第三个迟到 event、旧 file Waiting 不 bounce、双通道关闭 |
| HWND 的 PID 身份 | `expected_pid` 独立比对实际 PID；有 `proc_start` 时再校验进程实例；弹层激活动作传递 PID | 使用真实桌面窗口验证无 proc_start 时正确 PID 通过、错误 PID 拒绝 |
| deep_link 契约 | 白名单与文档统一为 `antigravity`、`codex`、`dsh`、`vscode`、`cursor` | 五种合法 scheme 及 cmd/http/file、非法字符、空路径测试通过 |

## 2026-11-14 P0 实测记录（历史有效基线）

| 检查 | 结果 |
| --- | --- |
| `build.cmd -Verify -Stage` | 通过；`cargo fmt --check`、严格 Clippy、230/230 测试、release 构建均成功 |
| 暂存与根目录一致性 | 最终暂存 SHA-256 与随后发布的根目录 EXE 同为 `C761E87E02C8D169D388A06DB168E7239F954D59655C2E964D960631B11D9061` |
| 发布与进程 | 旧版通过 `--quit` 正常退出；最终主程序与 watchdog 均从根目录 EXE 运行 |
| 文件完整性 | `icacls` 实测包含 `Mandatory Label\Medium Mandatory Level` |
| 最终 IPC/文件 smoke | file `Running@100` → IPC `Waiting@200` → `Closed@300` → 旧 `Closed@100` → 旧 `Running@200` → 同版本 file 重现；传输均返回 `0` |
| smoke 清理 | 最后发送新 Closed 并删除测试 JSON；`cleanup_file_remaining=false`，会话目录无 smoke 文件残留 |
| 静止主条 | 已确认 Antigravity、Codex、显式 dsh 三个 Agent 固定项及普通应用分区 |
| Agent 角标 | 两个独立 Waiting 会话显示 `2!` 和琥珀状态点 |
| 会话卡 | 8 个会话按 Waiting、Error、Running、Done/Idle 排序；150% DPI 下首屏显示六行及「查看全部」 |
| 滚动 | 实测从「1-6/8」滚动到「3-8/8」，末两项可达 |
| 离开关闭 | 指针移出 Dock 和卡片后，`popup_closed_on_leave=true`；测试结束后恢复鼠标位置和前台窗口 |

原始截图和 smoke JSON 位于 `.dsh/tmp/p0-state-20261114/`，仅作短期证据，不是新的 Agent 文档入口。

## P0 独立复核历史

独立只读复核确认五项 P0 的实现未见其他可复现 P0。审查过程中曾发现 `newest_channel_stamp` 反向选择较旧通道；该问题已在发布前修正，并用「file@100 → event Waiting@200 → 迟到 event Running@150」回归证明最终仍保持 Waiting@200。对称的文件门控也已验证旧 file Waiting 不产生隐藏提醒。

2026-11-15 曾提前记为闭环；2026-11-16 独立复审发现 5 项问题。2026-11-17 已修复这些问题及后续复核发现的稳态 rearm、Retired None 复活路径，当前以上方最新结论和证据为准。真实 OS 数值 PID 复用驱动与普通桌面视觉验收仍按 `PLAN.md` 保留。

## 旧审查闭环摘要

| 原审查项 | 当前状态 | 最新结论 |
| --- | --- | --- |
| 1、2、3、6、7、8、9、10 | **保持闭环** | 构建门槛、JSON 深度、精确焦点、会话可达性、离开关闭、固定项聚合、溢出优先级、标题刷新均保留原修复与回归 |
| 4. 文件会话防僵尸清理 | **已修复** | 空/失败快照不再清除 DeadLifecycle，缺项与同版本文件重现有回归 |
| 5. 旧事件覆盖新状态 | **已修复** | Closed 前置门、独立双通道版本戳、单调 Tombstone 和跨通道关闭均已覆盖 |
| 11. 外部 HWND 身份 | **已修复** | PID 不再依赖 proc_start 同时存在，弹层激活也传递并核验 PID |

2026-10-26 的四项历史结论保持闭环：GPU 重建前释放旧合成目标、抽屉弱生命周期身份、菜单稳定 entry key、命中区域包含 bottom_gap 与动画包络。

## 验证边界与证据规则

- watcher 目录/单文件读取失败、通知消费后的 fail-once 自动重试，以及真实 arm 后初始 READY 均有回归。立即 rearm 与 stop-drain 由独立静态复核确认；同步文件 load 和 OS 取消完成仍是退出时延边界，未宣称专门动态时序覆盖。PopupActivation 的完整弹层到激活端到端仍待普通桌面验收。
- 真实 OS 数值 PID 复用尚未由系统级驱动强制复现；当前证据是确定性排队/代际矩阵、真实子进程退出消息链路和精确 generation 清理。
- Track A 文案对比、唯一 Waiting 直达、精确 HWND 激活/错误回退、窄屏溢出和全屏恢复尚未全部实测，保留在 `PLAN.md`。
- 真实 Antigravity/Codex/dsh 生命周期适配器、IME、拖放、混合 DPI、多屏、GPU 故障、节能和性能结论仍需各自实测证据。
- 2026-11-15 验收时前台为 Phantom Blade Zero 全屏且 Dock 隐藏；未通过修改隐藏设置、移动鼠标或强制切换全屏应用制造结果。2026-11-14 的 P0 桌面证据则是在普通 QQ 前台下取得。
- 旧 `HANDOFF`、设计、调研、实施和日期版审查 Markdown 已合并到 `AGENTS.md`、`PLAN.md` 与本文件，后续只更新这三个入口。
