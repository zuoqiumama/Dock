//! Native settings window (standard Win32 controls — dependency-free, accessible),
//! dark-themed to match the dock. Choose the system-taskbar hide mode, dock mode,
//! startup options, and manage the pinned ("resident") apps. Changes apply live:
//! the taskbar is reconfigured immediately and the dock is notified to re-read.

use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, FillRect, GetMonitorInfoW, MonitorFromWindow,
    SetBkColor, SetBkMode, SetTextColor, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::settings::{self, DockMode, Settings, TaskbarMode};
use crate::theme::ThemePreset;
use crate::{apps, autostart, config, error_log, taskbar};

/// Posted to the dock when mode/fullscreen settings change (cheap re-rest).
pub const WM_SETTINGS_CHANGED: u32 = WM_APP + 0x22;
/// Posted to the dock when the pinned apps change (full dock rebuild).
pub const WM_PINS_CHANGED: u32 = WM_APP + 0x23;

const ID_TB_SHOW: usize = 101;
const ID_TB_AUTOHIDE: usize = 102;
const ID_TB_HIDDEN: usize = 103;
const ID_DOCK_ALWAYS: usize = 111;
const ID_DOCK_AUTOHIDE: usize = 112;
const ID_FULLSCREEN: usize = 121;
const ID_MAXIMIZED: usize = 122;
const ID_PIN_LIST: usize = 131;
const ID_PIN_ADD: usize = 132;
const ID_PIN_REMOVE: usize = 133;
const ID_AUTOSTART: usize = 141;
const ID_DRAWER_ENABLED: usize = 142;
const ID_HIDE_DESKTOP_ICONS: usize = 143;
const ID_THEME: usize = 144;
const ID_CLOSE: usize = 151;

const BST_CHECKED: usize = 1;
const FD_CBS_DROPDOWNLIST: u32 = 0x0003;
const FD_CB_GETCURSEL: u32 = 0x0147;
const FD_CB_ADDSTRING: u32 = 0x0143;
const FD_CB_SETCURSEL: u32 = 0x014E;
const FD_CBN_SELCHANGE: usize = 1;

static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

struct SettingsState {
    dock_hwnd: HWND,
    settings: Settings,
    tb_radios: [HWND; 3],
    dock_radios: [HWND; 2],
    fullscreen: HWND,
    maximized: HWND,
    theme_combo: HWND,
    autostart: HWND,
    drawer_enabled: HWND,
    hide_desktop: HWND,
    pin_list: HWND,
    /// Config item index for each listbox row, so "remove" targets the exact `[[item]]`
    /// (works for `app=` rows that have no path to match by string).
    pin_specs: Vec<usize>,
    font: HFONT,
    bg: HBRUSH,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

fn bg_color() -> COLORREF {
    rgb(0x22, 0x22, 0x26)
}
fn text_color() -> COLORREF {
    rgb(0xEC, 0xEC, 0xEC)
}

unsafe fn checked(hwnd: HWND) -> bool {
    SendMessageW(hwnd, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize == BST_CHECKED
}
unsafe fn set_checked(hwnd: HWND, on: bool) {
    SendMessageW(
        hwnd,
        BM_SETCHECK,
        WPARAM(if on { BST_CHECKED } else { 0 }),
        LPARAM(0),
    );
}
unsafe fn select_one(group: &[HWND], index: usize) {
    for (i, &hwnd) in group.iter().enumerate() {
        set_checked(hwnd, i == index);
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn control(
    parent: HWND,
    instance: HINSTANCE,
    class: PCWSTR,
    text: &str,
    style: u32,
    id: usize,
    (x, y, w, h): (i32, i32, i32, i32),
    scale: f32,
    font: HFONT,
) -> HWND {
    let label = wide(text);
    let s = |v: i32| (v as f32 * scale) as i32;
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(label.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
        s(x),
        s(y),
        s(w),
        s(h),
        parent,
        HMENU(id as *mut c_void),
        instance,
        None,
    )
    .unwrap_or_default();
    SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    hwnd
}

/// Apply the dark explorer theme to every child control (scrollbars, glyphs).
unsafe extern "system" fn theme_child(hwnd: HWND, _: LPARAM) -> BOOL {
    let _ = SetWindowTheme(hwnd, w!("DarkMode_Explorer"), PCWSTR::null());
    BOOL(1)
}

unsafe fn refresh_pins(state: &mut SettingsState) {
    SendMessageW(state.pin_list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
    state.pin_specs.clear();
    let specs = config::load()
        .ok()
        .flatten()
        .map(|cfg| cfg.items)
        .unwrap_or_default();
    for (index, spec) in specs.iter().enumerate() {
        // List only rows that actually launch something (matches what the dock shows), and
        // keep each row's config index so removal targets the exact entry — including an
        // `app="chrome.exe"` row whose resolved path wouldn't match by string.
        let Some(path) = apps::resolve_launch_path(spec) else {
            continue;
        };
        let label = spec.label.clone().unwrap_or_else(|| pin_label(&path));
        let wlabel = wide(&label);
        SendMessageW(
            state.pin_list,
            LB_ADDSTRING,
            WPARAM(0),
            LPARAM(wlabel.as_ptr() as isize),
        );
        state.pin_specs.push(index);
    }
}

fn pin_label(path: &str) -> String {
    let p = std::path::Path::new(path);
    p.file_stem()
        .or_else(|| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "应用".to_string())
}

unsafe fn notify(state: &SettingsState, message: u32) {
    let _ = PostMessageW(state.dock_hwnd, message, WPARAM(0), LPARAM(0));
}

/// Work area (left, top, width, height) of the monitor the dock is on, for centring.
unsafe fn monitor_work_area(dock_hwnd: HWND) -> (i32, i32, i32, i32) {
    let monitor = MonitorFromWindow(dock_hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let w = info.rcWork.right - info.rcWork.left;
        let h = info.rcWork.bottom - info.rcWork.top;
        (info.rcWork.left, info.rcWork.top, w, h)
    } else {
        (
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
        )
    }
}

/// Open (or focus, if already open) the settings window.
pub unsafe fn open(dock_hwnd: HWND) {
    let existing = SETTINGS_HWND.load(Ordering::Relaxed);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let _ = SetForegroundWindow(hwnd);
            return;
        }
    }

    let instance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(module) => module.into(),
        Err(_) => return,
    };
    let class_name = w!("FeatherDockSettings");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: HBRUSH(std::ptr::null_mut()), // we paint the dark bg in WM_ERASEBKGND
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassExW(&wc); // ignore "already registered"

    let scale = (GetDpiForWindow(dock_hwnd).max(96) as f32) / 96.0;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: (420.0 * scale) as i32,
        bottom: (708.0 * scale) as i32,
    };
    let _ = AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE(0));
    let win_w = rect.right - rect.left;
    let win_h = rect.bottom - rect.top;
    // Centre on the dock's monitor (work area), not always the primary screen, so the
    // settings window opens where the dock — and the user — actually are.
    let (area_x, area_y, area_w, area_h) = monitor_work_area(dock_hwnd);
    let x = area_x + (area_w - win_w) / 2;
    let y = area_y + (area_h - win_h) / 2;

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        class_name,
        w!("FeatherDock 设置"),
        style,
        x,
        y,
        win_w,
        win_h,
        None,
        None,
        instance,
        None,
    ) else {
        return;
    };

    // Dark title bar (Win11).
    let dark: i32 = 1;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const i32 as *const c_void,
        4,
    );

    let height = -((9.0 * scale * 96.0 / 72.0) as i32);
    let face = wide("Microsoft YaHei UI");
    let font = CreateFontW(
        height,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        1,
        0,
        0,
        0,
        0,
        PCWSTR(face.as_ptr()),
    );
    let bg = CreateSolidBrush(bg_color());

    let btn = w!("BUTTON");
    let group = WINDOW_STYLE(BS_GROUPBOX as u32).0;
    let radio = WINDOW_STYLE(BS_AUTORADIOBUTTON as u32).0;
    let check = WINDOW_STYLE(BS_AUTOCHECKBOX as u32).0;
    let push = WINDOW_STYLE(BS_PUSHBUTTON as u32).0;
    let mk =
        |text, style, id, rect| control(hwnd, instance, btn, text, style, id, rect, scale, font);

    mk("系统任务栏", group, 0, (12, 6, 396, 116));
    let tb_radios = [
        mk(
            "不隐藏（保留系统任务栏）",
            radio,
            ID_TB_SHOW,
            (26, 30, 366, 24),
        ),
        mk(
            "自动隐藏（推荐：腾出空间，移到边缘弹出）",
            radio,
            ID_TB_AUTOHIDE,
            (26, 58, 366, 24),
        ),
        mk(
            "完全隐藏（最纯净；退出时自动恢复）",
            radio,
            ID_TB_HIDDEN,
            (26, 86, 366, 24),
        ),
    ];

    mk("Dock 显示", group, 0, (12, 130, 396, 144));
    let dock_radios = [
        mk("常驻屏幕底部", radio, ID_DOCK_ALWAYS, (26, 154, 366, 24)),
        mk(
            "自动隐藏（鼠标到底部唤出）",
            radio,
            ID_DOCK_AUTOHIDE,
            (26, 182, 366, 24),
        ),
    ];
    let fullscreen = mk(
        "出现全屏应用时自动隐藏",
        check,
        ID_FULLSCREEN,
        (26, 210, 366, 24),
    );
    let maximized = mk(
        "出现最大化窗口时收起到边缘",
        check,
        ID_MAXIMIZED,
        (26, 238, 366, 24),
    );

    mk("外观", group, 0, (12, 282, 396, 56));
    let theme_combo = control(
        hwnd,
        instance,
        w!("COMBOBOX"),
        "",
        FD_CBS_DROPDOWNLIST | WS_TABSTOP.0 | WS_VSCROLL.0,
        ID_THEME,
        (26, 306, 180, 160),
        scale,
        font,
    );
    for preset in ThemePreset::ALL {
        let label = wide(preset.label());
        SendMessageW(
            theme_combo,
            FD_CB_ADDSTRING,
            WPARAM(0),
            LPARAM(label.as_ptr() as isize),
        );
    }

    mk("常驻应用（Dock 左侧固定项）", group, 0, (12, 350, 396, 150));
    let pin_list = control(
        hwnd,
        instance,
        w!("LISTBOX"),
        "",
        LBS_NOTIFY as u32 | WS_VSCROLL.0 | WS_BORDER.0,
        ID_PIN_LIST,
        (26, 374, 288, 116),
        scale,
        font,
    );
    mk("添加…", push, ID_PIN_ADD, (322, 374, 74, 26));
    mk("移除", push, ID_PIN_REMOVE, (322, 406, 74, 26));

    mk("程序抽屉 / 桌面", group, 0, (12, 506, 396, 88));
    let drawer_cb = mk(
        "启用程序抽屉（Dock 上的应用网格按钮）",
        check,
        ID_DRAWER_ENABLED,
        (26, 530, 366, 24),
    );
    let hide_desktop_cb = mk(
        "隐藏桌面图标（退出时自动恢复）",
        check,
        ID_HIDE_DESKTOP_ICONS,
        (26, 558, 366, 24),
    );

    mk("启动", group, 0, (12, 602, 396, 50));
    let autostart_cb = mk("开机自启", check, ID_AUTOSTART, (26, 624, 366, 24));

    mk("关闭", push, ID_CLOSE, (332, 660, 76, 28));

    let _ = EnumChildWindows(hwnd, Some(theme_child), LPARAM(0));

    let current = settings::load();
    select_one(
        &tb_radios,
        match current.taskbar_mode {
            TaskbarMode::Show => 0,
            TaskbarMode::AutoHide => 1,
            TaskbarMode::Hidden => 2,
        },
    );
    select_one(
        &dock_radios,
        match current.dock_mode {
            DockMode::Always => 0,
            DockMode::AutoHide => 1,
        },
    );
    set_checked(fullscreen, current.hide_on_fullscreen);
    set_checked(maximized, current.hide_on_maximized);
    let theme_index = ThemePreset::ALL
        .iter()
        .position(|preset| *preset == current.theme)
        .unwrap_or(0);
    SendMessageW(theme_combo, FD_CB_SETCURSEL, WPARAM(theme_index), LPARAM(0));
    set_checked(autostart_cb, autostart::is_enabled());
    set_checked(drawer_cb, current.drawer_enabled);
    set_checked(hide_desktop_cb, current.hide_desktop_icons);

    let state = Box::into_raw(Box::new(SettingsState {
        dock_hwnd,
        settings: current,
        tb_radios,
        dock_radios,
        fullscreen,
        maximized,
        theme_combo,
        autostart: autostart_cb,
        drawer_enabled: drawer_cb,
        hide_desktop: hide_desktop_cb,
        pin_list,
        pin_specs: Vec::new(),
        font,
        bg,
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    SETTINGS_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    refresh_pins(&mut *state);

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

unsafe fn persist_and_notify(state: &SettingsState) {
    let _ = settings::save(&state.settings);
    notify(state, WM_SETTINGS_CHANGED);
}

unsafe fn dark_ctl_color(state: *mut SettingsState, wparam: WPARAM) -> LRESULT {
    if state.is_null() {
        return LRESULT(0);
    }
    let hdc = HDC(wparam.0 as *mut c_void);
    SetTextColor(hdc, text_color());
    SetBkColor(hdc, bg_color());
    SetBkMode(hdc, TRANSPARENT);
    LRESULT((*state).bg.0 as isize)
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
        match msg {
            WM_ERASEBKGND if !ptr.is_null() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(hdc, &rc, (*ptr).bg);
                LRESULT(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => dark_ctl_color(ptr, wparam),
            WM_COMMAND if !ptr.is_null() => {
                let state = &mut *ptr;
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) & 0xFFFF;
                match id {
                    ID_TB_SHOW | ID_TB_AUTOHIDE | ID_TB_HIDDEN => {
                        let (mode, index) = match id {
                            ID_TB_SHOW => (TaskbarMode::Show, 0),
                            ID_TB_AUTOHIDE => (TaskbarMode::AutoHide, 1),
                            _ => (TaskbarMode::Hidden, 2),
                        };
                        state.settings.taskbar_mode = mode;
                        select_one(&state.tb_radios, index);
                        taskbar::apply(mode);
                        persist_and_notify(state);
                    }
                    ID_DOCK_ALWAYS | ID_DOCK_AUTOHIDE => {
                        let (mode, index) = if id == ID_DOCK_ALWAYS {
                            (DockMode::Always, 0)
                        } else {
                            (DockMode::AutoHide, 1)
                        };
                        state.settings.dock_mode = mode;
                        select_one(&state.dock_radios, index);
                        persist_and_notify(state);
                    }
                    ID_FULLSCREEN => {
                        state.settings.hide_on_fullscreen = checked(state.fullscreen);
                        persist_and_notify(state);
                    }
                    ID_MAXIMIZED => {
                        state.settings.hide_on_maximized = checked(state.maximized);
                        persist_and_notify(state);
                    }
                    ID_THEME if code == FD_CBN_SELCHANGE => {
                        let sel =
                            SendMessageW(state.theme_combo, FD_CB_GETCURSEL, WPARAM(0), LPARAM(0))
                                .0;
                        if sel >= 0 {
                            if let Some(theme) = ThemePreset::ALL.get(sel as usize) {
                                state.settings.theme = *theme;
                                persist_and_notify(state);
                            }
                        }
                    }
                    ID_PIN_ADD => match crate::pick_content(hwnd, false) {
                        Ok(Some(path)) => match config::add_item(&pin_label(&path), &path) {
                            Ok(_) => {
                                refresh_pins(state);
                                notify(state, WM_PINS_CHANGED);
                            }
                            Err(error) => error_log::write("添加常驻应用失败", &error),
                        },
                        Ok(None) => {}
                        Err(error) => error_log::write("选择应用失败", &error),
                    },
                    ID_PIN_REMOVE => {
                        let sel =
                            SendMessageW(state.pin_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                        if sel >= 0 {
                            if let Some(&index) = state.pin_specs.get(sel as usize) {
                                match config::remove_item_at_index(index) {
                                    Ok(_) => {
                                        refresh_pins(state);
                                        notify(state, WM_PINS_CHANGED);
                                    }
                                    Err(error) => error_log::write("移除常驻应用失败", &error),
                                }
                            }
                        }
                    }
                    ID_AUTOSTART => {
                        if let Err(error) = autostart::set(checked(state.autostart)) {
                            error_log::write("设置页开机自启失败", &error);
                            set_checked(state.autostart, autostart::is_enabled());
                        }
                    }
                    ID_DRAWER_ENABLED => {
                        // Show/hide the dock's app-drawer button → the dock must rebuild.
                        state.settings.drawer_enabled = checked(state.drawer_enabled);
                        let _ = settings::save(&state.settings);
                        notify(state, WM_PINS_CHANGED);
                    }
                    ID_HIDE_DESKTOP_ICONS => {
                        // Apply live, then persist AND notify the dock: it must re-read so its
                        // in-memory `hide_desktop_icons` is current — otherwise the exit cleanup
                        // (which restores icons only if the dock thinks it hid them) leaves the
                        // user with a blank desktop after FeatherDock quits.
                        state.settings.hide_desktop_icons = checked(state.hide_desktop);
                        crate::desktop_icons::set_hidden(state.settings.hide_desktop_icons);
                        persist_and_notify(state);
                    }
                    ID_CLOSE => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                SETTINGS_HWND.store(0, Ordering::Relaxed);
                if !ptr.is_null() {
                    let state = Box::from_raw(ptr);
                    let _ = DeleteObject(HGDIOBJ(state.font.0));
                    let _ = DeleteObject(HGDIOBJ(state.bg.0));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
