//! Minimal tray icon with a right-click "Exit" menu, so the dock is quittable.

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

pub struct Tray {
    nid: NOTIFYICONDATAW,
    taskbar_created: u32,
}

impl Tray {
    pub unsafe fn new(hwnd: HWND) -> Tray {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
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
        Shell_NotifyIconW(NIM_ADD, &self.nid).ok()?;
        let mut versioned = self.nid;
        versioned.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &versioned).ok()
    }

    pub fn taskbar_created_message(&self) -> u32 {
        self.taskbar_created
    }

    pub unsafe fn remove(&self) {
        let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
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
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            0 => None,
            id => Some(id),
        }
    }
}
