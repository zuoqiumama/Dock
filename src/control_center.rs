//! The control center: a frosted-glass popup, GPU-composited to match the dock,
//! summoned by the dock's right-side Control button. Deliberately minimal — a real
//! master-volume slider, three quick handoffs to the native Windows panels
//! (network / Bluetooth / input method), and a battery + clock status line. The
//! things Win11 no longer exposes to a process (e.g. the third-party tray icons) are
//! intentionally left to the system.
//!
//! It is its own top-level window (single-instance via `PANEL_HWND`) so it can open
//! and close on demand without disturbing the dock. It activates on open and closes
//! when it loses focus (click-away), on Esc, or when its toggle button is hit again.

use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::Instant;

use windows::core::*;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::glass::Glass;
use crate::sysctl::{self, AudioControl};

// --- layout, in logical px @96dpi (multiplied by dpi for device px) ---
const PANEL_W: f32 = 300.0;
const PANEL_H: f32 = 166.0;
const PAD: f32 = 14.0;
const RADIUS: f32 = 18.0;
const VOL_TOP: f32 = 16.0;
const VOL_H: f32 = 32.0;
const GLYPH_W: f32 = 30.0;
const BTN_TOP: f32 = 60.0;
const BTN_H: f32 = 58.0;
const BTN_GAP: f32 = 10.0;
const STATUS_H: f32 = 22.0;
const ANIM_SECS: f32 = 0.13;
/// Ignore a re-open that lands right after a click-away close (avoids flicker when
/// the Control button is clicked to dismiss).
const REOPEN_GUARD_MS: u32 = 220;

const BUTTON_LABELS: [&str; 3] = ["网络", "蓝牙", "输入法"];

static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);
static LAST_CLOSED_TICK: AtomicU32 = AtomicU32::new(0);

struct Panel {
    glass: Glass,
    brush: ID2D1SolidColorBrush,
    emoji: IDWriteTextFormat,
    label: IDWriteTextFormat,
    status_left: IDWriteTextFormat,
    status_right: IDWriteTextFormat,
    audio: Option<AudioControl>,
    dpi: f32,
    width: f32,
    height: f32,
    hovered: i32, // hovered button index, -1 = none
    dragging: bool,
    anim_start: Instant,
}

struct Layout {
    vol_glyph: D2D_RECT_F,
    vol_row: D2D_RECT_F,
    vol_bar: D2D_RECT_F,
    buttons: [D2D_RECT_F; 3],
    status: D2D_RECT_F,
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn in_rect(rc: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= rc.left && x <= rc.right && y >= rc.top && y <= rc.bottom
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn layout(width: f32, height: f32, dpi: f32) -> Layout {
    let s = |v: f32| v * dpi;
    let cl = s(PAD);
    let cr = width - s(PAD);
    let vol_row = rect(cl, s(VOL_TOP), cr, s(VOL_TOP) + s(VOL_H));
    let vol_glyph = rect(cl, s(VOL_TOP), cl + s(GLYPH_W), s(VOL_TOP) + s(VOL_H));
    let bar_left = cl + s(GLYPH_W) + s(6.0);
    let mid = s(VOL_TOP) + s(VOL_H) / 2.0;
    let bh = s(2.5);
    let vol_bar = rect(bar_left, mid - bh, cr, mid + bh);
    let bw = (cr - cl - 2.0 * s(BTN_GAP)) / 3.0;
    let buttons = [0usize, 1, 2].map(|i| {
        let left = cl + i as f32 * (bw + s(BTN_GAP));
        rect(left, s(BTN_TOP), left + bw, s(BTN_TOP) + s(BTN_H))
    });
    let stop = height - s(PAD) - s(STATUS_H);
    let status = rect(cl, stop, cr, stop + s(STATUS_H));
    Layout {
        vol_glyph,
        vol_row,
        vol_bar,
        buttons,
        status,
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn fill_round(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    rc: D2D_RECT_F,
    radius: f32,
    color: D2D1_COLOR_F,
) {
    brush.SetColor(&color);
    dc.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: rc,
            radiusX: radius,
            radiusY: radius,
        },
        brush,
    );
}

unsafe fn stroke_round(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    rc: D2D_RECT_F,
    radius: f32,
    width: f32,
    color: D2D1_COLOR_F,
) {
    brush.SetColor(&color);
    dc.DrawRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: rc,
            radiusX: radius,
            radiusY: radius,
        },
        brush,
        width,
        None,
    );
}

unsafe fn draw_text(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    text: &str,
    format: &IDWriteTextFormat,
    rc: D2D_RECT_F,
    color: D2D1_COLOR_F,
    emoji: bool,
) {
    brush.SetColor(&color);
    let chars = wide(text);
    let options = if emoji {
        D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT
    } else {
        D2D1_DRAW_TEXT_OPTIONS_NONE
    };
    dc.DrawText(
        &chars,
        format,
        &rc,
        brush,
        options,
        DWRITE_MEASURING_MODE_NATURAL,
    );
}

unsafe fn render(panel: &Panel) {
    let a = smoothstep(panel.anim_start.elapsed().as_secs_f32() / ANIM_SECS);
    let dpi = panel.dpi;
    let lay = layout(panel.width, panel.height, dpi);
    let dc = panel.glass.dc();

    dc.BeginDraw();
    dc.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
    // Whole panel rises a few px and fades in as it opens.
    let ty = (1.0 - a) * 8.0 * dpi;
    dc.SetTransform(&Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 0.0,
        M32: ty,
    });

    // --- glass background ---
    let bg = rect(
        0.5 * dpi,
        0.5 * dpi,
        panel.width - 0.5 * dpi,
        panel.height - 0.5 * dpi,
    );
    fill_round(
        dc,
        &panel.brush,
        bg,
        RADIUS * dpi,
        rgba(0.12, 0.12, 0.14, 0.86 * a),
    );
    stroke_round(
        dc,
        &panel.brush,
        bg,
        RADIUS * dpi,
        1.0 * dpi,
        rgba(1.0, 1.0, 1.0, 0.12 * a),
    );

    // --- volume row ---
    let (level, muted) = panel
        .audio
        .as_ref()
        .map(|audio| (audio.level(), audio.muted()))
        .unwrap_or((0.0, false));
    let speaker = if muted || level <= 0.0001 {
        "\u{1F507}" // 🔇 muted
    } else {
        "\u{1F50A}" // 🔊
    };
    draw_text(
        dc,
        &panel.brush,
        speaker,
        &panel.emoji,
        lay.vol_glyph,
        rgba(1.0, 1.0, 1.0, 0.95 * a),
        true,
    );
    let bar = lay.vol_bar;
    let bar_radius = (bar.bottom - bar.top) / 2.0;
    fill_round(
        dc,
        &panel.brush,
        bar,
        bar_radius,
        rgba(1.0, 1.0, 1.0, 0.16 * a),
    );
    let shown = if muted { 0.0 } else { level };
    let fill_right = bar.left + shown * (bar.right - bar.left);
    if fill_right > bar.left + bar_radius {
        let fill = rect(bar.left, bar.top, fill_right, bar.bottom);
        fill_round(
            dc,
            &panel.brush,
            fill,
            bar_radius,
            rgba(0.36, 0.64, 0.99, 0.95 * a),
        );
    }
    // knob
    let mid = (bar.top + bar.bottom) / 2.0;
    let knob = 7.0 * dpi;
    panel.brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.98 * a));
    dc.FillEllipse(
        &D2D1_ELLIPSE {
            point: D2D_POINT_2F {
                x: fill_right.clamp(bar.left, bar.right),
                y: mid,
            },
            radiusX: knob,
            radiusY: knob,
        },
        &panel.brush,
    );

    // --- quick buttons ---
    for (i, &rc) in lay.buttons.iter().enumerate() {
        let hot = panel.hovered == i as i32;
        let fill = if hot { 0.16 } else { 0.08 };
        fill_round(
            dc,
            &panel.brush,
            rc,
            12.0 * dpi,
            rgba(1.0, 1.0, 1.0, fill * a),
        );
        stroke_round(
            dc,
            &panel.brush,
            rc,
            12.0 * dpi,
            1.0 * dpi,
            rgba(1.0, 1.0, 1.0, 0.10 * a),
        );
        draw_text(
            dc,
            &panel.brush,
            BUTTON_LABELS[i],
            &panel.label,
            rc,
            rgba(0.95, 0.95, 0.97, 0.96 * a),
            false,
        );
    }

    // --- status line: battery (left) + clock (right) ---
    let battery = sysctl::battery();
    if battery.present {
        let text = if battery.charging {
            format!("充电中 {}%", battery.percent)
        } else {
            format!("电池 {}%", battery.percent)
        };
        draw_text(
            dc,
            &panel.brush,
            &text,
            &panel.status_left,
            lay.status,
            rgba(0.78, 0.80, 0.84, 0.92 * a),
            false,
        );
    }
    let (hm, md) = sysctl::clock();
    draw_text(
        dc,
        &panel.brush,
        &format!("{}   {}", hm, md),
        &panel.status_right,
        lay.status,
        rgba(0.78, 0.80, 0.84, 0.92 * a),
        false,
    );

    dc.SetTransform(&Matrix3x2::identity());
    let _ = panel.glass.present();
}

/// Drive the open fade. Returns false once settled so the caller can drop the timer.
unsafe fn animate(panel: &Panel) -> bool {
    render(panel);
    panel.anim_start.elapsed().as_secs_f32() < ANIM_SECS
}

unsafe fn handle_press(hwnd: HWND, panel: &mut Panel, x: f32, y: f32) {
    let lay = layout(panel.width, panel.height, panel.dpi);
    // Tap the speaker to toggle mute.
    if in_rect(&lay.vol_glyph, x, y) {
        if let Some(audio) = &panel.audio {
            audio.set_muted(!audio.muted());
        }
        render(panel);
        return;
    }
    // Grab the slider anywhere along its row.
    if in_rect(&lay.vol_row, x, y) {
        panel.dragging = true;
        let _ = SetCapture(hwnd);
        apply_slider(panel, &lay, x);
        render(panel);
    }
}

unsafe fn apply_slider(panel: &Panel, lay: &Layout, x: f32) {
    if let Some(audio) = &panel.audio {
        let bar = lay.vol_bar;
        let level = ((x - bar.left) / (bar.right - bar.left)).clamp(0.0, 1.0);
        audio.set_level(level);
    }
}

unsafe fn handle_release(hwnd: HWND, panel: &mut Panel, x: f32, y: f32) {
    if panel.dragging {
        panel.dragging = false;
        let _ = ReleaseCapture();
        return;
    }
    let lay = layout(panel.width, panel.height, panel.dpi);
    for (i, rc) in lay.buttons.iter().enumerate() {
        if in_rect(rc, x, y) {
            run_button(i);
            let _ = DestroyWindow(hwnd); // dismiss like the native flyouts do
            return;
        }
    }
}

fn run_button(index: usize) {
    match index {
        0 => sysctl::open_uri("ms-settings:network-status"),
        1 => sysctl::open_uri("ms-settings:bluetooth"),
        _ => sysctl::switch_input_method(),
    }
}

unsafe fn handle_move(panel: &mut Panel, x: f32, y: f32) {
    let lay = layout(panel.width, panel.height, panel.dpi);
    if panel.dragging {
        apply_slider(panel, &lay, x);
        render(panel);
        return;
    }
    let mut hovered = -1;
    for (i, rc) in lay.buttons.iter().enumerate() {
        if in_rect(rc, x, y) {
            hovered = i as i32;
            break;
        }
    }
    if hovered != panel.hovered {
        panel.hovered = hovered;
        render(panel);
    }
}

/// Open the panel (or, if it's already up, this is a no-op — `toggle` handles the
/// close path). Anchors above the dock button, centered on it and clamped on-screen.
unsafe fn open(dock_hwnd: HWND, anchor_cx: i32, anchor_top: i32) {
    let instance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(module) => module.into(),
        Err(_) => return,
    };
    let class_name = w!("FeatherDockControlCenter");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassExW(&wc); // ignore "already registered"

    let dpi = (GetDpiForWindow(dock_hwnd).max(96) as f32) / 96.0;
    let width = (PANEL_W * dpi).round() as i32;
    let height = (PANEL_H * dpi).round() as i32;

    // Place above the anchor, centered, clamped to the anchor monitor's work area.
    let mut x = anchor_cx - width / 2;
    let mut y = anchor_top - height - (10.0 * dpi) as i32;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let monitor = MonitorFromPoint(
        POINT {
            x: anchor_cx,
            y: anchor_top,
        },
        MONITOR_DEFAULTTONEAREST,
    );
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let margin = (8.0 * dpi) as i32;
        x = x.clamp(
            info.rcWork.left + margin,
            info.rcWork.right - width - margin,
        );
        y = y.max(info.rcWork.top + margin);
    }

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
        class_name,
        w!("FeatherDock 控制中心"),
        WS_POPUP,
        x,
        y,
        width,
        height,
        None,
        None,
        instance,
        None,
    ) else {
        return;
    };

    let glass = match Glass::new(hwnd, width as u32, height as u32) {
        Ok(glass) => glass,
        Err(error) => {
            crate::error_log::write("控制中心 GPU 初始化失败", &error);
            let _ = DestroyWindow(hwnd);
            return;
        }
    };

    let (brush, emoji, label, status_left, status_right) = match resources(glass.dc(), dpi) {
        Ok(resources) => resources,
        Err(error) => {
            crate::error_log::write("控制中心字体初始化失败", &error);
            let _ = DestroyWindow(hwnd);
            return;
        }
    };

    let panel = Box::into_raw(Box::new(Panel {
        glass,
        brush,
        emoji,
        label,
        status_left,
        status_right,
        audio: AudioControl::open(),
        dpi,
        width: width as f32,
        height: height as f32,
        hovered: -1,
        dragging: false,
        anim_start: Instant::now(),
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, panel as isize);
    PANEL_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    render(&*panel);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    SetTimer(hwnd, 1, 16, None);
}

/// Build the brush + the four text formats (emoji glyph, button label, status L/R).
unsafe fn resources(
    dc: &ID2D1DeviceContext,
    dpi: f32,
) -> Result<(
    ID2D1SolidColorBrush,
    IDWriteTextFormat,
    IDWriteTextFormat,
    IDWriteTextFormat,
    IDWriteTextFormat,
)> {
    let brush = dc.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 1.0), None)?;
    let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

    let make = |family: PCWSTR, size: f32| -> Result<IDWriteTextFormat> {
        dwrite.CreateTextFormat(
            family,
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!(""),
        )
    };

    let emoji = make(w!("Segoe UI Emoji"), 17.0 * dpi)?;
    emoji.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    emoji.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let label = make(w!("Microsoft YaHei UI"), 14.0 * dpi)?;
    label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    label.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let status_left = make(w!("Microsoft YaHei UI"), 12.5 * dpi)?;
    status_left.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
    status_left.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let status_right = make(w!("Microsoft YaHei UI"), 12.5 * dpi)?;
    status_right.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
    status_right.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    Ok((brush, emoji, label, status_left, status_right))
}

/// Toggle the control center anchored above a dock button. Closes it if open;
/// otherwise opens it (unless we *just* closed it via click-away, to avoid flicker).
pub unsafe fn toggle(dock_hwnd: HWND, anchor_cx: i32, anchor_top: i32) {
    let existing = PANEL_HWND.load(Ordering::Relaxed);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let _ = DestroyWindow(hwnd);
            return;
        }
    }
    let since = GetTickCount().wrapping_sub(LAST_CLOSED_TICK.load(Ordering::Relaxed));
    if since < REOPEN_GUARD_MS {
        return;
    }
    open(dock_hwnd, anchor_cx, anchor_top);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Panel;
        match msg {
            WM_TIMER => {
                if !ptr.is_null() && !animate(&*ptr) {
                    let _ = KillTimer(hwnd, 1);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE if !ptr.is_null() => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                handle_move(&mut *ptr, x, y);
                LRESULT(0)
            }
            WM_LBUTTONDOWN if !ptr.is_null() => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                handle_press(hwnd, &mut *ptr, x, y);
                LRESULT(0)
            }
            WM_LBUTTONUP if !ptr.is_null() => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                handle_release(hwnd, &mut *ptr, x, y);
                LRESULT(0)
            }
            WM_ACTIVATE => {
                // Lost activation (clicked another window) -> dismiss. WA_INACTIVE == 0.
                if (wparam.0 & 0xFFFF) == 0 {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(hwnd, 1);
                LAST_CLOSED_TICK.store(GetTickCount(), Ordering::Relaxed);
                PANEL_HWND.store(0, Ordering::Relaxed);
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
