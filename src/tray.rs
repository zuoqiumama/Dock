//! Notification-area icon and the dock's management menu.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::autostart;

pub const WM_TRAY: u32 = WM_APP + 1;
pub const ID_EXIT: usize = 1001;
pub const ID_AUTOSTART: usize = 1002;
pub const ID_ADD: usize = 1003;
pub const ID_ADD_FOLDER: usize = 1004;
pub const ID_SETTINGS: usize = 1005;
pub const ID_TOGGLE_DOCK_MODE: usize = 1006;
// Keep retrying if Explorer is still starting or restarting when NIM_ADD runs.
pub const TIMER_TRAY_RETRY: usize = 1007;
const TRAY_RETRY_MS: u32 = 1000;
const TRAY_ICON_ID: u32 = 1;
// shellapi.h defines this expression as a macro, omitted by windows 0.58.
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

pub struct Tray {
    nid: NOTIFYICONDATAW,
    taskbar_created: u32,
}

impl Tray {
    pub unsafe fn new(hwnd: HWND) -> Tray {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: crate::app_icon::load(),
            ..Default::default()
        };
        let tip: Vec<u16> = "FeatherDock".encode_utf16().collect();
        for (i, c) in tip.iter().enumerate().take(127) {
            nid.szTip[i] = *c;
        }
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let tray = Tray {
            nid,
            taskbar_created,
        };
        if let Err(error) = tray.restore() {
            crate::error_log::write("添加托盘图标失败", &error);
        }
        tray
    }

    pub unsafe fn restore(&self) -> Result<()> {
        // Explorer can reject the initial add while its notification area is
        // unavailable. Do not leave the user without settings until a restart.
        // A duplicate TaskbarCreated notification may also reach an existing
        // icon, in which case MODIFY is enough to refresh it.
        let (added, add_error) = notify_icon(NIM_ADD, &self.nid);
        if !added.as_bool() {
            let (modified, modify_error) = notify_icon(NIM_MODIFY, &self.nid);
            if !modified.as_bool() {
                let code = if modify_error.0 != 0 {
                    HRESULT::from_win32(modify_error.0)
                } else if add_error.0 != 0 {
                    HRESULT::from_win32(add_error.0)
                } else {
                    E_FAIL
                };
                let error = Error::new(
                    code,
                    format!(
                        "Shell_NotifyIconW rejected registration: NIM_ADD BOOL={} GetLastError={}; NIM_MODIFY BOOL={} GetLastError={}; hwnd={:?} hIcon={:?} uID={}. LastError is advisory; zero does not mean success.",
                        added.0, add_error.0, modified.0, modify_error.0,
                        self.nid.hWnd, self.nid.hIcon, self.nid.uID
                    ),
                );
                crate::error_log::write("托盘注册失败（将重试）", &error);
                SetTimer(self.nid.hWnd, TIMER_TRAY_RETRY, TRAY_RETRY_MS, None);
                return Err(error);
            }
        }
        let _ = KillTimer(self.nid.hWnd, TIMER_TRAY_RETRY);
        let mut versioned = self.nid;
        versioned.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // The icon is already present even if an older shell refuses version 4.
        // Its legacy callbacks are accepted by is_menu_event as well.
        let (version_set, version_error) = notify_icon(NIM_SETVERSION, &versioned);
        if !version_set.as_bool() {
            let code = if version_error.0 == 0 {
                E_FAIL
            } else {
                HRESULT::from_win32(version_error.0)
            };
            crate::error_log::write(
                "设置托盘回调版本失败",
                &Error::new(
                    code,
                    format!(
                        "NIM_SETVERSION BOOL={} GetLastError={} (advisory)",
                        version_set.0, version_error.0
                    ),
                ),
            );
        }
        Ok(())
    }

    pub fn taskbar_created_message(&self) -> u32 {
        self.taskbar_created
    }

    pub unsafe fn remove(&self) {
        let _ = KillTimer(self.nid.hWnd, TIMER_TRAY_RETRY);
        let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
    }

    /// Version 4 puts the event in LOWORD(lParam) and the icon ID in HIWORD.
    /// It reports context-menu and keyboard selection events instead of only
    /// the legacy mouse-up messages.
    pub fn is_menu_event(lparam: LPARAM) -> bool {
        let event = (lparam.0 & 0xffff) as u32;
        match ((lparam.0 >> 16) & 0xffff) as u32 {
            // Version 4 also sends raw mouse messages. Act only on its semantic
            // activation so one click cannot open the menu twice.
            TRAY_ICON_ID => matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT),
            0 => matches!(event, WM_RBUTTONUP | WM_LBUTTONUP),
            _ => false,
        }
    }

    /// Show the context menu and return the chosen command id (the caller acts on it).
    /// `always_mode` controls the check next to the "resident at bottom" toggle.
    pub unsafe fn show_menu(&self, hwnd: HWND, always_mode: bool) -> Option<usize> {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let Ok(menu) = CreatePopupMenu() else {
            return None;
        };
        let add: Vec<u16> = "添加文件或应用…\0".encode_utf16().collect();
        let _ = AppendMenuW(menu, MF_STRING, ID_ADD, PCWSTR(add.as_ptr()));
        let add_folder: Vec<u16> = "添加文件夹…\0".encode_utf16().collect();
        let _ = AppendMenuW(menu, MF_STRING, ID_ADD_FOLDER, PCWSTR(add_folder.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let dock_mode: Vec<u16> = "常驻底部（非全屏时）\0".encode_utf16().collect();
        let mflags = MF_STRING
            | if always_mode {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
        let _ = AppendMenuW(
            menu,
            mflags,
            ID_TOGGLE_DOCK_MODE,
            PCWSTR(dock_mode.as_ptr()),
        );
        let astr: Vec<u16> = "开机自启\0".encode_utf16().collect();
        let aflags = MF_STRING
            | if autostart::is_enabled() {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
        let _ = AppendMenuW(menu, aflags, ID_AUTOSTART, PCWSTR(astr.as_ptr()));
        let settings: Vec<u16> = "设置…\0".encode_utf16().collect();
        let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, PCWSTR(settings.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let exit: Vec<u16> = "退出 FeatherDock\0".encode_utf16().collect();
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, PCWSTR(exit.as_ptr()));
        // Required so the menu dismisses correctly when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let menu_style = crate::menu_style::style(menu, crate::dock_theme(hwnd), hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        drop(menu_style);
        let _ = DestroyMenu(menu);
        // Finish the foreground-menu interaction before the next invocation.
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = Shell_NotifyIconW(NIM_SETFOCUS, &self.nid);
        match cmd.0 as usize {
            0 => None,
            id => Some(id),
        }
    }
}

unsafe fn notify_icon(
    operation: NOTIFY_ICON_MESSAGE,
    icon: &NOTIFYICONDATAW,
) -> (BOOL, WIN32_ERROR) {
    // Shell_NotifyIconW documents its BOOL result, not a last-error contract.
    // Clear the thread's stale error and capture its value immediately anyway
    // so logs retain both actual return values without claiming FALSE/0 worked.
    SetLastError(WIN32_ERROR(0));
    let result = Shell_NotifyIconW(operation, icon);
    (result, GetLastError())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_four_tray_menu_events_include_mouse_and_keyboard_selection() {
        for event in [WM_CONTEXTMENU, NIN_SELECT, NIN_KEYSELECT] {
            assert!(Tray::is_menu_event(LPARAM(((1 << 16) | event) as isize)));
        }
        assert!(!Tray::is_menu_event(LPARAM(
            ((1 << 16) | WM_MOUSEMOVE) as isize
        )));
        assert!(!Tray::is_menu_event(LPARAM(
            ((1 << 16) | NIN_BALLOONUSERCLICK) as isize
        )));
        assert!(!Tray::is_menu_event(LPARAM(
            ((1 << 16) | WM_LBUTTONUP) as isize
        )));
        assert!(!Tray::is_menu_event(LPARAM(
            ((2 << 16) | WM_CONTEXTMENU) as isize
        )));
    }

    #[test]
    fn legacy_tray_mouse_up_events_still_open_the_menu() {
        for event in [WM_LBUTTONUP, WM_RBUTTONUP] {
            assert!(Tray::is_menu_event(LPARAM(event as isize)));
        }
        assert!(!Tray::is_menu_event(LPARAM(WM_LBUTTONDOWN as isize)));
    }
}
