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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningWindow {
    pub hwnd: isize,
    pub title: String,
    pub exe_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningGroup {
    pub key: String,
    pub label: String,
    pub icon_path: Option<String>,
    pub windows: Vec<RunningWindow>,
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
                exe_path: process_exe_path(hwnd),
            });
        }
    }
    BOOL(1)
}

pub fn group_by_application(running: &[RunningWindow]) -> Vec<RunningGroup> {
    let mut groups: Vec<RunningGroup> = Vec::new();
    for window in running {
        let key = window
            .exe_path
            .as_deref()
            .map(normalize_app_key)
            .unwrap_or_else(|| format!("hwnd:{}", window.hwnd));
        if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
            group.windows.push(window.clone());
            continue;
        }

        let label = window
            .exe_path
            .as_deref()
            .map(app_label_from_path)
            .unwrap_or_else(|| window.title.clone());
        groups.push(RunningGroup {
            key,
            label,
            icon_path: window.exe_path.clone(),
            windows: vec![window.clone()],
        });
    }

    for group in &mut groups {
        group.windows.sort_by_key(|window| window.hwnd);
    }
    groups.sort_by_key(|group| group.windows.first().map(|window| window.hwnd).unwrap_or(0));
    groups
}

fn normalize_app_key(path: &str) -> String {
    path.trim().trim_matches('"').to_ascii_lowercase()
}

fn app_label_from_path(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "App".to_string());
    match stem.to_ascii_lowercase().as_str() {
        "msedge" => "Edge".to_string(),
        "chrome" => "Chrome".to_string(),
        "code" => "VS Code".to_string(),
        "explorer" => "Explorer".to_string(),
        _ => title_case_identifier(&stem),
    }
}

fn title_case_identifier(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut new_word = true;
    for ch in value.chars() {
        if matches!(ch, '-' | '_' | '.') {
            out.push(' ');
            new_word = true;
        } else if new_word {
            out.extend(ch.to_uppercase());
            new_word = false;
        } else {
            out.push(ch);
        }
    }
    out
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
    let covers_monitor = rect.left <= info.rcMonitor.left
        && rect.top <= info.rcMonitor.top
        && rect.right >= info.rcMonitor.right
        && rect.bottom >= info.rcMonitor.bottom;
    // Hiding the taskbar frees the work area, so a normal *maximized* window now spans
    // the whole monitor too — but it isn't fullscreen and must NOT retract an always-
    // resident dock. A maximized window keeps its title-bar caption; true fullscreen
    // apps (games, video) are borderless. Only borderless monitor-fillers count.
    let style = GetWindowLongPtrW(foreground, GWL_STYLE) as u32;
    let has_caption = (style & WS_CAPTION.0) == WS_CAPTION.0;
    fullscreen_should_retract_dock(covers_monitor, has_caption)
}

/// A foreground window retracts an always-resident dock only when it is *true*
/// fullscreen: it fills the monitor AND is borderless (no title-bar caption). A normal
/// maximized window fills the monitor now that the work area is freed, but keeps its
/// caption, so it must not hide the dock.
fn fullscreen_should_retract_dock(covers_monitor: bool, has_caption: bool) -> bool {
    covers_monitor && !has_caption
}

/// True if a normal *maximized* window — zoomed AND with a title-bar caption — is in
/// front on the dock's monitor. This is the cue to retract the dock to its reveal strip
/// (so it doesn't cover the window) while STILL letting a bottom-edge hover summon it.
/// Distinct from `is_fullscreen_present`: a borderless monitor-filler (game/video) is
/// fullscreen — caption-less — and gets fully hidden with no hover, not merely retracted.
pub unsafe fn is_maximized_present(dock_monitor: HMONITOR) -> bool {
    let foreground = GetForegroundWindow();
    if foreground.is_invalid() || is_shell_class(foreground) {
        return false;
    }
    let fg_monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
    if fg_monitor != dock_monitor {
        return false;
    }
    if !IsZoomed(foreground).as_bool() {
        return false; // not maximized
    }
    // A genuine maximized app window keeps its caption; a borderless zoomed monitor-filler
    // is fullscreen (handled above) and must NOT be downgraded to "maximized", or a hover
    // could summon the dock over a game.
    let style = GetWindowLongPtrW(foreground, GWL_STYLE) as u32;
    maximized_should_retract_dock((style & WS_CAPTION.0) == WS_CAPTION.0)
}

/// A maximized foreground window retracts the dock (to a hover-revealable strip) only
/// when it is a real app window — i.e. it has a caption. (Caption-less zoomed windows are
/// fullscreen, not maximized.)
fn maximized_should_retract_dock(has_caption: bool) -> bool {
    has_caption
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
    // Window lifecycle + foreground, plus a foreground-ONLY location change. We still do
    // NOT hook NAMECHANGE (title churn we don't display) nor unfiltered LOCATIONCHANGE
    // (window drags fire continuously). LOCATIONCHANGE is admitted only for the current
    // foreground window — so a browser/game toggling fullscreen (F11) while already in
    // front, which fires no foreground event, is still caught — and the callback rejects
    // everything else, so a static idle screen produces no events and 0% CPU.
    const RANGES: [(u32, u32); 4] = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
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
    // 1 = foreground/geometry change (only re-check fullscreen; the set is unchanged).
    let kind = match event {
        EVENT_OBJECT_CREATE
        | EVENT_OBJECT_DESTROY
        | EVENT_OBJECT_SHOW
        | EVENT_OBJECT_HIDE
        | EVENT_OBJECT_CLOAKED
        | EVENT_OBJECT_UNCLOAKED => 0usize,
        EVENT_OBJECT_LOCATIONCHANGE => {
            // A top-level move/resize only matters for fullscreen detection, and only for
            // the window actually in front (a browser/game toggling F11 while already
            // foreground). Drop everything else so dragging a background window — or an
            // idle screen — never wakes the dock.
            if hwnd != GetForegroundWindow() {
                return;
            }
            1usize
        }
        _ => 1usize, // EVENT_SYSTEM_FOREGROUND -> just re-check fullscreen
    };
    let _ = PostMessageW(
        hwnd_from(target),
        WM_WINDOWS_CHANGED,
        WPARAM(kind),
        LPARAM(0),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(hwnd: isize, title: &str, exe_path: Option<&str>) -> RunningWindow {
        RunningWindow {
            hwnd,
            title: title.to_string(),
            exe_path: exe_path.map(str::to_string),
        }
    }

    #[test]
    fn only_borderless_monitor_fillers_retract_the_dock() {
        // Borderless app filling the screen (game / video) -> fullscreen, retract.
        assert!(fullscreen_should_retract_dock(true, false));
        // Maximized normal window: fills the monitor (freed work area) but has a
        // caption -> NOT fullscreen, keep the always-resident dock.
        assert!(!fullscreen_should_retract_dock(true, true));
        // Anything not covering the whole monitor never counts.
        assert!(!fullscreen_should_retract_dock(false, false));
        assert!(!fullscreen_should_retract_dock(false, true));
    }

    #[test]
    fn only_captioned_zoomed_windows_count_as_maximized() {
        // A real maximized app window has a caption -> retract to the hover strip.
        assert!(maximized_should_retract_dock(true));
        // A borderless zoomed monitor-filler is fullscreen, NOT "maximized" — it must
        // stay on the fullscreen path (fully hidden, no hover) so it never gets a strip.
        assert!(!maximized_should_retract_dock(false));
    }

    #[test]
    fn running_windows_with_same_exe_are_grouped_for_one_dock_icon() {
        let groups = group_by_application(&[
            running(30, "Inbox - Edge", Some(r"C:\Apps\Edge\msedge.exe")),
            running(10, "Docs - Edge", Some(r"c:\apps\edge\msedge.exe")),
            running(20, "Notes", Some(r"C:\Apps\Notes\notes.exe")),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label, "Edge");
        assert_eq!(groups[0].key, r"c:\apps\edge\msedge.exe");
        assert_eq!(
            groups[0]
                .windows
                .iter()
                .map(|window| (window.hwnd, window.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(10, "Docs - Edge"), (30, "Inbox - Edge")]
        );
        assert_eq!(groups[1].label, "Notes");
        assert_eq!(groups[1].windows.len(), 1);
    }

    #[test]
    fn windows_without_process_path_do_not_collapse_together() {
        let groups =
            group_by_application(&[running(1, "Untitled", None), running(2, "Untitled", None)]);

        assert_eq!(groups.len(), 2);
        assert_ne!(groups[0].key, groups[1].key);
    }
}
