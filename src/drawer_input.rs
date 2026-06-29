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

struct InputState {
    edit: HWND,
    result: Option<String>,
    done: bool,
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

/// Show the prompt centred on screen, anchored above the dock like the drawer. Returns
/// the trimmed text on OK/Enter, or None on Cancel/Esc/close. Runs a modal loop.
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
    let win_w = rc.right - rc.left;
    let win_h = rc.bottom - rc.top;
    let x = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
    let y = (GetSystemMetrics(SM_CYSCREEN) - win_h) / 2;

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

    let mut state = InputState {
        edit: HWND::default(),
        result: None,
        done: false,
        bg,
    };
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut state as *mut _ as isize);

    let mk = |class: PCWSTR,
              text: &str,
              exstyle: WINDOW_EX_STYLE,
              style: u32,
              id: usize,
              (cx, cy, cw, ch): (i32, i32, i32, i32)|
     -> HWND {
        let label = wide(text);
        let h = CreateWindowExW(
            exstyle,
            class,
            PCWSTR(label.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            s(cx as f32),
            s(cy as f32),
            s(cw as f32),
            s(ch as f32),
            hwnd,
            HMENU(id as *mut c_void),
            instance,
            None,
        )
        .unwrap_or_default();
        SendMessageW(h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        h
    };

    let edit = mk(
        w!("EDIT"),
        initial,
        WS_EX_CLIENTEDGE,
        ES_AUTOHSCROLL as u32 | WS_TABSTOP.0,
        ID_EDIT,
        (14, 14, 272, 26),
    );
    let _ = SetWindowTheme(edit, w!("DarkMode_CFD"), PCWSTR::null());
    state.edit = edit;
    let btn = w!("BUTTON");
    mk(
        btn,
        "确定",
        WINDOW_EX_STYLE(0),
        BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0,
        IDOK,
        (132, 54, 70, 28),
    );
    mk(
        btn,
        "取消",
        WINDOW_EX_STYLE(0),
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        IDCANCEL,
        (216, 54, 70, 28),
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
                SetTextColor(hdc, text_color());
                SetBkColor(hdc, bg_color());
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
