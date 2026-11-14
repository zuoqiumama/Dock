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
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_EDIT: usize = 4301;
const ID_LIST: usize = 4302;
const MAX_RESULTS: usize = 18;
const PALETTE_SUBCLASS_ID: usize = 1;
pub(crate) const WM_ENTRIES_READY: u32 = WM_APP + 0x40;

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
    action_id: String,
    history_key: String,
    aliases: Vec<String>,
    used: u64,
}

impl CommandEntry {
    pub fn open_path(label: impl Into<String>, path: impl Into<String>) -> CommandEntry {
        let path = path.into();
        CommandEntry {
            label: label.into(),
            detail: path.clone(),
            action_id: crate::app_identity::command_action_id(&path),
            history_key: crate::app_identity::cached_launch_key(&path),
            aliases: Vec::new(),
            used: 0,
            action: CommandAction::OpenPath(path),
        }
    }

    fn activate_window(label: impl Into<String>, hwnd: isize) -> CommandEntry {
        CommandEntry {
            label: label.into(),
            detail: "运行中的窗口".to_string(),
            action: CommandAction::ActivateWindow(hwnd),
            action_id: format!("window:{hwnd}"),
            history_key: String::new(),
            aliases: Vec::new(),
            used: 0,
        }
    }
}

pub fn matches_query(entry: &CommandEntry, query: &str) -> bool {
    crate::search::matches_names(
        std::iter::once(entry.label.as_str()).chain(entry.aliases.iter().map(String::as_str)),
        &entry.detail,
        query,
    )
}

fn score(entry: &CommandEntry, query: &str) -> i32 {
    crate::search::score_names(
        std::iter::once(entry.label.as_str()).chain(entry.aliases.iter().map(String::as_str)),
        query,
    )
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
            .then_with(|| b.used.cmp(&a.used))
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
    material: crate::theme::SurfaceTheme,
    pending: Option<std::sync::mpsc::Receiver<Vec<CommandEntry>>>,
}

fn wide_nul(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn material_color(rgb: (f32, f32, f32)) -> COLORREF {
    COLORREF(
        (rgb.0 * 255.0).round() as u32
            | (((rgb.1 * 255.0).round() as u32) << 8)
            | (((rgb.2 * 255.0).round() as u32) << 16),
    )
}

fn window_launch_history_key(
    entries: &[CommandEntry],
    app_id: Option<&str>,
    exe_path: Option<&str>,
    running: &[crate::windows_list::RunningWindow],
) -> Option<String> {
    if let (Some(app_id), Some(exe_path)) = (app_id, exe_path) {
        let exe_path = crate::app_identity::normalize(exe_path);
        let ids: std::collections::BTreeSet<&str> = running
            .iter()
            .filter(|window| {
                window
                    .exe_path
                    .as_deref()
                    .map(crate::app_identity::normalize)
                    .as_deref()
                    == Some(exe_path.as_str())
            })
            .filter_map(|window| window.app_id.as_deref())
            .collect();
        if ids.len() != 1 || ids.iter().next().copied() != Some(app_id) {
            return None;
        }
    }
    let exe_path = exe_path.map(crate::app_identity::normalize);
    let mut keys = std::collections::BTreeSet::new();
    for entry in entries.iter().filter(|entry| {
        matches!(entry.action, CommandAction::OpenPath(_))
            && (app_id.is_some_and(|id| id == entry.history_key)
                || exe_path
                    .as_ref()
                    .is_some_and(|path| path == &entry.history_key))
    }) {
        keys.insert(entry.history_key.clone());
    }
    if keys.len() == 1 {
        keys.into_iter().next()
    } else {
        None
    }
}

fn unify_history_keys(
    entries: &mut [CommandEntry],
    running: &[crate::windows_list::RunningWindow],
) {
    let mappings: Vec<(isize, String)> = running
        .iter()
        .filter_map(|window| {
            window_launch_history_key(
                entries,
                window.app_id.as_deref(),
                window.exe_path.as_deref(),
                running,
            )
            .map(|key| (window.hwnd, key))
        })
        .collect();
    for entry in entries.iter_mut() {
        let hwnd = match &entry.action {
            CommandAction::ActivateWindow(hwnd) => *hwnd,
            _ => continue,
        };
        if let Some((_, key)) = mappings.iter().find(|(candidate, _)| candidate == &hwnd) {
            entry.history_key = key.clone();
        }
    }
}

fn unified_history(
    history: &[(String, u64)],
    entries: &[CommandEntry],
    running: &[crate::windows_list::RunningWindow],
) -> Vec<(String, u64)> {
    let launch_keys: std::collections::BTreeSet<String> = entries
        .iter()
        .filter(|entry| matches!(entry.action, CommandAction::OpenPath(_)))
        .map(|entry| entry.history_key.clone())
        .collect();
    let mut unified: Vec<(String, u64)> = Vec::new();
    for (key, time) in history {
        let key = if launch_keys.contains(key) {
            key.clone()
        } else {
            running
                .iter()
                .find_map(|window| {
                    if window.app_id.as_deref() != Some(key.as_str())
                        && window
                            .exe_path
                            .as_deref()
                            .map(crate::app_identity::normalize)
                            .as_deref()
                            != Some(key.as_str())
                    {
                        return None;
                    }
                    window_launch_history_key(
                        entries,
                        window.app_id.as_deref(),
                        window.exe_path.as_deref(),
                        running,
                    )
                })
                .unwrap_or_else(|| key.clone())
        };
        if let Some(existing) = unified.iter_mut().find(|(old, _)| old == &key) {
            existing.1 = existing.1.max(*time);
        } else {
            unified.push((key, *time));
        }
    }
    unified.sort_by_key(|(_, time)| std::cmp::Reverse(*time));
    unified.truncate(128);
    unified
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
                let mut entry = CommandEntry::open_path(label, path.clone());
                entry.history_key = unsafe { crate::app_identity::resolve_launch_key(&path) };
                entries.push(entry);
            }
        }
    }
    let running = unsafe { crate::windows_list::enumerate_sorted() };
    for window in &running {
        let mut entry = CommandEntry::activate_window(window.title.clone(), window.hwnd);
        entry.history_key = window
            .app_id
            .clone()
            .or_else(|| {
                window
                    .exe_path
                    .as_deref()
                    .map(crate::app_identity::normalize)
            })
            .unwrap_or_default();
        entries.push(entry);
    }
    {
        for entry in crate::desktop_scan::snapshot() {
            if let Some(path) = entry.path {
                let mut entry = CommandEntry::open_path(entry.label, path.clone());
                entry.history_key = unsafe { crate::app_identity::resolve_launch_key(&path) };
                entries.push(entry);
            }
        }
    }
    unify_history_keys(&mut entries, &running);
    let history = unified_history(&load_history(), &entries, &running);
    merge_entries(entries, &history)
}

fn add_alias(entry: &mut CommandEntry, alias: String) {
    if !entry.label.eq_ignore_ascii_case(&alias)
        && !entry
            .aliases
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&alias))
    {
        entry.aliases.push(alias);
    }
}

fn merge_entries(entries: Vec<CommandEntry>, history: &[(String, u64)]) -> Vec<CommandEntry> {
    let mut merged: Vec<CommandEntry> = Vec::new();
    for mut entry in entries {
        entry.used = if entry.history_key.is_empty() {
            0
        } else {
            history
                .iter()
                .find(|(key, _)| key == &entry.history_key)
                .map_or(0, |(_, time)| *time)
        };
        if matches!(entry.action, CommandAction::OpenPath(_)) {
            if let Some(existing) = merged.iter_mut().find(|existing| {
                existing.action_id == entry.action_id
                    && matches!(existing.action, CommandAction::OpenPath(_))
            }) {
                add_alias(existing, entry.label.clone());
                for alias in entry.aliases {
                    add_alias(existing, alias);
                }
                existing.used = existing.used.max(entry.used);
                continue;
            }
        }
        merged.push(entry);
    }
    merged
}

fn history_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("search-history.tsv")
}
fn load_history() -> Vec<(String, u64)> {
    std::fs::read_to_string(history_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (time, key) = line.split_once('\t')?;
            Some((key.to_string(), time.parse().ok()?))
        })
        .take(128)
        .collect()
}
static HISTORY_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn remember(key: String) {
    if key.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let _guard = HISTORY_WRITE.lock().unwrap_or_else(|e| e.into_inner());
        let mut history = load_history();
        history.retain(|(old, _)| old != &key);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        history.insert(0, (key, time));
        history.truncate(128);
        let data: String = history
            .iter()
            .map(|(key, time)| format!("{time}\t{}\n", key.replace(['\n', '\r', '\t'], " ")))
            .collect();
        if let Err(error) = crate::atomic::write(&history_path(), data.as_bytes()) {
            crate::error_log::write("保存搜索历史失败", &error);
        }
    });
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

fn refreshed_selection(
    previous: &[CommandEntry],
    selected: isize,
    current: &[CommandEntry],
    preserve: bool,
) -> Option<usize> {
    if current.is_empty() {
        return None;
    }
    if preserve && selected >= 0 {
        if let Some(selected) = previous.get(selected as usize) {
            if let Some(index) = current
                .iter()
                .position(|entry| entry.action_id == selected.action_id)
            {
                return Some(index);
            }
        }
    }
    Some(0)
}

unsafe fn refresh_list(state: &mut Palette, preserve_selection: bool) {
    let query = read_text(state.edit);
    let selected = SendMessageW(state.list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    let filtered = filter_entries(&state.entries, &query, MAX_RESULTS);
    let selection = refreshed_selection(&state.filtered, selected, &filtered, preserve_selection);
    state.filtered = filtered;
    SendMessageW(state.list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
    for entry in &state.filtered {
        let kind = if matches!(entry.action, CommandAction::ActivateWindow(_)) {
            "切换窗口"
        } else {
            "打开"
        };
        let text = wide_nul(&format!("{}   · {}", entry.label, kind));
        SendMessageW(
            state.list,
            LB_ADDSTRING,
            WPARAM(0),
            LPARAM(text.as_ptr() as isize),
        );
    }
    if let Some(selected) = selection {
        SendMessageW(state.list, LB_SETCURSEL, WPARAM(selected), LPARAM(0));
    } else {
        let text = wide_nul(if state.pending.is_some() {
            "正在后台加载…"
        } else {
            "没有匹配的项目"
        });
        SendMessageW(
            state.list,
            LB_ADDSTRING,
            WPARAM(0),
            LPARAM(text.as_ptr() as isize),
        );
    }
}

unsafe fn run_entry(entry: &CommandEntry) {
    remember(entry.history_key.clone());
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

unsafe fn install_key_subclass(hwnd: HWND) -> bool {
    SetWindowSubclass(hwnd, Some(key_subclass_proc), PALETTE_SUBCLASS_ID, 0).as_bool()
}

unsafe extern "system" fn key_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN
        && GetPropW(hwnd, w!("FeatherDock.IME")).is_invalid()
        && (wparam.0 == VK_RETURN.0 as usize || wparam.0 == VK_ESCAPE.0 as usize)
    {
        if let Ok(parent) = GetParent(hwnd) {
            SendMessageW(parent, msg, wparam, lparam);
        }
        return LRESULT(0);
    }
    if msg == WM_IME_STARTCOMPOSITION {
        let _ = SetPropW(
            hwnd,
            w!("FeatherDock.IME"),
            HANDLE(std::ptr::dangling_mut::<c_void>()),
        );
    }
    if msg == WM_IME_ENDCOMPOSITION {
        let _ = RemovePropW(hwnd, w!("FeatherDock.IME"));
    }
    let composing = !GetPropW(hwnd, w!("FeatherDock.IME")).is_invalid();
    if msg == WM_KEYDOWN && !composing && wparam.0 == 0x09 {
        if let Ok(parent) = GetParent(hwnd) {
            let id = if GetWindowLongPtrW(hwnd, GWLP_ID) == ID_EDIT as isize {
                ID_LIST
            } else {
                ID_EDIT
            };
            if let Ok(target) = GetDlgItem(parent, id as i32) {
                let _ = SetFocus(target);
            }
        }
        return LRESULT(0);
    }
    if msg == WM_KEYDOWN
        && !composing
        && GetWindowLongPtrW(hwnd, GWLP_ID) == ID_EDIT as isize
        && matches!(wparam.0, 0x26 | 0x28 | 0x21 | 0x22)
    {
        if let Ok(parent) = GetParent(hwnd) {
            if let Ok(list) = GetDlgItem(parent, ID_LIST as i32) {
                let count = SendMessageW(list, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0;
                let current = SendMessageW(list, LB_GETCURSEL, WPARAM(0), LPARAM(0))
                    .0
                    .max(0);
                let delta = match wparam.0 {
                    0x26 => -1,
                    0x21 => -8,
                    0x22 => 8,
                    _ => 1,
                };
                if count > 0 {
                    SendMessageW(
                        list,
                        LB_SETCURSEL,
                        WPARAM((current + delta).clamp(0, count - 1) as usize),
                        LPARAM(0),
                    );
                }
            }
        }
        return LRESULT(0);
    }
    if msg == WM_NCDESTROY {
        let _ = RemovePropW(hwnd, w!("FeatherDock.IME"));
        let _ = RemoveWindowSubclass(hwnd, Some(key_subclass_proc), subclass_id);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

unsafe fn collect_async(hwnd: HWND, state: &mut Palette) {
    let (send, receive) = std::sync::mpsc::channel();
    state.pending = Some(receive);
    let owner = hwnd.0 as isize;
    std::thread::spawn(move || {
        let initialized = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
            .is_ok()
        };
        let entries = std::panic::catch_unwind(collect_entries).unwrap_or_default();
        if initialized {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
        if send.send(entries).is_ok() {
            unsafe {
                let _ = PostMessageW(
                    HWND(owner as *mut c_void),
                    WM_ENTRIES_READY,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
}

pub unsafe fn close() {
    let hwnd = HWND(PALETTE_HWND.load(Ordering::Relaxed) as *mut c_void);
    if IsWindow(hwnd).as_bool() {
        let _ = DestroyWindow(hwnd);
    }
}

pub unsafe fn desktop_ready() {
    let hwnd = HWND(PALETTE_HWND.load(Ordering::Relaxed) as *mut c_void);
    if IsWindow(hwnd).as_bool() {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Palette;
        if !ptr.is_null() {
            collect_async(hwnd, &mut *ptr);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ControlLayout {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaletteLayout {
    edit: ControlLayout,
    list: ControlLayout,
    font_height: i32,
}

fn palette_layout(client_width: i32, client_height: i32, dpi_scale: f32) -> PaletteLayout {
    let width = client_width.max(0);
    let height = client_height.max(0);
    let scale = dpi_scale
        .max(0.0)
        .min(width as f32 / 560.0)
        .min(height as f32 / 420.0);
    let s = |value: f32| (value * scale).round() as i32;
    let margin = s(14.0);
    let left = margin.min(width);
    let right = (width - margin).max(left);
    let edit_y = margin.min(height);
    let list_y = s(54.0).min(height);
    PaletteLayout {
        edit: ControlLayout {
            x: left,
            y: edit_y,
            width: right - left,
            height: s(30.0).min(height - edit_y),
        },
        list: ControlLayout {
            x: left,
            y: list_y,
            width: right - left,
            height: (height - list_y - s(46.0)).max(0),
        },
        font_height: (10.0 * scale * 96.0 / 72.0).round().max(1.0) as i32,
    }
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
    let mut parent_rect = RECT::default();
    let _ = GetWindowRect(owner, &mut parent_rect);
    let area = crate::geometry::work_area(parent_rect.left, parent_rect.top);
    let win_w = (rc.right - rc.left).min((area.right - area.left - 16).max(1));
    let win_h = (rc.bottom - rc.top).min((area.bottom - area.top - 16).max(1));
    let x = area.left + (area.right - area.left - win_w) / 2;
    let y = area.top + (area.bottom - area.top - win_h) / 3;

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

    let material = crate::dock_theme(owner).surface();
    let light = material.rgb.0 > 0.5;
    let dark: i32 = i32::from(!light);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const i32 as *const c_void,
        4,
    );

    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let layout = palette_layout(
        client.right - client.left,
        client.bottom - client.top,
        scale,
    );
    let face = wide_nul("Microsoft YaHei UI");
    let font = CreateFontW(
        -layout.font_height,
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
    let bg = CreateSolidBrush(material_color(material.rgb));

    let edit = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        w!("EDIT"),
        w!(""),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32 | WS_TABSTOP.0),
        layout.edit.x,
        layout.edit.y,
        layout.edit.width,
        layout.edit.height,
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
        layout.list.x,
        layout.list.y,
        layout.list.width,
        layout.list.height,
        hwnd,
        HMENU(ID_LIST as *mut c_void),
        instance,
        None,
    )
    .unwrap_or_default();
    for control in [edit, list] {
        let _ = install_key_subclass(control);
    }
    SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    SendMessageW(list, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    let _ = SetWindowTheme(
        edit,
        if light {
            w!("Explorer")
        } else {
            w!("DarkMode_CFD")
        },
        PCWSTR::null(),
    );
    let _ = SetWindowTheme(
        list,
        if light {
            w!("Explorer")
        } else {
            w!("DarkMode_Explorer")
        },
        PCWSTR::null(),
    );

    let mut state = Box::new(Palette {
        edit,
        list,
        entries: Vec::new(),
        filtered: Vec::new(),
        bg,
        font,
        material,
        pending: None,
    });
    collect_async(hwnd, &mut state);
    crate::desktop_scan::refresh_async(owner, false);
    refresh_list(&mut state, false);
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
            WM_ENTRIES_READY if !ptr.is_null() => {
                let state = &mut *ptr;
                if let Some(entries) = state.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
                    state.entries = entries;
                    state.pending = None;
                    refresh_list(state, true);
                }
                LRESULT(0)
            }
            WM_ERASEBKGND if !ptr.is_null() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(hdc, &rc, (*ptr).bg);
                LRESULT(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX if !ptr.is_null() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, material_color((*ptr).material.text_rgb));
                SetBkColor(hdc, material_color((*ptr).material.rgb));
                SetBkMode(hdc, TRANSPARENT);
                LRESULT((*ptr).bg.0 as isize)
            }
            WM_COMMAND if !ptr.is_null() => {
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) & 0xFFFF;
                if id == ID_EDIT && code == EN_CHANGE as usize {
                    refresh_list(&mut *ptr, false);
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
    use std::sync::atomic::AtomicUsize;

    use windows::Win32::UI::Input::KeyboardAndMouse::VK_DOWN;

    use super::*;

    static NEXT_TEST_CLASS_ID: AtomicUsize = AtomicUsize::new(0);

    struct RoutingTestKeys(Vec<(usize, isize)>);

    impl std::ops::Deref for RoutingTestKeys {
        type Target = Vec<(usize, isize)>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl std::ops::DerefMut for RoutingTestKeys {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    extern "system" fn routing_test_parent_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if msg == WM_KEYDOWN {
                let keys = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RoutingTestKeys;
                if !keys.is_null() {
                    (*keys).0.push((wparam.0, lparam.0));
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }

    struct RoutingTestWindows {
        instance: HINSTANCE,
        class_name: Vec<u16>,
        class_registered: bool,
        parent: HWND,
        edit: HWND,
        list: HWND,
        keys: Box<RoutingTestKeys>,
    }

    impl RoutingTestWindows {
        fn new() -> Self {
            unsafe {
                let instance: HINSTANCE =
                    GetModuleHandleW(None).expect("test module handle").into();
                let class_name = wide_nul(&format!(
                    "FeatherDockPaletteSubclassTest.{}.{}",
                    std::process::id(),
                    NEXT_TEST_CLASS_ID.fetch_add(1, Ordering::Relaxed)
                ));
                let mut windows = Self {
                    instance,
                    class_name,
                    class_registered: false,
                    parent: HWND::default(),
                    edit: HWND::default(),
                    list: HWND::default(),
                    keys: Box::new(RoutingTestKeys(Vec::new())),
                };
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(routing_test_parent_wndproc),
                    hInstance: instance,
                    lpszClassName: PCWSTR(windows.class_name.as_ptr()),
                    ..Default::default()
                };
                assert_ne!(RegisterClassExW(&wc), 0, "register test parent class");
                windows.class_registered = true;
                windows.parent = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    PCWSTR(windows.class_name.as_ptr()),
                    w!(""),
                    WS_OVERLAPPED,
                    0,
                    0,
                    200,
                    120,
                    None,
                    None,
                    instance,
                    None,
                )
                .expect("create hidden test parent");
                let keys = &mut *windows.keys as *mut RoutingTestKeys;
                SetWindowLongPtrW(windows.parent, GWLP_USERDATA, keys as isize);
                windows.edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD,
                    0,
                    0,
                    180,
                    24,
                    windows.parent,
                    None,
                    instance,
                    None,
                )
                .expect("create test EDIT");
                windows.list = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("LISTBOX"),
                    w!(""),
                    WS_CHILD | WINDOW_STYLE(LBS_NOINTEGRALHEIGHT as u32),
                    0,
                    30,
                    180,
                    80,
                    windows.parent,
                    None,
                    instance,
                    None,
                )
                .expect("create test LISTBOX");
                assert!(install_key_subclass(windows.edit), "subclass EDIT");
                assert!(install_key_subclass(windows.list), "subclass LISTBOX");
                windows
            }
        }
    }

    impl Drop for RoutingTestWindows {
        fn drop(&mut self) {
            unsafe {
                if IsWindow(self.parent).as_bool() {
                    let _ = DestroyWindow(self.parent);
                }
                if self.class_registered {
                    let _ = UnregisterClassW(PCWSTR(self.class_name.as_ptr()), self.instance);
                }
            }
        }
    }

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

    #[test]
    fn edit_and_listbox_route_only_enter_and_escape_to_parent_once() {
        let mut windows = RoutingTestWindows::new();
        unsafe {
            for (index, label) in ["first", "second"].into_iter().enumerate() {
                let label = wide_nul(label);
                let added = SendMessageW(
                    windows.list,
                    LB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(label.as_ptr() as isize),
                );
                assert_eq!(added.0, index as isize);
            }
            SendMessageW(windows.list, LB_SETCURSEL, WPARAM(0), LPARAM(0));

            for control in [windows.edit, windows.list] {
                for key in [VK_RETURN, VK_ESCAPE] {
                    windows.keys.clear();
                    let key_flags = 0x0001_0001isize;
                    SendMessageW(
                        control,
                        WM_KEYDOWN,
                        WPARAM(key.0 as usize),
                        LPARAM(key_flags),
                    );
                    assert_eq!(
                        windows.keys.as_slice(),
                        &[(key.0 as usize, key_flags)],
                        "parent must receive one unchanged key message"
                    );
                }

                windows.keys.clear();
                SendMessageW(
                    control,
                    WM_KEYDOWN,
                    WPARAM(VK_DOWN.0 as usize),
                    LPARAM(0x0050_0001),
                );
                assert!(
                    windows.keys.is_empty(),
                    "non-Enter/Escape keys must not be forwarded"
                );
            }

            assert_eq!(
                SendMessageW(windows.list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0,
                1,
                "LISTBOX must still handle its native navigation key"
            );
        }
    }
}

#[cfg(test)]
mod search_regression_tests {
    use super::*;

    #[test]
    fn chinese_and_english_aliases_match_in_either_order() {
        for (primary, alias) in [("微信", "WeChat"), ("WeChat", "微信")] {
            let merged = merge_entries(
                vec![
                    CommandEntry::open_path(primary, r"C:\WeChat.exe"),
                    CommandEntry::open_path(alias, r"C:\WeChat.exe"),
                ],
                &[],
            );
            assert_eq!(merged.len(), 1);
            for query in ["wx", "weixin", "wechat", alias] {
                assert!(matches_query(&merged[0], query), "query: {query}");
            }
        }
    }

    #[test]
    fn launch_aliases_merge_but_distinct_windows_remain_reachable() {
        let a = CommandEntry::open_path("微信", r"C:\WeChat.exe");
        let b = CommandEntry::open_path("WeChat", r"C:\WeChat.exe");
        let mut first = CommandEntry::activate_window("聊天一", 1);
        first.history_key = a.history_key.clone();
        let mut second = CommandEntry::activate_window("聊天二", 2);
        second.history_key = a.history_key.clone();
        let merged = merge_entries(vec![a, b, first, second], &[]);
        assert_eq!(merged.len(), 3);
        assert!(matches_query(&merged[0], "wechat"));
        assert!(matches_query(&merged[0], "wx"));
        assert_ne!(merged[1].action_id, merged[2].action_id);
        assert_eq!(merged[1].history_key, merged[2].history_key);
    }

    #[test]
    fn links_with_the_same_target_keep_distinct_actions() {
        let mut first = CommandEntry::open_path("微信", r"C:\Links\WeChat.lnk");
        let mut second = CommandEntry::open_path("工作微信", r"C:\Links\WeChat Work.lnk");
        first.history_key = crate::app_identity::normalize(r"C:\Apps\WeChat.exe");
        second.history_key = first.history_key.clone();
        let duplicate = CommandEntry::open_path("WeChat", first.detail.clone());

        let merged = merge_entries(vec![first, second, duplicate], &[]);

        assert_eq!(merged.len(), 2);
        assert_ne!(merged[0].action_id, merged[1].action_id);
        assert_eq!(merged[0].history_key, merged[1].history_key);
        assert!(matches_query(&merged[0], "wechat"));
        assert!(matches_query(&merged[1], "gongzuoweixin"));
    }

    #[test]
    fn history_breaks_equal_match_ties_without_overriding_a_better_match() {
        let mut recent = CommandEntry::open_path("Visual Studio Code", r"C:\Code.exe");
        recent.used = 50;
        let preferred = CommandEntry::open_path("Code Notes", r"C:\Notes.exe");
        assert_eq!(
            filter_entries(&[recent.clone(), preferred], "code", 10)[0].label,
            "Code Notes"
        );
        let mut older = CommandEntry::open_path("Other", r"C:\Other.exe");
        older.used = 1;
        assert_eq!(filter_entries(&[older, recent], "", 10)[0].used, 50);
    }

    #[test]
    fn async_refresh_preserves_the_second_action_but_query_changes_reset() {
        let previous = vec![
            CommandEntry::open_path("Alpha", r"C:\A.exe"),
            CommandEntry::open_path("Beta", r"C:\B.exe"),
            CommandEntry::open_path("Gamma", r"C:\C.exe"),
        ];
        let current = vec![
            previous[2].clone(),
            previous[0].clone(),
            previous[1].clone(),
        ];

        let selected = refreshed_selection(&previous, 1, &current, true);
        assert_eq!(selected, Some(2));
        assert_eq!(current[selected.unwrap()].action_id, previous[1].action_id);
        assert_eq!(refreshed_selection(&previous, 1, &current, false), Some(0));
    }

    #[test]
    fn direct_exe_and_unique_app_id_share_history_without_merging_ambiguous_apps() {
        let exe = r"C:\Tools\App.exe";
        let running = |app_id: &str| crate::windows_list::RunningWindow {
            hwnd: 1,
            title: "App".into(),
            exe_path: Some(exe.into()),
            app_id: Some(app_id.into()),
        };
        let exe_key = crate::app_identity::normalize(exe);
        let running_entry = |app_id: &str, hwnd| {
            let mut entry = CommandEntry::activate_window("App window", hwnd);
            entry.history_key = app_id.into();
            entry
        };
        let mut entries = vec![
            CommandEntry::open_path("App", exe),
            running_entry("aumid:vendor.one", 1),
        ];
        unify_history_keys(&mut entries, &[running("aumid:vendor.one")]);
        assert!(entries.iter().all(|entry| entry.history_key == exe_key));

        let history = unified_history(
            &[("aumid:vendor.one".to_string(), 100)],
            &entries,
            &[running("aumid:vendor.one")],
        );
        assert_eq!(history, vec![(exe_key.clone(), 100)]);
        let launch_entries = vec![CommandEntry::open_path("App", exe)];
        let helper = |hwnd| crate::windows_list::RunningWindow {
            hwnd,
            title: "Helper".into(),
            exe_path: Some(r"C:\Tools\Helper.exe".into()),
            app_id: Some("aumid:vendor.one".into()),
        };
        for windows in [
            vec![helper(0), running("aumid:vendor.one")],
            vec![running("aumid:vendor.one"), helper(2)],
        ] {
            let history = unified_history(
                &[("aumid:vendor.one".to_string(), 100)],
                &launch_entries,
                &windows,
            );
            assert_eq!(history, vec![(exe_key.clone(), 100)]);
        }
        let merged = merge_entries(entries, &history);
        assert!(merged.iter().all(|entry| entry.used == 100));

        let after_exit = vec![CommandEntry::open_path("App", exe)];
        let history = unified_history(&history, &after_exit, &[]);
        let merged = merge_entries(after_exit, &history);
        assert_eq!(merged[0].used, 100);

        let mut entries = vec![
            CommandEntry::open_path("App", exe),
            running_entry("aumid:vendor.one", 1),
            running_entry("aumid:vendor.two", 2),
        ];
        unify_history_keys(
            &mut entries,
            &[running("aumid:vendor.one"), running("aumid:vendor.two")],
        );
        assert_eq!(entries[0].history_key, exe_key);
        assert_eq!(entries[1].history_key, "aumid:vendor.one");
        assert_eq!(entries[2].history_key, "aumid:vendor.two");
    }

    #[test]
    fn palette_controls_fit_scaled_and_tiny_client_areas() {
        for (width, height, scale) in [
            (560, 420, 1.0),
            (840, 630, 1.5),
            (300, 180, 2.0),
            (24, 16, 3.0),
            (1, 1, 1.0),
        ] {
            let layout = palette_layout(width, height, scale);
            for control in [layout.edit, layout.list] {
                assert!(control.x >= 0 && control.y >= 0);
                assert!(control.width >= 0 && control.height >= 0);
                assert!(control.x + control.width <= width);
                assert!(control.y + control.height <= height);
            }
            assert!(layout.edit.y + layout.edit.height <= layout.list.y);
            assert!(layout.font_height > 0);
        }
        assert_eq!(palette_layout(840, 630, 1.5).font_height, 20);
        assert!(palette_layout(300, 180, 2.0).font_height < 27);
    }
}
