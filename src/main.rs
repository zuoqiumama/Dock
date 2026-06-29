#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// FeatherDock — an ultra-light, GPU-composited macOS-style dock for Windows.
// NOTE: console subsystem kept ON during early dev so panics are visible.
// Flip to `#![windows_subsystem = "windows"]` once stable.

use std::time::{Duration, Instant};

mod app_icon;
mod apps;
mod atomic;
mod autostart;
mod categories;
mod command_palette;
mod config;
mod content;
mod control_center;
mod desktop_icons;
mod desktop_scan;
mod dock;
mod drawer;
mod drawer_input;
mod drawer_layout;
mod error_log;
mod folder_stack;
mod glass;
mod graphics;
mod icons;
mod render;
mod settings;
mod settings_window;
mod single_instance;
mod sysctl;
mod taskbar;
mod theme;
mod tray;
mod watchdog;
mod window_preview;
mod windows_list;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromWindow, SetWindowRgn, HGDIOBJ,
    HRGN, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
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
const QUIT_WAIT_MS: u32 = 5_000;
const ID_SEARCH_HOTKEY: i32 = 3001;
const QUIT_FLAG: &str = "--quit";
const RESTORE_SYSTEM_FLAG: &str = "--restore-system";
const WM_SHOW_EXISTING: u32 = 0x8033;

// Right-click context-menu command ids (kept clear of the tray ids in tray.rs).
const ID_WIN_CLOSE: usize = 2001;
const ID_WIN_MINIMIZE: usize = 2002;
const ID_WIN_MAXIMIZE: usize = 2003;
const ID_WIN_PIN: usize = 2004;
const ID_PIN_OPEN: usize = 2101;
const ID_PIN_LOCATION: usize = 2102;
const ID_PIN_REMOVE: usize = 2103;

#[derive(Clone, Copy, PartialEq, Eq)]
struct WatchdogPlan {
    restore_taskbar: bool,
    restore_desktop_icons: bool,
}

impl WatchdogPlan {
    fn needed(self) -> bool {
        self.restore_taskbar || self.restore_desktop_icons
    }
}

struct App {
    _instance: single_instance::SingleInstance,
    gpu: Gpu,
    dock: Dock,
    tray: Tray,
    hooks: Vec<HWINEVENTHOOK>,    // WinEvent hooks tracking open windows
    running_sig: Vec<RunningSig>, // last seen open-window identity (change detection)
    settings: settings::Settings, // persisted dock mode + fullscreen behavior
    fullscreen_active: bool,      // a fullscreen (borderless) app is in front on our monitor
    maximized_active: bool,       // a maximized (captioned) window is in front on our monitor
    animating: bool,
    tracking: bool,
    full: (i32, i32, i32, i32), // full window rect (x, y, w, h)
    strip_h: i32,               // window height when collapsed to the reveal strip
    expanded: bool,             // true = full window, false = thin bottom strip
    window_hidden: bool,        // fully SW_HIDE'd because a fullscreen app is in front
    region_full: bool,          // input region: true = whole window, false = clipped to pill
    pending_relayout: bool,     // shrink the window to fit once exit animations settle
    pending_rebuild: bool, // a window-set/display change deferred while a fullscreen app owned the screen
    taskbar_invoked_at: Option<Instant>,
    watchdog: Option<std::process::Child>, // sibling that restores system state if we crash
    watchdog_plan: Option<WatchdogPlan>,
}

/// True while a fullscreen app (game / video) is in front and we're set to yield to it.
/// In this state the dock is fully hidden — not merely collapsed to the reveal strip —
/// and ignores bottom-edge hover, so it can never pop over a game and kick an exclusive-
/// fullscreen title back to the desktop.
fn fullscreen_suppressed(app: &App) -> bool {
    app.settings.hide_on_fullscreen && app.fullscreen_active
}

/// True while a *maximized* window is in front and we're set to yield to it: the dock
/// retracts to its reveal strip so it doesn't cover the window, but a bottom-edge hover
/// STILL summons it — unlike `fullscreen_suppressed`, which fully hides it and ignores
/// hover. (A window can't be both: maximized has a caption, fullscreen does not.)
fn maximized_retracted(app: &App) -> bool {
    app.settings.hide_on_maximized && app.maximized_active
}

/// True if a borderless monitor-filling app (a fullscreen game / video) is in front on
/// the dock's monitor — the cue to keep the dock COMPLETELY passive: no `SetWindowPos`
/// on our window, no window reflow. Any z-order churn at that moment knocks such an app
/// out of exclusive fullscreen and bounces it to the desktop (the "can't enter the game"
/// bug). Checked fresh (not from cached state) since a display-mode change can arrive
/// before the foreground event that updates `fullscreen_active`.
unsafe fn fullscreen_app_present(hwnd: HWND) -> bool {
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    windows_list::is_fullscreen_present(monitor)
}

/// Where `reveal` should ease to when the cursor is NOT over the dock: hidden in auto-hide
/// mode, while a fullscreen app is in front, or while a maximized window is in front;
/// otherwise resident.
fn resting_target(app: &App) -> f32 {
    let hide = app.settings.dock_mode == settings::DockMode::AutoHide
        || fullscreen_suppressed(app)
        || maximized_retracted(app);
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

/// Assemble the full dock row: Start, pinned apps, then (if any open windows that
/// AREN'T already pinned) a divider followed by one slot per such window group.
///
/// A running window whose executable matches a pinned app is *merged into that pinned
/// icon* (it gains a running dot and activates on click) rather than shown a second
/// time on the right — so the dock never carries the same app twice (macOS-style).
fn compose_items(
    running: &[windows_list::RunningWindow],
    drawer_enabled: bool,
) -> Result<Vec<dock::DockItem>> {
    let mut pinned = pinned_items()?;
    let mut unpinned: Vec<windows_list::RunningGroup> = Vec::new();
    for group in windows_list::group_by_application(running) {
        match pinned
            .iter_mut()
            .find(|item| pinned_matches_group(item, &group))
        {
            Some(item) => apps::attach_running(item, &group),
            None => unpinned.push(group),
        }
    }

    let mut items = Vec::with_capacity(pinned.len() + unpinned.len() + 4);
    items.push(apps::start_item());
    if drawer_enabled {
        items.push(apps::drawer_item());
    }
    items.extend(pinned);
    if !unpinned.is_empty() {
        items.push(apps::divider_item());
        for group in &unpinned {
            items.push(apps::running_item(group));
        }
    }
    items.push(apps::control_item()); // always last → fixed far-right button
    Ok(items)
}

fn pinned_matches_group(item: &dock::DockItem, group: &windows_list::RunningGroup) -> bool {
    item.windows.is_empty()
        && pinned_identity_matches_group(&item.label, item.path.as_deref(), group)
}

fn pinned_identity_matches_group(
    label: &str,
    path: Option<&str>,
    group: &windows_list::RunningGroup,
) -> bool {
    path.is_some_and(|path| apps::exe_matches(path, &group.key))
        || apps::title_matches_label(label, group)
}

/// Signature of the open-window set (handles only — we don't display titles, so a
/// title change must NOT trigger a reload), used to skip needless rebuilds when a
/// WinEvent fires but the set we show is unchanged (focus switches, renames).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningSig {
    hwnd: isize,
    title: String,
    exe_path: Option<String>,
}

fn running_sig(running: &[windows_list::RunningWindow]) -> Vec<RunningSig> {
    running
        .iter()
        .map(|window| RunningSig {
            hwnd: window.hwnd,
            title: window.title.clone(),
            exe_path: window.exe_path.clone(),
        })
        .collect()
}

unsafe fn monitor_layout(
    hwnd: HWND,
    items: &[dock::DockItem],
) -> Result<(f32, i32, i32, u32, u32)> {
    let dpi = (GetDpiForWindow(hwnd).max(96) as f32) / 96.0;
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    monitor_geometry(monitor, dpi, items)
}

/// Initial placement, computed before the dock window exists. Targets the PRIMARY
/// monitor — the dock is a system-taskbar replacement and the taskbar lives on primary,
/// so this is the deliberate, predictable home — using that monitor's own effective DPI
/// and bounds rather than the system DPI + `SM_CXSCREEN`. Going through the same monitor
/// model as `monitor_layout` means the first frame is sized correctly and doesn't jump on
/// the first relayout (the multi-monitor / mixed-DPI gap).
unsafe fn primary_monitor_layout(items: &[dock::DockItem]) -> Result<(f32, i32, i32, u32, u32)> {
    let monitor = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    let dpi = if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() {
        (dpi_x.max(96) as f32) / 96.0
    } else {
        (GetDpiForSystem().max(96) as f32) / 96.0
    };
    monitor_geometry(monitor, dpi, items)
}

/// Bottom-centre the dock on `monitor` at `dpi`. Anchored to the monitor's true bottom
/// edge (not the work area) so the auto-hide reveal trigger sits at the very edge.
unsafe fn monitor_geometry(
    monitor: windows::Win32::Graphics::Gdi::HMONITOR,
    dpi: f32,
    items: &[dock::DockItem],
) -> Result<(f32, i32, i32, u32, u32)> {
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
/// Where the dock should sit in the z-order. Normally the top of the topmost band;
/// while the user has explicitly revealed the system taskbar (Start click), just
/// *below* the taskbar so they can operate it on top of the dock.
unsafe fn dock_z_insert_after(app: &App) -> HWND {
    if app.taskbar_invoked_at.is_some() {
        let tray = taskbar::tray_hwnd();
        if !tray.is_invalid() {
            return tray;
        }
    }
    HWND_TOPMOST
}

/// Drop the dock just below the (revealed) taskbar so the taskbar is operable; both
/// stay topmost. Called when a Start click reveals the taskbar.
unsafe fn place_dock_behind_taskbar(dock: HWND) {
    let tray = taskbar::tray_hwnd();
    if tray.is_invalid() {
        return;
    }
    let _ = SetWindowPos(
        tray,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    let _ = SetWindowPos(
        dock,
        tray,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

/// Restore the dock to the top of the topmost band (its normal resting z-order).
unsafe fn raise_dock_topmost(dock: HWND) {
    let _ = SetWindowPos(
        dock,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

unsafe fn set_expanded(hwnd: HWND, app: &mut App, expand: bool) -> Result<()> {
    if app.expanded == expand {
        return Ok(());
    }
    let (x, y, w, h) = app.full;
    let z = dock_z_insert_after(app);
    if expand {
        SetWindowPos(hwnd, z, x, y, w, h, SWP_NOACTIVATE)
    } else {
        SetWindowPos(
            hwnd,
            z,
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

/// Fully show or hide the dock window for fullscreen suppression — distinct from the
/// auto-hide *strip*. A hidden window is out of the z-order entirely, so it can neither
/// kick an exclusive-fullscreen game to the desktop nor be summoned by a bottom-edge
/// hover. On re-show we leave the geometry at full size and let `reveal` animate the
/// dock back in.
unsafe fn set_window_hidden(hwnd: HWND, app: &mut App, hide: bool) {
    if app.window_hidden == hide {
        return;
    }
    app.window_hidden = hide;
    let _ = ShowWindow(hwnd, if hide { SW_HIDE } else { SW_SHOWNOACTIVATE });
}

/// Clip the window's *input* region to just the visible pill (at rest) or remove the clip
/// so the whole window takes input (while hovering / animating, when magnified icons rise
/// above the pill and must stay interactive).
///
/// This is what truly frees the head-room above and slack beside the dock: `HTTRANSPARENT`
/// from `WM_NCHITTEST` does NOT pass clicks through for this layered DirectComposition
/// window — the window physically covers the area and swallows the click. A real window
/// region makes those pixels not belong to the window at all, so clicks reach the app
/// beneath (e.g. a Send button just above the dock). Rounded to match the pill's corners.
unsafe fn set_region_full(hwnd: HWND, app: &mut App, full: bool) {
    if app.region_full == full {
        return;
    }
    app.region_full = full;
    if full {
        // NULL region = the whole window receives input again.
        let _ = SetWindowRgn(hwnd, HRGN::default(), BOOL(1));
        return;
    }
    let (l, t, r, b) = app.dock.frame().pill;
    let ellipse = (44.0 * app.dock.dpi) as i32; // 2x the 22*dpi pill corner radius
    let rgn = CreateRoundRectRgn(
        l.floor() as i32,
        t.floor() as i32,
        r.ceil() as i32 + 1, // CreateRoundRectRgn's right/bottom are exclusive
        b.ceil() as i32 + 1,
        ellipse,
        ellipse,
    );
    // On success the system owns the region; on failure we still own it -> free it.
    if SetWindowRgn(hwnd, rgn, BOOL(1)) == 0 {
        let _ = DeleteObject(HGDIOBJ(rgn.0));
    }
}

/// Rebuild the dock from a fresh scan of open windows (e.g. after a config edit).
unsafe fn reload_app(hwnd: HWND, app: &mut App) -> Result<()> {
    let running = windows_list::enumerate_sorted();
    app.running_sig = running_sig(&running);
    reload_with(hwnd, app, &running)
}

/// Rebuild the dock from an already-enumerated window set. Reconciles the desired
/// items against the live ones in place, so surviving slots keep their animation
/// state and icon while new windows ease *in* and closed ones collapse *out* — no
/// wholesale rebuild, no snap. The window is sized to the merged row at its widest
/// (entering + exiting both counted) so nothing clips; the slack is reclaimed once
/// the exit animations settle (`relayout_to_fit`).
unsafe fn reload_with(
    hwnd: HWND,
    app: &mut App,
    running: &[windows_list::RunningWindow],
) -> Result<()> {
    window_preview::hide();
    // The pill width is about to change as slots ease in/out — drop any pill clip so the
    // reconcile animation isn't cut off; the settle re-clips to the new pill at rest.
    set_region_full(hwnd, app, true);
    let desired = compose_items(running, app.settings.drawer_enabled)?;
    let remap = app.dock.reconcile(desired);
    let (dpi, x, y, width, height) = monitor_layout(hwnd, &app.dock.items)?;
    app.gpu.resize(width, height)?;
    app.gpu.remap_icons(&remap, &app.dock.items);
    // Update geometry on the (preserved) dock; reveal stays put so a list change
    // never flashes the dock in or out — ease toward what the mode currently wants.
    app.dock.dpi = dpi;
    app.dock.win_w = width as f32;
    app.dock.win_h = height as f32;
    app.dock.reveal_target = resting_target(app);
    app.full = (x, y, width as i32, height as i32);
    app.strip_h = ((6.0 * dpi).round() as i32).max(4);
    app.expanded = true;
    app.pending_relayout = true;
    let z = dock_z_insert_after(app);
    SetWindowPos(hwnd, z, x, y, width as i32, height as i32, SWP_NOACTIVATE)?;
    app.gpu.render(&app.dock)?;
    app.animating = true;
    Ok(())
}

/// Resize the window to exactly fit the current item row, recentred on the monitor.
/// Called once the appear/disappear animations settle: we hold the window wide while
/// slots collapse so they don't clip, then reclaim the slack here. No-op when already
/// the right size, so it's safe to call on every settle.
unsafe fn relayout_to_fit(hwnd: HWND, app: &mut App) -> Result<()> {
    let (dpi, x, y, width, height) = monitor_layout(hwnd, &app.dock.items)?;
    if width as i32 == app.full.2 && height as i32 == app.full.3 {
        return Ok(());
    }
    app.gpu.resize(width, height)?;
    app.dock.dpi = dpi;
    app.dock.win_w = width as f32;
    app.dock.win_h = height as f32;
    app.full = (x, y, width as i32, height as i32);
    app.strip_h = ((6.0 * dpi).round() as i32).max(4);
    let z = dock_z_insert_after(app);
    SetWindowPos(hwnd, z, x, y, width as i32, height as i32, SWP_NOACTIVATE)?;
    app.gpu.render(&app.dock)?;
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
    // Watchdog mode: a tiny sibling copy of ourselves that restores the taskbar if the
    // dock dies abnormally. Handle it before any GUI / single-instance / COM setup.
    if let Some(args) = watchdog::parse_args() {
        watchdog::run(args);
        return;
    }
    if handle_control_args() {
        return;
    }
    if let Err(error) = run() {
        unsafe {
            show_error(HWND::default(), "FeatherDock 启动失败", error);
        }
    }
}

fn handle_control_args() -> bool {
    match std::env::args().nth(1).as_deref() {
        Some(QUIT_FLAG) => {
            unsafe { request_graceful_quit() };
            true
        }
        Some(RESTORE_SYSTEM_FLAG) => {
            unsafe { restore_stranded_system_state() };
            true
        }
        _ => false,
    }
}

unsafe fn dock_window() -> HWND {
    FindWindowW(w!("FeatherDockWindow"), PCWSTR::null()).unwrap_or_default()
}

unsafe fn reveal_existing_instance() {
    let hwnd = dock_window();
    if hwnd.is_invalid() {
        restore_stranded_system_state();
        let text: Vec<u16> = "FeatherDock is already running, but its dock window could not be found.\n\nThe taskbar and desktop icons were restored. End the remaining FeatherDock.exe process in Task Manager, then start FeatherDock again."
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            w!("FeatherDock"),
            MB_OK | MB_ICONWARNING,
        );
        return;
    }
    let _ = PostMessageW(hwnd, WM_SHOW_EXISTING, WPARAM(0), LPARAM(0));
}

unsafe fn request_graceful_quit() {
    let hwnd = dock_window();
    if hwnd.is_invalid() {
        restore_stranded_system_state();
        return;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let process = if pid == 0 {
        None
    } else {
        OpenProcess(PROCESS_SYNCHRONIZE, BOOL(0), pid).ok()
    };
    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
    if let Some(process) = process {
        if !process.is_invalid() {
            WaitForSingleObject(process, QUIT_WAIT_MS);
            let _ = CloseHandle(process);
            return;
        }
    }
    for _ in 0..50 {
        if !IsWindow(hwnd).as_bool() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

unsafe fn restore_stranded_system_state() {
    taskbar::recover_if_stranded();
    desktop_icons::set_hidden(false);
}

/// Install a panic hook that restores the taskbar before the default hook runs (and, in
/// release builds, the process aborts without unwinding). Without this, a panic after we
/// hid the taskbar would leave the user with no visible taskbar.
fn install_taskbar_panic_guard() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        unsafe { taskbar::restore() };
        previous(info);
    }));
}

fn desired_watchdog_plan(app: &App) -> Option<WatchdogPlan> {
    let plan = WatchdogPlan {
        restore_taskbar: app.settings.taskbar_mode != settings::TaskbarMode::Show,
        restore_desktop_icons: app.settings.hide_desktop_icons,
    };
    plan.needed().then_some(plan)
}

/// Keep the abnormal-exit watchdog aligned with the system state FeatherDock currently
/// owns. Rebuild it when settings change so a stale helper never misses a new side effect
/// such as desktop-icon hiding, and stop it once there is nothing left to recover.
unsafe fn reconcile_watchdog(app: &mut App) {
    let desired = desired_watchdog_plan(app);
    if app.watchdog_plan == desired {
        if let Some(child) = app.watchdog.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                return;
            }
        }
        app.watchdog = None;
        app.watchdog_plan = None;
    }
    stop_watchdog(app);
    if let Some(plan) = desired {
        app.watchdog = watchdog::spawn(
            taskbar::original_autohide(),
            plan.restore_taskbar,
            plan.restore_desktop_icons,
        );
        if app.watchdog.is_some() {
            app.watchdog_plan = Some(plan);
        }
    }
}

unsafe fn stop_watchdog(app: &mut App) {
    if let Some(mut watchdog) = app.watchdog.take() {
        let _ = watchdog.kill();
        let _ = watchdog.wait();
    }
    app.watchdog_plan = None;
}

fn run() -> Result<()> {
    unsafe {
        let Some(instance) = single_instance::SingleInstance::acquire()? else {
            reveal_existing_instance();
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

        let running = windows_list::enumerate_sorted();
        let dock_settings = settings::load();
        let items = compose_items(&running, dock_settings.drawer_enabled)?;
        // Anchor to the primary monitor's bottom-centre using that monitor's own DPI and
        // bounds (see `primary_monitor_layout`) so the auto-hide reveal trigger sits at the
        // very edge and the first frame is already sized for the right monitor.
        let (dpi, x, y, win_w, win_h) = primary_monitor_layout(&items)?;

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
        let dock = Dock::new(items, dpi, win_w as f32, win_h as f32, dock_settings.theme);
        gpu.load_icons(&dock.items, dpi);
        let tray = Tray::new(hwnd);
        // Track open windows event-driven (no polling) so the right zone stays live.
        let hooks = windows_list::install_hooks(hwnd);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let fullscreen_active = windows_list::is_fullscreen_present(monitor);
        let maximized_active = windows_list::is_maximized_present(monitor);
        // If a previous run died without restoring the taskbar (power loss, or both the
        // dock and its watchdog killed at once), put it back BEFORE we read the "original"
        // state — otherwise we'd capture the stranded, invisible bar as the new baseline.
        taskbar::recover_if_stranded();
        // Capture the user's taskbar auto-hide preference before we touch it, then
        // apply the saved mode (so a "hidden" choice persists across launches).
        taskbar::capture_original();
        // A panic (release builds abort without unwinding) must still put the bar back.
        install_taskbar_panic_guard();
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
            maximized_active,
            animating: true, // ease to the resting state (resident or hidden)
            tracking: false,
            full: (x, y, win_w as i32, win_h as i32),
            strip_h: ((6.0 * dpi).round() as i32).max(4),
            expanded: true,
            window_hidden: false,
            region_full: true,
            pending_relayout: false,
            pending_rebuild: false,
            taskbar_invoked_at: None,
            watchdog: None,
            watchdog_plan: None,
        }));
        // Ease toward the resting state: resident at the bottom by default, or
        // hidden if in auto-hide mode / a fullscreen app is already in front.
        (*app_ptr).dock.reveal_target = resting_target(&*app_ptr);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
        // Arm the external safety net if we modified system state: a sibling process that
        // restores it should we die abnormally (Task-Manager kill, crash, panic).
        reconcile_watchdog(&mut *app_ptr);
        // If we start hidden, grow any pre-maximized windows into the freed work area
        // (taskbar::apply already made the bar transparent + auto-hide above).
        if (*app_ptr).settings.taskbar_mode == settings::TaskbarMode::Hidden {
            taskbar::reflow_maximized_windows();
        }
        // Apply the saved "hide desktop icons" choice (restored on exit in `cleanup`).
        if (*app_ptr).settings.hide_desktop_icons {
            desktop_icons::set_hidden(true);
        }
        DragAcceptFiles(hwnd, BOOL(1)); // files, folders, shortcuts, and applications
        let _ = RegisterHotKey(
            hwnd,
            ID_SEARCH_HOTKEY,
            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
            VK_SPACE.0 as u32,
        );

        (*app_ptr).gpu.render(&(*app_ptr).dock)?;
        // Don't flash the dock on screen if a fullscreen app is already in front
        // (e.g. the dock was relaunched while a game is running) — stay hidden.
        if fullscreen_suppressed(&*app_ptr) {
            (*app_ptr).window_hidden = true;
        } else {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        if (*app_ptr).settings.drawer_enabled {
            drawer::warm_cache();
        }

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
                // Drop slots that finished collapsing, keeping GPU icons in lockstep.
                let removed = (*app_ptr).dock.take_finished_exits();
                if !removed.is_empty() {
                    (*app_ptr).gpu.drop_icons(&removed);
                }
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
                    // Exit animations are done — reclaim the width we held for the
                    // collapsing slots, shrinking the window back to fit the row.
                    if (*app_ptr).pending_relayout {
                        (*app_ptr).pending_relayout = false;
                        if let Err(error) = relayout_to_fit(hwnd, &mut *app_ptr) {
                            show_error(hwnd, "重新布局 Dock 失败", error);
                        }
                    }
                    // Eased fully out. If a fullscreen app owns the screen, hide the
                    // window ENTIRELY (no strip, no z-order presence) so it can't disturb
                    // an exclusive-fullscreen game; otherwise collapse to the reveal strip
                    // so the area it covered becomes click-through. Then go idle.
                    if (*app_ptr).dock.reveal <= 0.01 {
                        if fullscreen_suppressed(&*app_ptr) {
                            set_window_hidden(hwnd, &mut *app_ptr, true);
                        } else if let Err(error) = set_expanded(hwnd, &mut *app_ptr, false) {
                            show_error(hwnd, "收起 Dock 窗口失败", error);
                        }
                        // Strip is a thin sliver and a hidden window has no surface — no
                        // head-room to clip, so the whole (tiny/absent) window takes input.
                        set_region_full(hwnd, &mut *app_ptr, true);
                    } else {
                        // Resident at rest: clip the input region to the visible pill so
                        // clicks above and beside it fall through to the app underneath.
                        set_region_full(hwnd, &mut *app_ptr, false);
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
        // Stop the watchdog first (while it's still blocked on us) so it can't also fire
        // a redundant restore, then put the taskbar back ourselves. Never strand the user.
        stop_watchdog(&mut *app_ptr);
        taskbar::restore();
        // Never leave the user with a blank desktop: put the icons back if we hid them.
        if (*app_ptr).settings.hide_desktop_icons {
            desktop_icons::set_hidden(false);
        }
        windows_list::remove_hooks(std::mem::take(&mut (*app_ptr).hooks));
        (*app_ptr).tray.remove();
        drop(Box::from_raw(app_ptr));
    }
}

/// (Re-)enter the hidden resting state: make the taskbar transparent + auto-hide
/// (freeing its row) and grow any maximized windows into the freed work area. Used on
/// the deliberate hidden-entry paths (Explorer restart, settings switch).
unsafe fn enter_hidden(_hwnd: HWND, _app: &mut App) {
    taskbar::apply(settings::TaskbarMode::Hidden);
    taskbar::reflow_maximized_windows();
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
/// `windows` are all open windows in this slot's app group (a running icon can stand
/// for several windows); per-window closing is also available from the hover preview.
unsafe fn show_window_menu(owner: HWND, app: &mut App, windows: &[isize]) {
    let Some(&primary) = windows.first() else {
        return;
    };
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let Ok(menu) = CreatePopupMenu() else { return };
    if windows.len() == 1 {
        append_item(menu, ID_WIN_MAXIMIZE, "最大化 / 还原");
        append_item(menu, ID_WIN_MINIMIZE, "最小化");
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, ID_WIN_PIN, "固定在 Dock");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let close_label = if windows.len() > 1 {
        "关闭全部窗口"
    } else {
        "关闭窗口"
    };
    append_item(menu, ID_WIN_CLOSE, close_label);
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
        ID_WIN_CLOSE => windows.iter().for_each(|&w| windows_list::close_window(w)),
        ID_WIN_MINIMIZE => windows_list::minimize_window(primary),
        ID_WIN_MAXIMIZE => windows_list::toggle_maximize(primary),
        ID_WIN_PIN => pin_running_window(owner, app, primary),
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

/// Right-click on a pinned item: open it, reveal it in Explorer, or unpin it. When the
/// pinned app is running, its open windows (`windows`) also get window actions —
/// minimize / maximize / close — so a launched pinned app can be closed from the dock.
unsafe fn show_pinned_menu(owner: HWND, app: &mut App, path: &str, windows: &[isize]) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let Ok(menu) = CreatePopupMenu() else { return };
    append_item(
        menu,
        ID_PIN_OPEN,
        if windows.is_empty() {
            "打开"
        } else {
            "打开新实例"
        },
    );
    append_item(menu, ID_PIN_LOCATION, "打开文件所在位置");
    if !windows.is_empty() {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if windows.len() == 1 {
            append_item(menu, ID_WIN_MAXIMIZE, "最大化 / 还原");
            append_item(menu, ID_WIN_MINIMIZE, "最小化");
            append_item(menu, ID_WIN_CLOSE, "关闭窗口");
        } else {
            append_item(menu, ID_WIN_CLOSE, "关闭全部窗口");
        }
    }
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
        ID_WIN_CLOSE => windows.iter().for_each(|&w| windows_list::close_window(w)),
        ID_WIN_MINIMIZE => {
            if let Some(&w) = windows.first() {
                windows_list::minimize_window(w);
            }
        }
        ID_WIN_MAXIMIZE => {
            if let Some(&w) = windows.first() {
                windows_list::toggle_maximize(w);
            }
        }
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

/// Screen-space anchor (center-x, top-y) just above dock item `i`'s icon, used to
/// pop a glass panel (control center / app drawer) centered over the button.
unsafe fn anchor_above(hwnd: HWND, app: &App, i: usize) -> (i32, i32) {
    let frame = app.dock.frame();
    let cx = frame
        .icons
        .iter()
        .find(|ic| ic.idx == i)
        .map(|ic| ic.cx)
        .unwrap_or(0.0);
    let top = frame.pill.1;
    let mut wr = RECT::default();
    let _ = GetWindowRect(hwnd, &mut wr);
    (wr.left + cx as i32, wr.top + top as i32)
}

unsafe fn show_running_preview(hwnd: HWND, app: &App, index: usize) {
    let Some(item) = app.dock.items.get(index) else {
        window_preview::hide();
        return;
    };
    // Any dock item with open windows previews them — both the right-side running
    // apps and pinned apps that are running (their windows merged into the pinned icon).
    if item.windows.is_empty() {
        window_preview::hide();
        return;
    }
    let frame = app.dock.frame();
    let Some(icon) = frame.icons.iter().find(|icon| icon.idx == index) else {
        window_preview::hide();
        return;
    };
    let mut wr = RECT::default();
    if GetWindowRect(hwnd, &mut wr).is_err() {
        window_preview::hide();
        return;
    }
    let key = item.group_key.as_deref().unwrap_or(item.label.as_str());
    window_preview::show(
        hwnd,
        key,
        &item.windows,
        wr.left + icon.cx as i32,
        wr.top + frame.pill.1 as i32,
        app.dock.dpi,
    );
}

#[cfg(test)]
fn activation_target(item: &dock::DockItem) -> Option<isize> {
    activation_order_with_foreground(item, None)
        .into_iter()
        .next()
}

#[cfg(test)]
fn activation_order_with_foreground(
    item: &dock::DockItem,
    foreground: Option<isize>,
) -> Vec<isize> {
    activation_order_with_foreground_owned(item, foreground, false)
}

fn activation_order_with_foreground_owned(
    item: &dock::DockItem,
    foreground: Option<isize>,
    foreground_owned_by_item: bool,
) -> Vec<isize> {
    let label = item.label.trim();
    let mut order = Vec::new();
    let mut push = |raw: isize| {
        if !order.contains(&raw) {
            order.push(raw);
        }
    };

    if let Some(raw) = foreground {
        if foreground_owned_by_item
            || item.hwnd == Some(raw)
            || item.windows.iter().any(|window| window.hwnd == raw)
        {
            push(raw);
        }
    }
    for window in &item.windows {
        if title_equals_label(&window.title, label) {
            push(window.hwnd);
        }
    }
    for window in &item.windows {
        if title_starts_with_label(&window.title, label) {
            push(window.hwnd);
        }
    }
    if let Some(primary) = item.hwnd {
        push(primary);
    }
    for window in &item.windows {
        push(window.hwnd);
    }

    order
}

fn title_equals_label(title: &str, label: &str) -> bool {
    !label.is_empty() && title.trim().eq_ignore_ascii_case(label)
}

fn title_starts_with_label(title: &str, label: &str) -> bool {
    !label.is_empty()
        && title
            .trim()
            .to_ascii_lowercase()
            .starts_with(&format!("{} ", label.to_ascii_lowercase()))
}

unsafe fn activate_item_windows(item: &dock::DockItem) -> bool {
    let foreground = GetForegroundWindow();
    let foreground = (!foreground.is_invalid()).then_some(foreground.0 as isize);
    let foreground_owned =
        foreground.is_some_and(|raw| foreground_window_belongs_to_item(item, raw));
    for raw in activation_order_with_foreground_owned(item, foreground, foreground_owned) {
        if windows_list::activate(raw) {
            return true;
        }
    }
    false
}

unsafe fn foreground_window_belongs_to_item(item: &dock::DockItem, raw: isize) -> bool {
    if item.hwnd == Some(raw) || item.windows.iter().any(|window| window.hwnd == raw) {
        return true;
    }
    if !windows_list::is_running_window(raw) {
        return false;
    }
    let Some(exe_path) = windows_list::process_exe_path_for_window(raw) else {
        return false;
    };
    if item
        .group_key
        .as_deref()
        .is_some_and(|key| apps::exe_matches(&exe_path, key))
    {
        return true;
    }
    item.kind == content::ContentKind::Application
        && item
            .path
            .as_deref()
            .is_some_and(|path| apps::exe_matches(path, &exe_path))
}

unsafe fn activate_live_pinned_window(owner: HWND, app: &mut App, index: usize) -> bool {
    let label = app.dock.items[index].label.clone();
    let path = app.dock.items[index].path.clone();
    let running = windows_list::enumerate_sorted();
    let sig = running_sig(&running);
    let Some(group) = windows_list::group_by_application(&running)
        .into_iter()
        .find(|group| pinned_identity_matches_group(&label, path.as_deref(), group))
    else {
        return false;
    };

    app.running_sig = sig;
    apps::attach_running(&mut app.dock.items[index], &group);
    if activate_item_windows(&app.dock.items[index]) {
        window_preview::hide();
    } else {
        show_running_preview(owner, app, index);
    }
    app.dock.bump(index);
    app.animating = true;
    true
}

unsafe fn activate_live_running_window(owner: HWND, app: &mut App, index: usize) -> bool {
    let label = app.dock.items[index].label.clone();
    let group_key = app.dock.items[index].group_key.clone();
    let running = windows_list::enumerate_sorted();
    let sig = running_sig(&running);
    let Some(group) = windows_list::group_by_application(&running)
        .into_iter()
        .find(|group| {
            group_key.as_deref() == Some(group.key.as_str())
                || apps::title_matches_label(&label, group)
        })
    else {
        return false;
    };

    app.running_sig = sig;
    app.dock.items[index] = apps::running_item(&group);
    if activate_item_windows(&app.dock.items[index]) {
        window_preview::hide();
    } else {
        show_running_preview(owner, app, index);
    }
    app.dock.bump(index);
    app.animating = true;
    true
}

unsafe fn update_running_preview(hwnd: HWND, app: &App, x: f32, y: f32) {
    if let Some(index) = app.dock.hit_test(x, y) {
        if !app.dock.items[index].windows.is_empty() {
            show_running_preview(hwnd, app, index);
            return;
        }
    }
    window_preview::hide();
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
            // Explorer restarted: the taskbar is back, visible and reserving its row.
            // Re-apply the hidden state so the dock stays the only bar at the bottom.
            if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                && app.taskbar_invoked_at.is_none()
            {
                enter_hidden(hwnd, app);
            }
            return LRESULT(0);
        }
        match msg {
            WM_MOUSEMOVE => {
                // While a fullscreen app owns the screen, never let a bottom-edge hover
                // summon the dock — popping it topmost would kick an exclusive-fullscreen
                // game straight back to the desktop. This is the core fix.
                if fullscreen_suppressed(app) {
                    return LRESULT(0);
                }
                // Cursor is on the dock: take input across the whole window again so the
                // magnification can bulge icons up above the pill without losing the mouse.
                set_region_full(hwnd, app, true);
                let _ = set_expanded(hwnd, app, true); // grow back to full so it can slide in
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                app.dock.cursor_x = Some(x);
                app.dock.reveal_target = 1.0; // mouse is over the dock zone -> reveal
                app.animating = true;
                update_running_preview(hwnd, app, x, y);
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
                let over_preview = window_preview::contains_cursor();
                if !over_preview {
                    window_preview::hide();
                }
                // Resident by default; slide away only in auto-hide mode or fullscreen.
                let target = if over_preview {
                    1.0
                } else {
                    resting_target(app)
                };
                app.dock.reveal_target = target;
                app.animating = true;
                app.tracking = false;
                LRESULT(0)
            }
            WM_NCHITTEST => {
                // Fully click-through while a fullscreen app owns the screen, so the game
                // gets every bit of input at the bottom edge and we generate no hover.
                if fullscreen_suppressed(app) {
                    return LRESULT(HTTRANSPARENT as isize);
                }
                // Only the dock zone grabs the mouse; everything else is click-through.
                // When hidden, that zone shrinks to a thin strip at the screen's edge.
                let sx = (lparam.0 & 0xFFFF) as i16 as i32;
                let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let cx = (sx - wr.left) as f32;
                let cy = (sy - wr.top) as f32;
                let (l, t, r, b) = if app.expanded {
                    // Dynamic: just the visible pill at rest, the full magnification
                    // envelope only while the cursor is actually over the dock — so the
                    // dock's box stops swallowing clicks above/beside it when unused.
                    app.dock.pointer_hit_zone()
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
                    let kind = app.dock.items[i].kind;
                    match role {
                        dock::ItemRole::Start => {
                            if taskbar::reveal_for_start_invocation(app.settings.taskbar_mode) {
                                app.taskbar_invoked_at = Some(Instant::now());
                                // Drop the dock behind the revealed taskbar so the user
                                // can operate the taskbar on top of it.
                                place_dock_behind_taskbar(hwnd);
                                let _ = SetTimer(
                                    hwnd,
                                    TIMER_TASKBAR_REHIDE,
                                    TIMER_TASKBAR_REHIDE_MS,
                                    None,
                                );
                            }
                            if taskbar::should_open_start_menu_for_start_invocation(
                                app.settings.taskbar_mode,
                            ) {
                                windows_list::open_start_menu();
                            }
                            app.dock.bump(i);
                            app.animating = true;
                        }
                        dock::ItemRole::Running => {
                            if activate_item_windows(&app.dock.items[i]) {
                                window_preview::hide();
                                app.dock.bump(i);
                                app.animating = true;
                            } else if activate_live_running_window(hwnd, app, i) {
                            } else if let Some(raw) = item_hwnd {
                                window_preview::hide();
                                windows_list::activate(raw);
                                app.dock.bump(i);
                                app.animating = true;
                            } else {
                                show_running_preview(hwnd, app, i);
                            }
                        }
                        dock::ItemRole::Pinned => {
                            // A pinned app that's running activates its window(s) instead
                            // of launching a duplicate (macOS-style). Right-click → 打开
                            // still launches a fresh instance.
                            if activate_item_windows(&app.dock.items[i]) {
                                window_preview::hide();
                                app.dock.bump(i);
                                app.animating = true;
                            } else if activate_live_pinned_window(hwnd, app, i) {
                            } else if let Some(path) = path {
                                if kind == content::ContentKind::Folder {
                                    let (acx, atop) = anchor_above(hwnd, app, i);
                                    folder_stack::show(hwnd, &path, acx, atop);
                                    app.dock.bump(i);
                                    app.animating = true;
                                } else {
                                    match open_content(&path) {
                                        Ok(()) => {
                                            app.dock.bump(i);
                                            app.animating = true;
                                        }
                                        Err(error) => show_error(hwnd, "打开内容失败", error),
                                    }
                                }
                            }
                        }
                        dock::ItemRole::Control => {
                            // Anchor the panel above this button's center, then toggle.
                            let (acx, atop) = anchor_above(hwnd, app, i);
                            control_center::toggle(hwnd, acx, atop);
                            app.dock.bump(i);
                            app.animating = true;
                        }
                        dock::ItemRole::Drawer => {
                            // Anchor the drawer above this button's center, then toggle.
                            let (acx, atop) = anchor_above(hwnd, app, i);
                            drawer::toggle(hwnd, acx, atop);
                            app.dock.bump(i);
                            app.animating = true;
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
                    let path = app.dock.items[i].path.clone();
                    // All open windows behind this dock slot (a running app can have many).
                    let windows: Vec<isize> =
                        app.dock.items[i].windows.iter().map(|w| w.hwnd).collect();
                    match role {
                        dock::ItemRole::Running => {
                            show_window_menu(hwnd, app, &windows);
                        }
                        dock::ItemRole::Pinned => {
                            if let Some(path) = path {
                                show_pinned_menu(hwnd, app, &path, &windows);
                            }
                        }
                        dock::ItemRole::Start => run_tray_menu(hwnd, app),
                        dock::ItemRole::Control => {}
                        dock::ItemRole::Drawer => {}
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
            WM_HOTKEY if wparam.0 == ID_SEARCH_HOTKEY as usize => {
                command_palette::toggle(hwnd);
                LRESULT(0)
            }
            WM_SHOW_EXISTING => {
                if app.window_hidden {
                    set_window_hidden(hwnd, app, false);
                }
                if let Err(error) = set_expanded(hwnd, app, true) {
                    show_error(hwnd, "Show existing FeatherDock failed", error);
                }
                app.dock.reveal_target = 1.0;
                app.animating = true;
                raise_dock_topmost(hwnd);
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
                // Cheap on every event: re-check whether a fullscreen (borderless) app or a
                // maximized (captioned) window is in front and retract / restore the dock.
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let fullscreen = windows_list::is_fullscreen_present(monitor);
                let maximized = windows_list::is_maximized_present(monitor);
                if fullscreen != app.fullscreen_active || maximized != app.maximized_active {
                    app.fullscreen_active = fullscreen;
                    app.maximized_active = maximized;
                    // No longer fully suppressed (game closed, or fullscreen→maximized):
                    // re-show the fully-hidden window so the reveal/strip can take over.
                    if !fullscreen_suppressed(app) && app.window_hidden {
                        set_window_hidden(hwnd, app, false);
                    }
                    // Fullscreen-suppressed -> force out (target 0) regardless of any stale
                    // hover; otherwise honour the cursor (maximized stays hover-revealable).
                    let target = if app.dock.cursor_x.is_some() && !fullscreen_suppressed(app) {
                        1.0
                    } else {
                        resting_target(app)
                    };
                    app.dock.reveal_target = target;
                    app.animating = true;
                }
                // A fullscreen app just left the foreground: apply any dock rebuild we
                // deferred while it owned the screen (we keep the dock fully passive during
                // fullscreen so we never kick a game out of exclusive mode).
                if app.pending_rebuild && !fullscreen {
                    app.pending_rebuild = false;
                    if let Err(error) = reload_app(hwnd, app) {
                        show_error(hwnd, "刷新窗口列表失败", error);
                    }
                    if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                        && app.taskbar_invoked_at.is_none()
                    {
                        taskbar::reassert_hidden();
                    }
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
                    // Never re-stack the dock (reload_with does SetWindowPos) while a
                    // fullscreen app owns the screen — it kicks an exclusive-fullscreen
                    // game to the desktop. Defer the rebuild until the game exits.
                    if fullscreen_app_present(hwnd) {
                        app.pending_rebuild = true;
                    } else if let Err(error) = reload_with(hwnd, app, &running) {
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
                        raise_dock_topmost(hwnd); // taskbar gone → dock back on top
                    } else if app.settings.taskbar_mode != settings::TaskbarMode::Hidden {
                        let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                        app.taskbar_invoked_at = None;
                        raise_dock_topmost(hwnd);
                    }
                } else {
                    let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                }
                LRESULT(0)
            }
            settings_window::WM_SETTINGS_CHANGED => {
                // The settings window changed dock_mode / fullscreen / taskbar mode and
                // applied the taskbar mode itself; re-read and ease to the new resting
                // state. If it switched us to hidden, defeat the auto-hide entry slide
                // and grow maximized windows into the freed work area.
                app.settings = settings::load();
                app.dock.theme = app.settings.theme;
                if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                    && app.taskbar_invoked_at.is_none()
                {
                    enter_hidden(hwnd, app);
                }
                // Re-assert the desktop-icon visibility to match the (possibly toggled)
                // setting — idempotent, in case the settings window didn't apply it.
                desktop_icons::set_hidden(app.settings.hide_desktop_icons);
                // Settings can change which system effects need abnormal-exit recovery.
                reconcile_watchdog(app);
                let target = if app.dock.cursor_x.is_some() {
                    1.0
                } else {
                    resting_target(app)
                };
                app.dock.reveal_target = target;
                app.animating = true;
                LRESULT(0)
            }
            window_preview::WM_PREVIEW_CLOSED => {
                // The hover preview was dismissed by the user. The dock's own WM_MOUSELEAVE
                // already fired (and kept it revealed) when the cursor crossed onto the
                // preview; now that the preview is gone and the cursor isn't back on the
                // dock, ease to the resting state so an auto-hide dock actually retracts.
                if app.dock.cursor_x.is_none() {
                    app.dock.reveal_target = resting_target(app);
                    app.animating = true;
                }
                LRESULT(0)
            }
            settings_window::WM_PINS_CHANGED => {
                // Pinned apps (or the drawer-button toggle) changed in the settings window.
                // Re-read settings FIRST so `compose_items` sees the current
                // `drawer_enabled` (this path doesn't go through WM_SETTINGS_CHANGED), then
                // rebuild the dock.
                app.settings = settings::load();
                app.dock.theme = app.settings.theme;
                if let Err(error) = reload_app(hwnd, app) {
                    show_error(hwnd, "刷新常驻应用失败", error);
                }
                LRESULT(0)
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                // A fullscreen game switching display mode broadcasts WM_DISPLAYCHANGE.
                // Reacting now — re-stacking the dock topmost (reload_app) or reflowing
                // windows — knocks the game out of exclusive fullscreen and bounces it to
                // the desktop. Stay completely passive while a fullscreen app owns the
                // screen; rebuild once it exits (see WM_WINDOWS_CHANGED).
                if fullscreen_app_present(hwnd) {
                    app.pending_rebuild = true;
                    return LRESULT(0);
                }
                if let Err(error) = reload_app(hwnd, app) {
                    show_error(hwnd, "重新布局 Dock 失败", error);
                }
                // A display / work-area change can let the shell repaint the taskbar or
                // re-reserve its row; re-assert the transparent auto-hide state (cheap
                // and idempotent). On a resolution change also regrow maximized windows
                // into the freed work area.
                if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                    && app.taskbar_invoked_at.is_none()
                {
                    taskbar::reassert_hidden();
                    if msg == WM_DISPLAYCHANGE {
                        taskbar::reflow_maximized_windows();
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = UnregisterHotKey(hwnd, ID_SEARCH_HOTKEY);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_ref(hwnd: isize, title: &str) -> dock::RunningWindowRef {
        dock::RunningWindowRef {
            hwnd,
            title: title.to_string(),
        }
    }

    fn item_with_windows(label: &str, windows: Vec<dock::RunningWindowRef>) -> dock::DockItem {
        dock::DockItem {
            label: label.to_string(),
            glyph: "",
            color: (0.0, 0.0, 0.0),
            path: None,
            icon: None,
            kind: content::ContentKind::Application,
            role: dock::ItemRole::Running,
            hwnd: windows.first().map(|window| window.hwnd),
            group_key: None,
            windows,
        }
    }

    fn pinned_item(label: &str, path: &str) -> dock::DockItem {
        dock::DockItem {
            label: label.to_string(),
            glyph: "",
            color: (0.0, 0.0, 0.0),
            path: Some(path.to_string()),
            icon: None,
            kind: content::ContentKind::Application,
            role: dock::ItemRole::Pinned,
            hwnd: None,
            group_key: None,
            windows: Vec::new(),
        }
    }

    fn running_group(
        key: &str,
        label: &str,
        windows: Vec<windows_list::RunningWindow>,
    ) -> windows_list::RunningGroup {
        windows_list::RunningGroup {
            key: key.to_string(),
            label: label.to_string(),
            icon_path: Some(key.to_string()),
            windows,
        }
    }

    fn running_window(hwnd: isize, title: &str, exe_path: &str) -> windows_list::RunningWindow {
        windows_list::RunningWindow {
            hwnd,
            title: title.to_string(),
            exe_path: Some(exe_path.to_string()),
        }
    }

    #[test]
    fn running_signature_changes_when_window_title_changes() {
        let before = vec![running_window(
            42,
            "Loading",
            r"C:\Program Files\WeGame\browser.exe",
        )];
        let after = vec![running_window(
            42,
            "WeGame",
            r"C:\Program Files\WeGame\browser.exe",
        )];

        assert_ne!(running_sig(&before), running_sig(&after));
    }

    #[test]
    fn pinned_launcher_matches_helper_window_by_exact_title() {
        let item = pinned_item("WeGame", r"C:\Program Files\WeGame\wegame.exe");
        let group = running_group(
            r"c:\program files\wegame\browser.exe",
            "Browser",
            vec![running_window(
                42,
                "WeGame",
                r"C:\Program Files\WeGame\browser.exe",
            )],
        );

        assert!(pinned_matches_group(&item, &group));
    }

    #[test]
    fn pinned_identity_match_ignores_stale_cached_windows() {
        let group = running_group(
            r"c:\program files\wegame\browser.exe",
            "Browser",
            vec![running_window(
                42,
                "WeGame",
                r"C:\Program Files\WeGame\browser.exe",
            )],
        );

        assert!(pinned_identity_matches_group(
            "WeGame",
            Some(r"C:\Program Files\WeGame\wegame.exe"),
            &group
        ));
    }

    #[test]
    fn activation_prefers_the_window_whose_title_matches_the_app_label() {
        let item = item_with_windows(
            "WeGame",
            vec![
                running_ref(10, "WeGame Helper"),
                running_ref(20, "WeGame"),
                running_ref(30, "Settings"),
            ],
        );

        assert_eq!(activation_target(&item), Some(20));
    }

    #[test]
    fn activation_falls_back_to_the_primary_running_window() {
        let item = item_with_windows(
            "Explorer",
            vec![running_ref(10, "Downloads"), running_ref(20, "Desktop")],
        );

        assert_eq!(activation_target(&item), Some(10));
    }

    #[test]
    fn activation_prefers_foreground_window_in_same_running_group() {
        let item = item_with_windows(
            "Browser",
            vec![running_ref(10, "Docs"), running_ref(20, "Mail")],
        );

        assert_eq!(activation_order_with_foreground(&item, Some(20))[0], 20);
    }

    #[test]
    fn activation_prefers_live_foreground_window_owned_by_same_app() {
        let item = item_with_windows(
            "Explorer",
            vec![running_ref(10, "Downloads"), running_ref(20, "Desktop")],
        );

        assert_eq!(
            activation_order_with_foreground_owned(&item, Some(99), true)[0],
            99
        );
    }
}
