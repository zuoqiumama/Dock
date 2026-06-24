//! Control the real Windows taskbar: leave it, set it to auto-hide, or fully hide
//! it. We capture the user's original auto-hide state at startup and ALWAYS restore
//! the taskbar to visible on exit, so we never strand them without a taskbar.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABM_SETSTATE, ABS_ALWAYSONTOP, ABS_AUTOHIDE, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::settings::TaskbarMode;

const ABM_GETSTATE: u32 = 4;

/// The user's auto-hide preference, captured once at startup so "Show" restores it
/// instead of forcing the taskbar always-on-top.
static ORIGINAL_AUTOHIDE: AtomicBool = AtomicBool::new(false);

/// The original desktop work area (left, top, right, bottom), captured at startup.
/// When we fully hide the taskbar we expand the work area to the whole monitor so
/// maximized / fullscreen apps actually fill the screen (no taskbar-sized gap), and
/// we restore this on "Show" / exit.
static ORIGINAL_WORK_AREA: [AtomicI32; 4] = [
    AtomicI32::new(0),
    AtomicI32::new(0),
    AtomicI32::new(0),
    AtomicI32::new(0),
];
static WORK_AREA_CAPTURED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RectSpec {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl RectSpec {
    fn into_rect(self) -> RECT {
        RECT {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
}

impl From<RECT> for RectSpec {
    fn from(rect: RECT) -> Self {
        RectSpec {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn primary() -> HWND {
    let class = wide("Shell_TrayWnd");
    FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()).unwrap_or_default()
}

/// Current auto-hide state of the system taskbar.
unsafe fn current_autohide() -> bool {
    let tray = primary();
    if tray.is_invalid() {
        return false;
    }
    let mut data = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: tray,
        ..Default::default()
    };
    (SHAppBarMessage(ABM_GETSTATE, &mut data) as u32 & ABS_AUTOHIDE) != 0
}

/// Capture the user's auto-hide preference AND the current work area once, at
/// startup, before we change anything — so "Show"/exit restore exactly what they had.
pub unsafe fn capture_original() {
    ORIGINAL_AUTOHIDE.store(current_autohide(), Ordering::Relaxed);
    let mut work = RECT::default();
    if SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut RECT as *mut c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok()
    {
        ORIGINAL_WORK_AREA[0].store(work.left, Ordering::Relaxed);
        ORIGINAL_WORK_AREA[1].store(work.top, Ordering::Relaxed);
        ORIGINAL_WORK_AREA[2].store(work.right, Ordering::Relaxed);
        ORIGINAL_WORK_AREA[3].store(work.bottom, Ordering::Relaxed);
        WORK_AREA_CAPTURED.store(true, Ordering::Relaxed);
    }
}

unsafe fn set_work_area(mut rect: RECT) {
    let _ = SystemParametersInfoW(
        SPI_SETWORKAREA,
        0,
        Some(&mut rect as *mut RECT as *mut c_void),
        SPIF_SENDCHANGE,
    );
}

fn reclaimed_work_area(taskbar_monitor: RectSpec) -> RectSpec {
    taskbar_monitor
}

#[cfg(test)]
fn maximized_window_target(mode: TaskbarMode, monitor: RectSpec, work_area: RectSpec) -> RectSpec {
    match mode {
        TaskbarMode::Hidden => monitor,
        TaskbarMode::Show | TaskbarMode::AutoHide => work_area,
    }
}

fn should_refresh_maximized_windows(_mode: TaskbarMode, work_area_changed: bool) -> bool {
    work_area_changed
}

unsafe fn primary_monitor_bounds() -> RectSpec {
    RectSpec {
        left: 0,
        top: 0,
        right: GetSystemMetrics(SM_CXSCREEN),
        bottom: GetSystemMetrics(SM_CYSCREEN),
    }
}

unsafe fn monitor_rects_for_window(hwnd: HWND) -> Option<(RectSpec, RectSpec)> {
    if hwnd.is_invalid() {
        return None;
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        Some((info.rcMonitor.into(), info.rcWork.into()))
    } else {
        None
    }
}

unsafe fn taskbar_monitor_bounds() -> RectSpec {
    monitor_rects_for_window(primary())
        .map(|(monitor, _)| monitor)
        .unwrap_or_else(|| primary_monitor_bounds())
}

/// Expand the taskbar monitor's work area to its full bounds (used when hiding).
unsafe fn reclaim_work_area() {
    set_work_area(reclaimed_work_area(taskbar_monitor_bounds()).into_rect());
}

unsafe fn restore_work_area() {
    if WORK_AREA_CAPTURED.load(Ordering::Relaxed) {
        set_work_area(RECT {
            left: ORIGINAL_WORK_AREA[0].load(Ordering::Relaxed),
            top: ORIGINAL_WORK_AREA[1].load(Ordering::Relaxed),
            right: ORIGINAL_WORK_AREA[2].load(Ordering::Relaxed),
            bottom: ORIGINAL_WORK_AREA[3].load(Ordering::Relaxed),
        });
    }
}

unsafe fn set_autohide(autohide: bool) {
    let tray = primary();
    if tray.is_invalid() {
        return;
    }
    let mut data = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: tray,
        lParam: LPARAM(if autohide {
            ABS_AUTOHIDE
        } else {
            ABS_ALWAYSONTOP
        } as isize),
        ..Default::default()
    };
    SHAppBarMessage(ABM_SETSTATE, &mut data);
}

fn autohide_for_mode(mode: TaskbarMode, original_autohide: bool) -> bool {
    match mode {
        TaskbarMode::Show => original_autohide,
        TaskbarMode::AutoHide | TaskbarMode::Hidden => true,
    }
}

fn should_disable_autohide_for_start_invocation(_mode: TaskbarMode) -> bool {
    false
}

/// Show or hide every top-level window of a class (covers multi-monitor secondaries).
unsafe fn show_class(class: &str, show: bool) {
    let cmd = if show { SW_SHOW } else { SW_HIDE };
    let name = wide(class);
    let mut hwnd = FindWindowExW(
        HWND::default(),
        HWND::default(),
        PCWSTR(name.as_ptr()),
        PCWSTR::null(),
    )
    .unwrap_or_default();
    while !hwnd.is_invalid() {
        let _ = ShowWindow(hwnd, cmd);
        hwnd = FindWindowExW(HWND::default(), hwnd, PCWSTR(name.as_ptr()), PCWSTR::null())
            .unwrap_or_default();
    }
}

pub fn should_reveal_for_start_invocation(mode: TaskbarMode) -> bool {
    mode == TaskbarMode::Hidden
}

pub fn should_rehide_after_start_invocation(
    mode: TaskbarMode,
    grace_elapsed: bool,
    pointer_over_taskbar: bool,
) -> bool {
    mode == TaskbarMode::Hidden && grace_elapsed && !pointer_over_taskbar
}

/// In full-hide mode, temporarily show the real taskbar for an explicit Start
/// button click. We keep the reclaimed work area in place, so this behaves like
/// an overlay and does not resize maximized windows.
pub unsafe fn reveal_for_start_invocation(mode: TaskbarMode) -> bool {
    if !should_reveal_for_start_invocation(mode) {
        return false;
    }
    show_class("Shell_TrayWnd", true);
    show_class("Shell_SecondaryTrayWnd", true);
    if should_disable_autohide_for_start_invocation(mode) {
        set_autohide(false);
    }
    true
}

pub unsafe fn rehide_after_start_invocation(mode: TaskbarMode) {
    if mode != TaskbarMode::Hidden {
        return;
    }
    set_autohide(autohide_for_mode(
        mode,
        ORIGINAL_AUTOHIDE.load(Ordering::Relaxed),
    ));
    show_class("Shell_TrayWnd", false);
    show_class("Shell_SecondaryTrayWnd", false);
}

unsafe fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 96];
    let len = GetClassNameW(hwnd, &mut buf);
    if len == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

fn is_taskbar_window_class(class: &str) -> bool {
    matches!(
        class,
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "TrayNotifyWnd"
            | "TrayClockWClass"
            | "MSTaskSwWClass"
            | "MSTaskListWClass"
            | "ReBarWindow32"
            | "Windows.UI.Composition.DesktopWindowContentBridge"
    )
}

fn is_shell_or_dock_window_class(class: &str) -> bool {
    is_taskbar_window_class(class)
        || matches!(
            class,
            "Progman" | "WorkerW" | "FeatherDockWindow" | "Windows.UI.Core.CoreWindow"
        )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaximizedWindowRefresh {
    ReapplyMaximize,
    Skip,
}

fn maximized_window_refresh_action(
    visible: bool,
    zoomed: bool,
    class: &str,
) -> MaximizedWindowRefresh {
    if visible && zoomed && !is_shell_or_dock_window_class(class) {
        MaximizedWindowRefresh::ReapplyMaximize
    } else {
        MaximizedWindowRefresh::Skip
    }
}

unsafe fn refresh_maximized_windows(_mode: TaskbarMode) {
    let _ = EnumWindows(Some(refresh_maximized_window), LPARAM(0));
}

unsafe extern "system" fn refresh_maximized_window(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let action = maximized_window_refresh_action(
        IsWindowVisible(hwnd).as_bool(),
        IsZoomed(hwnd).as_bool(),
        &window_class(hwnd),
    );
    if action == MaximizedWindowRefresh::ReapplyMaximize {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
    }
    BOOL(1)
}

pub unsafe fn pointer_over_taskbar() -> bool {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return false;
    }
    let mut hwnd = WindowFromPoint(pt);
    for _ in 0..8 {
        if hwnd.is_invalid() {
            return false;
        }
        if is_taskbar_window_class(&window_class(hwnd)) {
            return true;
        }
        hwnd = GetParent(hwnd).unwrap_or_default();
    }
    false
}

pub unsafe fn apply(mode: TaskbarMode) {
    let work_area_changed = true;
    match mode {
        // "Show" = leave it as the user had it (respect their original auto-hide).
        TaskbarMode::Show => {
            restore_work_area();
            show_class("Shell_TrayWnd", true);
            show_class("Shell_SecondaryTrayWnd", true);
            set_autohide(autohide_for_mode(
                mode,
                ORIGINAL_AUTOHIDE.load(Ordering::Relaxed),
            ));
            if should_refresh_maximized_windows(mode, work_area_changed) {
                refresh_maximized_windows(mode);
            }
        }
        // OS-managed auto-hide already reclaims the work area for maximized windows.
        TaskbarMode::AutoHide => {
            restore_work_area();
            show_class("Shell_TrayWnd", true);
            show_class("Shell_SecondaryTrayWnd", true);
            set_autohide(autohide_for_mode(
                mode,
                ORIGINAL_AUTOHIDE.load(Ordering::Relaxed),
            ));
            if should_refresh_maximized_windows(mode, work_area_changed) {
                refresh_maximized_windows(mode);
            }
        }
        // Fully hidden: reclaim the work area so apps fill the whole screen (no gap).
        TaskbarMode::Hidden => {
            set_autohide(autohide_for_mode(
                mode,
                ORIGINAL_AUTOHIDE.load(Ordering::Relaxed),
            ));
            show_class("Shell_TrayWnd", false);
            show_class("Shell_SecondaryTrayWnd", false);
            reclaim_work_area();
            if should_refresh_maximized_windows(mode, work_area_changed) {
                refresh_maximized_windows(mode);
            }
        }
    }
}

/// Restore the taskbar to the user's original state. Always call this on exit.
pub unsafe fn restore() {
    apply(TaskbarMode::Show);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_mode_reveals_taskbar_for_explicit_start_invocation() {
        assert!(should_reveal_for_start_invocation(TaskbarMode::Hidden));
        assert!(!should_reveal_for_start_invocation(TaskbarMode::Show));
        assert!(!should_reveal_for_start_invocation(TaskbarMode::AutoHide));
    }

    #[test]
    fn invoked_taskbar_rehides_only_after_grace_and_when_not_in_use() {
        assert!(should_rehide_after_start_invocation(
            TaskbarMode::Hidden,
            true,
            false
        ));
        assert!(!should_rehide_after_start_invocation(
            TaskbarMode::Hidden,
            false,
            false
        ));
        assert!(!should_rehide_after_start_invocation(
            TaskbarMode::Hidden,
            true,
            true
        ));
        assert!(!should_rehide_after_start_invocation(
            TaskbarMode::Show,
            true,
            false
        ));
    }

    #[test]
    fn hidden_mode_reclaims_the_taskbar_monitor_full_bounds() {
        let monitor = RectSpec {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1600,
        };

        assert_eq!(reclaimed_work_area(monitor), monitor);
    }

    #[test]
    fn hidden_mode_refreshes_maximized_windows_to_monitor_not_old_work_area() {
        let monitor = RectSpec {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1600,
        };
        let old_work_area = RectSpec {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1560,
        };

        assert_eq!(
            maximized_window_target(TaskbarMode::Hidden, monitor, old_work_area),
            monitor
        );
        assert_eq!(
            maximized_window_target(TaskbarMode::Show, monitor, old_work_area),
            old_work_area
        );
    }

    #[test]
    fn start_invocation_rehide_does_not_refresh_maximized_windows() {
        assert!(should_refresh_maximized_windows(TaskbarMode::Hidden, true));
        assert!(!should_refresh_maximized_windows(
            TaskbarMode::Hidden,
            false
        ));
    }

    #[test]
    fn maximized_window_refresh_uses_system_maximize_cycle() {
        assert_eq!(
            maximized_window_refresh_action(true, true, "Chrome_WidgetWin_1"),
            MaximizedWindowRefresh::ReapplyMaximize
        );
        assert_eq!(
            maximized_window_refresh_action(true, false, "Chrome_WidgetWin_1"),
            MaximizedWindowRefresh::Skip
        );
        assert_eq!(
            maximized_window_refresh_action(true, true, "Shell_TrayWnd"),
            MaximizedWindowRefresh::Skip
        );
    }

    #[test]
    fn hidden_mode_keeps_system_autohide_enabled_to_release_work_area() {
        assert!(autohide_for_mode(TaskbarMode::Hidden, false));
        assert!(autohide_for_mode(TaskbarMode::AutoHide, false));
        assert!(!autohide_for_mode(TaskbarMode::Show, false));
        assert!(autohide_for_mode(TaskbarMode::Show, true));
    }

    #[test]
    fn explicit_start_invocation_does_not_disable_autohide() {
        assert!(!should_disable_autohide_for_start_invocation(
            TaskbarMode::Hidden
        ));
        assert!(!should_disable_autohide_for_start_invocation(
            TaskbarMode::Show
        ));
    }
}
