#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// FeatherDock — an ultra-light, GPU-composited macOS-style dock for Windows.
// NOTE: console subsystem kept ON during early dev so panics are visible.
// Flip to `#![windows_subsystem = "windows"]` once stable.

use std::time::{Duration, Instant};

mod app_icon;
mod apps;
mod autostart;
mod config;
mod content;
mod dock;
mod error_log;
mod graphics;
mod icons;
mod render;
mod settings;
mod settings_window;
mod single_instance;
mod taskbar;
mod tray;
mod windows_list;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, FileOpenDialog, IFileOpenDialog, ShellExecuteW,
    FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, HDROP,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use dock::Dock;
use graphics::Gpu;
use tray::{Tray, WM_TRAY};

// Not exported by windows 0.58 under our features; define the Win32 value.
const WM_MOUSELEAVE: u32 = 0x02A3;
const HRESULT_CANCELLED: HRESULT = HRESULT(0x800704C7u32 as i32);

// Coalescing timer: bursts of window create/destroy/show/hide events (apps churn
// hidden helper windows) collapse into one EnumWindows rescan instead of one each.
const TIMER_WINDOWS: usize = 1;
const TIMER_WINDOWS_MS: u32 = 180;
const TIMER_TASKBAR_REHIDE: usize = 2;
const TIMER_TASKBAR_REHIDE_MS: u32 = 400;
const TASKBAR_INVOCATION_GRACE: Duration = Duration::from_secs(8);

// Right-click context-menu command ids (kept clear of the tray ids in tray.rs).
const ID_WIN_CLOSE: usize = 2001;
const ID_WIN_MINIMIZE: usize = 2002;
const ID_WIN_MAXIMIZE: usize = 2003;
const ID_WIN_PIN: usize = 2004;
const ID_PIN_OPEN: usize = 2101;
const ID_PIN_LOCATION: usize = 2102;
const ID_PIN_REMOVE: usize = 2103;

struct App {
    _instance: single_instance::SingleInstance,
    gpu: Gpu,
    dock: Dock,
    tray: Tray,
    hooks: Vec<HWINEVENTHOOK>,    // WinEvent hooks tracking open windows
    running_sig: Vec<isize>,      // last seen open-window handles (change detection)
    settings: settings::Settings, // persisted dock mode + fullscreen behavior
    fullscreen_active: bool,      // a fullscreen app is in front on our monitor
    animating: bool,
    tracking: bool,
    full: (i32, i32, i32, i32), // full window rect (x, y, w, h)
    strip_h: i32,               // window height when collapsed to the reveal strip
    expanded: bool,             // true = full window, false = thin bottom strip
    taskbar_invoked_at: Option<Instant>,
}

/// Where `reveal` should ease to when the cursor is NOT over the dock: hidden in
/// auto-hide mode or while a fullscreen app is in front, otherwise resident.
fn resting_target(app: &App) -> f32 {
    let hide = app.settings.dock_mode == settings::DockMode::AutoHide
        || (app.settings.hide_on_fullscreen && app.fullscreen_active);
    if hide {
        0.0
    } else {
        1.0
    }
}

fn io_error(context: &str, error: std::io::Error) -> Error {
    Error::new(E_FAIL, format!("{context}: {error}"))
}

/// The pinned (left-of-divider) items, from the user's config or built-in defaults.
fn pinned_items() -> Result<Vec<dock::DockItem>> {
    match config::load().map_err(|error| io_error("读取配置失败", error))? {
        Some(cfg) => {
            let items = apps::from_config(&cfg.items);
            if items.is_empty() {
                Ok(apps::default_items())
            } else {
                Ok(items)
            }
        }
        None => {
            let items = apps::default_items();
            config::write_default(&items).map_err(|error| io_error("创建默认配置失败", error))?;
            Ok(items)
        }
    }
}

/// Assemble the full dock row: Start, pinned apps, then (if any open windows) a
/// divider followed by one slot per running window.
fn compose_items(running: &[windows_list::RunningWindow]) -> Result<Vec<dock::DockItem>> {
    let mut items = Vec::with_capacity(running.len() + 6);
    items.push(apps::start_item());
    items.extend(pinned_items()?);
    if !running.is_empty() {
        items.push(apps::divider_item());
        for window in running {
            items.push(apps::running_item(&window.title, window.hwnd));
        }
    }
    Ok(items)
}

/// Signature of the open-window set (handles only — we don't display titles, so a
/// title change must NOT trigger a reload), used to skip needless rebuilds when a
/// WinEvent fires but the set we show is unchanged (focus switches, renames).
fn running_sig(running: &[windows_list::RunningWindow]) -> Vec<isize> {
    running.iter().map(|window| window.hwnd).collect()
}

unsafe fn monitor_layout(
    hwnd: HWND,
    items: &[dock::DockItem],
) -> Result<(f32, i32, i32, u32, u32)> {
    let dpi = (GetDpiForWindow(hwnd).max(96) as f32) / 96.0;
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return Err(Error::from_win32());
    }
    let (width, height) = dock::window_size(items, dpi);
    let x = info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - width as i32) / 2;
    let y = info.rcMonitor.bottom - height as i32;
    Ok((dpi, x, y, width, height))
}

unsafe fn show_error(owner: HWND, context: &str, error: impl std::fmt::Display) {
    error_log::write(context, &error);
    let text: Vec<u16> = format!("{context}\n\n{error}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = MessageBoxW(
        owner,
        PCWSTR(text.as_ptr()),
        w!("FeatherDock"),
        MB_OK | MB_ICONERROR,
    );
}

/// Grow the window to full size (to show the dock) or shrink it to a thin bottom
/// strip (so the area it used to cover becomes genuinely click-through).
unsafe fn set_expanded(hwnd: HWND, app: &mut App, expand: bool) -> Result<()> {
    if app.expanded == expand {
        return Ok(());
    }
    let (x, y, w, h) = app.full;
    if expand {
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE)
    } else {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y + h - app.strip_h,
            w,
            app.strip_h,
            SWP_NOACTIVATE,
        )
    }?;
    app.expanded = expand;
    Ok(())
}

/// Rebuild the dock from a fresh scan of open windows (e.g. after a config edit).
unsafe fn reload_app(hwnd: HWND, app: &mut App) -> Result<()> {
    let running = windows_list::enumerate_sorted();
    app.running_sig = running_sig(&running);
    reload_with(hwnd, app, &running)
}

/// Rebuild the dock from an already-enumerated window set: recompose items, resize
/// the swapchain + window, re-extract icons, and kick a frame.
unsafe fn reload_with(
    hwnd: HWND,
    app: &mut App,
    running: &[windows_list::RunningWindow],
) -> Result<()> {
    let prev_reveal = app.dock.reveal;
    let items = compose_items(running)?;
    let (dpi, x, y, width, height) = monitor_layout(hwnd, &items)?;
    app.gpu.resize(width, height)?;
    app.dock = Dock::new(items, dpi, width as f32, height as f32);
    // Keep the current slide position so a list change doesn't flash the dock in
    // or out; ease toward whatever the mode/fullscreen state currently wants.
    app.dock.reveal = prev_reveal;
    app.dock.reveal_target = resting_target(app);
    app.gpu.load_icons(&app.dock.items, dpi);
    app.full = (x, y, width as i32, height as i32);
    app.strip_h = ((6.0 * dpi).round() as i32).max(4);
    app.expanded = true;
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        x,
        y,
        width as i32,
        height as i32,
        SWP_NOACTIVATE,
    )?;
    app.gpu.render(&app.dock)?;
    app.animating = true;
    Ok(())
}

unsafe fn recover_gpu(hwnd: HWND, app: &mut App) -> Result<()> {
    let (_, _, width, height) = app.full;
    let mut gpu = Gpu::new(hwnd, width as u32, height as u32, app.dock.dpi)?;
    gpu.load_icons(&app.dock.items, app.dock.dpi);
    gpu.render(&app.dock)?;
    app.gpu = gpu;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        unsafe {
            show_error(HWND::default(), "FeatherDock 启动失败", error);
        }
    }
}

fn run() -> Result<()> {
    unsafe {
        let Some(instance) = single_instance::SingleInstance::acquire()? else {
            return Ok(());
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
        let large_icon = app_icon::load();
        let small_icon = app_icon::load();
        let class_name = w!("FeatherDockWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hIcon: large_icon,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIconSm: small_icon,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(Error::from_win32());
        }

        let dpi = GetDpiForSystem() as f32 / 96.0;
        let running = windows_list::enumerate_sorted();
        let items = compose_items(&running)?;
        let (win_w, win_h) = dock::window_size(&items, dpi);
        // Anchor the window's bottom to the true screen bottom so the auto-hide
        // reveal trigger sits at the very edge (slam the mouse down to summon it).
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let x = (screen_w - win_w as i32) / 2;
        let y = screen_h - win_h as i32;

        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("FeatherDock"),
            WS_POPUP,
            x,
            y,
            win_w as i32,
            win_h as i32,
            None,
            None,
            hinstance,
            None,
        )?;

        let mut gpu = Gpu::new(hwnd, win_w, win_h, dpi)?;
        let dock = Dock::new(items, dpi, win_w as f32, win_h as f32);
        gpu.load_icons(&dock.items, dpi);
        let tray = Tray::new(hwnd);
        // Track open windows event-driven (no polling) so the right zone stays live.
        let hooks = windows_list::install_hooks(hwnd);
        let dock_settings = settings::load();
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let fullscreen_active = windows_list::is_fullscreen_present(monitor);
        // Capture the user's taskbar auto-hide preference before we touch it, then
        // apply the saved mode (so a "hidden" choice persists across launches).
        taskbar::capture_original();
        taskbar::apply(dock_settings.taskbar_mode);
        let app_ptr = Box::into_raw(Box::new(App {
            _instance: instance,
            gpu,
            dock,
            tray,
            hooks,
            running_sig: running_sig(&running),
            settings: dock_settings,
            fullscreen_active,
            animating: true, // ease to the resting state (resident or hidden)
            tracking: false,
            full: (x, y, win_w as i32, win_h as i32),
            strip_h: ((6.0 * dpi).round() as i32).max(4),
            expanded: true,
            taskbar_invoked_at: None,
        }));
        // Ease toward the resting state: resident at the bottom by default, or
        // hidden if in auto-hide mode / a fullscreen app is already in front.
        (*app_ptr).dock.reveal_target = resting_target(&*app_ptr);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
        DragAcceptFiles(hwnd, BOOL(1)); // files, folders, shortcuts, and applications

        (*app_ptr).gpu.render(&(*app_ptr).dock)?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        // Event-driven loop: block in GetMessage when idle (0% CPU); when
        // animating, drain input then render one vsync-paced frame.
        let mut msg = MSG::default();
        loop {
            if (*app_ptr).animating {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        cleanup(app_ptr);
                        return Ok(());
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if !(*app_ptr).animating {
                    continue;
                }
                // Grow back to full size whenever any part of the dock should show —
                // covers reveals not driven by the mouse (fullscreen exit, mode
                // toggle), not just WM_MOUSEMOVE.
                if !(*app_ptr).expanded
                    && ((*app_ptr).dock.reveal > 0.01 || (*app_ptr).dock.reveal_target > 0.01)
                {
                    if let Err(error) = set_expanded(hwnd, &mut *app_ptr, true) {
                        show_error(hwnd, "展开 Dock 窗口失败", error);
                    }
                }
                let moving = (*app_ptr).dock.tick();
                if let Err(render_error) = (*app_ptr).gpu.render(&(*app_ptr).dock) {
                    recover_gpu(hwnd, &mut *app_ptr).map_err(|recovery_error| {
                        Error::new(
                            recovery_error.code(),
                            format!(
                                "渲染失败且无法恢复：{render_error}; recovery: {recovery_error}"
                            ),
                        )
                    })?;
                }
                // Keep rendering every vsync while the cursor is over the dock so the
                // bump tracks it at full refresh rate; stop only once it has left AND
                // the icons eased back to rest — then idle returns to 0% CPU.
                if !moving && (*app_ptr).dock.cursor_x.is_none() {
                    // Fully hidden -> collapse the window to the strip so the area it
                    // covered becomes click-through, then go idle.
                    if (*app_ptr).dock.reveal <= 0.01 {
                        if let Err(error) = set_expanded(hwnd, &mut *app_ptr, false) {
                            show_error(hwnd, "收起 Dock 窗口失败", error);
                        }
                    }
                    (*app_ptr).animating = false;
                }
            } else {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if r.0 <= 0 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        cleanup(app_ptr);
    }
    Ok(())
}

unsafe fn cleanup(app_ptr: *mut App) {
    if !app_ptr.is_null() {
        // Never leave the user without a taskbar.
        taskbar::restore();
        windows_list::remove_hooks(std::mem::take(&mut (*app_ptr).hooks));
        (*app_ptr).tray.remove();
        drop(Box::from_raw(app_ptr));
    }
}

/// Open an application, shortcut, file, or folder with the Windows Shell.
unsafe fn open_content(path: &str) -> Result<()> {
    let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let result = ShellExecuteW(
        HWND::default(),
        w!("open"),
        PCWSTR(w.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
    if result.0 as isize <= 32 {
        Err(Error::new(
            E_FAIL,
            format!(
                "Windows 无法打开此内容（ShellExecute={}）",
                result.0 as isize
            ),
        ))
    } else {
        Ok(())
    }
}

pub(crate) unsafe fn pick_content(owner: HWND, folder: bool) -> Result<Option<String>> {
    let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
    let mut options = dialog.GetOptions()?;
    options |= FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
    if folder {
        options |= FOS_PICKFOLDERS;
        dialog.SetTitle(w!("添加文件夹到 FeatherDock"))?;
    } else {
        options |= FOS_FILEMUSTEXIST;
        dialog.SetTitle(w!("添加文件或应用到 FeatherDock"))?;
    }
    dialog.SetOptions(options)?;
    if let Err(error) = dialog.Show(owner) {
        return if error.code() == HRESULT_CANCELLED {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let item = dialog.GetResult()?;
    let raw = item.GetDisplayName(SIGDN_FILESYSPATH)?;
    let path = raw.to_string()?;
    CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void));
    Ok(Some(path))
}

unsafe fn add_content(hwnd: HWND, app: &mut App, path: &str) -> Result<bool> {
    let paths = [path.to_string()];
    Ok(add_contents(hwnd, app, &paths)? > 0)
}

unsafe fn add_contents(hwnd: HWND, app: &mut App, paths: &[String]) -> Result<usize> {
    let mut added_count = 0;
    for path in paths {
        let path_ref = std::path::Path::new(path);
        if !path_ref.exists() {
            return Err(Error::new(E_FAIL, format!("路径不存在：{path}")));
        }
        let label = path_ref
            .file_stem()
            .or_else(|| path_ref.file_name())
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Item".to_string());
        if config::add_item(&label, path).map_err(|error| io_error("保存 Dock 内容失败", error))?
        {
            added_count += 1;
        }
    }
    if added_count > 0 {
        reload_app(hwnd, app)?;
    }
    Ok(added_count)
}

unsafe fn append_item(menu: HMENU, id: usize, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide.as_ptr()));
}

/// Right-click on a running window: window actions, plus pinning it to the dock.
unsafe fn show_window_menu(owner: HWND, app: &mut App, target: isize) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let Ok(menu) = CreatePopupMenu() else { return };
    append_item(menu, ID_WIN_MAXIMIZE, "最大化 / 还原");
    append_item(menu, ID_WIN_MINIMIZE, "最小化");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, ID_WIN_PIN, "固定在 Dock");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, ID_WIN_CLOSE, "关闭窗口");
    let _ = SetForegroundWindow(owner); // so the menu dismisses on click-away
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        owner,
        None,
    );
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        ID_WIN_CLOSE => windows_list::close_window(target),
        ID_WIN_MINIMIZE => windows_list::minimize_window(target),
        ID_WIN_MAXIMIZE => windows_list::toggle_maximize(target),
        ID_WIN_PIN => pin_running_window(owner, app, target),
        _ => {}
    }
}

unsafe fn pin_running_window(owner: HWND, app: &mut App, target: isize) {
    let Some(path) = windows_list::process_exe_path_for_window(target) else {
        show_error(
            owner,
            "固定运行程序失败",
            Error::new(E_FAIL, "无法读取此窗口的程序路径"),
        );
        return;
    };
    if let Err(error) = add_content(owner, app, &path) {
        show_error(owner, "固定运行程序失败", error);
    }
}

/// Right-click on a pinned item: open it, reveal it in Explorer, or unpin it.
unsafe fn show_pinned_menu(owner: HWND, app: &mut App, path: &str) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let Ok(menu) = CreatePopupMenu() else { return };
    append_item(menu, ID_PIN_OPEN, "打开");
    append_item(menu, ID_PIN_LOCATION, "打开文件所在位置");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, ID_PIN_REMOVE, "从 Dock 移除");
    let _ = SetForegroundWindow(owner);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        owner,
        None,
    );
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        ID_PIN_OPEN => {
            if let Err(error) = open_content(path) {
                show_error(owner, "打开内容失败", error);
            }
        }
        ID_PIN_LOCATION => reveal_in_explorer(path),
        ID_PIN_REMOVE => match config::remove_item(path) {
            Ok(true) => {
                if let Err(error) = reload_app(owner, app) {
                    show_error(owner, "移除后刷新失败", error);
                }
            }
            Ok(false) => {}
            Err(error) => show_error(owner, "从配置移除失败", io_error("移除内容失败", error)),
        },
        _ => {}
    }
}

/// Open Explorer with the file selected.
unsafe fn reveal_in_explorer(path: &str) {
    let file: Vec<u16> = "explorer.exe"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args: Vec<u16> = format!("/select,\"{path}\"")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = ShellExecuteW(
        HWND::default(),
        w!("open"),
        PCWSTR(file.as_ptr()),
        PCWSTR(args.as_ptr()),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Shared tray / Start-button menu: add content, toggle dock mode, settings, exit.
unsafe fn run_tray_menu(hwnd: HWND, app: &mut App) {
    let always = app.settings.dock_mode == settings::DockMode::Always;
    match app.tray.show_menu(hwnd, always) {
        Some(tray::ID_EXIT) => PostQuitMessage(0),
        Some(tray::ID_AUTOSTART) => {
            if let Err(error) = autostart::set(!autostart::is_enabled()) {
                show_error(hwnd, "更新开机自启失败", error);
            }
        }
        Some(tray::ID_ADD) => match pick_content(hwnd, false) {
            Ok(Some(path)) => {
                if let Err(error) = add_content(hwnd, app, &path) {
                    show_error(hwnd, "添加内容失败", error);
                }
            }
            Ok(None) => {}
            Err(error) => show_error(hwnd, "打开文件选择器失败", error),
        },
        Some(tray::ID_ADD_FOLDER) => match pick_content(hwnd, true) {
            Ok(Some(path)) => {
                if let Err(error) = add_content(hwnd, app, &path) {
                    show_error(hwnd, "添加文件夹失败", error);
                }
            }
            Ok(None) => {}
            Err(error) => show_error(hwnd, "打开文件夹选择器失败", error),
        },
        Some(tray::ID_SETTINGS) => settings_window::open(hwnd),
        Some(tray::ID_TOGGLE_DOCK_MODE) => {
            app.settings.dock_mode = match app.settings.dock_mode {
                settings::DockMode::Always => settings::DockMode::AutoHide,
                settings::DockMode::AutoHide => settings::DockMode::Always,
            };
            if let Err(error) = settings::save(&app.settings) {
                show_error(hwnd, "保存设置失败", io_error("保存设置", error));
            }
            let target = if app.dock.cursor_x.is_some() {
                1.0
            } else {
                resting_target(app)
            };
            app.dock.reveal_target = target;
            app.animating = true;
        }
        _ => {}
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *ptr;
        if msg == app.tray.taskbar_created_message() {
            if let Err(error) = app.tray.restore() {
                show_error(hwnd, "恢复托盘图标失败", error);
            }
            return LRESULT(0);
        }
        match msg {
            WM_MOUSEMOVE => {
                let _ = set_expanded(hwnd, app, true); // grow back to full so it can slide in
                app.dock.cursor_x = Some((lparam.0 & 0xFFFF) as i16 as f32);
                app.dock.reveal_target = 1.0; // mouse is over the dock zone -> reveal
                app.animating = true;
                if !app.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    app.tracking = true;
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                app.dock.cursor_x = None;
                // Resident by default; slide away only in auto-hide mode or fullscreen.
                let target = resting_target(app);
                app.dock.reveal_target = target;
                app.animating = true;
                app.tracking = false;
                LRESULT(0)
            }
            WM_NCHITTEST => {
                // Only the dock zone grabs the mouse; everything else is click-through.
                // When hidden, that zone shrinks to a thin strip at the screen's edge.
                let sx = (lparam.0 & 0xFFFF) as i16 as i32;
                let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let cx = (sx - wr.left) as f32;
                let cy = (sy - wr.top) as f32;
                let (l, t, r, b) = if app.expanded {
                    app.dock.interactive_hit_zone()
                } else {
                    // collapsed strip: only the dock's horizontal span at the very edge
                    let (sl, sr) = app.dock.dock_span_x();
                    (sl, 0.0, sr, app.strip_h as f32)
                };
                if cx >= l && cx <= r && cy >= t && cy <= b {
                    LRESULT(HTCLIENT as isize)
                } else {
                    LRESULT(HTTRANSPARENT as isize)
                }
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                if let Some(i) = app.dock.hit_test(x, y) {
                    // Copy the bits we need so we can mutate the dock (bump) after.
                    let role = app.dock.items[i].role;
                    let item_hwnd = app.dock.items[i].hwnd;
                    let path = app.dock.items[i].path.clone();
                    match role {
                        dock::ItemRole::Start => {
                            if taskbar::reveal_for_start_invocation(app.settings.taskbar_mode) {
                                app.taskbar_invoked_at = Some(Instant::now());
                                let _ = SetTimer(
                                    hwnd,
                                    TIMER_TASKBAR_REHIDE,
                                    TIMER_TASKBAR_REHIDE_MS,
                                    None,
                                );
                            }
                            windows_list::open_start_menu();
                            app.dock.bump(i);
                            app.animating = true;
                        }
                        dock::ItemRole::Running => {
                            if let Some(raw) = item_hwnd {
                                windows_list::activate(raw);
                                app.dock.bump(i);
                                app.animating = true;
                            }
                        }
                        dock::ItemRole::Pinned => {
                            if let Some(path) = path {
                                match open_content(&path) {
                                    Ok(()) => {
                                        app.dock.bump(i);
                                        app.animating = true;
                                    }
                                    Err(error) => show_error(hwnd, "打开内容失败", error),
                                }
                            }
                        }
                        dock::ItemRole::Divider => {}
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                if let Some(i) = app.dock.hit_test(x, y) {
                    let role = app.dock.items[i].role;
                    let item_hwnd = app.dock.items[i].hwnd;
                    let path = app.dock.items[i].path.clone();
                    match role {
                        dock::ItemRole::Running => {
                            if let Some(raw) = item_hwnd {
                                show_window_menu(hwnd, app, raw);
                            }
                        }
                        dock::ItemRole::Pinned => {
                            if let Some(path) = path {
                                show_pinned_menu(hwnd, app, &path);
                            }
                        }
                        dock::ItemRole::Start => run_tray_menu(hwnd, app),
                        dock::ItemRole::Divider => {}
                    }
                }
                LRESULT(0)
            }
            WM_TRAY => {
                let evt = (lparam.0 & 0xFFFF) as u32;
                if evt == WM_RBUTTONUP || evt == WM_LBUTTONUP {
                    run_tray_menu(hwnd, app);
                }
                LRESULT(0)
            }
            WM_DROPFILES => {
                let hdrop = HDROP(wparam.0 as *mut core::ffi::c_void);
                let count = DragQueryFileW(hdrop, u32::MAX, None);
                let mut paths = Vec::with_capacity(count as usize);
                for index in 0..count {
                    let len = DragQueryFileW(hdrop, index, None);
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len as usize + 1];
                    let written = DragQueryFileW(hdrop, index, Some(&mut buf));
                    if written > 0 {
                        paths.push(String::from_utf16_lossy(&buf[..written as usize]));
                    }
                }
                DragFinish(hdrop);
                if let Err(error) = add_contents(hwnd, app, &paths) {
                    show_error(hwnd, "拖放内容失败", error);
                }
                LRESULT(0)
            }
            windows_list::WM_WINDOWS_CHANGED => {
                // Cheap on every event: re-check whether a fullscreen app is in
                // front and retract / restore the dock accordingly.
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let fullscreen = windows_list::is_fullscreen_present(monitor);
                if fullscreen != app.fullscreen_active {
                    app.fullscreen_active = fullscreen;
                    let target = if app.dock.cursor_x.is_some() {
                        1.0
                    } else {
                        resting_target(app)
                    };
                    app.dock.reveal_target = target;
                    app.animating = true;
                }
                // Expensive (wParam == 0 only): the window SET may have changed, so
                // re-scan and rebuild if what we display actually differs.
                if wparam.0 == 0 {
                    // Debounce: (re)arm a one-shot timer instead of enumerating now.
                    // A burst of events just keeps resetting it; we scan once it settles.
                    SetTimer(hwnd, TIMER_WINDOWS, TIMER_WINDOWS_MS, None);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_WINDOWS => {
                let _ = KillTimer(hwnd, TIMER_WINDOWS);
                let running = windows_list::enumerate_sorted();
                let sig = running_sig(&running);
                if sig != app.running_sig {
                    app.running_sig = sig;
                    if let Err(error) = reload_with(hwnd, app, &running) {
                        show_error(hwnd, "刷新窗口列表失败", error);
                    }
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_TASKBAR_REHIDE => {
                if let Some(invoked_at) = app.taskbar_invoked_at {
                    let grace_elapsed = invoked_at.elapsed() >= TASKBAR_INVOCATION_GRACE;
                    let pointer_over_taskbar = taskbar::pointer_over_taskbar();
                    if taskbar::should_rehide_after_start_invocation(
                        app.settings.taskbar_mode,
                        grace_elapsed,
                        pointer_over_taskbar,
                    ) {
                        let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                        app.taskbar_invoked_at = None;
                        taskbar::rehide_after_start_invocation(app.settings.taskbar_mode);
                    } else if app.settings.taskbar_mode != settings::TaskbarMode::Hidden {
                        let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                        app.taskbar_invoked_at = None;
                    }
                } else {
                    let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                }
                LRESULT(0)
            }
            settings_window::WM_SETTINGS_CHANGED => {
                // The settings window changed dock_mode / fullscreen behavior; re-read
                // and ease to the new resting state. (taskbar_mode it applied itself.)
                app.settings = settings::load();
                let target = if app.dock.cursor_x.is_some() {
                    1.0
                } else {
                    resting_target(app)
                };
                app.dock.reveal_target = target;
                app.animating = true;
                LRESULT(0)
            }
            settings_window::WM_PINS_CHANGED => {
                // Pinned apps changed in the settings window — rebuild the dock.
                if let Err(error) = reload_app(hwnd, app) {
                    show_error(hwnd, "刷新常驻应用失败", error);
                }
                LRESULT(0)
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                if let Err(error) = reload_app(hwnd, app) {
                    show_error(hwnd, "重新布局 Dock 失败", error);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
