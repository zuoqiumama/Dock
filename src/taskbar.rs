//! Control the real Windows taskbar: leave it, set it to auto-hide, or fully hide
//! it. We capture the user's original auto-hide preference at startup and ALWAYS
//! restore the taskbar to visible on exit, so we never strand them without one.
//!
//! "Hidden" is the key mode, and the recipe is subtle (verified empirically on
//! Win11 26120):
//!
//! * The shell reserves a row of the desktop work area for an ALWAYS-ON-TOP taskbar
//!   and re-asserts it continuously — `SPI_SETWORKAREA` to reclaim it is overridden
//!   instantly, leaving an empty ~48px strip that breaks fullscreen. The ONLY way to
//!   free that row is to put the shell appbar into AUTO-HIDE, where it stops reserving
//!   space by construction.
//! * But an auto-hide taskbar slides into view whenever the pointer touches the
//!   bottom edge — which the user does constantly to reach the dock — and `SW_HIDE`
//!   does NOT survive that (the first reveal re-shows it). So hiding the *window* is
//!   not enough.
//! * The fix that satisfies BOTH "no reserved row" AND "never appears on hover" is to
//!   make the taskbar window itself invisible: `WS_EX_LAYERED` + a layered alpha of 0
//!   renders it fully transparent whether the OS parks it as the 2px sliver or slides
//!   it up on edge-hover, and `WS_EX_TRANSPARENT` lets clicks fall through it. Auto-
//!   hide frees the row; alpha 0 hides the pixels. No `SW_HIDE`, no polling, no work-
//!   area tug-of-war — set once, 0% idle.
//!
//! For an explicit Start-button click we make the bar opaque + interactive again and
//! pin it ALWAYS-ON-TOP so it slides fully on-screen and is usable, then drop back to
//! the transparent auto-hide state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABM_SETSTATE, ABS_ALWAYSONTOP, ABS_AUTOHIDE, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::settings::TaskbarMode;

const ABM_GETSTATE: u32 = 4;
const EX_INVISIBLE: i32 = WS_EX_LAYERED.0 as i32 | WS_EX_TRANSPARENT.0 as i32;

/// The user's auto-hide preference, captured once at startup so "Show" restores it
/// instead of forcing the taskbar always-on-top.
static ORIGINAL_AUTOHIDE: AtomicBool = AtomicBool::new(false);

/// Whether this process ever put the taskbar into a non-`Show` state (auto-hide or
/// hidden). If we never touched it, the clean-exit `restore()` is a no-op — otherwise we
/// would clobber an auto-hide preference the user changed *themselves* while we ran.
static TASKBAR_TOUCHED: AtomicBool = AtomicBool::new(false);

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

/// Capture the user's auto-hide preference once, at startup, before we change
/// anything — so "Show"/exit restore exactly what they had.
pub unsafe fn capture_original() {
    ORIGINAL_AUTOHIDE.store(current_autohide(), Ordering::Relaxed);
}

/// The user's auto-hide preference captured at startup — handed to the watchdog so it
/// can restore the real original even though it never ran `capture_original` itself.
pub fn original_autohide() -> bool {
    ORIGINAL_AUTOHIDE.load(Ordering::Relaxed)
}

/// Path of the on-disk guard marker (next to settings.toml). Its *presence* means "we
/// diverged the taskbar from the user's own state and have not cleanly restored it yet";
/// its contents record the original auto-hide preference so recovery is exact.
fn guard_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("FeatherDock")
        .join("taskbar.guard")
}

/// Drop the guard marker before we modify the taskbar.
fn mark_guarded(original_autohide: bool) {
    let path = guard_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, if original_autohide { "1" } else { "0" });
}

/// Clear the guard marker: the taskbar is back to the user's own state.
pub fn clear_guard() {
    let _ = std::fs::remove_file(guard_path());
}

/// The recorded original auto-hide preference, if a guard marker is present.
fn guarded_original() -> Option<bool> {
    let text = std::fs::read_to_string(guard_path()).ok()?;
    Some(text.trim().starts_with('1'))
}

/// If a previous run modified the taskbar and died without restoring it — power loss, or
/// both the dock AND its watchdog killed at once — the guard marker is still on disk. Put
/// the taskbar back to the recorded original and clear the marker. MUST run *before*
/// `capture_original`, so we never capture a stranded (auto-hidden + invisible) bar as
/// the new baseline.
pub unsafe fn recover_if_stranded() {
    if let Some(original_autohide) = guarded_original() {
        restore_to(original_autohide);
        clear_guard();
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
        // Auto-hide AND fully-hidden both want the shell in auto-hide state: that's
        // what frees the desktop work area (no reserved row). They differ only in
        // whether the taskbar is left visible (AutoHide) or made transparent (Hidden).
        TaskbarMode::AutoHide | TaskbarMode::Hidden => true,
    }
}

/// Run `f` over every top-level window of `class` (covers multi-monitor secondaries).
unsafe fn for_each_window_of_class(class: &str, mut f: impl FnMut(HWND)) {
    let name = wide(class);
    let mut hwnd = FindWindowExW(
        HWND::default(),
        HWND::default(),
        PCWSTR(name.as_ptr()),
        PCWSTR::null(),
    )
    .unwrap_or_default();
    while !hwnd.is_invalid() {
        f(hwnd);
        hwnd = FindWindowExW(HWND::default(), hwnd, PCWSTR(name.as_ptr()), PCWSTR::null())
            .unwrap_or_default();
    }
}

/// Show or hide every taskbar window of a class. Idempotent: only touches windows
/// whose visibility actually differs from `show`.
unsafe fn show_class(class: &str, show: bool) {
    let cmd = if show { SW_SHOW } else { SW_HIDE };
    for_each_window_of_class(class, |hwnd| {
        if IsWindowVisible(hwnd).as_bool() != show {
            let _ = ShowWindow(hwnd, cmd);
        }
    });
}

/// Make the taskbar windows fully transparent (alpha 0) and click-through, so they
/// never paint anything — even the 2px auto-hide sliver or an edge-hover slide-in —
/// and never eat clicks meant for the dock or the desktop below.
unsafe fn make_taskbar_transparent() {
    for class in ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
        for_each_window_of_class(class, |hwnd| {
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
            if ex & EX_INVISIBLE != EX_INVISIBLE {
                SetWindowLongW(hwnd, GWL_EXSTYLE, ex | EX_INVISIBLE);
            }
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        });
    }
}

/// Undo `make_taskbar_transparent`: drop the layered + click-through styles so the
/// bar paints and responds normally again.
unsafe fn make_taskbar_opaque() {
    for class in ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
        for_each_window_of_class(class, |hwnd| {
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
            if ex & EX_INVISIBLE != 0 {
                SetWindowLongW(hwnd, GWL_EXSTYLE, ex & !EX_INVISIBLE);
            }
        });
    }
}

/// Re-assert the hidden state after the shell may have changed things (display
/// change, work-area broadcast, etc.): make sure it's still auto-hide + transparent.
/// Cheap and idempotent, so it's safe to call on frequent events.
pub unsafe fn reassert_hidden() {
    if !current_autohide() {
        set_autohide(true);
    }
    make_taskbar_transparent();
}

pub fn should_reveal_for_start_invocation(mode: TaskbarMode) -> bool {
    mode == TaskbarMode::Hidden
}

pub fn should_open_start_menu_for_start_invocation(mode: TaskbarMode) -> bool {
    mode != TaskbarMode::Hidden
}

pub fn should_rehide_after_start_invocation(
    mode: TaskbarMode,
    grace_elapsed: bool,
    pointer_over_taskbar: bool,
) -> bool {
    mode == TaskbarMode::Hidden && grace_elapsed && !pointer_over_taskbar
}

/// In full-hide mode, temporarily show the real taskbar for an explicit Start-button
/// click: make it opaque + interactive and pin it ALWAYS-ON-TOP so it slides fully
/// on-screen and is usable.
pub unsafe fn reveal_for_start_invocation(mode: TaskbarMode) -> bool {
    if !should_reveal_for_start_invocation(mode) {
        return false;
    }
    make_taskbar_opaque();
    set_autohide(false);
    show_class("Shell_TrayWnd", true);
    show_class("Shell_SecondaryTrayWnd", true);
    true
}

/// Return to the hidden resting state after a Start-button reveal.
pub unsafe fn rehide_after_start_invocation(mode: TaskbarMode) {
    if mode != TaskbarMode::Hidden {
        return;
    }
    apply(TaskbarMode::Hidden);
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

/// Windows we must never "reflow" as if they were ordinary maximized app windows:
/// the shell's own surfaces, the desktop, and our dock.
fn is_shell_or_dock_window_class(class: &str) -> bool {
    is_taskbar_window_class(class)
        || matches!(
            class,
            "Progman" | "WorkerW" | "FeatherDockWindow" | "Windows.UI.Core.CoreWindow"
        )
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

/// After we free the work area (by auto-hiding the taskbar), windows that were
/// maximized while the taskbar still reserved its row stay ~48px short until
/// re-maximized. Cycle each maximized app window's maximize so it re-fits the now-full
/// screen. Called only on deliberate hidden-entry paths (startup, mode switch,
/// Explorer restart, resolution change) — never on hot/frequent events.
pub unsafe fn reflow_maximized_windows() {
    let _ = EnumWindows(Some(reflow_one), LPARAM(0));
}

unsafe extern "system" fn reflow_one(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    if IsWindowVisible(hwnd).as_bool()
        && IsZoomed(hwnd).as_bool()
        && !is_shell_or_dock_window_class(&window_class(hwnd))
    {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
    }
    BOOL(1)
}

pub unsafe fn apply(mode: TaskbarMode) {
    let original = ORIGINAL_AUTOHIDE.load(Ordering::Relaxed);
    let autohide = autohide_for_mode(mode, original);
    match mode {
        // "Show" = leave it as the user had it (respect their original auto-hide). Back
        // to their own state -> nothing for an abnormal exit to undo: clear the guard.
        TaskbarMode::Show => {
            restore_to(autohide);
            clear_guard();
        }
        // OS-managed auto-hide: freed work area, slides in (visibly) on edge-hover.
        // Diverges from the user's state, so mark the guard for crash recovery.
        TaskbarMode::AutoHide => {
            TASKBAR_TOUCHED.store(true, Ordering::Relaxed);
            mark_guarded(original);
            make_taskbar_opaque();
            set_autohide(autohide);
            show_class("Shell_TrayWnd", true);
            show_class("Shell_SecondaryTrayWnd", true);
        }
        // Fully hidden: transparent FIRST (so the auto-hide transition below never
        // flashes on screen), then auto-hide to free the work area. The bar stays
        // WS_VISIBLE but alpha-0, so it shows nothing whether parked as the sliver or
        // slid up on hover, yet reserves no row — the "pure dock" behaviour. Mark the
        // guard: stranded in this state, the user has no visible taskbar at all.
        TaskbarMode::Hidden => {
            TASKBAR_TOUCHED.store(true, Ordering::Relaxed);
            mark_guarded(original);
            make_taskbar_transparent();
            set_autohide(autohide);
        }
    }
}

/// Put the taskbar back to a usable, visible state with an explicitly supplied auto-hide
/// preference. Takes the value as an argument (rather than reading the captured static)
/// so the watchdog process — which never ran `capture_original` — and on-launch recovery
/// can both call it.
pub unsafe fn restore_to(original_autohide: bool) {
    make_taskbar_opaque();
    set_autohide(original_autohide);
    show_class("Shell_TrayWnd", true);
    show_class("Shell_SecondaryTrayWnd", true);
}

/// Restore the taskbar to the user's original state. Always call this on a clean exit.
/// No-op if this process never diverged the taskbar from "Show": in that case the bar is
/// already in whatever state the user wants, and re-applying our captured baseline would
/// undo an auto-hide toggle they flipped themselves while we were running.
pub unsafe fn restore() {
    if !TASKBAR_TOUCHED.load(Ordering::Relaxed) {
        return;
    }
    apply(TaskbarMode::Show);
}

/// The primary taskbar window, so the dock can sit just below it in z-order while
/// the user has the taskbar explicitly revealed (and operate it on top of the dock).
pub unsafe fn tray_hwnd() -> HWND {
    primary()
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
    fn hidden_start_invocation_reveals_taskbar_without_synthesizing_win_key() {
        assert!(!should_open_start_menu_for_start_invocation(
            TaskbarMode::Hidden
        ));
        assert!(should_open_start_menu_for_start_invocation(
            TaskbarMode::Show
        ));
        assert!(should_open_start_menu_for_start_invocation(
            TaskbarMode::AutoHide
        ));
    }

    #[test]
    fn hidden_and_autohide_free_the_work_area_via_shell_autohide() {
        // Both hidden and auto-hide put the shell in auto-hide state, which is what
        // reclaims the desktop work area (no reserved row). "Show" respects whatever
        // the user originally had.
        assert!(autohide_for_mode(TaskbarMode::Hidden, false));
        assert!(autohide_for_mode(TaskbarMode::Hidden, true));
        assert!(autohide_for_mode(TaskbarMode::AutoHide, false));
        assert!(autohide_for_mode(TaskbarMode::AutoHide, true));
        assert!(!autohide_for_mode(TaskbarMode::Show, false));
        assert!(autohide_for_mode(TaskbarMode::Show, true));
    }

    #[test]
    fn invisible_exstyle_is_layered_plus_click_through() {
        assert_eq!(
            EX_INVISIBLE,
            WS_EX_LAYERED.0 as i32 | WS_EX_TRANSPARENT.0 as i32
        );
        // Layered is required for an alpha of 0 to take effect; transparent makes the
        // (invisible) bar click-through so it never eats dock/desktop clicks.
        assert_ne!(EX_INVISIBLE & WS_EX_LAYERED.0 as i32, 0);
        assert_ne!(EX_INVISIBLE & WS_EX_TRANSPARENT.0 as i32, 0);
    }

    #[test]
    fn never_reflow_shell_or_dock_surfaces() {
        assert!(is_shell_or_dock_window_class("Shell_TrayWnd"));
        assert!(is_shell_or_dock_window_class("WorkerW"));
        assert!(is_shell_or_dock_window_class("FeatherDockWindow"));
        assert!(!is_shell_or_dock_window_class("Chrome_WidgetWin_1"));
    }
}
