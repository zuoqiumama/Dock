//! Live list of "alt-tab" windows — the apps currently open on the taskbar.
//!
//! Updated ENTIRELY via WinEvent hooks (create/destroy/show/hide/foreground/
//! name-change), never by polling. Idle = no hooks fire = 0% CPU, preserving the
//! project's core promise. On a real change we post one message to the dock and
//! it re-enumerates + re-renders a single frame.

use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::PWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the dock window whenever the set of open windows may have changed.
pub const WM_WINDOWS_CHANGED: u32 = WM_APP + 0x21;

const VK_LWIN: VIRTUAL_KEY = VIRTUAL_KEY(0x5B);
const OBJID_WINDOW: i32 = 0;
const GCLP_HICON: i32 = -14;

/// Target dock window the hook callback posts to (set in `install_hooks`).
static DOCK_HWND: AtomicIsize = AtomicIsize::new(0);

pub struct RunningWindow {
    pub hwnd: isize,
    pub title: String,
}

fn hwnd_from(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// Enumerate open windows, sorted by handle for a stable left-to-right order
/// (so icons don't reshuffle every time focus changes).
pub unsafe fn enumerate_sorted() -> Vec<RunningWindow> {
    let mut list: Vec<RunningWindow> = Vec::new();
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut list as *mut _ as isize));
    list.sort_by_key(|w| w.hwnd);
    list
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<RunningWindow>);
    if is_alt_tab_window(hwnd) {
        let title = window_title(hwnd);
        if !title.is_empty() {
            list.push(RunningWindow {
                hwnd: hwnd.0 as isize,
                title,
            });
        }
    }
    BOOL(1)
}

/// Practical "does this belong on the taskbar?" filter: visible, top-level
/// (unowned unless APPWINDOW), not a tool window, not cloaked, has a title, and
/// not a known shell surface (desktop, tray, our own dock).
unsafe fn is_alt_tab_window(hwnd: HWND) -> bool {
    if !IsWindowVisible(hwnd).as_bool() {
        return false;
    }
    let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
    let is_app = (ex & WS_EX_APPWINDOW).0 != 0;
    let is_tool = (ex & WS_EX_TOOLWINDOW).0 != 0;
    // Owned windows (dialogs/popups) stay off the bar unless they force APPWINDOW.
    let has_owner = GetWindow(hwnd, GW_OWNER)
        .map(|owner| !owner.0.is_null())
        .unwrap_or(false);
    if !is_app && has_owner {
        return false;
    }
    if is_tool && !is_app {
        return false;
    }
    if GetWindowTextLengthW(hwnd) == 0 {
        return false;
    }
    if is_shell_class(hwnd) {
        return false;
    }
    // DwmGetWindowAttribute is the priciest check — do it last, only on survivors.
    !is_cloaked(hwnd)
}

unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let ok = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut c_void,
        std::mem::size_of::<u32>() as u32,
    );
    ok.is_ok() && cloaked != 0
}

unsafe fn is_shell_class(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut buf);
    if len == 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buf[..len as usize]);
    matches!(
        class.as_str(),
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "FeatherDockWindow"
            | "Windows.UI.Core.CoreWindow"
    )
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let written = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..written as usize])
}

/// Icon for a running window: ask the window (WM_GETICON), then fall back to its
/// window-class icon. The returned HICON is owned by the target app — do NOT free.
pub unsafe fn window_icon(hwnd: HWND) -> Option<HICON> {
    for kind in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
        let mut result: usize = 0;
        let ok = SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(kind as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            120,
            Some(&mut result),
        );
        if ok.0 != 0 && result != 0 {
            return Some(HICON(result as *mut c_void));
        }
    }
    let class_icon = GetClassLongPtrW(hwnd, GET_CLASS_LONG_INDEX(GCLP_HICON));
    if class_icon != 0 {
        Some(HICON(class_icon as *mut c_void))
    } else {
        None
    }
}

/// Full path to the executable backing a window's process, so we can extract a
/// crisp full-resolution app icon (sharper than the window's own small HICON).
pub unsafe fn process_exe_path(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return None;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = vec![0u16; 512];
    let mut len = buf.len() as u32;
    let result = QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_WIN32,
        PWSTR(buf.as_mut_ptr()),
        &mut len,
    );
    let _ = CloseHandle(process);
    result.ok()?;
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

pub unsafe fn process_exe_path_for_window(raw: isize) -> Option<String> {
    process_exe_path(hwnd_from(raw))
}

/// True if a genuinely fullscreen app (covering the whole monitor, not just the
/// work area) is in front on the dock's monitor — the cue to retract the dock.
/// A merely *maximized* window covers only the work area, so it doesn't count.
pub unsafe fn is_fullscreen_present(dock_monitor: HMONITOR) -> bool {
    let foreground = GetForegroundWindow();
    if foreground.is_invalid() || is_shell_class(foreground) {
        return false;
    }
    let fg_monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
    if fg_monitor != dock_monitor {
        return false; // fullscreen on another screen shouldn't hide our dock
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(fg_monitor, &mut info).as_bool() {
        return false;
    }
    let mut rect = RECT::default();
    if GetWindowRect(foreground, &mut rect).is_err() {
        return false;
    }
    rect.left <= info.rcMonitor.left
        && rect.top <= info.rcMonitor.top
        && rect.right >= info.rcMonitor.right
        && rect.bottom >= info.rcMonitor.bottom
}

pub unsafe fn close_window(raw: isize) {
    let _ = PostMessageW(hwnd_from(raw), WM_CLOSE, WPARAM(0), LPARAM(0));
}

pub unsafe fn minimize_window(raw: isize) {
    let _ = ShowWindow(hwnd_from(raw), SW_MINIMIZE);
}

pub unsafe fn toggle_maximize(raw: isize) {
    let hwnd = hwnd_from(raw);
    if IsZoomed(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    } else {
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
    }
}

/// Bring a window to the foreground, or minimize it if it's already in front
/// (click-to-toggle, like the taskbar). Uses AttachThreadInput to defeat the
/// foreground-lock that would otherwise just flash the taskbar button.
pub unsafe fn activate(raw: isize) {
    let hwnd = hwnd_from(raw);
    if IsIconic(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        force_foreground(hwnd);
        return;
    }
    if GetForegroundWindow() == hwnd {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
        return;
    }
    force_foreground(hwnd);
}

unsafe fn force_foreground(hwnd: HWND) {
    let foreground = GetForegroundWindow();
    let our_thread = GetCurrentThreadId();
    let target_thread = GetWindowThreadProcessId(foreground, None);
    let attached = foreground != hwnd
        && target_thread != 0
        && AttachThreadInput(our_thread, target_thread, BOOL(1)).as_bool();
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    if attached {
        let _ = AttachThreadInput(our_thread, target_thread, BOOL(0));
    }
}

/// Open the Windows Start menu by synthesizing a tap of the Win key.
pub unsafe fn open_start_menu() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_LWIN,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

/// Install out-of-context WinEvent hooks. They run on our UI thread's message
/// loop, so they fire only while we pump messages and never spin a background
/// thread. SKIPOWNPROCESS keeps our own window's events from looping back.
pub unsafe fn install_hooks(dock: HWND) -> Vec<HWINEVENTHOOK> {
    DOCK_HWND.store(dock.0 as isize, Ordering::Relaxed);
    // Only window lifecycle + foreground. We deliberately do NOT hook NAMECHANGE or
    // LOCATIONCHANGE: those fire continuously (terminals/media-player titles, window
    // drags) and we display neither titles nor positions — hooking them would burn
    // idle CPU for nothing. Fullscreen is detected on foreground change.
    const RANGES: [(u32, u32); 3] = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    RANGES
        .iter()
        .filter_map(|&(min, max)| {
            let hook = SetWinEventHook(
                min,
                max,
                HMODULE::default(),
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            (!hook.is_invalid()).then_some(hook)
        })
        .collect()
}

pub unsafe fn remove_hooks(hooks: Vec<HWINEVENTHOOK>) {
    for hook in hooks {
        let _ = UnhookWinEvent(hook);
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Only top-level window events; ignore child controls and caret/cursor noise.
    if id_object != OBJID_WINDOW || id_child != 0 || hwnd.is_invalid() {
        return;
    }
    let target = DOCK_HWND.load(Ordering::Relaxed);
    if target == 0 {
        return;
    }
    // wParam classifies the event: 0 = the window set may have changed (re-scan),
    // 1 = foreground change (only re-check fullscreen; the set is unchanged).
    let kind = match event {
        EVENT_OBJECT_CREATE
        | EVENT_OBJECT_DESTROY
        | EVENT_OBJECT_SHOW
        | EVENT_OBJECT_HIDE
        | EVENT_OBJECT_CLOAKED
        | EVENT_OBJECT_UNCLOAKED => 0usize,
        _ => 1usize, // EVENT_SYSTEM_FOREGROUND -> just re-check fullscreen
    };
    let _ = PostMessageW(
        hwnd_from(target),
        WM_WINDOWS_CHANGED,
        WPARAM(kind),
        LPARAM(0),
    );
}
