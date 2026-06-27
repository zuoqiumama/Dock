use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
    DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
    EndPaint, FillRect, GetMonitorInfoW, GetStockObject, InvalidateRect, LineTo, MonitorFromPoint,
    MoveToEx, PtInRect, SelectObject, SetBkMode, SetTextColor, UpdateWindow, DT_END_ELLIPSIS,
    DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, NULL_PEN, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dock::RunningWindowRef;
use crate::windows_list;

const MAX_COLUMNS: usize = 4;
const THUMB_W: i32 = 220;
const THUMB_H: i32 = 124;
const HEADER_H: i32 = 24; // top strip per card: title (left) + close button (right)
const CLOSE_D: i32 = 16; // diameter of the red close button
const PAD: i32 = 10;
const GAP: i32 = 10;
const WM_MOUSELEAVE: u32 = 0x02A3;

static PREVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone)]
struct PreviewLayout {
    columns: usize,
    rows: usize,
    width: i32,
    height: i32,
    items: Vec<RECT>,
}

struct PreviewState {
    key: String,
    windows: Vec<RunningWindowRef>,
    layout: PreviewLayout,
    thumbnails: Vec<isize>,
    font: HFONT,
    bg: HBRUSH,
    card: HBRUSH,
    hover: HBRUSH,
    hovered: Option<usize>,
    hovered_close: Option<usize>, // index whose close button the cursor is over
    tracking: bool,
    dpi: f32,
    anchor_x: i32, // dock-icon center the preview is anchored above (for in-place rebuild)
    anchor_y: i32,
}

fn scaled(value: i32, dpi: f32) -> i32 {
    ((value as f32) * dpi).round() as i32
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn preview_layout(count: usize, dpi: f32) -> PreviewLayout {
    let count = count.max(1);
    let columns = count.min(MAX_COLUMNS);
    let rows = count.div_ceil(columns);
    let thumb_w = scaled(THUMB_W, dpi);
    let thumb_h = scaled(THUMB_H, dpi);
    let header_h = scaled(HEADER_H, dpi);
    let pad = scaled(PAD, dpi);
    let gap = scaled(GAP, dpi);
    let width = pad * 2 + columns as i32 * thumb_w + (columns.saturating_sub(1) as i32) * gap;
    let cell_h = header_h + thumb_h;
    let height = pad * 2 + rows as i32 * cell_h + (rows.saturating_sub(1) as i32) * gap;
    let mut items = Vec::with_capacity(count);
    for i in 0..count {
        let col = i % columns;
        let row = i / columns;
        let left = pad + col as i32 * (thumb_w + gap);
        let cell_top = pad + row as i32 * (cell_h + gap);
        // `items[i]` is the live-thumbnail (DWM) area; the header strip sits above it.
        let top = cell_top + header_h;
        items.push(RECT {
            left,
            top,
            right: left + thumb_w,
            bottom: top + thumb_h,
        });
    }
    PreviewLayout {
        columns,
        rows,
        width,
        height,
        items,
    }
}

/// Whole card (header strip + thumbnail) — used for hover highlight + activate hit.
fn card_rect(layout: &PreviewLayout, index: usize, dpi: f32) -> RECT {
    let item = layout.items[index];
    RECT {
        left: item.left,
        top: item.top - scaled(HEADER_H, dpi),
        right: item.right,
        bottom: item.bottom,
    }
}

/// The window title, drawn in the header strip left of the close button.
fn title_rect(layout: &PreviewLayout, index: usize, dpi: f32) -> RECT {
    let item = layout.items[index];
    let close = close_rect(layout, index, dpi);
    RECT {
        left: item.left + scaled(8, dpi),
        top: item.top - scaled(HEADER_H, dpi),
        right: close.left - scaled(6, dpi),
        bottom: item.top,
    }
}

/// The red close button, in the top-right corner of the card's header strip — drawn
/// in the header (NOT over the live thumbnail, which the DWM composites on top of us).
fn close_rect(layout: &PreviewLayout, index: usize, dpi: f32) -> RECT {
    let item = layout.items[index];
    let d = scaled(CLOSE_D, dpi);
    let margin = scaled(4, dpi);
    let header_top = item.top - scaled(HEADER_H, dpi);
    let top = header_top + (scaled(HEADER_H, dpi) - d) / 2;
    RECT {
        left: item.right - d - margin,
        top,
        right: item.right - margin,
        bottom: top + d,
    }
}

fn same_windows(a: &[RunningWindowRef], b: &[RunningWindowRef]) -> bool {
    a.iter()
        .map(|window| window.hwnd)
        .eq(b.iter().map(|window| window.hwnd))
}

unsafe fn state(hwnd: HWND) -> Option<&'static mut PreviewState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PreviewState;
    (!ptr.is_null()).then(|| &mut *ptr)
}

unsafe fn register_class() -> Option<HINSTANCE> {
    let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    if !CLASS_REGISTERED.swap(true, Ordering::Relaxed) {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: w!("FeatherDockPreview"),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            CLASS_REGISTERED.store(false, Ordering::Relaxed);
            return None;
        }
    }
    Some(instance)
}

unsafe fn preview_origin(anchor_x: i32, anchor_y: i32, layout: &PreviewLayout) -> (i32, i32) {
    let point = POINT {
        x: anchor_x,
        y: anchor_y,
    };
    let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let margin = 8;
    let mut x = anchor_x - layout.width / 2;
    let mut y = anchor_y - layout.height - 12;
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        x = x.clamp(
            info.rcWork.left + margin,
            info.rcWork.right - layout.width - margin,
        );
        if y < info.rcWork.top + margin {
            y = anchor_y + 12;
        }
    }
    (x, y)
}

pub unsafe fn show(
    owner: HWND,
    group_key: &str,
    windows: &[RunningWindowRef],
    anchor_x: i32,
    anchor_y: i32,
    dpi: f32,
) {
    if windows.is_empty() {
        hide();
        return;
    }

    let existing_raw = PREVIEW_HWND.load(Ordering::Relaxed);
    if existing_raw != 0 {
        let existing = HWND(existing_raw as *mut c_void);
        if IsWindow(existing).as_bool() {
            if let Some(state) = state(existing) {
                if state.key == group_key && same_windows(&state.windows, windows) {
                    return;
                }
            }
        }
        hide();
    }

    let Some(instance) = register_class() else {
        return;
    };
    let layout = preview_layout(windows.len(), dpi);
    let (x, y) = preview_origin(anchor_x, anchor_y, &layout);
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        w!("FeatherDockPreview"),
        w!("FeatherDockPreview"),
        WS_POPUP,
        x,
        y,
        layout.width,
        layout.height,
        owner,
        None,
        instance,
        None,
    ) else {
        return;
    };

    let face = wide("Microsoft YaHei UI");
    let font = CreateFontW(
        -scaled(13, dpi),
        0,
        0,
        0,
        500,
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
    let state_ptr = Box::into_raw(Box::new(PreviewState {
        key: group_key.to_string(),
        windows: windows.to_vec(),
        layout,
        thumbnails: Vec::new(),
        font,
        bg: CreateSolidBrush(rgb(24, 24, 28)),
        card: CreateSolidBrush(rgb(38, 38, 44)),
        hover: CreateSolidBrush(rgb(52, 58, 68)),
        hovered: None,
        hovered_close: None,
        tracking: false,
        dpi,
        anchor_x,
        anchor_y,
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    PREVIEW_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    register_thumbnails(hwnd, &mut *state_ptr);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = UpdateWindow(hwnd);
}

pub unsafe fn hide() {
    let raw = PREVIEW_HWND.swap(0, Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if IsWindow(hwnd).as_bool() {
        let _ = DestroyWindow(hwnd);
    }
}

pub unsafe fn contains_cursor() -> bool {
    let raw = PREVIEW_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return false;
    }
    let hwnd = HWND(raw as *mut c_void);
    if !IsWindow(hwnd).as_bool() {
        return false;
    }
    let mut pt = POINT::default();
    let mut rect = RECT::default();
    GetCursorPos(&mut pt).is_ok()
        && GetWindowRect(hwnd, &mut rect).is_ok()
        && PtInRect(&rect, pt).as_bool()
}

unsafe fn register_thumbnails(hwnd: HWND, state: &mut PreviewState) {
    for (index, window) in state.windows.iter().enumerate() {
        let source = HWND(window.hwnd as *mut c_void);
        if !IsWindow(source).as_bool() {
            continue;
        }
        let Ok(thumbnail) = DwmRegisterThumbnail(hwnd, source) else {
            continue;
        };
        let source_size = DwmQueryThumbnailSourceSize(thumbnail).unwrap_or(SIZE { cx: 16, cy: 9 });
        let dest = fit_thumbnail_rect(state.layout.items[index], source_size);
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION
                | DWM_TNP_VISIBLE
                | DWM_TNP_OPACITY
                | DWM_TNP_SOURCECLIENTAREAONLY,
            rcDestination: dest,
            opacity: 255,
            fVisible: BOOL(1),
            fSourceClientAreaOnly: BOOL(1),
            ..Default::default()
        };
        if DwmUpdateThumbnailProperties(thumbnail, &props).is_ok() {
            state.thumbnails.push(thumbnail);
        } else {
            let _ = DwmUnregisterThumbnail(thumbnail);
        }
    }
}

fn fit_thumbnail_rect(bounds: RECT, source: SIZE) -> RECT {
    if source.cx <= 0 || source.cy <= 0 {
        return bounds;
    }
    let bw = bounds.right - bounds.left;
    let bh = bounds.bottom - bounds.top;
    let scale = (bw as f32 / source.cx as f32).min(bh as f32 / source.cy as f32);
    let width = (source.cx as f32 * scale).round() as i32;
    let height = (source.cy as f32 * scale).round() as i32;
    let left = bounds.left + (bw - width) / 2;
    let top = bounds.top + (bh - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

unsafe fn paint(hwnd: HWND, state: &PreviewState) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let outer = RECT {
        left: 0,
        top: 0,
        right: state.layout.width,
        bottom: state.layout.height,
    };
    FillRect(hdc, &outer, state.bg);
    let old = SelectObject(hdc, HGDIOBJ(state.font.0));
    SetTextColor(hdc, rgb(238, 238, 242));
    SetBkMode(hdc, TRANSPARENT);
    let dpi = state.dpi;
    for (index, window) in state.windows.iter().enumerate() {
        let card = card_rect(&state.layout, index, dpi);
        FillRect(
            hdc,
            &card,
            if state.hovered == Some(index) {
                state.hover
            } else {
                state.card
            },
        );
        let mut title = title_rect(&state.layout, index, dpi);
        let mut text = wide(&window.title);
        DrawTextW(
            hdc,
            &mut text,
            &mut title,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        // Red close button in the header strip — closes just this one window.
        draw_close_button(
            hdc,
            close_rect(&state.layout, index, dpi),
            state.hovered_close == Some(index),
            dpi,
        );
    }
    let _ = SelectObject(hdc, old);
    let _ = EndPaint(hwnd, &ps);
}

/// Draw a filled red circle with a white "×" — the per-window close button.
unsafe fn draw_close_button(hdc: HDC, rc: RECT, hot: bool, dpi: f32) {
    let fill = if hot {
        rgb(245, 95, 95)
    } else {
        rgb(214, 78, 78)
    };
    let brush = CreateSolidBrush(fill);
    let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
    let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN)); // borderless circle
    let _ = Ellipse(hdc, rc.left, rc.top, rc.right, rc.bottom);

    let inset = (((rc.right - rc.left) as f32) * 0.30).round() as i32;
    let pen_w = ((1.4 * dpi).round() as i32).max(1);
    let cross = CreatePen(PS_SOLID, pen_w, rgb(255, 255, 255));
    let old_cross = SelectObject(hdc, HGDIOBJ(cross.0));
    let (l, t, r, b) = (
        rc.left + inset,
        rc.top + inset,
        rc.right - inset,
        rc.bottom - inset,
    );
    let _ = MoveToEx(hdc, l, t, None);
    let _ = LineTo(hdc, r, b);
    let _ = MoveToEx(hdc, r, t, None);
    let _ = LineTo(hdc, l, b);

    SelectObject(hdc, old_cross);
    let _ = DeleteObject(HGDIOBJ(cross.0));
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(HGDIOBJ(brush.0));
}

fn in_rect(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x <= rc.right && y >= rc.top && y <= rc.bottom
}

fn hit_test(state: &PreviewState, x: i32, y: i32) -> Option<usize> {
    (0..state.windows.len())
        .find(|&index| in_rect(&card_rect(&state.layout, index, state.dpi), x, y))
}

/// Which window's close button (if any) the point is over. Checked before `hit_test`
/// so a click on the × closes that window instead of activating it.
fn close_hit_test(state: &PreviewState, x: i32, y: i32) -> Option<usize> {
    (0..state.windows.len())
        .find(|&index| in_rect(&close_rect(&state.layout, index, state.dpi), x, y))
}

unsafe fn cleanup(hwnd: HWND) {
    if let Some(state) = state(hwnd) {
        for thumbnail in state.thumbnails.drain(..) {
            let _ = DwmUnregisterThumbnail(thumbnail);
        }
        let _ = DeleteObject(HGDIOBJ(state.font.0));
        let _ = DeleteObject(HGDIOBJ(state.bg.0));
        let _ = DeleteObject(HGDIOBJ(state.card.0));
        let _ = DeleteObject(HGDIOBJ(state.hover.0));
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PreviewState;
    if !ptr.is_null() {
        drop(Box::from_raw(ptr));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
    if PREVIEW_HWND.load(Ordering::Relaxed) == hwnd.0 as isize {
        PREVIEW_HWND.store(0, Ordering::Relaxed);
    }
}

/// Close-button handler: drop window `index` from the live preview, re-laying out the
/// remaining cards in place — or hiding the preview if that was the last one. The dock
/// rebuilds too once the window actually closes; this just gives instant feedback.
unsafe fn remove_window(hwnd: HWND, state: &mut PreviewState, index: usize) {
    for thumbnail in state.thumbnails.drain(..) {
        let _ = DwmUnregisterThumbnail(thumbnail);
    }
    if index < state.windows.len() {
        state.windows.remove(index);
    }
    if state.windows.is_empty() {
        hide(); // destroys the window; `state` is freed in WM_DESTROY — don't touch it after
        return;
    }
    let layout = preview_layout(state.windows.len(), state.dpi);
    let (x, y) = preview_origin(state.anchor_x, state.anchor_y, &layout);
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        x,
        y,
        layout.width,
        layout.height,
        SWP_NOACTIVATE,
    );
    state.layout = layout;
    state.hovered = None;
    state.hovered_close = None;
    register_thumbnails(hwnd, state);
    let _ = InvalidateRect(hwnd, None, BOOL(1));
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                if let Some(state) = state(hwnd) {
                    paint(hwnd, state);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_MOUSEMOVE => {
                if let Some(state) = state(hwnd) {
                    if !state.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        state.tracking = true;
                    }
                    let x = (lparam.0 & 0xFFFF) as i16 as i32;
                    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                    let hovered = hit_test(state, x, y);
                    let hovered_close = close_hit_test(state, x, y);
                    if hovered != state.hovered || hovered_close != state.hovered_close {
                        state.hovered = hovered;
                        state.hovered_close = hovered_close;
                        let _ = InvalidateRect(hwnd, None, BOOL(0));
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                hide();
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if let Some(state) = state(hwnd) {
                    let x = (lparam.0 & 0xFFFF) as i16 as i32;
                    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                    // A click on a card's red × closes just that window and keeps the
                    // preview open for the rest (the whole row never closes at once).
                    if let Some(index) = close_hit_test(state, x, y) {
                        let target = state.windows.get(index).map(|window| window.hwnd);
                        if let Some(target) = target {
                            windows_list::close_window(target);
                        }
                        remove_window(hwnd, state, index);
                        return LRESULT(0); // `state` may now be freed — stop here
                    }
                    if let Some(index) = hit_test(state, x, y) {
                        if let Some(window) = state.windows.get(index) {
                            windows_list::activate(window.hwnd);
                        }
                    }
                }
                hide();
                LRESULT(0)
            }
            WM_DESTROY => {
                cleanup(hwnd);
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
    fn preview_layout_wraps_after_four_windows() {
        let layout = preview_layout(5, 1.0);

        assert_eq!(layout.columns, 4);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.items.len(), 5);
        assert!(layout.width > layout.items[3].right);
        assert!(layout.items[4].top > layout.items[0].top);
    }
}
