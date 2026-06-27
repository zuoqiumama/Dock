//! Hide / show the Windows desktop icons — the same toggle Explorer's right-click
//! "View ▸ Show desktop icons" flips. We locate the desktop's icon `SysListView32`
//! and `ShowWindow` it; nothing else on the desktop is touched. Reversible,
//! non-destructive, no registry writes. FeatherDock restores it on exit so the user
//! is never left with a blank desktop.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Hide (`true`) or re-show (`false`) the desktop icons. Idempotent and safe to call
/// repeatedly; a no-op if the desktop view can't be found (e.g. Explorer not running).
pub unsafe fn set_hidden(hidden: bool) {
    let view = desktop_list_view();
    if view.is_invalid() {
        return;
    }
    let cmd = if hidden { SW_HIDE } else { SW_SHOW };
    if IsWindowVisible(view).as_bool() == hidden {
        let _ = ShowWindow(view, cmd);
    }
}

/// The desktop's icon `SysListView32`. It lives under `Progman ▸ SHELLDLL_DefView`,
/// or — when a wallpaper slideshow / "active desktop" is on — under a top-level
/// `WorkerW ▸ SHELLDLL_DefView`. Returns an invalid HWND if neither is found.
unsafe fn desktop_list_view() -> HWND {
    let progman = FindWindowW(PCWSTR(wide("Progman").as_ptr()), PCWSTR::null()).unwrap_or_default();
    let mut def_view = child(progman, "SHELLDLL_DefView");
    if def_view.is_invalid() {
        def_view = worker_def_view();
    }
    if def_view.is_invalid() {
        return HWND::default();
    }
    child(def_view, "SysListView32")
}

/// First child of `parent` of the given window class (invalid HWND if none).
unsafe fn child(parent: HWND, class: &str) -> HWND {
    if parent.is_invalid() {
        return HWND::default();
    }
    let name = wide(class);
    FindWindowExW(
        parent,
        HWND::default(),
        PCWSTR(name.as_ptr()),
        PCWSTR::null(),
    )
    .unwrap_or_default()
}

/// Walk every top-level `WorkerW` looking for the one that hosts `SHELLDLL_DefView`.
unsafe fn worker_def_view() -> HWND {
    let class = wide("WorkerW");
    let mut hwnd = FindWindowExW(
        HWND::default(),
        HWND::default(),
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
    )
    .unwrap_or_default();
    while !hwnd.is_invalid() {
        let def_view = child(hwnd, "SHELLDLL_DefView");
        if !def_view.is_invalid() {
            return def_view;
        }
        hwnd = FindWindowExW(
            HWND::default(),
            hwnd,
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
        )
        .unwrap_or_default();
    }
    HWND::default()
}
