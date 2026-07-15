//! The app drawer: a frosted-glass popup, GPU-composited to match the dock, summoned
//! by the dock's "app grid" button. It lists every program sitting on the desktop
//! (the current user's Desktop plus the shared Public Desktop) as an icon grid — a
//! launcher you can use *instead* of desktop icons, so the desktop itself can be kept
//! clean. The desktop scan is cached and invalidated by a short TTL / desktop signature
//! so opening the drawer stays cheap without a background watcher.
//!
//! Like the control center it is its own top-level window (single-instance via
//! `PANEL_HWND`): it activates on open and closes on click-away, on Esc, on launching
//! an item, or when its toggle button is hit again. Long desktops scroll with the wheel.

use core::ffi::c_void;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
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
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, ReleaseCapture, SetCapture, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::categories::{self, Categories};
use crate::content::{classify_path, fallback_visual};
use crate::desktop_scan::{self, Pidl};
use crate::drawer_layout::{self, Layout, Section, SectionKind};
use crate::glass::Glass;
use crate::icons::{self, IconLoader, OwnedIcon};

/// Cursor travel (device px) before a press turns into a drag rather than a click.
const DRAG_THRESHOLD: f32 = 6.0;

#[derive(Clone, Copy, PartialEq)]
enum HeaderAction {
    Rename,
    Delete,
}

/// In-flight drag of a program tile between categories.
struct DragState {
    entry: usize, // index into `entries`; its source tile is dimmed
    start_x: f32,
    start_y: f32,
    cur_x: f32,
    cur_y: f32,
    active: bool, // crossed the movement threshold
}

// --- layout, in logical px @96dpi (multiplied by dpi for device px) ---
const LPAD: f32 = 14.0; // left/right padding
const BPAD: f32 = 14.0; // bottom padding
const HEADER_H: f32 = 32.0; // title row above the grid
const COLS: usize = 5;
const CELL_W: f32 = 92.0;
const CELL_H: f32 = 94.0;
const ICON: f32 = 46.0;
const GAP_X: f32 = 6.0;
const GAP_Y: f32 = 6.0;
const RADIUS: f32 = 18.0;
const OPEN_ANIM_SECS: f32 = 0.26;
const CLOSE_ANIM_SECS: f32 = 0.13;
/// How far (device px @96dpi) the panel rises into place as it pops open.
const RISE: f32 = 12.0;
const TILE_RISE: f32 = 9.0;
const HEADER_RISE: f32 = 6.0;
/// Seconds the open "cascade" front takes to sweep top→bottom across the viewport,
/// so headers and tiles assemble as one wave instead of a flat per-index stagger.
const WAVE_SECS: f32 = 0.09;
const SECTION_H: f32 = 30.0; // per-category header row height
const SECTION_GAP: f32 = 6.0; // vertical gap between category sections
const ADD_H: f32 = 34.0; // the "new category" button row at the bottom
/// Ignore a re-open landing right after a click-away close (avoids button-toggle flicker).
const REOPEN_GUARD_MS: u32 = 220;

const ID_CTX_OPEN: usize = 3101;
const ID_CTX_PIN: usize = 3102;
const ID_CTX_HIDE: usize = 3103;
const ID_CTX_UNCATEGORIZED: usize = 3104;
const ID_CTX_NEW_CATEGORY: usize = 3105;
const ID_CTX_RESTORE: usize = 3106;
const ID_CTX_REFRESH: usize = 3107;
const ID_CTX_CATEGORY_BASE: usize = 3200;

static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);
static LAST_CLOSED_TICK: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static PRELOADED_ICONS: RefCell<HashMap<String, Rc<OwnedIcon>>> =
        RefCell::new(HashMap::new());
}

struct Entry {
    label: String,
    key: String,          // stable id for category assignment
    path: Option<String>, // filesystem path, when it is one
    pidl: Option<Pidl>,   // absolute PIDL, for virtual items (此电脑 / 回收站 / …)
    icon: Option<ID2D1Bitmap1>,
    preloaded_icon: Option<Rc<OwnedIcon>>,
    glyph: &'static str,    // fallback glyph if the icon can't be extracted
    color: (f32, f32, f32), // fallback tile color
}

struct Drawer {
    owner: HWND,
    glass: Glass,
    brush: ID2D1SolidColorBrush,
    title: IDWriteTextFormat,
    section: IDWriteTextFormat,
    center: IDWriteTextFormat,
    label: IDWriteTextFormat,
    glyph: IDWriteTextFormat,
    entries: Vec<Entry>,
    categories: Categories,
    sections: Vec<Section>,
    layout: Layout,
    dpi: f32,
    width: f32,
    height: f32,
    anchor_x: f32, // dock-button centre in client px → the pop's horizontal scale origin
    viewport_h: f32, // device px of the scrollable area (below the fixed title)
    scroll: f32,
    max_scroll: f32,
    hovered: i32, // hovered cell index (into layout.cells), -1 = none
    drag: Option<DragState>,
    editing: bool, // a rename/new-category popup is open → don't dismiss on deactivate
    closing: bool,
    anim_start: Instant,
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

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// Snappy onset with a long, gentle settle — reads as more "premium" than cubic and,
/// unlike a back-ease, never overshoots past 1.0 (the window is sized to the panel, so
/// an overshoot would clip).
fn ease_out_quint(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(5)
}

fn offset_rect(r: D2D_RECT_F, dx: f32, dy: f32) -> D2D_RECT_F {
    rect(r.left + dx, r.top + dy, r.right + dx, r.bottom + dy)
}

/// Matrix product `a * b` (apply `a`, then `b`) — composes the scroll + open transforms.
fn mul(a: Matrix3x2, b: Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}

/// Uniform scale `s` about anchor (ax, ay), plus a vertical offset `dy`.
fn scale_about(s: f32, ax: f32, ay: f32, dy: f32) -> Matrix3x2 {
    Matrix3x2 {
        M11: s,
        M12: 0.0,
        M21: 0.0,
        M22: s,
        M31: ax * (1.0 - s),
        M32: ay * (1.0 - s) + dy,
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn translate(dx: f32, dy: f32) -> Matrix3x2 {
    Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: dx,
        M32: dy,
    }
}

/// Pixel metrics for the layout module, derived from the panel width + DPI.
fn metrics(width: f32, dpi: f32) -> drawer_layout::Metrics {
    let s = |v: f32| v * dpi;
    drawer_layout::Metrics {
        width,
        cols: COLS,
        lpad: s(LPAD),
        cell_w: s(CELL_W),
        cell_h: s(CELL_H),
        gap_x: s(GAP_X),
        gap_y: s(GAP_Y),
        header_h: s(SECTION_H),
        section_gap: s(SECTION_GAP),
        add_h: s(ADD_H),
    }
}

fn to_d2d(r: drawer_layout::Rect) -> D2D_RECT_F {
    rect(r.left, r.top, r.right, r.bottom)
}

impl Drawer {
    fn grid_top(&self) -> f32 {
        HEADER_H * self.dpi
    }

    /// Per-row open progress (0→1) for a content-space element at vertical `content_top`.
    /// Elements near the top of the viewport start first and lower ones follow, so the
    /// whole drawer — section headers, tiles and the add button — assembles as a single
    /// top-to-bottom wave. Closing returns 1.0 so the dismiss is a flat, quick fade.
    fn row_progress(&self, elapsed: f32, content_top: f32) -> f32 {
        if self.closing {
            return 1.0;
        }
        let frac = ((content_top - self.scroll) / self.viewport_h.max(1.0)).clamp(0.0, 1.0);
        let delay = frac * WAVE_SECS;
        let span = (OPEN_ANIM_SECS - WAVE_SECS).max(0.001);
        ease_out_cubic((elapsed - delay) / span)
    }

    /// Recompute the sections + geometry from the current entries and categories,
    /// keeping the scroll position valid. Call after any edit (drag, add, rename…).
    fn relayout(&mut self) {
        let keys: Vec<String> = self.entries.iter().map(|e| e.key.clone()).collect();
        self.sections = drawer_layout::sectionize(&keys, &self.categories);
        self.layout = drawer_layout::compute(&self.sections, &metrics(self.width, self.dpi));
        self.max_scroll = (self.layout.content_h - self.viewport_h).max(0.0);
        self.scroll = self.scroll.clamp(0.0, self.max_scroll);
    }

    /// Map a client-space Y to content space (un-scrolled), or None if outside the
    /// scrollable viewport (i.e. over the fixed title bar).
    fn content_y(&self, client_y: f32) -> Option<f32> {
        let grid_top = self.grid_top();
        if client_y < grid_top || client_y > grid_top + self.viewport_h {
            return None;
        }
        Some(client_y - grid_top + self.scroll)
    }

    /// The layout cell index under a client-space point, accounting for scroll.
    fn hit_cell(&self, x: f32, y: f32) -> Option<usize> {
        let cy = self.content_y(y)?;
        drawer_layout::hit_cell(&self.layout, x, cy)
    }

    /// True if a client-space point is over the "new category" button.
    fn hit_add_button(&self, x: f32, y: f32) -> bool {
        self.content_y(y)
            .is_some_and(|cy| in_rect(&to_d2d(self.layout.add_button), x, cy))
    }

    /// A custom category's rename/delete button under a client-space point, returned as
    /// `(category index, action)`. Only custom sections carry these controls.
    fn hit_header_action(&self, x: f32, y: f32) -> Option<(usize, HeaderAction)> {
        let cy = self.content_y(y)?;
        for band in &self.layout.bands {
            if let SectionKind::Custom(ci) = self.sections[band.section].kind {
                let (rename, delete) = header_action_rects(to_d2d(band.header), self.dpi);
                if in_rect(&delete, x, cy) {
                    return Some((ci, HeaderAction::Delete));
                }
                if in_rect(&rename, x, cy) {
                    return Some((ci, HeaderAction::Rename));
                }
            }
        }
        None
    }
}

/// The (rename, delete) button rectangles at the right edge of a section header.
fn header_action_rects(header: D2D_RECT_F, dpi: f32) -> (D2D_RECT_F, D2D_RECT_F) {
    let sz = 22.0 * dpi;
    let gap = 4.0 * dpi;
    let pad = 6.0 * dpi;
    let mid = (header.top + header.bottom) / 2.0;
    let del = rect(
        header.right - pad - sz,
        mid - sz / 2.0,
        header.right - pad,
        mid + sz / 2.0,
    );
    let ren = rect(
        del.left - gap - sz,
        mid - sz / 2.0,
        del.left - gap,
        mid + sz / 2.0,
    );
    (ren, del)
}

/// Build the drawer's entries from the cached desktop-program scan, attaching a fallback
/// glyph/color for entries whose real icon cannot be extracted.
unsafe fn build_entries(categories: &Categories) -> Vec<Entry> {
    let mut entries: Vec<Entry> = desktop_scan::scan_programs_cached()
        .into_iter()
        .filter(|d| !categories.is_hidden(&d.key))
        .map(|d| {
            let (glyph, color) = fallback_visual(d.kind);
            Entry {
                label: d.label,
                key: d.key,
                path: d.path,
                pidl: d.pidl,
                icon: None,
                preloaded_icon: None,
                glyph,
                color,
            }
        })
        .collect();
    attach_preloaded_icons(&mut entries);
    entries
}

fn attach_preloaded_icons(entries: &mut [Entry]) {
    PRELOADED_ICONS.with(|cache| {
        let cache = cache.borrow();
        for entry in entries {
            entry.preloaded_icon = cache.get(&entry.key).cloned();
        }
    });
}

unsafe fn preload_missing_icons(entries: &mut [Entry]) {
    PRELOADED_ICONS.with(|cache| {
        let mut cache = cache.borrow_mut();
        for entry in entries {
            if let Some(icon) = cache.get(&entry.key).cloned() {
                entry.preloaded_icon = Some(icon);
                continue;
            }

            let icon = match &entry.path {
                Some(path) => icons::source_icon(path, 256),
                None => entry
                    .pidl
                    .as_ref()
                    .and_then(|pidl| icons::source_icon_pidl(pidl.as_ptr())),
            };
            if let Some(icon) = icon {
                let icon = Rc::new(icon);
                cache.insert(entry.key.clone(), icon.clone());
                entry.preloaded_icon = Some(icon);
            }
        }
    });
}

/// Warm the drawer's shell scan + icon handles during dock startup, before the user opens it.
pub unsafe fn warm_cache() {
    let categories = categories::load();
    let mut entries = build_entries(&categories);
    preload_missing_icons(&mut entries);
}

unsafe fn load_entry_icons(dc: &ID2D1DeviceContext, dpi: f32, entries: &mut [Entry]) {
    let Ok(loader) = IconLoader::new() else {
        return;
    };
    let icon_px = ((ICON * 2.0 * dpi).round() as u32).clamp(48, 256);
    for entry in entries {
        if let Some(icon) = &entry.preloaded_icon {
            entry.icon = loader.load_hicon(dc, icon.raw());
            if entry.icon.is_some() {
                continue;
            }
        }
        entry.icon = match &entry.path {
            Some(path) => {
                let kind = classify_path(Path::new(path));
                loader.load(dc, path, icon_px, kind)
            }
            None => entry
                .pidl
                .as_ref()
                .and_then(|pidl| loader.load_pidl(dc, pidl.as_ptr())),
        };
    }
}

unsafe fn reload_entries(panel: &mut Drawer, refresh_scan: bool) {
    if refresh_scan {
        desktop_scan::invalidate_cache();
    }
    panel.entries = build_entries(&panel.categories);
    if refresh_scan {
        preload_missing_icons(&mut panel.entries);
    }
    let dc = panel.glass.dc().clone();
    load_entry_icons(&dc, panel.dpi, &mut panel.entries);
    panel.hovered = -1;
    panel.drag = None;
    panel.relayout();
    render(panel);
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
        D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT | D2D1_DRAW_TEXT_OPTIONS_CLIP
    } else {
        D2D1_DRAW_TEXT_OPTIONS_CLIP
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

unsafe fn fit_icon(container: D2D_RECT_F, size: D2D_SIZE_F) -> D2D_RECT_F {
    if size.width <= 0.0 || size.height <= 0.0 {
        return container;
    }
    let cw = container.right - container.left;
    let ch = container.bottom - container.top;
    let scale = (cw / size.width).min(ch / size.height).min(1.0);
    let w = size.width * scale;
    let h = size.height * scale;
    let left = container.left + (cw - w) / 2.0;
    let top = container.top + (ch - h) / 2.0;
    rect(left, top, left + w, top + h)
}

/// Draw one program tile (icon + label) into its content-space `cell` rectangle.
/// `dragging` dims the source tile while it's being dragged elsewhere.
unsafe fn draw_cell(
    panel: &Drawer,
    cell: D2D_RECT_F,
    entry: &Entry,
    a: f32,
    hovered: bool,
    dragging: bool,
) {
    let dc = panel.glass.dc();
    let dpi = panel.dpi;
    let s = |v: f32| v * dpi;
    let alpha = if dragging { a * 0.3 } else { a };
    if hovered && !dragging {
        fill_round(
            dc,
            &panel.brush,
            cell,
            12.0 * dpi,
            rgba(1.0, 1.0, 1.0, 0.12 * a),
        );
    }
    let icon_box = rect(
        cell.left + (s(CELL_W) - s(ICON)) / 2.0,
        cell.top + s(8.0),
        cell.right - (s(CELL_W) - s(ICON)) / 2.0,
        cell.top + s(8.0) + s(ICON),
    );
    match &entry.icon {
        Some(bmp) => {
            let dest = fit_icon(icon_box, bmp.GetSize());
            dc.DrawBitmap(
                bmp,
                Some(&dest),
                alpha,
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                None,
                None,
            );
        }
        None => {
            let (r, g, b) = entry.color;
            fill_round(
                dc,
                &panel.brush,
                icon_box,
                s(ICON) * 0.22,
                rgba(r, g, b, alpha),
            );
            draw_text(
                dc,
                &panel.brush,
                entry.glyph,
                &panel.glyph,
                icon_box,
                rgba(1.0, 1.0, 1.0, 0.97 * alpha),
                true,
            );
        }
    }
    let label_box = rect(
        cell.left + s(2.0),
        icon_box.bottom + s(4.0),
        cell.right - s(2.0),
        cell.bottom,
    );
    draw_text(
        dc,
        &panel.brush,
        &entry.label,
        &panel.label,
        label_box,
        rgba(0.92, 0.93, 0.96, 0.95 * alpha),
        false,
    );
}

unsafe fn render(panel: &Drawer) {
    let elapsed = panel.anim_start.elapsed().as_secs_f32();
    let (a, e) = if panel.closing {
        let t = (elapsed / CLOSE_ANIM_SECS).clamp(0.0, 1.0);
        (1.0 - smoothstep(t), 1.0 - ease_in_cubic(t))
    } else {
        let t = (elapsed / OPEN_ANIM_SECS).clamp(0.0, 1.0);
        (smoothstep(t), ease_out_quint(t))
    };
    let dpi = panel.dpi;
    let s = |v: f32| v * dpi;
    let dc = panel.glass.dc();

    // The whole panel subtly scales and rises from the dock button. We anchor the scale
    // at the button's X (not the panel centre) so a panel clamped to a monitor edge still
    // appears to grow *out of the button that summoned it*. Close reverses the same
    // transform, keeping the motion spatial instead of snapping away.
    let open_scale = 0.96 + 0.04 * e;
    let open_dy = (1.0 - e) * RISE * dpi;
    let base = scale_about(open_scale, panel.anchor_x, panel.height, open_dy);

    dc.BeginDraw();
    dc.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
    dc.SetTransform(&base);

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
        rgba(0.12, 0.12, 0.14, 0.88 * a),
    );
    panel.brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.12 * a));
    dc.DrawRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: bg,
            radiusX: RADIUS * dpi,
            radiusY: RADIUS * dpi,
        },
        &panel.brush,
        1.0 * dpi,
        None,
    );

    // --- header title ---
    let header = rect(s(LPAD), s(8.0), panel.width - s(LPAD), panel.grid_top());
    let title = if panel.entries.is_empty() {
        "桌面".to_string()
    } else {
        format!("桌面 · {} 个程序", panel.entries.len())
    };
    draw_text(
        dc,
        &panel.brush,
        &title,
        &panel.title,
        header,
        rgba(0.96, 0.96, 0.98, 0.96 * a),
        false,
    );

    if panel.entries.is_empty() {
        let empty = rect(
            s(LPAD),
            panel.grid_top(),
            panel.width - s(LPAD),
            panel.height - s(BPAD),
        );
        draw_text(
            dc,
            &panel.brush,
            "桌面上没有程序",
            &panel.label,
            empty,
            rgba(0.72, 0.74, 0.78, 0.9 * a),
            false,
        );
        dc.SetTransform(&Matrix3x2::identity());
        let _ = panel.glass.present();
        return;
    }

    // --- scrollable categorized content (clipped to the viewport, translated by scroll) ---
    let grid_top = panel.grid_top();
    let viewport = rect(0.0, grid_top, panel.width, grid_top + panel.viewport_h);
    dc.PushAxisAlignedClip(&viewport, D2D1_ANTIALIAS_MODE_ALIASED);
    dc.SetTransform(&mul(translate(0.0, grid_top - panel.scroll), base));

    let vis_top = panel.scroll;
    let vis_bot = panel.scroll + panel.viewport_h;

    // highlight the section a dragged tile would drop into (drawn under everything else)
    if let Some(drag) = panel.drag.as_ref().filter(|d| d.active) {
        if let Some(cy) = panel.content_y(drag.cur_y) {
            let m = metrics(panel.width, panel.dpi);
            if let Some((sec_idx, _)) =
                drawer_layout::drop_target(&panel.sections, &panel.layout, &m, drag.cur_x, cy)
            {
                if let Some(band) = panel.layout.bands.iter().find(|b| b.section == sec_idx) {
                    let hl = rect(
                        s(LPAD) * 0.5,
                        band.header.top,
                        panel.width - s(LPAD) * 0.5,
                        band.grid_bottom,
                    );
                    fill_round(
                        dc,
                        &panel.brush,
                        hl,
                        s(10.0),
                        rgba(0.30, 0.55, 0.95, 0.14 * a),
                    );
                }
            }
        }
    }

    // section headers (category name + count, hairline divider, and edit buttons)
    for band in &panel.layout.bands {
        if band.grid_bottom < vis_top || band.header.top > vis_bot {
            continue;
        }
        let sec = &panel.sections[band.section];
        // Ride the same top-to-bottom wave as the tiles below this header.
        let hp = panel.row_progress(elapsed, band.header.top);
        let ha = a * hp;
        let hdr = offset_rect(to_d2d(band.header), 0.0, (1.0 - hp) * s(HEADER_RISE));
        let is_custom = matches!(sec.kind, SectionKind::Custom(_));
        let name = if sec.entries.is_empty() {
            sec.name.clone()
        } else {
            format!("{} · {}", sec.name, sec.entries.len())
        };
        let right = if is_custom {
            hdr.right - s(58.0)
        } else {
            hdr.right - s(4.0)
        };
        let label_rc = rect(hdr.left + s(4.0), hdr.top, right, hdr.bottom);
        draw_text(
            dc,
            &panel.brush,
            &name,
            &panel.section,
            label_rc,
            rgba(0.86, 0.88, 0.93, 0.92 * ha),
            false,
        );
        panel.brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.07 * ha));
        let ly = hdr.bottom - s(1.0);
        dc.FillRectangle(&rect(hdr.left, ly, hdr.right, ly + s(1.0)), &panel.brush);
        if is_custom {
            let (ren, del) = header_action_rects(hdr, dpi);
            draw_text(
                dc,
                &panel.brush,
                "\u{270E}",
                &panel.center,
                ren,
                rgba(0.82, 0.85, 0.92, 0.7 * ha),
                false,
            );
            draw_text(
                dc,
                &panel.brush,
                "\u{2715}",
                &panel.center,
                del,
                rgba(0.90, 0.62, 0.62, 0.7 * ha),
                false,
            );
        }
    }

    // program tiles (the one being dragged is dimmed at its source)
    for (i, cell) in panel.layout.cells.iter().enumerate() {
        if cell.rect.bottom < vis_top || cell.rect.top > vis_bot {
            continue;
        }
        let drag_src = panel
            .drag
            .as_ref()
            .is_some_and(|d| d.active && d.entry == cell.entry);
        let hovered = panel.hovered == i as i32 && panel.drag.is_none();
        let cell_a = panel.row_progress(elapsed, cell.rect.top);
        let rise = (1.0 - cell_a) * s(TILE_RISE);
        draw_cell(
            panel,
            offset_rect(to_d2d(cell.rect), 0.0, rise),
            &panel.entries[cell.entry],
            a * cell_a,
            hovered,
            drag_src,
        );
    }

    // "new category" button at the bottom of the content (last to ride the wave in)
    let ab0 = to_d2d(panel.layout.add_button);
    if ab0.bottom >= vis_top && ab0.top <= vis_bot {
        let abp = panel.row_progress(elapsed, panel.layout.add_button.top);
        let aba = a * abp;
        let ab = offset_rect(ab0, 0.0, (1.0 - abp) * s(HEADER_RISE));
        let round = D2D1_ROUNDED_RECT {
            rect: ab,
            radiusX: s(10.0),
            radiusY: s(10.0),
        };
        panel.brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.05 * aba));
        dc.FillRoundedRectangle(&round, &panel.brush);
        panel.brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.16 * aba));
        dc.DrawRoundedRectangle(&round, &panel.brush, s(1.0), None);
        let txt = rect(ab.left + s(10.0), ab.top, ab.right - s(10.0), ab.bottom);
        draw_text(
            dc,
            &panel.brush,
            "＋  新建分类",
            &panel.section,
            txt,
            rgba(0.82, 0.85, 0.90, 0.92 * aba),
            false,
        );
    }

    dc.SetTransform(&base);
    dc.PopAxisAlignedClip();

    // --- slim scrollbar indicator (only when there's overflow) ---
    if panel.max_scroll > 0.5 {
        let track_h = panel.viewport_h;
        let frac = (panel.viewport_h / (panel.viewport_h + panel.max_scroll)).clamp(0.1, 1.0);
        let thumb_h = track_h * frac;
        let travel = track_h - thumb_h;
        let thumb_top = grid_top + travel * (panel.scroll / panel.max_scroll);
        let x = panel.width - s(5.0);
        let bar = rect(x, thumb_top, x + s(3.0), thumb_top + thumb_h);
        fill_round(dc, &panel.brush, bar, s(1.5), rgba(1.0, 1.0, 1.0, 0.22 * a));
    }

    // --- drag ghost: the tile being dragged, floating under the cursor (client space) ---
    if let Some(drag) = panel.drag.as_ref().filter(|d| d.active) {
        let cw = s(CELL_W);
        let ch = s(CELL_H);
        let gx = drag.cur_x - cw / 2.0;
        let gy = drag.cur_y - ch / 2.0;
        let gcell = rect(gx, gy, gx + cw, gy + ch);
        fill_round(
            dc,
            &panel.brush,
            gcell,
            12.0 * dpi,
            rgba(0.22, 0.24, 0.28, 0.92 * a),
        );
        panel.brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.14 * a));
        dc.DrawRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: gcell,
                radiusX: 12.0 * dpi,
                radiusY: 12.0 * dpi,
            },
            &panel.brush,
            s(1.0),
            None,
        );
        draw_cell(panel, gcell, &panel.entries[drag.entry], a, false, false);
    }

    dc.SetTransform(&Matrix3x2::identity());
    let _ = panel.glass.present();
}

unsafe fn wake_owner(panel: &Drawer) {
    if !panel.owner.is_invalid() {
        let _ = PostMessageW(panel.owner, crate::WM_ANIMATION_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// Drive the open/close fade. Returns false once settled so the caller stops re-posting.
unsafe fn animate(hwnd: HWND, panel: &Drawer) -> bool {
    render(panel);
    let elapsed = panel.anim_start.elapsed().as_secs_f32();
    if panel.closing && elapsed >= CLOSE_ANIM_SECS {
        let _ = DestroyWindow(hwnd);
        return false;
    }
    elapsed
        < if panel.closing {
            CLOSE_ANIM_SECS
        } else {
            OPEN_ANIM_SECS
        }
}

unsafe fn start_close(_hwnd: HWND, panel: &mut Drawer) {
    if panel.closing {
        return;
    }
    panel.closing = true;
    panel.hovered = -1;
    panel.drag = None;
    let _ = ReleaseCapture();
    panel.anim_start = Instant::now();
    render(panel);
    wake_owner(panel);
}

/// Render one drawer frame from the Dock's shared, vsync-paced animation loop.
pub unsafe fn animate_frame() -> bool {
    let raw = PANEL_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return false;
    }

    let hwnd = HWND(raw as *mut c_void);
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Drawer;
    if ptr.is_null() {
        return false;
    }

    animate(hwnd, &*ptr)
}

/// Press on a tile arms a potential drag (it becomes a plain click if it never moves
/// past the threshold). We grab the mouse so the drag tracks outside the tile.
unsafe fn on_down(hwnd: HWND, panel: &mut Drawer, x: f32, y: f32) {
    if let Some(ci) = panel.hit_cell(x, y) {
        panel.drag = Some(DragState {
            entry: panel.layout.cells[ci].entry,
            start_x: x,
            start_y: y,
            cur_x: x,
            cur_y: y,
            active: false,
        });
        SetCapture(hwnd);
    }
}

unsafe fn on_move(panel: &mut Drawer, x: f32, y: f32) {
    if let Some(drag) = panel.drag.as_mut() {
        drag.cur_x = x;
        drag.cur_y = y;
        if !drag.active {
            let (dx, dy) = (x - drag.start_x, y - drag.start_y);
            if (dx * dx + dy * dy).sqrt() > DRAG_THRESHOLD * panel.dpi {
                drag.active = true;
            }
        }
        if drag.active {
            render(panel);
        }
        return;
    }
    let hovered = panel.hit_cell(x, y).map(|i| i as i32).unwrap_or(-1);
    if hovered != panel.hovered {
        panel.hovered = hovered;
        render(panel);
    }
}

/// Create a new category (a generic name) and immediately open its rename prompt.
unsafe fn add_category(hwnd: HWND, panel: &mut Drawer) {
    let n = panel.categories.categories.len() + 1;
    let idx = panel.categories.add(&format!("新分类 {n}"));
    let _ = categories::save(&panel.categories);
    panel.relayout();
    render(panel);
    begin_rename(hwnd, idx);
}

unsafe fn append_menu_item(menu: HMENU, id: usize, text: &str, enabled: bool) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let flags = if enabled {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let _ = AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr()));
}

unsafe fn append_submenu(menu: HMENU, submenu: HMENU, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(menu, MF_POPUP, submenu.0 as usize, PCWSTR(wide.as_ptr()));
}

fn path_is_pinned(path: &str) -> bool {
    let target = crate::config::path_key(Path::new(path));
    crate::config::load().ok().flatten().is_some_and(|cfg| {
        cfg.items.iter().any(|spec| {
            spec.path
                .as_deref()
                .or(spec.exe.as_deref())
                .is_some_and(|value| crate::config::path_key(Path::new(value)) == target)
        })
    })
}

unsafe fn pin_entry_to_dock(panel: &Drawer, entry: usize) {
    let Some(item) = panel.entries.get(entry) else {
        return;
    };
    let Some(path) = item.path.as_deref() else {
        return;
    };
    match crate::config::add_item(&item.label, path) {
        Ok(true) => {
            let _ = PostMessageW(
                panel.owner,
                crate::settings_window::WM_PINS_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        }
        Ok(false) => {}
        Err(error) => crate::error_log::write("固定抽屉程序失败", &error),
    }
}

unsafe fn hide_entry(panel: &mut Drawer, entry: usize) {
    let Some(item) = panel.entries.get(entry) else {
        return;
    };
    let key = item.key.clone();
    panel.categories.hide_item(&key);
    if let Err(error) = categories::save(&panel.categories) {
        crate::error_log::write("保存抽屉隐藏项失败", &error);
    }
    panel.entries.remove(entry);
    panel.relayout();
    render(panel);
}

unsafe fn move_entry_to_category(panel: &mut Drawer, entry: usize, target: Option<usize>) {
    let Some(item) = panel.entries.get(entry) else {
        return;
    };
    let key = item.key.clone();
    panel.categories.move_item(&key, target, usize::MAX);
    if let Err(error) = categories::save(&panel.categories) {
        crate::error_log::write("保存抽屉分类失败", &error);
    }
    panel.relayout();
    render(panel);
}

unsafe fn move_entry_to_new_category(hwnd: HWND, panel: &mut Drawer, entry: usize) {
    let Some(item) = panel.entries.get(entry) else {
        return;
    };
    let key = item.key.clone();
    let n = panel.categories.categories.len() + 1;
    let idx = panel.categories.add(&format!("新分类 {n}"));
    panel.categories.move_item(&key, Some(idx), 0);
    if let Err(error) = categories::save(&panel.categories) {
        crate::error_log::write("保存抽屉新分类失败", &error);
    }
    panel.relayout();
    render(panel);
    begin_rename(hwnd, idx);
}

unsafe fn show_entry_menu(hwnd: HWND, panel: &mut Drawer, entry: usize) {
    let Some(item) = panel.entries.get(entry) else {
        return;
    };
    let can_pin_path = item.path.as_deref();
    let already_pinned = can_pin_path.is_some_and(path_is_pinned);
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    append_menu_item(menu, ID_CTX_OPEN, "打开", true);
    append_menu_item(
        menu,
        ID_CTX_PIN,
        if already_pinned {
            "已固定到 Dock"
        } else {
            "固定到 Dock"
        },
        can_pin_path.is_some() && !already_pinned,
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_menu_item(menu, ID_CTX_HIDE, "移除该程序", true);

    if let Ok(cat_menu) = CreatePopupMenu() {
        append_menu_item(cat_menu, ID_CTX_UNCATEGORIZED, "未分类", true);
        if !panel.categories.categories.is_empty() {
            let _ = AppendMenuW(cat_menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        for (ci, cat) in panel.categories.categories.iter().enumerate() {
            append_menu_item(cat_menu, ID_CTX_CATEGORY_BASE + ci, &cat.name, true);
        }
        let _ = AppendMenuW(cat_menu, MF_SEPARATOR, 0, PCWSTR::null());
        append_menu_item(cat_menu, ID_CTX_NEW_CATEGORY, "新建分类...", true);
        append_submenu(menu, cat_menu, "放入分类");
    }

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
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
        ID_CTX_OPEN => {
            if let Some(item) = panel.entries.get(entry) {
                desktop_scan::launch(item.path.as_deref(), item.pidl.as_ref().map(|p| p.as_ptr()));
                start_close(hwnd, panel);
            }
        }
        ID_CTX_PIN => pin_entry_to_dock(panel, entry),
        ID_CTX_HIDE => hide_entry(panel, entry),
        ID_CTX_UNCATEGORIZED => move_entry_to_category(panel, entry, None),
        ID_CTX_NEW_CATEGORY => move_entry_to_new_category(hwnd, panel, entry),
        id if id >= ID_CTX_CATEGORY_BASE => {
            let category = id - ID_CTX_CATEGORY_BASE;
            if category < panel.categories.categories.len() {
                move_entry_to_category(panel, entry, Some(category));
            }
        }
        _ => {}
    }
}

unsafe fn show_background_menu(hwnd: HWND, panel: &mut Drawer) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    append_menu_item(menu, ID_CTX_NEW_CATEGORY, "新建分类", true);
    append_menu_item(
        menu,
        ID_CTX_RESTORE,
        "恢复已移除项目",
        !panel.categories.hidden.is_empty(),
    );
    append_menu_item(menu, ID_CTX_REFRESH, "刷新程序列表", true);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
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
        ID_CTX_NEW_CATEGORY => add_category(hwnd, panel),
        ID_CTX_RESTORE => {
            panel.categories.restore_hidden();
            if let Err(error) = categories::save(&panel.categories) {
                crate::error_log::write("恢复抽屉隐藏项失败", &error);
            }
            reload_entries(panel, false);
        }
        ID_CTX_REFRESH => reload_entries(panel, true),
        _ => {}
    }
}

/// Commit a dropped tile into the section + slot under the cursor (None = uncategorized).
unsafe fn commit_drop(panel: &mut Drawer, entry: usize, x: f32, y: f32) {
    let Some(cy) = panel.content_y(y) else {
        return;
    };
    let m = metrics(panel.width, panel.dpi);
    let Some((sec_idx, index)) =
        drawer_layout::drop_target(&panel.sections, &panel.layout, &m, x, cy)
    else {
        return;
    };
    let key = panel.entries[entry].key.clone();
    let target = match panel.sections[sec_idx].kind {
        SectionKind::Custom(ci) => Some(ci),
        SectionKind::Uncategorized => None,
    };
    panel.categories.move_item(&key, target, index);
    let _ = categories::save(&panel.categories);
    panel.relayout();
}

/// Delete a category; its members fall back to "未分类" (non-destructive).
unsafe fn delete_category(panel: &mut Drawer, category: usize) {
    panel.categories.remove(category);
    let _ = categories::save(&panel.categories);
    panel.relayout();
    render(panel);
}

/// Open the rename prompt for a category. The drawer is disabled (truly modal) and
/// flagged `editing` so it won't dismiss itself while the popup has focus. Uses the raw
/// window pointer so no `&mut Drawer` is held across the popup's nested message loop.
unsafe fn begin_rename(hwnd: HWND, category: usize) {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Drawer;
    if ptr.is_null() {
        return;
    }
    // Short-lived borrow to read the current name + flag editing — released before the
    // popup's modal loop runs, so no `&mut Drawer` is held across it.
    let initial = {
        let panel = &mut *ptr;
        panel.editing = true;
        panel
            .categories
            .categories
            .get(category)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    };
    let _ = EnableWindow(hwnd, false);

    let result = crate::drawer_input::prompt(hwnd, "分类名称", &initial);

    let _ = EnableWindow(hwnd, true);
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Drawer;
    if ptr.is_null() {
        return;
    }
    let panel = &mut *ptr;
    panel.editing = false;
    if let Some(name) = result {
        if !name.is_empty() {
            panel.categories.rename(category, &name);
            let _ = categories::save(&panel.categories);
            panel.relayout();
        }
    }
    let _ = SetForegroundWindow(hwnd);
    render(panel);
}

unsafe fn handle_scroll(panel: &mut Drawer, delta: f32) {
    if panel.max_scroll <= 0.0 {
        return;
    }
    let step = (CELL_H + GAP_Y) * panel.dpi;
    panel.scroll = (panel.scroll - (delta / 120.0) * step).clamp(0.0, panel.max_scroll);
    render(panel);
}

unsafe fn on_up(hwnd: HWND, panel: &mut Drawer, x: f32, y: f32) {
    if let Some(drag) = panel.drag.take() {
        let _ = ReleaseCapture();
        if drag.active {
            commit_drop(panel, drag.entry, x, y);
            panel.hovered = -1;
            render(panel);
        } else {
            // Never moved → a plain click: launch the tile and dismiss.
            let entry = &panel.entries[drag.entry];
            desktop_scan::launch(
                entry.path.as_deref(),
                entry.pidl.as_ref().map(|p| p.as_ptr()),
            );
            start_close(hwnd, panel);
        }
        return;
    }
    // Not a tile press: category rename/delete controls, then the "new category" button.
    if let Some((ci, action)) = panel.hit_header_action(x, y) {
        match action {
            HeaderAction::Delete => delete_category(panel, ci),
            HeaderAction::Rename => begin_rename(hwnd, ci),
        }
        return;
    }
    if panel.hit_add_button(x, y) {
        add_category(hwnd, panel);
    }
}

/// Open the drawer anchored above the dock button, sized to fit the desktop programs
/// and clamped to the work area above the dock (rows scroll if there are too many).
unsafe fn open(dock_hwnd: HWND, anchor_cx: i32, anchor_top: i32) {
    let instance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(module) => module.into(),
        Err(_) => return,
    };
    let class_name = w!("FeatherDockDrawer");
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
    let s = |v: f32| v * dpi;

    let categories = categories::load();
    let mut entries = build_entries(&categories);
    let width = s(LPAD) * 2.0 + COLS as f32 * s(CELL_W) + (COLS as f32 - 1.0) * s(GAP_X);

    // Lay the categorized content out (pure geometry) to learn its full height.
    let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
    let sections = drawer_layout::sectionize(&keys, &categories);
    let layout = drawer_layout::compute(&sections, &metrics(width, dpi));
    let content_h = layout.content_h;

    // Clamp the height to what fits above the dock on this monitor.
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
    let margin = s(8.0);
    let work_top = if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork.top as f32
    } else {
        0.0
    };
    let avail_above = (anchor_top as f32 - work_top - margin - s(10.0)).max(s(120.0));

    let grid_top = s(HEADER_H);
    let max_view = (avail_above - grid_top - s(BPAD)).max(s(120.0));
    let viewport_h = content_h.min(max_view);
    let max_scroll = (content_h - viewport_h).max(0.0);
    let height = grid_top + viewport_h + s(BPAD);

    // Place above the anchor, centered, clamped to the monitor work area.
    let mut x = anchor_cx - (width / 2.0) as i32;
    let mut y = anchor_top - height as i32 - s(10.0) as i32;
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let m = margin as i32;
        x = x.clamp(info.rcWork.left + m, info.rcWork.right - width as i32 - m);
        y = y.max(info.rcWork.top + m);
    }

    // The dock button's centre expressed in the panel's own client space (device px). The
    // panel may have been clamped sideways, so this is where the pop should grow *from*.
    let anchor_x = ((anchor_cx - x) as f32).clamp(0.0, width);

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
        class_name,
        w!("FeatherDock 应用抽屉"),
        WS_POPUP,
        x,
        y,
        width as i32,
        height as i32,
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
            crate::error_log::write("应用抽屉 GPU 初始化失败", &error);
            let _ = DestroyWindow(hwnd);
            return;
        }
    };

    let (brush, title, section, center, label, glyph) = match resources(glass.dc(), dpi) {
        Ok(resources) => resources,
        Err(error) => {
            crate::error_log::write("应用抽屉字体初始化失败", &error);
            let _ = DestroyWindow(hwnd);
            return;
        }
    };

    // Icons are loaded before the first visible frame. Startup warm_cache() usually
    // makes this a cheap HICON-to-D2D conversion; if the cache was cold, we still prefer
    // a short open delay over showing placeholder icons that visibly swap in.
    load_entry_icons(glass.dc(), dpi, &mut entries);

    let panel = Box::into_raw(Box::new(Drawer {
        owner: dock_hwnd,
        glass,
        brush,
        title,
        section,
        center,
        label,
        glyph,
        entries,
        categories,
        sections,
        layout,
        dpi,
        width,
        height,
        anchor_x,
        viewport_h,
        scroll: 0.0,
        max_scroll,
        hovered: -1,
        drag: None,
        editing: false,
        closing: false,
        anim_start: Instant::now(),
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, panel as isize);
    PANEL_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    render(&*panel);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    wake_owner(&*panel);
}

/// Build the brush + the text formats (drawer title, section header, centred small
/// glyph for header buttons, cell label, fallback glyph).
unsafe fn resources(
    dc: &ID2D1DeviceContext,
    dpi: f32,
) -> Result<(
    ID2D1SolidColorBrush,
    IDWriteTextFormat,
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

    let title = make(w!("Microsoft YaHei UI"), 14.0 * dpi)?;
    title.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
    title.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    // Section headers: a touch smaller than the title, left-aligned, vertically centred.
    let section = make(w!("Microsoft YaHei UI"), 12.0 * dpi)?;
    section.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
    section.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    // Centred small glyphs (the rename/delete buttons on a category header).
    let center = make(w!("Segoe UI"), 13.0 * dpi)?;
    center.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    center.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let label = make(w!("Microsoft YaHei UI"), 11.5 * dpi)?;
    label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    label.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
    label.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;

    let glyph = make(w!("Segoe UI Emoji"), ICON * 0.5 * dpi)?;
    glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    glyph.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    Ok((brush, title, section, center, label, glyph))
}

/// Toggle the drawer anchored above a dock button. Closes it if open; otherwise opens
/// it (unless we *just* closed it via click-away, to avoid a button-toggle flicker).
pub unsafe fn toggle(dock_hwnd: HWND, anchor_cx: i32, anchor_top: i32) {
    let existing = PANEL_HWND.load(Ordering::Relaxed);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let _ = SendMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
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
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Drawer;
        match msg {
            WM_LBUTTONDOWN if !ptr.is_null() => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                on_down(hwnd, &mut *ptr, x, y);
                LRESULT(0)
            }
            WM_MOUSEMOVE if !ptr.is_null() => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                on_move(&mut *ptr, x, y);
                LRESULT(0)
            }
            WM_MOUSEWHEEL if !ptr.is_null() => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32;
                handle_scroll(&mut *ptr, delta);
                LRESULT(0)
            }
            WM_LBUTTONUP if !ptr.is_null() => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                on_up(hwnd, &mut *ptr, x, y);
                LRESULT(0)
            }
            WM_RBUTTONUP if !ptr.is_null() => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                let panel = &mut *ptr;
                if panel.closing {
                    return LRESULT(0);
                }
                if let Some(ci) = panel.hit_cell(x, y) {
                    let entry = panel.layout.cells[ci].entry;
                    show_entry_menu(hwnd, panel, entry);
                } else {
                    show_background_menu(hwnd, panel);
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                // Lost activation (clicked another window) -> dismiss. WA_INACTIVE == 0.
                // But never while a rename popup is up (it steals focus on purpose).
                let editing = !ptr.is_null() && (*ptr).editing;
                if (wparam.0 & 0xFFFF) == 0 && !editing {
                    if !ptr.is_null() {
                        start_close(hwnd, &mut *ptr);
                    } else {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                // Esc cancels an in-flight drag; otherwise it closes the drawer.
                if ptr.is_null() {
                    let _ = DestroyWindow(hwnd);
                } else {
                    let panel = &mut *ptr;
                    if panel.drag.is_some() {
                        panel.drag = None;
                        let _ = ReleaseCapture();
                        render(panel);
                    } else {
                        start_close(hwnd, panel);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if ptr.is_null() {
                    let _ = DestroyWindow(hwnd);
                } else {
                    start_close(hwnd, &mut *ptr);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
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

#[cfg(test)]
mod tests {
    #[test]
    fn drawer_uses_shared_frame_loop_instead_of_self_posting() {
        let source = include_str!("drawer.rs");
        let production = source
            .rsplit_once("#[cfg(test)]")
            .map(|(production, _)| production)
            .expect("drawer production source");

        assert!(production.contains("pub unsafe fn animate_frame()"));
        assert!(!production.contains("const WM_ANIM"));
        assert!(!production.contains("schedule_frame("));
    }
}
