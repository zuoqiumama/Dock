//! Warm Glass hover card for an Agent slot: lists active sessions and open GUI
//! windows, updates live while open, and forwards clicks back to the Dock.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_POINT_2F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    ID2D1DeviceContext, ID2D1SolidColorBrush, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE,
    D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PtInRect, HBRUSH, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, GetWindowLongPtrW, GetWindowRect, IsWindow, IsWindowVisible, KillTimer,
    LoadCursorW, PostMessageW, RegisterClassExW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TrackPopupMenu, GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW,
    MA_NOACTIVATE, MF_STRING, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE,
    TPM_NONOTIFY, TPM_RETURNCMD, WM_APP, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::agent_state::{self, AgentDockSummary, AgentState, SessionSource};
use crate::geometry;
use crate::glass::Glass;
use crate::theme::ThemePreset;

const WM_MOUSELEAVE: u32 = 0x02A3;
pub const WM_AGENT_POPUP_CLOSED: u32 = WM_APP + 0x52;
pub const WM_AGENT_POPUP_ACTIVATED: u32 = WM_APP + 0x53;

const POPUP_CLASS_NAME: windows::core::PCWSTR = w!("FeatherDockAgentPopup");
const TIMER_LEAVE_CHECK: usize = 1;
const LEAVE_GRACE_MS: u32 = 140;
const MAX_VISIBLE_ROWS: usize = 6;
const CARD_WIDTH_LOGICAL: f32 = 324.0;
const CARD_PAD_X: f32 = 12.0;
const CARD_PAD_Y: f32 = 10.0;
const HEADER_HEIGHT: f32 = 34.0;
const ROW_HEIGHT: f32 = 52.0;
const ROW_GAP: f32 = 5.0;
const FOOTER_HEIGHT: f32 = 24.0;
const ANCHOR_GAP: f32 = 10.0;
const WORK_MARGIN: i32 = 8;

static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
static POPUP_HWND: AtomicIsize = AtomicIsize::new(0);
static PENDING_ACTIVATION: Mutex<Option<PopupActivation>> = Mutex::new(None);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupActivation {
    pub agent_id: String,
    pub session_id: Option<String>,
    pub hwnd: Option<isize>,
    pub pid: Option<u32>,
    pub deep_link: Option<String>,
    pub launch_path: Option<String>,
}

pub fn take_activation() -> Option<PopupActivation> {
    PENDING_ACTIVATION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

#[derive(Clone, Debug)]
struct PopupLayout {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    visible_rows: usize,
    overflow_rows: usize,
    pad_x: f32,
    header_top: f32,
    header_bottom: f32,
    rows_top: f32,
    row_height: f32,
    row_gap: f32,
    footer_top: f32,
    footer_height: f32,
    radius: f32,
}

impl PopupLayout {
    fn row_rect(&self, index: usize) -> Option<(f32, f32, f32, f32)> {
        if index >= self.visible_rows {
            return None;
        }
        let top = self.rows_top + index as f32 * (self.row_height + self.row_gap);
        let bottom = top + self.row_height;
        Some((self.pad_x, top, self.width as f32 - self.pad_x, bottom))
    }

    fn row_at(&self, px: f32, py: f32) -> Option<usize> {
        if px < self.pad_x || px > self.width as f32 - self.pad_x {
            return None;
        }
        (0..self.visible_rows).find(|&idx| {
            let Some((_, t, _, b)) = self.row_rect(idx) else {
                return false;
            };
            py >= t && py <= b
        })
    }

    fn footer_rect(&self) -> Option<(f32, f32, f32, f32)> {
        (self.overflow_rows > 0).then_some((
            self.pad_x,
            self.footer_top,
            self.width as f32 - self.pad_x,
            self.footer_top + self.footer_height,
        ))
    }

    fn footer_hit(&self, px: f32, py: f32) -> bool {
        self.footer_rect()
            .is_some_and(|(l, t, r, b)| px >= l && px <= r && py >= t && py <= b)
    }
}

fn max_rows_for_height(avail_h: f32, row_h: f32, row_gap: f32) -> usize {
    if avail_h < row_h {
        1
    } else {
        1 + ((avail_h - row_h) / (row_h + row_gap)).floor() as usize
    }
}

fn compute_layout(
    session_count: usize,
    dpi: f32,
    work: RECT,
    anchor_x: i32,
    anchor_y: i32,
) -> PopupLayout {
    let dpi = dpi.max(1.0);
    let pad_x = (CARD_PAD_X * dpi).round();
    let pad_y = (CARD_PAD_Y * dpi).round();
    let header_h = (HEADER_HEIGHT * dpi).round();
    let mut row_h = (ROW_HEIGHT * dpi).round();
    let row_gap = (ROW_GAP * dpi).round();
    let header_divider_gap = (8.0 * dpi).round();
    let footer_gap = (4.0 * dpi).round();
    let full_footer_h = (FOOTER_HEIGHT * dpi).round();

    let max_w = (work.right - work.left - 2 * WORK_MARGIN).max(200);
    let width = ((CARD_WIDTH_LOGICAL * dpi).round() as i32).clamp(200, max_w);
    let max_h = (work.bottom - work.top - 2 * WORK_MARGIN).max(120);

    let overhead_no_footer = pad_y + header_h + header_divider_gap + pad_y;
    let overhead_with_footer = overhead_no_footer + footer_gap + full_footer_h;

    let (visible_rows, overflow_rows, footer_h) = if session_count == 0 {
        let avail = (max_h as f32 - overhead_no_footer).max(24.0);
        if row_h > avail {
            row_h = avail;
        }
        (1, 0, 0.0)
    } else {
        let avail_no_footer = (max_h as f32 - overhead_no_footer).max(0.0);
        let max_no_footer = max_rows_for_height(avail_no_footer, row_h, row_gap);
        if session_count <= MAX_VISIBLE_ROWS
            && session_count <= max_no_footer
            && avail_no_footer >= row_h
        {
            (session_count, 0, 0.0)
        } else {
            let avail_with_footer = (max_h as f32 - overhead_with_footer).max(24.0);
            if row_h > avail_with_footer {
                row_h = avail_with_footer;
            }
            let max_with_footer =
                max_rows_for_height(avail_with_footer, row_h, row_gap).clamp(1, MAX_VISIBLE_ROWS);
            let visible = session_count.min(max_with_footer).max(1);
            let overflow = session_count.saturating_sub(visible);
            let fh = if overflow > 0 { full_footer_h } else { 0.0 };
            (visible, overflow, fh)
        }
    };

    let rows_h = visible_rows as f32 * row_h + visible_rows.saturating_sub(1) as f32 * row_gap;
    let footer_block = if overflow_rows > 0 {
        footer_gap + footer_h
    } else {
        0.0
    };
    let natural_h = (overhead_no_footer + rows_h + footer_block).ceil() as i32;
    let height = natural_h.min(max_h);

    let x_min = work.left + WORK_MARGIN;
    let x_max = (work.right - WORK_MARGIN - width).max(x_min);
    let x = (anchor_x - width / 2).clamp(x_min, x_max);

    let gap = (ANCHOR_GAP * dpi).round() as i32;
    let preferred_y = anchor_y - height - gap;
    let y_min = work.top + WORK_MARGIN;
    let y_max = (work.bottom - WORK_MARGIN - height).max(y_min);
    let y = if preferred_y < y_min && anchor_y + gap + height <= work.bottom - WORK_MARGIN {
        anchor_y + gap
    } else {
        preferred_y.clamp(y_min, y_max)
    };

    let header_top = pad_y;
    let header_bottom = header_top + header_h;
    let rows_top = header_bottom + header_divider_gap;
    let footer_top = rows_top + rows_h + footer_gap;

    PopupLayout {
        x,
        y,
        width,
        height,
        visible_rows,
        overflow_rows,
        pad_x,
        header_top,
        header_bottom,
        rows_top,
        row_height: row_h,
        row_gap,
        footer_top,
        footer_height: footer_h,
        radius: 16.0 * dpi,
    }
}

struct TextFormats {
    header: IDWriteTextFormat,
    pill: IDWriteTextFormat,
    title: IDWriteTextFormat,
    detail: IDWriteTextFormat,
    meta: IDWriteTextFormat,
    footer: IDWriteTextFormat,
}

impl TextFormats {
    unsafe fn new(dpi: f32) -> windows::core::Result<Self> {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let header = create_trimmed_format(
            &factory,
            13.5 * dpi,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        let pill = create_trimmed_format(
            &factory,
            10.5 * dpi,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
        let title = create_trimmed_format(
            &factory,
            12.5 * dpi,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        let detail = create_trimmed_format(
            &factory,
            11.0 * dpi,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        let meta = create_trimmed_format(
            &factory,
            10.5 * dpi,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_TEXT_ALIGNMENT_TRAILING,
        )?;
        let footer = create_trimmed_format(
            &factory,
            10.5 * dpi,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
        Ok(Self {
            header,
            pill,
            title,
            detail,
            meta,
            footer,
        })
    }
}

unsafe fn create_trimmed_format(
    factory: &IDWriteFactory,
    size: f32,
    weight: windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT,
    align: windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_ALIGNMENT,
) -> windows::core::Result<IDWriteTextFormat> {
    let fmt = factory.CreateTextFormat(
        w!("Microsoft YaHei UI"),
        None,
        weight,
        DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_STRETCH_NORMAL,
        size,
        w!(""),
    )?;
    fmt.SetTextAlignment(align)?;
    fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    let trimming = DWRITE_TRIMMING {
        granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
        delimiter: 0,
        delimiterCount: 0,
    };
    let sign = factory.CreateEllipsisTrimmingSign(&fmt)?;
    fmt.SetTrimming(&trimming, &sign)?;
    Ok(fmt)
}

struct PopupState {
    owner: HWND,
    glass: Glass,
    brush: ID2D1SolidColorBrush,
    formats: TextFormats,
    format_dpi: f32,
    theme: ThemePreset,
    dpi: f32,
    anchor_x: i32,
    anchor_y: i32,
    summary: AgentDockSummary,
    launch_path: Option<String>,
    layout: PopupLayout,
    scroll_offset: usize,
    hovered_row: Option<usize>,
    hovered_footer: bool,
    tracking_leave: bool,
    in_menu: bool,
}

fn ensure_class() -> Option<HINSTANCE> {
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None).ok()?.into() };
    if !CLASS_REGISTERED.swap(true, Ordering::Relaxed) {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: POPUP_CLASS_NAME,
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&wc) } == 0 {
            CLASS_REGISTERED.store(false, Ordering::Relaxed);
            return None;
        }
    }
    Some(instance)
}

pub unsafe fn owns_window(hwnd: HWND) -> bool {
    let raw = POPUP_HWND.load(Ordering::Relaxed);
    raw != 0 && hwnd.0 as isize == raw
}

pub unsafe fn active_agent() -> Option<String> {
    let raw = POPUP_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return None;
    }
    let hwnd = HWND(raw as *mut c_void);
    if !IsWindow(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() {
        return None;
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PopupState;
    (!ptr.is_null()).then(|| (*ptr).summary.agent_id.clone())
}

pub unsafe fn contains_cursor() -> bool {
    let raw = POPUP_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return false;
    }
    let hwnd = HWND(raw as *mut c_void);
    if !IsWindow(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() {
        return false;
    }
    let mut rect = RECT::default();
    let mut pt = POINT::default();
    GetWindowRect(hwnd, &mut rect).is_ok()
        && GetCursorPos(&mut pt).is_ok()
        && PtInRect(&rect, pt).as_bool()
}

pub unsafe fn show(
    owner: HWND,
    summary: &AgentDockSummary,
    launch_path: Option<&str>,
    anchor_x: i32,
    anchor_y: i32,
    dpi: f32,
) {
    let work = geometry::work_area(anchor_x, anchor_y);
    let layout = compute_layout(summary.sessions.len(), dpi, work, anchor_x, anchor_y);
    let theme = crate::dock_theme(owner);

    let existing_raw = POPUP_HWND.load(Ordering::Relaxed);
    if existing_raw != 0 {
        let hwnd = HWND(existing_raw as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                let state = &mut *ptr;
                let same_agent = state.summary.agent_id == summary.agent_id;
                state.owner = owner;
                state.theme = theme;
                state.anchor_x = anchor_x;
                state.anchor_y = anchor_y;
                state.summary = summary.clone();
                state.launch_path = launch_path.map(str::to_string);
                if (state.format_dpi - dpi).abs() > 0.05 {
                    if let Ok(formats) = TextFormats::new(dpi) {
                        state.formats = formats;
                        state.format_dpi = dpi;
                    }
                }
                state.dpi = dpi;
                let size_changed =
                    state.layout.width != layout.width || state.layout.height != layout.height;
                state.layout = layout;
                let max_scroll = state
                    .summary
                    .sessions
                    .len()
                    .saturating_sub(state.layout.visible_rows);
                state.scroll_offset = if same_agent {
                    state.scroll_offset.min(max_scroll)
                } else {
                    0
                };
                state.hovered_row = None;
                state.hovered_footer = false;
                let _ = KillTimer(hwnd, TIMER_LEAVE_CHECK);
                if size_changed {
                    let _ = state
                        .glass
                        .resize(state.layout.width as u32, state.layout.height as u32);
                }
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    state.layout.x,
                    state.layout.y,
                    state.layout.width,
                    state.layout.height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                paint(state);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                return;
            }
        }
        POPUP_HWND.store(0, Ordering::Relaxed);
    }

    let Some(instance) = ensure_class() else {
        return;
    };
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
        POPUP_CLASS_NAME,
        w!("FeatherDock Agent Sessions"),
        WS_POPUP,
        layout.x,
        layout.y,
        layout.width,
        layout.height,
        owner,
        None,
        instance,
        None,
    ) else {
        return;
    };

    let shared = crate::dock_shared_gpu(owner);
    let glass = match Glass::new(
        hwnd,
        layout.width as u32,
        layout.height as u32,
        shared.as_ref().map(|(d3d, d2d)| (d3d, d2d)),
    ) {
        Ok(g) => g,
        Err(_) => {
            let _ = DestroyWindow(hwnd);
            return;
        }
    };
    let Ok(brush) = glass
        .dc()
        .CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 1.0), None)
    else {
        let _ = DestroyWindow(hwnd);
        return;
    };
    let Ok(formats) = TextFormats::new(dpi) else {
        let _ = DestroyWindow(hwnd);
        return;
    };

    let state = Box::new(PopupState {
        owner,
        glass,
        brush,
        formats,
        format_dpi: dpi,
        theme,
        dpi,
        anchor_x,
        anchor_y,
        summary: summary.clone(),
        launch_path: launch_path.map(str::to_string),
        layout,
        scroll_offset: 0,
        hovered_row: None,
        hovered_footer: false,
        tracking_leave: false,
        in_menu: false,
    });
    let ptr = Box::into_raw(state);
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
    POPUP_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    paint(&*ptr);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
}

pub unsafe fn refresh_if_open(owner: HWND, summary: &AgentDockSummary, launch_path: Option<&str>) {
    let raw = POPUP_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if !IsWindow(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() {
        return;
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
    if ptr.is_null() {
        return;
    }
    let state = &mut *ptr;
    if state.summary.agent_id != summary.agent_id || state.in_menu {
        return;
    }
    show(
        owner,
        summary,
        launch_path,
        state.anchor_x,
        state.anchor_y,
        state.dpi,
    );
}

/// Schedules a short grace check so moving from the Dock across the anchor gap
/// into the popup keeps it open, while leaving to the desktop hides it.
pub unsafe fn schedule_leave_check() {
    let raw = POPUP_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if IsWindow(hwnd).as_bool() && IsWindowVisible(hwnd).as_bool() {
        let _ = SetTimer(hwnd, TIMER_LEAVE_CHECK, LEAVE_GRACE_MS, None);
    }
}

pub unsafe fn cancel_leave_check() {
    let raw = POPUP_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if IsWindow(hwnd).as_bool() {
        let _ = KillTimer(hwnd, TIMER_LEAVE_CHECK);
    }
}

unsafe fn cursor_in_popup_or_dock(popup: HWND, owner: HWND) -> bool {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return false;
    }
    let mut popup_rect = RECT::default();
    if IsWindow(popup).as_bool()
        && GetWindowRect(popup, &mut popup_rect).is_ok()
        && PtInRect(&popup_rect, pt).as_bool()
    {
        return true;
    }
    !owner.is_invalid() && crate::dock_contains_cursor(owner)
}

fn max_scroll_offset(total: usize, visible_rows: usize) -> usize {
    total.saturating_sub(visible_rows.max(1))
}

fn resolve_row_session_index(
    total: usize,
    visible_rows: usize,
    scroll_offset: usize,
    row_idx: usize,
) -> Option<usize> {
    if row_idx >= visible_rows || total == 0 {
        return None;
    }
    let start = scroll_offset.min(max_scroll_offset(total, visible_rows));
    let actual = start + row_idx;
    (actual < total).then_some(actual)
}

fn activation_for_session(
    agent_id: &str,
    launch_path: Option<&str>,
    session: Option<&agent_state::AgentSession>,
) -> PopupActivation {
    if let Some(session) = session {
        PopupActivation {
            agent_id: agent_id.to_string(),
            session_id: Some(session.session_id.clone()),
            hwnd: session.hwnd,
            pid: session.pid,
            deep_link: session.deep_link.clone(),
            launch_path: launch_path.map(str::to_string),
        }
    } else {
        PopupActivation {
            agent_id: agent_id.to_string(),
            session_id: None,
            hwnd: None,
            pid: None,
            deep_link: None,
            launch_path: launch_path.map(str::to_string),
        }
    }
}

unsafe fn post_activation(owner: HWND, activation: PopupActivation) {
    *PENDING_ACTIVATION.lock().unwrap_or_else(|e| e.into_inner()) = Some(activation);
    hide();
    if !owner.is_invalid() {
        let _ = PostMessageW(owner, WM_AGENT_POPUP_ACTIVATED, WPARAM(0), LPARAM(0));
    }
}

pub unsafe fn hide() {
    let raw = POPUP_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if !IsWindow(hwnd).as_bool() {
        POPUP_HWND.store(0, Ordering::Relaxed);
        return;
    }
    let _ = KillTimer(hwnd, TIMER_LEAVE_CHECK);
    let was_visible = IsWindowVisible(hwnd).as_bool();
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
    let owner = if !ptr.is_null() {
        let state = &mut *ptr;
        state.hovered_row = None;
        state.hovered_footer = false;
        state.tracking_leave = false;
        state.owner
    } else {
        HWND::default()
    };
    if was_visible {
        let _ = ShowWindow(hwnd, SW_HIDE);
        if !owner.is_invalid() {
            let _ = PostMessageW(owner, WM_AGENT_POPUP_CLOSED, WPARAM(0), LPARAM(0));
        }
    }
}

pub unsafe fn shutdown() {
    let raw = POPUP_HWND.swap(0, Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if IsWindow(hwnd).as_bool() {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
        if !ptr.is_null() {
            (*ptr).owner = HWND::default();
        }
        let _ = DestroyWindow(hwnd);
    }
}

fn header_status_badge(summary: &AgentDockSummary) -> (String, AgentState) {
    if summary.waiting_count > 0 {
        (
            format!("{} 待确认", summary.waiting_count),
            AgentState::Waiting,
        )
    } else if summary.error_count > 0 {
        (format!("{} 异常", summary.error_count), AgentState::Error)
    } else if summary.active_count > 0 {
        (
            format!("{} 运行中", summary.active_count),
            AgentState::Running,
        )
    } else if summary.done_count > 0 {
        (format!("{} 已完成", summary.done_count), AgentState::Done)
    } else if summary.total_sessions > 0 {
        let windows_only = summary
            .sessions
            .iter()
            .all(|s| s.source == SessionSource::Window && s.updated_at == 0);
        let label = if windows_only {
            format!("{} 仅窗口", summary.total_sessions)
        } else {
            format!("{} 个会话", summary.total_sessions)
        };
        (label, AgentState::Idle)
    } else {
        ("空闲".to_string(), AgentState::Idle)
    }
}

fn state_dot_color(
    state: AgentState,
    palette: &crate::theme::AgentTheme,
    surface: &crate::theme::SurfaceTheme,
) -> (f32, f32, f32) {
    match state {
        AgentState::Waiting => palette.waiting_rgb,
        AgentState::Error => palette.error_rgb,
        AgentState::Running => palette.running_rgb,
        AgentState::Done => palette.done_rgb,
        AgentState::Idle => surface.muted_rgb,
    }
}

fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

unsafe fn paint(state: &PopupState) {
    let layout = &state.layout;
    let w = layout.width as f32;
    let h = layout.height as f32;
    let surface = state.theme.surface();
    let palette = state.theme.agent_palette();
    let dpi = state.dpi.max(1.0);

    let _ = state.glass.update_backdrop(
        (0.0, 0.0, w, h),
        layout.radius,
        if surface.backdrop { 1.0 } else { 0.0 },
    );

    let dc = state.glass.dc();
    let brush = &state.brush;
    dc.BeginDraw();
    dc.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));

    let card_rr = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: 0.5,
            top: 0.5,
            right: w - 0.5,
            bottom: h - 0.5,
        },
        radiusX: layout.radius,
        radiusY: layout.radius,
    };
    let bg_alpha = if surface.backdrop && !state.glass.has_backdrop() {
        0.96
    } else {
        surface.alpha
    };
    brush.SetColor(&rgba(surface.rgb.0, surface.rgb.1, surface.rgb.2, bg_alpha));
    dc.FillRoundedRectangle(&card_rr, brush);
    brush.SetColor(&rgba(
        surface.border_rgb.0,
        surface.border_rgb.1,
        surface.border_rgb.2,
        surface.border_alpha,
    ));
    dc.DrawRoundedRectangle(&card_rr, brush, 1.0 * dpi.min(1.5), None);

    // Header: Agent name + aggregate badge pill
    let (badge_label, badge_state) = header_status_badge(&state.summary);
    let badge_w =
        ((badge_label.chars().count() as f32 * 9.5 + 16.0) * dpi).clamp(54.0 * dpi, 112.0 * dpi);
    let badge_h = 20.0 * dpi;
    let badge_r = w - layout.pad_x;
    let badge_l = badge_r - badge_w;
    let badge_t = layout.header_top + (layout.header_bottom - layout.header_top - badge_h) * 0.5;
    let badge_rect = D2D_RECT_F {
        left: badge_l,
        top: badge_t,
        right: badge_r,
        bottom: badge_t + badge_h,
    };
    let badge_rgb = state_dot_color(badge_state, &palette, &surface);
    let badge_alpha = if badge_state == AgentState::Idle {
        0.14
    } else {
        0.18
    };
    brush.SetColor(&rgba(badge_rgb.0, badge_rgb.1, badge_rgb.2, badge_alpha));
    dc.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: badge_rect,
            radiusX: badge_h * 0.5,
            radiusY: badge_h * 0.5,
        },
        brush,
    );
    brush.SetColor(&rgba(badge_rgb.0, badge_rgb.1, badge_rgb.2, 0.95));
    let badge_wide: Vec<u16> = badge_label.encode_utf16().collect();
    dc.DrawText(
        &badge_wide,
        &state.formats.pill,
        &badge_rect,
        brush,
        D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );

    brush.SetColor(&rgba(
        surface.text_rgb.0,
        surface.text_rgb.1,
        surface.text_rgb.2,
        0.96,
    ));
    let header_wide: Vec<u16> = state.summary.display_name.encode_utf16().collect();
    dc.DrawText(
        &header_wide,
        &state.formats.header,
        &D2D_RECT_F {
            left: layout.pad_x + 2.0 * dpi,
            top: layout.header_top,
            right: (badge_l - 8.0 * dpi).max(layout.pad_x + 20.0 * dpi),
            bottom: layout.header_bottom,
        },
        brush,
        D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );

    // Thin divider below header
    let div_y = layout.header_bottom + 3.0 * dpi;
    brush.SetColor(&rgba(
        surface.text_rgb.0,
        surface.text_rgb.1,
        surface.text_rgb.2,
        surface.divider_alpha,
    ));
    dc.DrawLine(
        D2D_POINT_2F {
            x: layout.pad_x,
            y: div_y,
        },
        D2D_POINT_2F {
            x: w - layout.pad_x,
            y: div_y,
        },
        brush,
        1.0,
        None,
    );

    let now = agent_state::now_unix_secs();
    if state.summary.sessions.is_empty() {
        if let Some((rl, rt, rr, rb)) = layout.row_rect(0) {
            let hovered = state.hovered_row == Some(0);
            draw_row_backdrop(dc, brush, &surface, (rl, rt, rr, rb), dpi, hovered, false);
            let title_wide: Vec<u16> = "暂无活跃会话".encode_utf16().collect();
            let detail_text = if state.launch_path.is_some() {
                "点击此处或 Dock 图标打开应用"
            } else {
                "等待 Agent 状态事件或 GUI 窗口"
            };
            let detail_wide: Vec<u16> = detail_text.encode_utf16().collect();
            brush.SetColor(&rgba(
                surface.text_rgb.0,
                surface.text_rgb.1,
                surface.text_rgb.2,
                0.90,
            ));
            dc.DrawText(
                &title_wide,
                &state.formats.title,
                &D2D_RECT_F {
                    left: rl + 12.0 * dpi,
                    top: rt + 6.0 * dpi,
                    right: rr - 10.0 * dpi,
                    bottom: rt + 26.0 * dpi,
                },
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            brush.SetColor(&rgba(
                surface.muted_rgb.0,
                surface.muted_rgb.1,
                surface.muted_rgb.2,
                0.85,
            ));
            dc.DrawText(
                &detail_wide,
                &state.formats.detail,
                &D2D_RECT_F {
                    left: rl + 12.0 * dpi,
                    top: rt + 26.0 * dpi,
                    right: rr - 10.0 * dpi,
                    bottom: rb - 6.0 * dpi,
                },
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    } else {
        let total = state.summary.sessions.len();
        let start = state
            .scroll_offset
            .min(max_scroll_offset(total, layout.visible_rows));
        for (idx, session) in state
            .summary
            .sessions
            .iter()
            .skip(start)
            .take(layout.visible_rows)
            .enumerate()
        {
            let Some((rl, rt, rr, rb)) = layout.row_rect(idx) else {
                continue;
            };
            let hovered = state.hovered_row == Some(idx);
            let highlight_wait = session.state == AgentState::Waiting;
            draw_row_backdrop(
                dc,
                brush,
                &surface,
                (rl, rt, rr, rb),
                dpi,
                hovered,
                highlight_wait,
            );

            // State dot
            let dot_rgb = state_dot_color(session.state, &palette, &surface);
            brush.SetColor(&rgba(dot_rgb.0, dot_rgb.1, dot_rgb.2, 0.95));
            dc.FillEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F {
                        x: rl + 13.0 * dpi,
                        y: rt + (rb - rt) * 0.34,
                    },
                    radiusX: 4.2 * dpi,
                    radiusY: 4.2 * dpi,
                },
                brush,
            );

            // Right meta label ("待确认 · 12s" or "仅识别窗口" for Track A fallback)
            let is_window_only = session.source == SessionSource::Window && session.updated_at == 0;
            let meta_str = if is_window_only {
                "仅识别窗口".to_string()
            } else {
                let age = agent_state::format_relative_age(session.updated_at, now);
                format!("{} · {age}", session.state.label_zh())
            };
            let meta_w = 88.0 * dpi;
            let meta_l = rr - 10.0 * dpi - meta_w;
            let meta_color = if session.state == AgentState::Idle {
                surface.muted_rgb
            } else {
                dot_rgb
            };
            brush.SetColor(&rgba(meta_color.0, meta_color.1, meta_color.2, 0.95));
            let meta_wide: Vec<u16> = meta_str.encode_utf16().collect();
            dc.DrawText(
                &meta_wide,
                &state.formats.meta,
                &D2D_RECT_F {
                    left: meta_l,
                    top: rt + 6.0 * dpi,
                    right: rr - 10.0 * dpi,
                    bottom: rt + 26.0 * dpi,
                },
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );

            // Session title
            let text_l = rl + 25.0 * dpi;
            brush.SetColor(&rgba(
                surface.text_rgb.0,
                surface.text_rgb.1,
                surface.text_rgb.2,
                0.95,
            ));
            let title_wide: Vec<u16> = session.title.encode_utf16().collect();
            dc.DrawText(
                &title_wide,
                &state.formats.title,
                &D2D_RECT_F {
                    left: text_l,
                    top: rt + 6.0 * dpi,
                    right: (meta_l - 6.0 * dpi).max(text_l + 24.0 * dpi),
                    bottom: rt + 26.0 * dpi,
                },
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );

            // Second-line detail / fallback cue
            let detail_str = if !session.detail.is_empty() {
                session.detail.as_str()
            } else if let Some(cwd) = session.cwd.as_deref() {
                cwd
            } else if is_window_only {
                "未接入任务事件 · 点击聚焦窗口"
            } else if session.hwnd.is_some() {
                "点击跳转到对应窗口"
            } else {
                "点击激活会话"
            };
            brush.SetColor(&rgba(
                surface.muted_rgb.0,
                surface.muted_rgb.1,
                surface.muted_rgb.2,
                0.88,
            ));
            let detail_wide: Vec<u16> = detail_str.encode_utf16().collect();
            dc.DrawText(
                &detail_wide,
                &state.formats.detail,
                &D2D_RECT_F {
                    left: text_l,
                    top: rt + 27.0 * dpi,
                    right: rr - 10.0 * dpi,
                    bottom: rb - 5.0 * dpi,
                },
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    if let Some((fl, ft, fr, fb)) = layout.footer_rect() {
        let total = state.summary.sessions.len();
        let start = state
            .scroll_offset
            .min(max_scroll_offset(total, layout.visible_rows));
        let shown_from = start + 1;
        let shown_to = (start + layout.visible_rows).min(total);
        if state.hovered_footer {
            draw_row_backdrop(dc, brush, &surface, (fl, ft, fr, fb), dpi, true, false);
        }
        let footer = format!("已显示 {shown_from}-{shown_to}/{total} · 查看全部");
        let footer_alpha = if state.hovered_footer { 0.96 } else { 0.85 };
        brush.SetColor(&rgba(
            surface.text_rgb.0,
            surface.text_rgb.1,
            surface.text_rgb.2,
            footer_alpha,
        ));
        let wide: Vec<u16> = footer.encode_utf16().collect();
        dc.DrawText(
            &wide,
            &state.formats.footer,
            &D2D_RECT_F {
                left: fl,
                top: ft,
                right: fr,
                bottom: fb,
            },
            brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }

    let _ = state.glass.present();
}

unsafe fn draw_row_backdrop(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    surface: &crate::theme::SurfaceTheme,
    rect: (f32, f32, f32, f32),
    dpi: f32,
    hovered: bool,
    highlight_wait: bool,
) {
    let rr = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: rect.0,
            top: rect.1,
            right: rect.2,
            bottom: rect.3,
        },
        radiusX: 10.0 * dpi,
        radiusY: 10.0 * dpi,
    };
    if hovered {
        brush.SetColor(&rgba(
            surface.hover_rgb.0,
            surface.hover_rgb.1,
            surface.hover_rgb.2,
            surface.hover_alpha,
        ));
        dc.FillRoundedRectangle(&rr, brush);
    } else if highlight_wait {
        brush.SetColor(&rgba(
            surface.hover_rgb.0,
            surface.hover_rgb.1,
            surface.hover_rgb.2,
            surface.hover_alpha * 0.45,
        ));
        dc.FillRoundedRectangle(&rr, brush);
    }
}

unsafe fn open_all_sessions_menu(
    popup: HWND,
    owner: HWND,
    theme: ThemePreset,
    agent_id: &str,
    launch_path: Option<&str>,
    sessions: &[agent_state::AgentSession],
) {
    let ptr = GetWindowLongPtrW(popup, GWLP_USERDATA) as *mut PopupState;
    if !ptr.is_null() {
        (*ptr).in_menu = true;
    }
    let _ = KillTimer(popup, TIMER_LEAVE_CHECK);
    let Ok(menu) = CreatePopupMenu() else {
        if IsWindow(popup).as_bool() {
            let ptr = GetWindowLongPtrW(popup, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                (*ptr).in_menu = false;
            }
        }
        return;
    };
    for (idx, session) in sessions.iter().enumerate() {
        let state_label = if session.source == SessionSource::Window && session.updated_at == 0 {
            "仅识别窗口"
        } else {
            session.state.label_zh()
        };
        let escaped_title = session.title.replace('&', "&&");
        let label = format!("[{state_label}] {escaped_title}");
        let mut wide: Vec<u16> = label.encode_utf16().collect();
        wide.push(0);
        let _ = AppendMenuW(menu, MF_STRING, idx + 1, PCWSTR(wide.as_ptr()));
    }
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let menu_owner = if !owner.is_invalid() { owner } else { popup };
    let _ = SetForegroundWindow(menu_owner);
    let guard = crate::menu_style::style(menu, theme, menu_owner);
    let selected = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY,
        cursor.x,
        cursor.y,
        0,
        menu_owner,
        None,
    )
    .0 as usize;
    drop(guard);
    let _ = DestroyMenu(menu);
    if IsWindow(popup).as_bool() {
        let ptr = GetWindowLongPtrW(popup, GWLP_USERDATA) as *mut PopupState;
        if !ptr.is_null() {
            (*ptr).in_menu = false;
        }
    }
    if let Some(session) = selected.checked_sub(1).and_then(|i| sessions.get(i)) {
        post_activation(
            owner,
            activation_for_session(agent_id, launch_path, Some(session)),
        );
    } else if !cursor_in_popup_or_dock(popup, owner) {
        hide();
    } else if IsWindow(popup).as_bool() && IsWindowVisible(popup).as_bool() {
        let _ = SetTimer(popup, TIMER_LEAVE_CHECK, LEAVE_GRACE_MS, None);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PopupState;
            if !ptr.is_null() {
                paint(&*ptr);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let _ = KillTimer(hwnd, TIMER_LEAVE_CHECK);
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                let state = &mut *ptr;
                if !state.tracking_leave {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        state.tracking_leave = true;
                    }
                }
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                let hovered = state.layout.row_at(x, y);
                let hovered_footer = state.layout.footer_hit(x, y);
                if hovered != state.hovered_row || hovered_footer != state.hovered_footer {
                    state.hovered_row = hovered;
                    state.hovered_footer = hovered_footer;
                    paint(state);
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                let state = &mut *ptr;
                state.tracking_leave = false;
                let changed = state.hovered_row.take().is_some() || state.hovered_footer;
                state.hovered_footer = false;
                if changed {
                    paint(state);
                }
                if !state.in_menu {
                    let _ = SetTimer(hwnd, TIMER_LEAVE_CHECK, LEAVE_GRACE_MS, None);
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_LEAVE_CHECK {
                let _ = KillTimer(hwnd, TIMER_LEAVE_CHECK);
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
                if !ptr.is_null() {
                    let state = &*ptr;
                    if !state.in_menu && !cursor_in_popup_or_dock(hwnd, state.owner) {
                        hide();
                    }
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEWHEEL => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                let state = &mut *ptr;
                let delta = ((wparam.0 >> 16) as u16) as i16;
                let max_scroll =
                    max_scroll_offset(state.summary.sessions.len(), state.layout.visible_rows);
                if max_scroll > 0 && delta != 0 {
                    let next = if delta < 0 {
                        (state.scroll_offset + 1).min(max_scroll)
                    } else {
                        state.scroll_offset.saturating_sub(1)
                    };
                    if next != state.scroll_offset {
                        state.scroll_offset = next;
                        paint(state);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                let (
                    owner,
                    agent_id,
                    launch_path,
                    theme,
                    sessions,
                    visible_rows,
                    scroll_offset,
                    footer_hit,
                    row_hit,
                ) = {
                    let state = &*ptr;
                    (
                        state.owner,
                        state.summary.agent_id.clone(),
                        state.launch_path.clone(),
                        state.theme,
                        state.summary.sessions.clone(),
                        state.layout.visible_rows,
                        state.scroll_offset,
                        state.layout.footer_hit(x, y),
                        state.layout.row_at(x, y),
                    )
                };
                if footer_hit && !sessions.is_empty() {
                    open_all_sessions_menu(
                        hwnd,
                        owner,
                        theme,
                        &agent_id,
                        launch_path.as_deref(),
                        &sessions,
                    );
                    return LRESULT(0);
                }
                if let Some(row_idx) = row_hit {
                    let activation = if sessions.is_empty() {
                        activation_for_session(&agent_id, launch_path.as_deref(), None)
                    } else {
                        let session = resolve_row_session_index(
                            sessions.len(),
                            visible_rows,
                            scroll_offset,
                            row_idx,
                        )
                        .and_then(|idx| sessions.get(idx));
                        activation_for_session(&agent_id, launch_path.as_deref(), session)
                    };
                    post_activation(owner, activation);
                    return LRESULT(0);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_LEAVE_CHECK);
            POPUP_HWND
                .compare_exchange(hwnd.0 as isize, 0, Ordering::Relaxed, Ordering::Relaxed)
                .ok();
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
            if !ptr.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_caps_visible_rows_and_clamps_within_work_area() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let layout = compute_layout(9, 1.0, work, 1900, 1000);
        assert_eq!(layout.visible_rows, MAX_VISIBLE_ROWS);
        assert_eq!(layout.overflow_rows, 3);
        assert!(layout.x + layout.width <= work.right - WORK_MARGIN);
        assert!(layout.y >= work.top + WORK_MARGIN);
        assert!(layout.y + layout.height <= 1000);

        let first = layout.row_rect(0).unwrap();
        assert_eq!(
            layout.row_at((first.0 + first.2) * 0.5, (first.1 + first.3) * 0.5),
            Some(0)
        );
        assert_eq!(layout.row_at(0.0, (first.1 + first.3) * 0.5), None);
    }

    #[test]
    fn empty_sessions_allocate_single_action_row() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        let layout = compute_layout(0, 1.5, work, 640, 680);
        assert_eq!(layout.visible_rows, 1);
        assert_eq!(layout.overflow_rows, 0);
        assert!(layout.row_rect(0).is_some());
        assert!(layout.row_rect(1).is_none());
    }

    #[test]
    fn high_dpi_reduces_visible_rows_so_all_visible_rows_fit_without_clipping() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        let layout = compute_layout(6, 2.0, work, 640, 700);
        assert!(layout.height <= 720 - WORK_MARGIN * 2);
        assert!(layout.visible_rows < 6);
        assert_eq!(layout.overflow_rows, 6 - layout.visible_rows);
        let last = layout.row_rect(layout.visible_rows - 1).unwrap();
        assert!(last.3 <= layout.footer_top);
        assert!(layout.footer_rect().is_some());
    }

    #[test]
    fn scroll_offset_and_footer_hit_cover_all_overflow_sessions() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let layout = compute_layout(10, 1.0, work, 900, 1000);
        assert_eq!(layout.visible_rows, MAX_VISIBLE_ROWS);
        assert_eq!(layout.overflow_rows, 4);

        let max_scroll = max_scroll_offset(10, layout.visible_rows);
        assert_eq!(max_scroll, 4);
        assert_eq!(
            resolve_row_session_index(10, layout.visible_rows, max_scroll, layout.visible_rows - 1),
            Some(9)
        );
        let footer = layout.footer_rect().unwrap();
        assert!(layout.footer_hit((footer.0 + footer.2) * 0.5, (footer.1 + footer.3) * 0.5));
    }
}
