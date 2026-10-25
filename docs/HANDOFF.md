# FeatherDock 开发交接

本文件记录已确认的产品方向、实现入口和不能丢失的修复。接手时先看最新用户消息、`git status --short` 与本文末尾的验证记录。本轮代码在当前工作区，尚未提交或推送；`backdrop.rs`、`menu_style.rs` 和本文是新增文件，仅看 `git diff` 会漏掉它们。

## 已确认的方向

- 应用图标采用用户选定的「折纸收纳」：暖白圆角底，薄荷绿和淡紫卡片，杏色纸袋。正式资源在 `assets/`，不要回退为旧羽毛或重新生成近似图。
- 默认主题选择「暖瓷玻璃」。这是在暖瓷色调上增加克制透明和柔化的方向，保留原生桌面工具的轻量感。
- 配置键继续使用 `theme = "glass"`；显示名称改为「暖瓷玻璃」。`compact`、`solid`、`macos`、`contrast` 仍保留，不能为上新主题而让其它预设失效。
- 主条、抽屉、控制中心、缩略图、设置、原生右键菜单使用同一主题来源。系统 Start 等外部界面继续由 Windows 管理。

## 原型参数与实现原则

用户确认的交互原型文件名为 `featherdock-warm-glass.html`。它是设计参考，不是程序运行依赖；关键参数在下表保留，避免接手者依赖本机原型路径。

| 项目 | 暖瓷玻璃参考值 |
| --- | --- |
| 底色 | `#FFFBF6`，RGB 255 / 251 / 246 |
| 主文字 / 次文字 | `#383633` / `#6B665E` |
| 强调色 | 薄荷灰绿 `#688879` |
| 主条底色不透明度 | 0.48（2026-10-24 按用户反馈下调；回退仍见下文） |
| 抽屉底色不透明度 | 0.88 |
| 抽屉圆角 | 32 个设计单位，按实际窗口 DPI 缩放 |
| 主条圆角 | 当前原生实现为 32 个设计单位，按实际窗口 DPI 缩放 |
| 边缘 | 细白色高光；避免深色粗描边 |
| 高光 / 阴影 | 很浅的白色渐变、低浓度暖灰阴影 |
| 原型背景柔化 | CSS `blur(28px)`；这是视觉目标，不能直接当成 Windows 已实现的能力 |

UI 底色应保持暖白，壁纸影响应受到控制。浅色主题下的系统按钮和分隔线也要调整，不能把旧的白色 glyph 原样放在浅色面板上。菜单和设置优先保证对比与阅读，可使用实色暖瓷底；不要为追求透明损坏原生交互。

真实背景模糊必须区分「对自身图层模糊」与「采样窗口背后内容」。当前使用 Windows 11 的 host backdrop brush 在 DWM 内采样背后内容，经 Gaussian effect 后放在前景交换链之下。独立三色棋盘 probe 已验证实际背景柔化；本轮也已分别运行正式应用的主条、抽屉和控制中心，证据见文末。不能把 probe 或浏览器原型当成整体验收。

能力不足、系统关闭透明效果、高对比度、远程会话或更新失败时，使用更实的暖瓷底色：主条 alpha 0.92，GPU 弹层 alpha 0.98。原生菜单、设置窗口和 DWM 缩略图外框使用同源实色，不宣称这些 GDI 界面拥有背景模糊。

## 模块地图

| 文件 | 接手时需要知道的职责 |
| --- | --- |
| `src/theme.rs` | 配置键、显示名称、`VisualTheme` 主条参数、`SurfaceTheme` 弹层颜色；其它组件应从这里取色 |
| `src/render.rs` | 主条底座、边缘、高光、运行状态与内置按钮绘制 |
| `src/graphics.rs` | 主条 D3D11 / D2D / DirectComposition 设备；后台 Shell 提取队列按 source 匹配当前项目，UI 每帧最多解码 4 个结果 |
| `src/backdrop.rs` | Windows 11 host backdrop、Gaussian effect、圆角裁剪、WinRT compositor / dispatcher 生命周期、能力与策略检查、失败后降级 |
| `src/glass.rs` | 按需创建的 GPU 弹层表面；背景材质能力、透明度和动画裁剪接入点 |
| `src/main.rs` | 消息循环、主题切换、主窗口 region / DPI、弹窗协调；静止 region 向外预留描边和抗锯齿像素，不再硬切弧边 |
| `src/drawer.rs` | 抽屉绘制、分类交互和统一图标区域；渲染与命中共享 motion / transform，关闭中可连续反向展开 |
| `src/control_center.rs` | 控制中心主页面和音频 / 网络等子页面；开窗读取主题快照，渲染与输入共享内容位移 |
| `src/window_preview.rs` | DWM 缩略图及其 GDI 外框；当前材质能力应按实际实现说明 |
| `src/settings_window.rs` | Win32 原生控件设置；浅色背景、标题栏、控件主题与 brush 生命周期 |
| `src/menu_style.rs` | 原生 HMENU 的作用域 owner-draw 样式，不替换原生弹窗和命令路由 |
| `src/tray.rs` / `src/folder_stack.rs` | 托盘菜单、文件夹菜单的调用位置 |
| `src/drawer_icons.rs` / `src/icons.rs` | Shell 图标助手、缓存、透明边距归一化、WIC / HICON 处理 |
| `src/settings.rs` / `src/config.rs` | 用户行为设置和固定项；通过已有 load/save 修改，禁止提交本机用户数据 |
| `src/taskbar.rs` / `src/watchdog.rs` | 任务栏控制与意外退出恢复；生命周期修改必须同时考虑这两处 |

`SurfaceTheme` 提供底色、不透明度、文字、次文字、强调、边缘、悬停和选中颜色、圆角、背景柔化开关等参数。主线程主题变化时需同步已打开的设置；使用主题快照的弹层应关闭后按新主题重开，避免同屏混合颜色。

`backdrop.rs` 在同一 UI 线程共享一个 WinRT compositor，按 HWND 使用弱引用查找背景实例；GPU 前景设备替换时可暂时共享旧背景，最后一个持有者才关闭对应 DesktopWindowTarget。背景位于 HWND 的非 topmost composition slot，图标和文字仍走既有 topmost DirectComposition 交换链。动画 bounds / opacity 变化才请求异步 commit，不增加第二个 Present 或背景截屏计时器。

## 构建环境与产物

本工作区已有独立工具链，位于仓库同级的 `../Dock-build-tools`；包含 Windows GNU Rust stable、Cargo、rustfmt、Clippy 与当前依赖缓存。`scripts/dev-env.ps1` 自动寻找它，否则使用机器已有的 Rust。不要把这个目录、缓存、个人路径或日志提交到仓库。

```powershell
.\build.cmd -Verify     # 格式、Clippy、测试通过后构建正式产物
.\scripts\dev-shell.cmd # 打开已配置的开发终端
```

最终运行和分发入口只有仓库根目录的 `FeatherDock.exe`。`target/` 与 `outputs/` 是忽略的中间产物；直接 `cargo build` 得到的文件不等于已经完成发布步骤。构建脚本会清理旧产物；若自定义 `CARGO_TARGET_DIR`，必须先确认解析后的绝对目录确实是本任务允许清理的目标。

环境准备只修改当前进程的环境变量，不改系统 PATH、注册表或 PowerShell 配置。`.cargo/gnu-linker.cmd` 使用 Rustup 自带的 GNU linker，`windows` crate 固定 0.58；不要随意升级到需要其它本机链接工具的方案。

## 上一轮修复必须保留

1. **Low → Medium 文件完整性标记。** 沙箱中产生的 EXE 曾继承 Low 标记，即使从 Explorer 启动也无法正常写用户缓存或与托盘交互。`build.ps1` 仅对最终 EXE 设置普通桌面程序的 Medium 标记，并检查结果。不要删掉这一步，也不要要求用户用管理员运行来掩盖问题。
2. **图标缓存必须可写。** 不要只检查目录存在；当前代码写探针验证可写性，再按顺序使用备用位置，实例退出后清理。失败写诊断信息，不能无声地一直显示默认图标。
3. **危险 Shell 提取放在助手进程。** 主程序通过自身 `--drawer-icons` 模式提取；不要把第三方 Shell 扩展调用挪回 UI 线程。
4. **小图标尺寸归一化。** 抽屉布局允许低分辨率源图等比放大；提取阶段仅裁去异常大的透明留白，保留普通图标边距和宽高比。尺寸一致不能制造源文件中没有的细节。
5. **预乘透明度只做一次。** 图标助手输出预乘 BGRA；读取已预乘 BMP 时不要再次相乘，避免黑边、变暗和透明错误。
6. **托盘可恢复且设置有直接入口。** Explorer 重启后可重新注册，注册失败可重试；右键最右侧滑杆按钮和 `--settings` 都能进入设置，不能只依赖通知区域图标。
7. **应用资源不泄露构建路径。** `scripts/build-icon-resource.py` 使用相对参数，并清除 CVTRES `.debug$S` 中的本机路径与时间戳；修改 ICO 后要同步重建 `.res.o`，普通构建不额外依赖资源编译器。

## 菜单和窗口的生命周期

菜单样式 guard 存活到 `TrackPopupMenu` 返回，在 `DestroyMenu` 之前释放。guard 恢复原来的菜单类型、itemData 与背景，再释放字体、刷子及线程局部映射。保留菜单字符串和命令 ID，使禁用、勾选、子菜单、助记键及可访问性仍由原生 HMENU 承载；系统高对比模式不接管绘制。

设置窗口切换主题时应替换背景 brush 并释放旧对象，再刷新所有子控件，涵盖 STATIC、BTN、LISTBOX、EDIT。关闭窗口时释放字体和最终 brush。可选的 Windows 11 标题栏颜色失败时应回到系统支持的外观。

退出清理顺序必须是：先同步关闭所有弹层和独立设置窗口，再释放 App / GPU 状态，最后调用 backdrop 退出流程并排空自建 DispatcherQueue。设置窗口通过 `settings_window::close()` 直接销毁，不能仅投递关闭消息后就释放 App；队列排空期间仍会分发消息，残存窗口不能访问已经释放的主状态。借用的外部 DispatcherQueue 不由 FeatherDock 关闭。

不要按镜像名强杀 FeatherDock：watchdog 和主程序是同一个 EXE。正常退出用托盘菜单或 `FeatherDock.exe --quit`；仅在需要恢复任务栏 / 桌面图标时使用 `--restore-system`。运行、关闭程序会改变用户桌面状态，做验证前先确认当前用户授权与已有进程情况。

## 验证方法

- 先运行 `cargo check --all-targets --locked`，再运行 `build.cmd -Verify`；遇到其它代理的未完成代码应先协调，禁止覆盖其工作来让检查通过。
- 检查 `glass` 配置兼容、五种主题可切换；主条与新开的弹层颜色一致，设置窗口能在浅深之间即时切换。
- 浅色、中性、复杂和深色壁纸都要检查；确认文字、运行点、分隔线、菜单选中 / 禁用状态可辨认。
- 检查不同 DPI、屏幕边缘位置、自动隐藏、抽屉开合、控制中心子页、缩略图关闭按钮，不因材质改变而裁掉点击区域。
- 菜单键盘方向键、Enter、Esc、勾选、子菜单和助记键应工作；用 GetMenuItemInfo 仍能读取原字符串，guard 结束后不残留临时 itemData。
- 确认应用图标、系统入口图标、托盘和设置入口；正式 EXE 完整性标记保持 Medium。
- 不运行应用时可创建原生菜单和内存绘制对象进行测试；不要为视觉测试隐藏任务栏或修改用户配置。

## 本轮状态与接手步骤

本轮实现及验证日期：2026-10-24。

- 已接通暖瓷主题 tokens、主条、抽屉、控制中心、设置、缩略图外框和原生菜单；旧 `glass` 配置无需迁移。
- `build.cmd -Verify` 的格式检查、严格 Clippy 与全部 **121 项测试**通过。测试包含菜单字符串 / itemData 恢复、助记键、无效背景边界、小图标归一化及原有功能回归。
- 正式 release 已构建；最终入口仍为根目录 `FeatherDock.exe`，构建会设置 Medium 完整性标记。不要运行 `target/` 下的缓存产物。
- 已启动正式应用并通过 Windows 原生截图和 UIA 树检查设置窗口：主题值为「暖瓷玻璃」，暖瓷底、深色文字和浅色控件正常。没有为验收改动用户的任务栏模式、固定项、开机自启或桌面图标选项。
- 背景材质通过独立原生三色棋盘像素验证：区域内随背后颜色变化而平滑模糊，圆角外仍保持清晰。证据 `outputs/backdrop-probe-pixels.bmp` 和对应 probe 源码只在本地忽略目录；程序运行不依赖这些文件。
- 直接引用生产 `backdrop.rs` 的隐藏生命周期验证通过：自建队列约 28 ms 内退出；借用外部队列后外部仍可接受任务。异步提交最多保留一个结果，在下一次自然更新时检查，空闲不创建轮询计时器。
- 已修复全屏期间延迟的系统材质策略更新：返回桌面时重新检查 backdrop。退出时先销毁材质弹层 / 设置、清主窗口 userdata、释放 App，再排空自己创建的 DispatcherQueue，最多等待 1 秒。
- 本轮没有提交或推送。上一轮已发布的基线为 `6e5da46`；不要误认为远端已包含本轮暖瓷代码。

后续优先事项：

1. 先保留当前实现，让用户验收实际壁纸下的主条、抽屉、控制中心和菜单；需要微调时优先改 `theme.rs`，不要重新设计已选定的品牌图标。
2. 补跨机器验证：Windows 10 / 不支持 HostBackdrop 的系统、系统关闭透明、高对比、远程会话、不同 DPI 和多显示器。代码已有更实底色的回退，但本轮没有通过改动用户系统选项逐项实机覆盖这些组合。
3. 正式桌面开合动画、主条外侧阴影和屏幕边缘仍可根据用户反馈精修。主条保留原来的实际窗口 region 来保证空白区点击穿透，没有把浏览器原型的大面积外阴影完整搬进拦截输入的区域；菜单、设置和 GDI 缩略图外框有意使用实色。
4. 异步合成失败会在下一次自然更新时被发现，不用持续唤醒来检测；若以后实现主动设备失效通知，仍需保持空闲不重绘。
5. 提交前检查 `git status --short` 中的新增文件，再按用户当前要求决定提交 / 推送。构建、日志、棋盘证据和本机工具链继续留在忽略目录。

## 边缘与交互复审（2026-10-24）

本轮针对用户提出的静止弧边黑点、背景过实和交互动效做了第二轮修复，保留上面暖瓷玻璃的品牌、材质与生命周期约束。

- 暖瓷主条 alpha 从 0.65 降为 **0.48**；抽屉 / 控制中心仍为 0.88，主条无背景材质时仍为 0.92，其它预设不变。`ThemePreset::dock_alpha()` 集中处理主条回退。
- 静止 `CreateRoundRectRgn` 不再是 Direct2D 弧边的硬裁剪：region 按 DPI 向外留出描边与抗锯齿空间，仍保留空白点击穿透。3840×2160、150% DPI 正式桌面截图已检查左右端部，未见黑点或断裂，证据为 `outputs/dock-after-rest.png`。默认捕获工具为 `outputs/capture-dock.ps1`（DPI 感知 + CAPTUREBLT）；不要用 `PrintWindow` 的旧帧或逻辑坐标截图代替实际 DWM 画面。
- 抽屉渲染 / 点击 / 右键 / 拖放共用 panel 逆变换和逐行 rise；关闭可从当前 alpha / scale 连续反向展开。click-away 导致的同一次 Dock 点击只消费一次，即使关闭动画尚未结束也不会错误重开；之后独立点击才反向展开。控制中心的打开 / 子页位移同样进入命中计算。
- 命令面板的 EDIT / LISTBOX 通过局部子类转发 Enter / Esc，保留其它原生按键。测试使用真实子控件，不启动外部程序。
- Dock 新图标改为后台助手提取、`WM_DOCK_ICONS_READY` 完成通知、按 source 匹配当前项目和每帧最多 4 个解码。队列独立于单个 GPU 设备，不占用空闲轮询，不把旧索引用于异步结果。已有 BMP 直接复用；助手超时后的终止确认也有界，不再无界 `Child::wait()`。
- 过期桌面缓存先被丢弃，再做逐项路径验证。设置保存失败会提示并恢复控件、内存设置和已应用的任务栏 / 桌面图标状态，成功后才通知主窗口。已有空固定项配置不再重新生成默认项；找不到已有主窗口时也不再按镜像名杀进程或隐式恢复系统。

验证结果：

- `cargo check --all-targets --locked` 通过；`build.cmd -Verify` 的格式、严格 Clippy、全部 **140 项测试**和 release 构建通过。最终根目录 `FeatherDock.exe` 已确认是 Medium 完整性。
- 正式 EXE 实际走过主条静止 / 悬停、抽屉打开 / 正常关闭 / 快速重开、原生右键菜单、取消拖拽、控制中心音频子页与返回、命令面板热键 / Esc、选择已有运行窗口后 Enter，以及设置入口。未为验收修改主题、固定项、任务栏模式、分类、音量或开机自启。
- `scripts/perf-smoke.ps1` 的 5 秒静止和连续悬停扫动采样中，主进程与 watchdog 的 CPU 均显示为 0%（脚本四舍五入）；没有持续空闲重绘或异常 CPU 增长。该采样不是逐帧延迟测量，不能据此宣称所有交互在所有机器上都流畅。
- `outputs/dock-input.ps1`、`window-exists.ps1` 只用于本地原生交互验收；它们和截图均在忽略目录，程序运行不依赖这些文件。本轮没有提交或推送。

后续需要保留的风险：

1. `drawer::warm_cache` 的冷启动预热、过期 `desktop_scan` Shell 扫描以及命令面板的冷缓存收集仍可能同步等待；本轮解决的是 Dock 新图标运行中加载，不应宣称所有启动 / 扫描路径已异步化。后续应复用完成消息模式，先展示缓存或空态。
2. `window_preview` 绘制回退图标和 `graphics::decode_icon` 的运行窗口 `WM_GETICON` 回退仍可能等待无响应外部窗口，需要异步缓存或免消息图标来源。原生菜单 / 分类写入失败的用户提示也尚未统一。
3. 本轮实际视觉覆盖当前 Windows 11、150% DPI 和动态壁纸；Windows 10、关闭透明、高对比、远程会话、多显示器及其它 DPI 仍需按上面矩阵验证，不能从单元测试推断这些组合已验收。

微软实现依据：[HostBackdrop 的 Win32 开关](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute)、[CreateHostBackdropBrush](https://learn.microsoft.com/en-us/uwp/api/windows.ui.composition.compositor.createhostbackdropbrush)、[同一 HWND 的两个合成槽](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-idcompositiondevice-createtargetforhwnd)、[Backdrop 作为 EffectBrush 输入](https://learn.microsoft.com/en-us/windows/apps/develop/composition/composition-brushes#paint-using-background-pixels)、[DispatcherQueue 的退出要求](https://learn.microsoft.com/en-us/windows/win32/api/dispatcherqueue/nf-dispatcherqueue-createdispatcherqueuecontroller)。
