//! Native layout/preferences editor. Preview is transient; only Save writes disk.
use crate::settings::Settings;
use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_PREVIEW_SETTINGS: u32 = WM_APP + 0x41;
pub const WM_SAVED: u32 = WM_APP + 0x42;
static WINDOW: AtomicIsize = AtomicIsize::new(0);
struct Field {
    label: &'static str,
    choices: Vec<(u32, String)>,
}
struct State {
    owner: HWND,
    dock: HWND,
    original: Settings,
    fields: Vec<Field>,
    controls: Vec<HWND>,
    status: HWND,
    font: HFONT,
    committed: bool,
    bg: HBRUSH,
    material: crate::theme::SurfaceTheme,
}

fn surface_color(rgb: (f32, f32, f32)) -> COLORREF {
    COLORREF(
        (rgb.0 * 255.0) as u32 | (((rgb.1 * 255.0) as u32) << 8) | (((rgb.2 * 255.0) as u32) << 16),
    )
}

fn choices(values: &[u32], suffix: &str) -> Vec<(u32, String)> {
    values
        .iter()
        .map(|v| (*v, format!("{v}{suffix}")))
        .collect()
}
fn current(settings: Settings) -> [u32; 11] {
    [
        settings.layout.icon_size,
        settings.layout.magnification,
        settings.layout.bottom_gap,
        settings.layout.opacity,
        settings.layout.preview_delay_ms,
        settings.monitor,
        settings.search_hotkey,
        u32::from(settings.follow_system),
        u32::from(settings.layout.reduce_motion),
        u32::from(settings.intellihide),
        settings.agent_history_minutes,
    ]
}
fn apply(mut settings: Settings, values: [u32; 11]) -> Settings {
    settings.layout.icon_size = values[0];
    settings.layout.magnification = values[1];
    settings.layout.bottom_gap = values[2];
    settings.layout.opacity = values[3];
    settings.layout.preview_delay_ms = values[4];
    settings.monitor = values[5];
    settings.search_hotkey = values[6];
    settings.follow_system = values[7] != 0;
    settings.layout.reduce_motion = values[8] != 0;
    settings.intellihide = values[9] != 0;
    settings.agent_history_minutes = values[10];
    settings
}

pub fn merge_preview(mut committed: Settings, preview: Settings) -> Settings {
    committed.layout = preview.layout;
    committed.monitor = preview.monitor;
    committed.search_hotkey = preview.search_hotkey;
    committed.follow_system = preview.follow_system;
    committed.intellihide = preview.intellihide;
    committed.agent_history_minutes = preview.agent_history_minutes;
    committed
}

unsafe fn draft(state: &State) -> Settings {
    let mut values = current(state.original);
    for (index, control) in state.controls.iter().enumerate() {
        let selected = SendMessageW(*control, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
        if let Some((value, _)) = state.fields[index].choices.get(selected as usize) {
            values[index] = *value;
        }
    }
    apply(state.original, values)
}
unsafe fn send_preview(state: &State, settings: Settings) {
    // Synchronous ownership transfer: no queued Box survives destruction/shutdown.
    SendMessageW(
        state.dock,
        WM_PREVIEW_SETTINGS,
        WPARAM(0),
        LPARAM(Box::into_raw(Box::new(settings)) as isize),
    );
}
#[allow(clippy::too_many_arguments)]
unsafe fn control(
    parent: HWND,
    class: PCWSTR,
    label: &str,
    style: u32,
    id: usize,
    bounds: (i32, i32, i32, i32),
    scale: f32,
    font: HFONT,
) -> HWND {
    let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
    let (x, y, w, h) = bounds;
    let window = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(style),
        (x as f32 * scale) as i32,
        (y as f32 * scale) as i32,
        (w as f32 * scale) as i32,
        (h as f32 * scale) as i32,
        parent,
        HMENU(id as *mut c_void),
        None,
        None,
    )
    .unwrap_or_default();
    SendMessageW(window, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    window
}

pub unsafe fn show(owner: HWND, dock: HWND, settings: Settings) {
    let existing = HWND(WINDOW.load(Ordering::Relaxed) as *mut c_void);
    if IsWindow(existing).as_bool() {
        let _ = SetForegroundWindow(existing);
        return;
    }
    let Ok(module) = GetModuleHandleW(None) else {
        return;
    };
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: module.into(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: GetSysColorBrush(COLOR_WINDOW),
        lpszClassName: w!("FeatherDockPreferences"),
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let mut bounds = RECT::default();
    let _ = GetWindowRect(owner, &mut bounds);
    let area = crate::geometry::work_area(bounds.left, bounds.top);
    let dpi = GetDpiForWindow(owner).max(96) as f32 / 96.0;
    let scale = dpi
        .min((area.bottom - area.top - 48).max(1) as f32 / 586.0)
        .min((area.right - area.left - 24).max(1) as f32 / 450.0);
    let width = (450.0 * scale) as i32;
    let height = (586.0 * scale) as i32;
    let x = crate::geometry::fit_axis(bounds.left, width, area.left, area.right, 8);
    let y = crate::geometry::fit_axis(bounds.top, height, area.top, area.bottom, 8);
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        w!("FeatherDockPreferences"),
        w!("布局与快捷键"),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        x,
        y,
        width,
        height,
        owner,
        None,
        module,
        None,
    ) else {
        return;
    };
    let font = CreateFontW(
        -(13.0 * scale) as i32,
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
        w!("Microsoft YaHei UI"),
    );
    let bools = || vec![(0, "关闭".into()), (1, "开启".into())];
    let mut monitors = vec![(0, "跟随主显示器".into())];
    monitors.extend(
        crate::geometry::displays()
            .into_iter()
            .map(|d| (d.id, d.label)),
    );
    let mut fields = vec![
        Field {
            label: "图标大小",
            choices: choices(&[24, 32, 40, 44, 48, 50, 56, 64, 72, 80], " px"),
        },
        Field {
            label: "悬停放大",
            choices: choices(&[100, 120, 140, 160, 180, 190, 200, 220], "%"),
        },
        Field {
            label: "底部距离",
            choices: choices(&[0, 4, 8, 12, 16, 24, 32], " px"),
        },
        Field {
            label: "玻璃底色不透明度",
            choices: choices(&[10, 20, 30, 40, 48, 50, 60, 70, 80, 90, 100], "%"),
        },
        Field {
            label: "窗口预览延迟",
            choices: choices(&[0, 150, 250, 300, 400, 600, 1000, 1500], " ms"),
        },
        Field {
            label: "Dock 所在屏幕",
            choices: monitors,
        },
        Field {
            label: "搜索快捷键",
            choices: vec![
                (0, "禁用".into()),
                (1, "Ctrl + Alt + Space".into()),
                (2, "Alt + Space".into()),
                (3, "Ctrl + Alt + Q".into()),
            ],
        },
        Field {
            label: "跟随系统深浅色",
            choices: bools(),
        },
        Field {
            label: "减少动画",
            choices: bools(),
        },
        Field {
            label: "仅在窗口重叠时避让",
            choices: bools(),
        },
        Field {
            label: "Agent 空闲历史",
            choices: vec![
                (30, "最近 30 分钟".into()),
                (60, "最近 1 小时".into()),
                (360, "最近 6 小时".into()),
                (1440, "最近 24 小时".into()),
                (0, "全部历史".into()),
            ],
        },
    ];
    let values = current(settings);
    let mut controls = Vec::new();
    for (index, field) in fields.iter_mut().enumerate() {
        if !field.choices.iter().any(|(v, _)| *v == values[index]) {
            field
                .choices
                .push((values[index], format!("{}（当前值）", values[index])));
        }
        let y = 16 + index as i32 * 36;
        control(
            hwnd,
            w!("STATIC"),
            field.label,
            0,
            0,
            (16, y + 5, 180, 24),
            scale,
            font,
        );
        let combo = control(
            hwnd,
            w!("COMBOBOX"),
            "",
            CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0,
            100 + index,
            (204, y, 220, 200),
            scale,
            font,
        );
        for (_, label) in &field.choices {
            let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            SendMessageW(
                combo,
                CB_ADDSTRING,
                WPARAM(0),
                LPARAM(text.as_ptr() as isize),
            );
        }
        let selected = field
            .choices
            .iter()
            .position(|(v, _)| *v == values[index])
            .unwrap_or(0);
        SendMessageW(combo, CB_SETCURSEL, WPARAM(selected), LPARAM(0));
        controls.push(combo);
    }
    let status = control(
        hwnd,
        w!("STATIC"),
        if crate::settings_window::hotkey_conflict() {
            "搜索快捷键被占用，请更换组合。"
        } else {
            "预览不会保存；取消会恢复原来的布局。"
        },
        0,
        0,
        (16, 418, 408, 48),
        scale,
        font,
    );
    control(
        hwnd,
        w!("BUTTON"),
        "预览",
        BS_PUSHBUTTON as u32,
        201,
        (16, 486, 90, 28),
        scale,
        font,
    );
    control(
        hwnd,
        w!("BUTTON"),
        "恢复默认",
        BS_PUSHBUTTON as u32,
        204,
        (114, 486, 90, 28),
        scale,
        font,
    );
    control(
        hwnd,
        w!("BUTTON"),
        "保存",
        BS_DEFPUSHBUTTON as u32,
        202,
        (236, 486, 90, 28),
        scale,
        font,
    );
    control(
        hwnd,
        w!("BUTTON"),
        "取消",
        BS_PUSHBUTTON as u32,
        203,
        (334, 486, 90, 28),
        scale,
        font,
    );
    let state = Box::new(State {
        owner,
        dock,
        original: settings,
        fields,
        controls,
        status,
        font,
        committed: false,
        bg: CreateSolidBrush(surface_color(settings.effective_theme().surface().rgb)),
        material: settings.effective_theme().surface(),
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
    crate::settings_window::apply_window_theme(hwnd, settings.effective_theme());
    let _ = EnableWindow(owner, false);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

pub unsafe fn route_dialog(msg: &MSG) -> bool {
    let hwnd = HWND(WINDOW.load(Ordering::Relaxed) as *mut c_void);
    !hwnd.is_invalid()
        && (msg.hwnd == hwnd || IsChild(hwnd, msg.hwnd).as_bool())
        && IsDialogMessageW(hwnd, msg).as_bool()
}

pub unsafe fn close() {
    let hwnd = HWND(WINDOW.load(Ordering::Relaxed) as *mut c_void);
    if IsWindow(hwnd).as_bool() {
        let _ = DestroyWindow(hwnd);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        match msg {
            WM_ERASEBKGND if !ptr.is_null() => {
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                FillRect(HDC(wparam.0 as *mut c_void), &rect, (*ptr).bg);
                LRESULT(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN
                if !ptr.is_null() =>
            {
                let state = &*ptr;
                let dc = HDC(wparam.0 as *mut c_void);
                SetTextColor(dc, surface_color(state.material.text_rgb));
                SetBkColor(dc, surface_color(state.material.rgb));
                LRESULT(state.bg.0 as isize)
            }
            WM_COMMAND if !ptr.is_null() => {
                let state = &mut *ptr;
                match wparam.0 & 0xffff {
                    201 => {
                        let settings = draft(state);
                        state.material = settings.effective_theme().surface();
                        let brush = CreateSolidBrush(surface_color(state.material.rgb));
                        let old = std::mem::replace(&mut state.bg, brush);
                        let _ = DeleteObject(HGDIOBJ(old.0));
                        crate::settings_window::apply_window_theme(
                            hwnd,
                            settings.effective_theme(),
                        );
                        let _ = InvalidateRect(hwnd, None, BOOL(1));
                        send_preview(state, settings);
                        let _ = SetWindowTextW(
                            state.status,
                            if crate::settings_window::hotkey_conflict() {
                                w!("搜索快捷键被占用，请更换组合。")
                            } else {
                                w!("正在预览；取消会恢复原布局，保存才会写入配置。")
                            },
                        );
                    }
                    202 => {
                        let settings = merge_preview(crate::settings::load(), draft(state));
                        match crate::settings::save(&settings) {
                            Ok(()) => {
                                state.committed = true;
                                SendMessageW(state.owner, WM_SAVED, WPARAM(0), LPARAM(0));
                                let _ = PostMessageW(
                                    state.dock,
                                    crate::settings_window::WM_SETTINGS_CHANGED,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                                let _ = DestroyWindow(hwnd);
                            }
                            Err(error) => {
                                crate::error_log::write("保存布局失败", &error);
                                let _ = SetWindowTextW(
                                    state.status,
                                    w!("保存失败，配置未更改；可取消恢复原布局。"),
                                );
                            }
                        }
                    }
                    2 | 203 => {
                        let _ = DestroyWindow(hwnd);
                    }
                    204 => {
                        let defaults = current(Settings::default());
                        for (index, control) in state.controls.iter().enumerate() {
                            if let Some(selected) = state.fields[index]
                                .choices
                                .iter()
                                .position(|(v, _)| *v == defaults[index])
                            {
                                SendMessageW(*control, CB_SETCURSEL, WPARAM(selected), LPARAM(0));
                            }
                        }
                    }
                    _ => {}
                };
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                WINDOW.store(0, Ordering::Relaxed);
                if !ptr.is_null() {
                    let state = Box::from_raw(ptr);
                    let _ = EnableWindow(state.owner, true);
                    if !state.committed && IsWindow(state.dock).as_bool() {
                        send_preview(&state, state.original);
                    }
                    let _ = DeleteObject(HGDIOBJ(state.font.0));
                    let _ = DeleteObject(HGDIOBJ(state.bg.0));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_draft_preserves_system_policy_and_can_be_discarded() {
        let original = Settings {
            hide_desktop_icons: true,
            ..Default::default()
        };
        let mut values = current(original);
        values[0] = 64;
        values[6] = 0;
        values[10] = 1440;
        let draft = apply(original, values);
        assert_eq!(draft.layout.icon_size, 64);
        assert_eq!(draft.search_hotkey, 0);
        assert_eq!(draft.agent_history_minutes, 1440);
        assert!(draft.hide_desktop_icons);
        assert_eq!(original.layout.icon_size, 50);
        assert_eq!(original.agent_history_minutes, 30);
    }

    #[test]
    fn cancel_preview_preserves_committed_tray_changes() {
        let original = Settings::default();
        let preview = Settings {
            dock_mode: crate::settings::DockMode::AutoHide,
            ..original
        };
        let mut values = current(preview);
        values[0] = 72;
        values[10] = 60;
        let preview = apply(preview, values);
        let committed = Settings {
            dock_mode: crate::settings::DockMode::AutoHide,
            ..original
        };

        let effective = merge_preview(committed, preview);
        assert_eq!(effective.layout.icon_size, 72);
        assert_eq!(effective.agent_history_minutes, 60);
        assert_eq!(effective.dock_mode, crate::settings::DockMode::AutoHide);

        let canceled = merge_preview(committed, original);
        assert_eq!(canceled.layout.icon_size, 50);
        assert_eq!(canceled.agent_history_minutes, 30);
        assert_eq!(canceled.dock_mode, crate::settings::DockMode::AutoHide);
    }
}
