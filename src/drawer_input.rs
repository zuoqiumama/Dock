//! A tiny dark text-input popup, used by the app drawer to name / rename a category.
//!
//! It is a standalone top-level window (NOT a child of the drawer): the drawer is a
//! `WS_EX_NOREDIRECTIONBITMAP` + DirectComposition surface, over which a GDI child EDIT
//! may not composite — and a separate window also gives us the system EDIT control's
//! IME for free (so Chinese category names just work). `prompt` runs a small modal
//! message loop and returns the entered text, or None if cancelled.

use core::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, FillRect, SetBkColor, SetBkMode, SetTextColor,
    HBRUSH, HDC, HGDIOBJ, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

const IDOK: usize = 1;
const IDCANCEL: usize = 2;
const ID_EDIT: usize = 3;
const EM_SETSEL: u32 = 0x00B1;
const INPUT_LOGICAL_WIDTH: f32 = 300.0;
const INPUT_LOGICAL_HEIGHT: f32 = 132.0;

type InputRect = (i32, i32, i32, i32);

fn effective_input_scale(dpi_scale: f32, client_width: i32, client_height: i32) -> f32 {
    dpi_scale
        .min(client_width.max(0) as f32 / INPUT_LOGICAL_WIDTH)
        .min(client_height.max(0) as f32 / INPUT_LOGICAL_HEIGHT)
        .max(0.0)
}

fn input_child_rects(scale: f32) -> [InputRect; 3] {
    [(14, 14, 272, 26), (132, 54, 70, 28), (216, 54, 70, 28)].map(|(x, y, width, height)| {
        (
            (x as f32 * scale) as i32,
            (y as f32 * scale) as i32,
            (width as f32 * scale) as i32,
            (height as f32 * scale) as i32,
        )
    })
}

fn input_font_height(scale: f32) -> i32 {
    (9.0 * scale * 96.0 / 72.0).max(1.0) as i32
}

struct InputState {
    edit: HWND,
    result: Option<String>,
    done: bool,
    bg: HBRUSH,
    material: crate::theme::SurfaceTheme,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn material_color(rgb: (f32, f32, f32)) -> COLORREF {
    COLORREF(
        (rgb.0 * 255.0).round() as u32
            | (((rgb.1 * 255.0).round() as u32) << 8)
            | (((rgb.2 * 255.0).round() as u32) << 16),
    )
}

pub unsafe fn prompt(parent: HWND, title: &str, initial: &str) -> Option<String> {
    let instance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(module) => module.into(),
        Err(_) => return None,
    };
    let class_name = w!("FeatherDockInput");
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

    let scale = (GetDpiForWindow(parent).max(96) as f32) / 96.0;
    let s = |v: f32| (v * scale) as i32;
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: s(300.0),
        bottom: s(132.0),
    };
    let _ = AdjustWindowRectEx(&mut rc, style, false, WS_EX_DLGMODALFRAME);
    let mut parent_rect = RECT::default();
    let _ = GetWindowRect(parent, &mut parent_rect);
    let area = crate::geometry::work_area(parent_rect.left, parent_rect.top);
    let win_w = (rc.right - rc.left).min((area.right - area.left - 16).max(1));
    let win_h = (rc.bottom - rc.top).min((area.bottom - area.top - 16).max(1));
    let x = area.left + (area.right - area.left - win_w) / 2;
    let y = area.top + (area.bottom - area.top - win_h) / 3;

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_DLGMODALFRAME | WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
        class_name,
        &HSTRING::from(title),
        style,
        x,
        y,
        win_w,
        win_h,
        parent,
        None,
        instance,
        None,
    ) else {
        return None;
    };

    let mut client_rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut client_rect);
    let scale = effective_input_scale(
        scale,
        client_rect.right - client_rect.left,
        client_rect.bottom - client_rect.top,
    );

    let material = crate::settings::load().effective_theme().surface();
    let light = material.rgb.0 > 0.5;
    let dark: i32 = i32::from(!light);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const i32 as *const c_void,
        4,
    );

    let height = -input_font_height(scale);
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
    let bg = CreateSolidBrush(material_color(material.rgb));

    let mut state = InputState {
        edit: HWND::default(),
        result: None,
        done: false,
        bg,
        material,
    };
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut state as *mut _ as isize);

    let mk = |class: PCWSTR,
              text: &str,
              exstyle: WINDOW_EX_STYLE,
              style: u32,
              id: usize,
              (x, y, width, height): InputRect|
     -> HWND {
        let label = wide(text);
        let h = CreateWindowExW(
            exstyle,
            class,
            PCWSTR(label.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            x,
            y,
            width,
            height,
            hwnd,
            HMENU(id as *mut c_void),
            instance,
            None,
        )
        .unwrap_or_default();
        SendMessageW(h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        h
    };

    let [edit_rect, ok_rect, cancel_rect] = input_child_rects(scale);
    let edit = mk(
        w!("EDIT"),
        initial,
        WS_EX_CLIENTEDGE,
        ES_AUTOHSCROLL as u32 | WS_TABSTOP.0,
        ID_EDIT,
        edit_rect,
    );
    let _ = SetWindowTheme(
        edit,
        if light {
            w!("Explorer")
        } else {
            w!("DarkMode_CFD")
        },
        PCWSTR::null(),
    );
    state.edit = edit;
    let btn = w!("BUTTON");
    mk(
        btn,
        "确定",
        WINDOW_EX_STYLE(0),
        BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0,
        IDOK,
        ok_rect,
    );
    mk(
        btn,
        "取消",
        WINDOW_EX_STYLE(0),
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        IDCANCEL,
        cancel_rect,
    );

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(edit);
    // Select the whole initial text so typing replaces it.
    SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));

    // Modal loop: IsDialogMessageW gives us Tab/Enter(OK)/Esc(Cancel) handling. The
    // IsWindow guard bails if the popup is torn down externally (e.g. the owning drawer
    // is dismissed), so the loop can never spin on a dead window.
    let mut msg = MSG::default();
    while !state.done && IsWindow(hwnd).as_bool() {
        let got = GetMessageW(&mut msg, None, 0, 0);
        if !got.as_bool() {
            // WM_QUIT — re-post so the app's main loop also sees it, then bail.
            PostQuitMessage(msg.wParam.0 as i32);
            break;
        }
        if IsDialogMessageW(hwnd, &msg).as_bool() {
            continue;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    if IsWindow(hwnd).as_bool() {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
    }
    let _ = DeleteObject(HGDIOBJ(font.0));
    let _ = DeleteObject(HGDIOBJ(bg.0));
    state.result
}

unsafe fn read_edit(edit: HWND) -> String {
    let len = GetWindowTextLengthW(edit);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(edit, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut InputState;
        match msg {
            WM_ERASEBKGND if !ptr.is_null() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(hdc, &rc, (*ptr).bg);
                LRESULT(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if !ptr.is_null() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, material_color((*ptr).material.text_rgb));
                SetBkColor(hdc, material_color((*ptr).material.rgb));
                SetBkMode(hdc, TRANSPARENT);
                LRESULT((*ptr).bg.0 as isize)
            }
            WM_COMMAND if !ptr.is_null() => {
                let id = wparam.0 & 0xFFFF;
                match id {
                    IDOK => {
                        let text = read_edit((*ptr).edit);
                        (*ptr).result = Some(text.trim().to_string());
                        (*ptr).done = true;
                    }
                    IDCANCEL => {
                        (*ptr).result = None;
                        (*ptr).done = true;
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE if !ptr.is_null() => {
                (*ptr).done = true;
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_input_scale, input_child_rects, input_font_height};

    #[test]
    fn actual_client_area_bounds_every_child_and_font() {
        let cases: [(i32, i32, f32); 5] = [
            (600, 264, 2.0),
            (120, 264, 0.4),
            (600, 66, 0.5),
            (120, 90, 0.4),
            (1, 1, 1.0 / 300.0),
        ];
        for (client_width, client_height, expected_scale) in cases {
            let scale = effective_input_scale(2.0, client_width, client_height);
            assert!((scale - expected_scale).abs() < 1e-4);
            for (x, y, width, height) in input_child_rects(scale) {
                assert!(x >= 0 && y >= 0 && width >= 0 && height >= 0);
                assert!(x + width <= client_width, "edit/button exceeds width");
                assert!(y + height <= client_height, "edit/button exceeds height");
            }
            let font_height = input_font_height(scale);
            assert!(font_height > 0);
            assert!(font_height <= input_font_height(2.0));
            assert!(font_height <= client_height);
        }
    }

    #[test]
    fn empty_client_area_collapses_instead_of_using_the_original_dpi_scale() {
        for (width, height) in [(0, 100), (100, 0), (-1, -1)] {
            assert_eq!(effective_input_scale(2.0, width, height), 0.0);
            assert!(input_child_rects(0.0)
                .iter()
                .all(|&(x, y, width, height)| x == 0 && y == 0 && width == 0 && height == 0));
        }
    }
}
