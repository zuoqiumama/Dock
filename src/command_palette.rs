use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, FillRect, SetBkColor, SetBkMode, SetTextColor,
    HBRUSH, HDC, HFONT, HGDIOBJ, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_EDIT: usize = 4301;
const ID_LIST: usize = 4302;
const MAX_RESULTS: usize = 18;

static PALETTE_HWND: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandAction {
    OpenPath(String),
    ActivateWindow(isize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandEntry {
    pub label: String,
    pub detail: String,
    action: CommandAction,
}

impl CommandEntry {
    pub fn open_path(label: impl Into<String>, path: impl Into<String>) -> CommandEntry {
        let path = path.into();
        CommandEntry {
            label: label.into(),
            detail: path.clone(),
            action: CommandAction::OpenPath(path),
        }
    }

    fn activate_window(label: impl Into<String>, hwnd: isize) -> CommandEntry {
        CommandEntry {
            label: label.into(),
            detail: "运行中的窗口".to_string(),
            action: CommandAction::ActivateWindow(hwnd),
        }
    }
}

fn haystack(entry: &CommandEntry) -> String {
    format!("{} {}", entry.label, entry.detail).to_lowercase()
}

pub fn matches_query(entry: &CommandEntry, query: &str) -> bool {
    let haystack = haystack(entry);
    query
        .split_whitespace()
        .all(|part| haystack.contains(&part.to_lowercase()))
}

fn score(entry: &CommandEntry, query: &str) -> i32 {
    let query = query.trim().to_lowercase();
    let label = entry.label.to_lowercase();
    if query.is_empty() {
        return 0;
    }
    if label.starts_with(&query) {
        0
    } else if label
        .split(|ch: char| ch.is_whitespace() || ch == '-' || ch == '_')
        .any(|word| word.starts_with(&query))
    {
        1
    } else if label.contains(&query) {
        2
    } else {
        3
    }
}

pub fn filter_entries(entries: &[CommandEntry], query: &str, limit: usize) -> Vec<CommandEntry> {
    let mut matches: Vec<CommandEntry> = entries
        .iter()
        .filter(|entry| matches_query(entry, query))
        .cloned()
        .collect();
    matches.sort_by(|a, b| {
        score(a, query)
            .cmp(&score(b, query))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    matches.truncate(limit);
    matches
}

struct Palette {
    edit: HWND,
    list: HWND,
    entries: Vec<CommandEntry>,
    filtered: Vec<CommandEntry>,
    bg: HBRUSH,
    font: HFONT,
}

fn wide_nul(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

fn collect_entries() -> Vec<CommandEntry> {
    let mut entries = Vec::new();
    if let Ok(Some(cfg)) = crate::config::load() {
        for spec in cfg.items {
            if let Some(path) = crate::apps::resolve_launch_path(&spec) {
                let label = spec.label.unwrap_or_else(|| {
                    std::path::Path::new(&path)
                        .file_stem()
                        .or_else(|| std::path::Path::new(&path).file_name())
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Item".to_string())
                });
                entries.push(CommandEntry::open_path(label, path));
            }
        }
    }
    unsafe {
        for window in crate::windows_list::enumerate_sorted() {
            entries.push(CommandEntry::activate_window(window.title, window.hwnd));
        }
    }
    unsafe {
        for entry in crate::desktop_scan::scan_programs_cached() {
            if let Some(path) = entry.path {
                entries.push(CommandEntry::open_path(entry.label, path));
            }
        }
    }
    entries.sort_by_key(|entry| entry.label.to_lowercase());
    entries.dedup_by(|a, b| {
        a.label.eq_ignore_ascii_case(&b.label) && a.detail.eq_ignore_ascii_case(&b.detail)
    });
    entries
}

unsafe fn read_text(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn refresh_list(state: &mut Palette) {
    let query = read_text(state.edit);
    state.filtered = filter_entries(&state.entries, &query, MAX_RESULTS);
    SendMessageW(state.list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
    for entry in &state.filtered {
        let text = wide_nul(&format!("{}   {}", entry.label, entry.detail));
        SendMessageW(
            state.list,
            LB_ADDSTRING,
            WPARAM(0),
            LPARAM(text.as_ptr() as isize),
        );
    }
    if !state.filtered.is_empty() {
        SendMessageW(state.list, LB_SETCURSEL, WPARAM(0), LPARAM(0));
    }
}

unsafe fn run_entry(entry: &CommandEntry) {
    match &entry.action {
        CommandAction::OpenPath(path) => {
            let target = wide_nul(path);
            let _ = ShellExecuteW(
                HWND::default(),
                w!("open"),
                PCWSTR(target.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
        CommandAction::ActivateWindow(hwnd) => {
            let _ = crate::windows_list::activate(*hwnd);
        }
    }
}

unsafe fn activate_selection(hwnd: HWND, state: &Palette) {
    let sel = SendMessageW(state.list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    if sel >= 0 {
        if let Some(entry) = state.filtered.get(sel as usize) {
            run_entry(entry);
            let _ = DestroyWindow(hwnd);
        }
    }
}

pub unsafe fn toggle(owner: HWND) {
    let existing = PALETTE_HWND.load(Ordering::Relaxed);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let _ = DestroyWindow(hwnd);
            return;
        }
    }
    open(owner);
}

unsafe fn open(owner: HWND) {
    let instance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(module) => module.into(),
        Err(_) => return,
    };
    let class_name = w!("FeatherDockCommandPalette");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassExW(&wc);

    let scale = (GetDpiForWindow(owner).max(96) as f32) / 96.0;
    let s = |value: f32| (value * scale).round() as i32;
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: s(560.0),
        bottom: s(420.0),
    };
    let _ = AdjustWindowRectEx(&mut rc, style, false, WS_EX_TOPMOST);
    let win_w = rc.right - rc.left;
    let win_h = rc.bottom - rc.top;
    let x = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
    let y = (GetSystemMetrics(SM_CYSCREEN) - win_h) / 3;

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class_name,
        w!("FeatherDock 搜索"),
        style,
        x,
        y,
        win_w,
        win_h,
        owner,
        None,
        instance,
        None,
    ) else {
        return;
    };

    let dark: i32 = 1;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const i32 as *const c_void,
        4,
    );

    let face = wide_nul("Microsoft YaHei UI");
    let font = CreateFontW(
        -((10.0 * scale * 96.0 / 72.0) as i32),
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
    let bg = CreateSolidBrush(rgb(0x20, 0x20, 0x24));

    let edit = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        w!("EDIT"),
        w!(""),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32 | WS_TABSTOP.0),
        s(14.0),
        s(14.0),
        s(532.0),
        s(30.0),
        hwnd,
        HMENU(ID_EDIT as *mut c_void),
        instance,
        None,
    )
    .unwrap_or_default();
    let list = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("LISTBOX"),
        w!(""),
        WS_CHILD
            | WS_VISIBLE
            | WS_VSCROLL
            | WINDOW_STYLE(LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32 | WS_BORDER.0),
        s(14.0),
        s(54.0),
        s(532.0),
        s(320.0),
        hwnd,
        HMENU(ID_LIST as *mut c_void),
        instance,
        None,
    )
    .unwrap_or_default();
    SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    SendMessageW(list, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    let _ = SetWindowTheme(edit, w!("DarkMode_CFD"), PCWSTR::null());
    let _ = SetWindowTheme(list, w!("DarkMode_Explorer"), PCWSTR::null());

    let mut state = Box::new(Palette {
        edit,
        list,
        entries: collect_entries(),
        filtered: Vec::new(),
        bg,
        font,
    });
    refresh_list(&mut state);
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    PALETTE_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(edit);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Palette;
        match msg {
            WM_ERASEBKGND if !ptr.is_null() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(hdc, &rc, (*ptr).bg);
                LRESULT(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX if !ptr.is_null() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, rgb(0xEC, 0xEC, 0xEC));
                SetBkColor(hdc, rgb(0x20, 0x20, 0x24));
                SetBkMode(hdc, TRANSPARENT);
                LRESULT((*ptr).bg.0 as isize)
            }
            WM_COMMAND if !ptr.is_null() => {
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) & 0xFFFF;
                if id == ID_EDIT && code == EN_CHANGE as usize {
                    refresh_list(&mut *ptr);
                } else if id == ID_LIST && code == LBN_DBLCLK as usize {
                    activate_selection(hwnd, &*ptr);
                }
                LRESULT(0)
            }
            WM_KEYDOWN if !ptr.is_null() && wparam.0 == VK_RETURN.0 as usize => {
                activate_selection(hwnd, &*ptr);
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) == 0 {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PALETTE_HWND.store(0, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_matches_label_words_and_detail() {
        let entry = CommandEntry::open_path("Visual Studio Code", "C:\\Tools\\Code.exe");
        assert!(matches_query(&entry, "code"));
        assert!(matches_query(&entry, "visual code"));
        assert!(matches_query(&entry, "tools code"));
        assert!(!matches_query(&entry, "photos"));
    }

    #[test]
    fn filter_prefers_label_prefixes_then_substrings() {
        let entries = vec![
            CommandEntry::open_path("Chrome", "C:\\Chrome.exe"),
            CommandEntry::open_path("Visual Studio Code", "C:\\Code.exe"),
            CommandEntry::open_path("Code Notes", "C:\\Notes.txt"),
        ];

        let filtered = filter_entries(&entries, "code", 10);
        let labels: Vec<&str> = filtered.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(labels, vec!["Code Notes", "Visual Studio Code"]);
    }
}
