# FeatherDock 审查与验证结果

本文件只保留**最新有效结论**和必要的历史摘要。下一次审查直接更新对应小节，不再新建日期版审查文档。待办统一进入 `PLAN.md`。

## 当前结论 · 2026-11-14

- 原计划五项 P0 已全部闭环，并补上「旧 Closed 不得删除新 file、新 Closed 同时清理双通道」的显式回归。
- `build.cmd -Verify -Stage` 最终通过：格式、严格 Clippy、**230/230 项测试**和 release 构建。
- 最终 EXE 已发布到仓库根目录并运行；大小 **1272 KiB**，SHA-256 为 `C761E87E02C8D169D388A06DB168E7239F954D59655C2E964D960631B11D9061`，完整性为 **Medium**。
- 最终版本的真实文件/IPC 交错 smoke 全部返回 `0`，测试文件已清理；主条、Agent 角标、8 个会话卡、滚动和离开关闭已做现场验收。
- 独立复核额外确认一项 **PID 复用/退出消息重排竞态**，已列为下一项 P1；真实适配器、剩余桌面路径和兼容/性能验收仍在 `PLAN.md`，当前不宣称 Phase 1 全部收尾。

## 五项 P0 修复与回归

| 项目 | 最终实现 | 回归证据 |
| --- | --- | --- |
| 文件快照与 Tombstone | 空快照只移除 live row，不再清除 `DeadLifecycle`；目录枚举失败保留缓存和已发布快照，只有成功空枚举才确认删除 | 真实临时目录覆盖缺失文件、同版本 JSON 重现、读取失败与确认空目录 |
| Closed 版本门 | Tombstone 与最新版本检查统一前移到所有状态写入和副作用之前；Tombstone 插入拒绝降级 | `Closed@300 → Closed@100 → Running@200` 始终关闭；旧 Closed 不删新 file，新 Closed 清理双通道 |
| file/IPC 版本仲裁 | `file_versions`、`event_versions` 独立保存；合流、prune、proc_start 按通道取戳；file 只有严格新于 event 才可产生状态和提醒 | 覆盖同/不同 instance、seq、updated_at 交错，第三个迟到 event、旧 file Waiting 不 bounce、双通道关闭 |
| HWND 的 PID 身份 | `expected_pid` 独立比对实际 PID；有 `proc_start` 时再校验进程实例；弹层激活动作传递 PID | 使用真实桌面窗口验证无 proc_start 时正确 PID 通过、错误 PID 拒绝 |
| deep_link 契约 | 白名单与文档统一为 `antigravity`、`codex`、`dsh`、`vscode`、`cursor` | 五种合法 scheme 及 cmd/http/file、非法字符、空路径测试通过 |

## 最终实测记录

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

## 独立复核结论与新增残余

独立只读复核确认五项 P0 的最新实现未见其他可复现 P0。审查过程中曾发现 `newest_channel_stamp` 反向选择较旧通道；该问题已在发布前修正，并用「file@100 → event Waiting@200 → 迟到 event Running@150」回归证明最终仍保持 Waiting@200。对称的文件门控也已验证旧 file Waiting 不产生隐藏提醒。

额外确认的 P1 竞态：

1. 旧进程退出回调只携带 PID，退出消息可能晚于 PID 复用。
2. 新 instance 可能继承同数值 PID 的旧 `proc_start`，而监视表已切换到新进程。
3. 旧退出消息可能移除新进程监视项，随后又因启动时间不匹配跳过状态清理，使新进程之后退出时缺少监视。

修复方案、退出代际回归，以及 watcher/prune/proc_start/read ack 故障注入矩阵已写入 `PLAN.md` P1。

## 旧审查闭环摘要

| 原审查项 | 当前状态 | 最新结论 |
| --- | --- | --- |
| 1、2、3、6、7、8、9、10 | **保持闭环** | 构建门槛、JSON 深度、精确焦点、会话可达性、离开关闭、固定项聚合、溢出优先级、标题刷新均保留原修复与回归 |
| 4. 文件会话防僵尸清理 | **已修复** | 空/失败快照不再清除 DeadLifecycle，缺项与同版本文件重现有回归 |
| 5. 旧事件覆盖新状态 | **已修复** | Closed 前置门、独立双通道版本戳、单调 Tombstone 和跨通道关闭均已覆盖 |
| 11. 外部 HWND 身份 | **已修复** | PID 不再依赖 proc_start 同时存在，弹层激活也传递并核验 PID |

2026-10-26 的四项历史结论保持闭环：GPU 重建前释放旧合成目标、抽屉弱生命周期身份、菜单稳定 entry key、命中区域包含 bottom_gap 与动画包络。

## 验证边界与证据规则

- 目录读取失败目前覆盖 loader 与发布逻辑，watcher 层故障注入列入后续矩阵；PopupActivation 的 PID 静态路径和独立窗口校验已测，完整弹层到激活端到端仍待验收。
- Track A 文案对比、唯一 Waiting 直达、精确 HWND 激活/错误回退、窄屏溢出和全屏恢复尚未全部实测，保留在 `PLAN.md`。
- 真实 Antigravity/Codex/dsh 生命周期适配器、IME、拖放、混合 DPI、多屏、GPU 故障、节能和性能结论仍需各自实测证据。
- 本轮桌面验收时前台为普通 QQ 窗口，Dock 可见；没有通过修改隐藏设置或强制切换全屏应用制造结果。
- 旧 `HANDOFF`、设计、调研、实施和日期版审查 Markdown 已合并到 `AGENTS.md`、`PLAN.md` 与本文件，后续只更新这三个入口。
