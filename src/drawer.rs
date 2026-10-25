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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
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
use crate::content::fallback_visual;
use crate::desktop_scan::{self, Pidl};
use crate::drawer_layout::{self, Layout, Section, SectionKind};
use crate::glass::Glass;
use crate::theme::SurfaceTheme;

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

#[derive(Clone, Copy, Debug, PartialEq)]
struct PanelMotion {
    alpha: f32,
    e: f32,
}

impl PanelMotion {
    const HIDDEN: Self = Self { alpha: 0.0, e: 0.0 };
    #[cfg(test)]
    const VISIBLE: Self = Self { alpha: 1.0, e: 1.0 };
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ReopenGuard {
    owner: isize,
    tick: u32,
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

/// How many cached BMPs are decoded into GPU bitmaps per timer tick while the
/// drawer is open, so icons stream in without stalling the first frame.
const ICON_DECODE_BATCH: usize = 6;
const TIMER_ICON_LOAD: usize = 3;
const TIMER_ICON_LOAD_MS: u32 = 30;
/// Posted by the background extraction thread once its batch is on disk.
const WM_APP_ICONS_READY: u32 = WM_APP + 0x25;

const ID_CTX_OPEN: usize = 3101;
const ID_CTX_PIN: usize = 3102;
const ID_CTX_HIDE: usize = 3103;
const ID_CTX_UNCATEGORIZED: usize = 3104;
const ID_CTX_NEW_CATEGORY: usize = 3105;
const ID_CTX_RESTORE: usize = 3106;
const ID_CTX_REFRESH: usize = 3107;
const ID_CTX_CATEGORY_BASE: usize = 3200;

static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static CLICK_AWAY_GUARD: RefCell<Option<ReopenGuard>> = const { RefCell::new(None) };
    // Extracted icon BMPs, keyed by entry key. Filled by the helper-process
    // extraction (`drawer_icons`); decoded into GPU bitmaps when the drawer opens.
    // The BMPs live in a persistent per-dock cache directory, so the paths stay
    // valid across open/close cycles (deleting them on close was what made every
    // icon fall back to its glyph tile on the second open).
    static PRELOADED_ICONS: RefCell<HashMap<String, PathBuf>> = RefCell::new(HashMap::new());
    // Decoded GPU bitmaps, keyed by entry key. Decoded ONCE (at startup by
    // `warm_cache`, or on first use) on the dock's shared Direct2D device, then
    // reused by every drawer open — opening the drawer uploads nothing.
    static BITMAP_CACHE: RefCell<HashMap<String, ID2D1Bitmap1>> = RefCell::new(HashMap::new());
    // Entry keys whose icon extraction failed (helper crash, timeout, or the
    // shell returned no icon). Skipped on later opens so a broken `.lnk` target
    // never spawns a fresh extraction batch on every open; cleared by an
    // explicit "刷新程序列表".
    static FAILED_EXTRACT: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
}

struct Entry {
    label: String,
    key: String,          // stable id for category assignment
    path: Option<String>, // filesystem path, when it is one
    pidl: Option<Pidl>,   // absolute PIDL, for virtual items (此电脑 / 回收站 / …)
    icon: Option<ID2D1Bitmap1>,
    preloaded_icon: Option<PathBuf>,
    glyph: &'static str,    // fallback glyph if the icon can't be extracted
    color: (f32, f32, f32), // fallback tile color
}

struct Drawer {
    owner: HWND,
    material: SurfaceTheme,
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
    motion_from: PanelMotion,
    cascade_suppressed: bool,
    pending_icons: Vec<usize>, // entry indices still waiting for a GPU bitmap
    extracting: bool,          // a background icon-extraction batch is in flight
    extraction_keys: Vec<String>, // keys sent to the background batch (failed ones are skipped later)
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

fn material_color(rgb: (f32, f32, f32), alpha: f32) -> D2D1_COLOR_F {
    rgba(rgb.0, rgb.1, rgb.2, alpha)
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

fn transition_from(from: PanelMotion, closing: bool, elapsed: f32) -> PanelMotion {
    if closing {
        let t = (elapsed / CLOSE_ANIM_SECS).clamp(0.0, 1.0);
        PanelMotion {
            alpha: from.alpha * (1.0 - smoothstep(t)),
            e: from.e * (1.0 - ease_in_cubic(t)),
        }
    } else {
        let t = (elapsed / OPEN_ANIM_SECS).clamp(0.0, 1.0);
        PanelMotion {
            alpha: from.alpha + (1.0 - from.alpha) * smoothstep(t),
            e: from.e + (1.0 - from.e) * ease_out_quint(t),
        }
    }
}

fn click_away_guard(owner: isize, activated: isize, tick: u32) -> Option<ReopenGuard> {
    (owner != 0 && activated == owner).then_some(ReopenGuard { owner, tick })
}

fn take_reopen_guard(slot: &mut Option<ReopenGuard>, owner: isize, now: u32) -> bool {
    slot.take()
        .is_some_and(|guard| guard.owner == owner && now.wrapping_sub(guard.tick) < REOPEN_GUARD_MS)
}

fn reopen_closing_with_toggle(slot: &mut Option<ReopenGuard>, owner: isize, now: u32) -> bool {
    !take_reopen_guard(slot, owner, now)
}

fn record_click_away_guard(owner: HWND, activated: HWND, tick: u32) {
    let guard = click_away_guard(owner.0 as isize, activated.0 as isize, tick);
    if guard.is_some() {
        CLICK_AWAY_GUARD.with(|slot| *slot.borrow_mut() = guard);
    }
}

fn consume_reopen_guard(owner: HWND, now: u32) -> bool {
    CLICK_AWAY_GUARD.with(|slot| take_reopen_guard(&mut slot.borrow_mut(), owner.0 as isize, now))
}

fn clear_reopen_guard() {
    CLICK_AWAY_GUARD.with(|slot| *slot.borrow_mut() = None);
}

fn offset_rect(r: D2D_RECT_F, dx: f32, dy: f32) -> D2D_RECT_F {
    rect(r.left + dx, r.top + dy, r.right + dx, r.bottom + dy)
}

fn risen_rect(r: D2D_RECT_F, progress: f32, rise: f32) -> D2D_RECT_F {
    offset_rect(r, 0.0, (1.0 - progress) * rise)
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

fn panel_transform(motion: PanelMotion, anchor_x: f32, height: f32, dpi: f32) -> Matrix3x2 {
    let scale = 0.96 + 0.04 * motion.e;
    let dy = (1.0 - motion.e) * RISE * dpi;
    scale_about(scale, anchor_x, height, dy)
}

fn transform_point(m: Matrix3x2, x: f32, y: f32) -> (f32, f32) {
    (x * m.M11 + y * m.M21 + m.M31, x * m.M12 + y * m.M22 + m.M32)
}

fn inverse_transform_point(m: Matrix3x2, x: f32, y: f32) -> Option<(f32, f32)> {
    let det = m.M11 * m.M22 - m.M12 * m.M21;
    if det.abs() <= f32::EPSILON {
        return None;
    }
    let x = x - m.M31;
    let y = y - m.M32;
    Some(((x * m.M22 - y * m.M21) / det, (y * m.M11 - x * m.M12) / det))
}

fn content_point_from_client(
    base: Matrix3x2,
    grid_top: f32,
    viewport_h: f32,
    scroll: f32,
    x: f32,
    y: f32,
) -> Option<(f32, f32)> {
    let (x, y) = inverse_transform_point(base, x, y)?;
    if y < grid_top || y > grid_top + viewport_h {
        return None;
    }
    Some((x, y - grid_top + scroll))
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

    fn motion_at(&self, elapsed: f32) -> PanelMotion {
        transition_from(self.motion_from, self.closing, elapsed)
    }

    fn motion(&self) -> PanelMotion {
        self.motion_at(self.anim_start.elapsed().as_secs_f32())
    }

    fn base_transform(&self, motion: PanelMotion) -> Matrix3x2 {
        panel_transform(motion, self.anchor_x, self.height, self.dpi)
    }

    /// Per-row open progress (0→1) for a content-space element at vertical `content_top`.
    /// Elements near the top of the viewport start first and lower ones follow, so the
    /// whole drawer — section headers, tiles and the add button — assembles as a single
    /// top-to-bottom wave. Closing and reopened panels keep the content assembled.
    fn row_progress(&self, elapsed: f32, content_top: f32) -> f32 {
        if self.closing || self.cascade_suppressed {
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

    /// Map a client point through the inverse panel transform into un-scrolled content.
    fn content_point(&self, x: f32, y: f32, motion: PanelMotion) -> Option<(f32, f32)> {
        content_point_from_client(
            self.base_transform(motion),
            self.grid_top(),
            self.viewport_h,
            self.scroll,
            x,
            y,
        )
    }

    /// The layout cell index under a client-space point, at its current animated pose.
    fn hit_cell(&self, x: f32, y: f32, motion: PanelMotion, elapsed: f32) -> Option<usize> {
        let (cx, cy) = self.content_point(x, y, motion)?;
        self.layout.cells.iter().position(|cell| {
            let progress = self.row_progress(elapsed, cell.rect.top);
            let cell = risen_rect(to_d2d(cell.rect), progress, TILE_RISE * self.dpi);
            in_rect(&cell, cx, cy)
        })
    }

    /// True if a client-space point is over the "new category" button.
    fn hit_add_button(&self, x: f32, y: f32, motion: PanelMotion, elapsed: f32) -> bool {
        let Some((cx, cy)) = self.content_point(x, y, motion) else {
            return false;
        };
        let progress = self.row_progress(elapsed, self.layout.add_button.top);
        let button = risen_rect(
            to_d2d(self.layout.add_button),
            progress,
            HEADER_RISE * self.dpi,
        );
        in_rect(&button, cx, cy)
    }

    /// A custom category's rename/delete button under a client-space point, returned as
    /// `(category index, action)`. Only custom sections carry these controls.
    fn hit_header_action(
        &self,
        x: f32,
        y: f32,
        motion: PanelMotion,
        elapsed: f32,
    ) -> Option<(usize, HeaderAction)> {
        let (cx, cy) = self.content_point(x, y, motion)?;
        for band in &self.layout.bands {
            if let SectionKind::Custom(ci) = self.sections[band.section].kind {
                let progress = self.row_progress(elapsed, band.header.top);
                let header = risen_rect(to_d2d(band.header), progress, HEADER_RISE * self.dpi);
                let (rename, delete) = header_action_rects(header, self.dpi);
                if in_rect(&delete, cx, cy) {
                    return Some((ci, HeaderAction::Delete));
                }
                if in_rect(&rename, cx, cy) {
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

/// Batch-extract icons for every path entry that lacks a cached BMP — in a helper
/// process, so a crashing third-party shell extension can never take the dock down.
/// Virtual (PIDL) entries get no icon at all; the drawer falls back to a glyph tile.
/// Synchronous: used at startup by `warm_cache`, before any window exists. The
/// interactive open path instead extracts in the background (see `schedule_icon_load`).
unsafe fn preload_missing_icons(entries: &mut [Entry]) {
    let jobs = missing_extraction_jobs(entries);
    if jobs.is_empty() {
        return;
    }
    let results = crate::drawer_icons::extract(&jobs);
    mark_failed_jobs(
        &jobs.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
        &results,
    );
    if results.is_empty() {
        return;
    }
    PRELOADED_ICONS.with(|cache| {
        let mut cache = cache.borrow_mut();
        for (key, png) in &results {
            cache.insert(key.clone(), png.clone());
        }
    });
    for entry in entries.iter_mut() {
        if let Some((_, png)) = results.iter().find(|(k, _)| *k == entry.key) {
            entry.preloaded_icon = Some(png.clone());
        }
    }
}

/// The `(key, path)` jobs for entries that still need an icon extracted — skipping
/// keys that already failed once this session.
fn missing_extraction_jobs(entries: &[Entry]) -> Vec<(String, String)> {
    let failed = FAILED_EXTRACT.with(|f| f.borrow().clone());
    entries
        .iter()
        .filter_map(|entry| {
            if entry.preloaded_icon.is_some() {
                return None;
            }
            entry
                .path
                .as_deref()
                .map(|path| (entry.key.clone(), path.to_string()))
        })
        .filter(|(key, _)| !failed.contains(key))
        .collect()
}

/// Remember which job keys produced no BMP, so they are not retried every open.
fn mark_failed_jobs(job_keys: &[String], results: &[(String, PathBuf)]) {
    let failed: Vec<String> = job_keys
        .iter()
        .filter(|key| !results.iter().any(|(result_key, _)| result_key == *key))
        .cloned()
        .collect();
    if !failed.is_empty() {
        FAILED_EXTRACT.with(|set| set.borrow_mut().extend(failed));
    }
}

/// Warm the drawer's shell scan + icon extraction during dock startup, before the
/// user opens it. Icon extraction runs out-of-process (`drawer_icons`), so a shell
/// extension fault here only costs the icons, never the dock.
///
/// `dc` is the dock's Direct2D context (on the shared device): every cached BMP is
/// ALSO decoded into `BITMAP_CACHE` right here, so the first drawer open reuses
/// ready-made GPU bitmaps and uploads nothing.
pub unsafe fn warm_cache(dc: &ID2D1DeviceContext) {
    let categories = categories::load();
    let mut entries = build_entries(&categories);
    preload_missing_icons(&mut entries);
    decode_into_cache(dc, &entries);
}

/// Decode every entry's cached BMP into `BITMAP_CACHE` (idempotent; existing keys
/// are kept). Runs at startup and after a GPU reset, never on the drawer open path.
unsafe fn decode_into_cache(dc: &ID2D1DeviceContext, entries: &[Entry]) {
    for entry in entries {
        let Some(bmp) = entry.preloaded_icon.as_deref() else {
            continue;
        };
        let key = entry.key.clone();
        let cached = BITMAP_CACHE.with(|c| c.borrow().contains_key(&key));
        if cached {
            continue;
        }
        if let Some(icon) = crate::drawer_icons::load_icon_bitmap(dc, bmp) {
            BITMAP_CACHE.with(|c| c.borrow_mut().insert(key, icon));
        }
    }
}

/// The entry's GPU bitmap from the shared-device cache, if one was decoded earlier.
/// Cloning a COM interface is an AddRef — the drawer and the cache share it.
fn cached_icon(key: &str) -> Option<ID2D1Bitmap1> {
    BITMAP_CACHE.with(|c| c.borrow().get(key).cloned())
}

/// Drop every cached GPU bitmap. Called when the dock's GPU is rebuilt (device
/// lost): bitmaps belong to the old device and must not be drawn on the new one.
pub fn invalidate_icon_cache() {
    BITMAP_CACHE.with(|c| c.borrow_mut().clear());
}

/// Re-decode the cached BMPs onto a (fresh) device, after a GPU reset. Uses the
/// fast cached desktop scan and never re-extracts — missing icons fall back to
/// the batched background path on the next drawer open.
pub unsafe fn reload_icon_cache(dc: &ID2D1DeviceContext) {
    let categories = categories::load();
    let entries = build_entries(&categories);
    decode_into_cache(dc, &entries);
}

/// Decode one small batch of pending cached BMPs into GPU bitmaps. The drawer is
/// already visible — glyph tiles hold the place — so this runs on a short timer
/// instead of blocking the open. Returns true while more icons remain pending.
/// Successfully decoded bitmaps join `BITMAP_CACHE` so later opens reuse them.
unsafe fn decode_pending_batch(
    dc: &ID2D1DeviceContext,
    entries: &mut [Entry],
    pending: &mut Vec<usize>,
) -> bool {
    let take = pending.len().min(ICON_DECODE_BATCH);
    for _ in 0..take {
        let Some(idx) = pending.pop() else { break };
        let Some(entry) = entries.get_mut(idx) else {
            continue;
        };
        if let Some(bmp) = entry.preloaded_icon.as_deref() {
            if let Some(icon) = unsafe { crate::drawer_icons::load_icon_bitmap(dc, bmp) } {
                let key = entry.key.clone();
                BITMAP_CACHE.with(|c| c.borrow_mut().insert(key.clone(), icon.clone()));
                entry.icon = Some(icon);
            }
        }
    }
    !pending.is_empty()
}

/// Start loading icons without blocking the open:
///
/// * Entries that already have a cached BMP (the common case — `warm_cache` filled
///   them at startup) are queued for batched decode on a short timer, so the first
///   frame is cheap and icons pop in over a few ticks instead of stalling the
///   `ShowWindow`.
/// * Entries with no cached BMP yet are extracted in a background helper process;
///   when its results arrive (`WM_APP_ICONS_READY`) they join the same decode
///   queue. A hung/crashed helper costs icons, never the open.
unsafe fn schedule_icon_load(hwnd: HWND, panel: &mut Drawer) {
    if !panel.pending_icons.is_empty() {
        let _ = SetTimer(hwnd, TIMER_ICON_LOAD, TIMER_ICON_LOAD_MS, None);
    }
    if panel.extracting {
        return;
    }
    let jobs = missing_extraction_jobs(&panel.entries);
    if jobs.is_empty() {
        return;
    }
    panel.extracting = true;
    panel.extraction_keys = jobs.iter().map(|(key, _)| key.clone()).collect();
    let owner = hwnd.0 as isize;
    std::thread::spawn(move || {
        // `extract` never panics and caps its wait, so the thread always posts
        // (or frees its payload if the drawer window is already gone).
        let results = crate::drawer_icons::extract(&jobs);
        let payload = Box::into_raw(Box::new(results));
        let ok = unsafe {
            PostMessageW(
                HWND(owner as *mut c_void),
                WM_APP_ICONS_READY,
                WPARAM(0),
                LPARAM(payload as isize),
            )
        }
        .is_ok();
        if !ok {
            drop(unsafe { Box::from_raw(payload) });
        }
    });
}

unsafe fn reload_entries(hwnd: HWND, panel: &mut Drawer, refresh_scan: bool) {
    if refresh_scan {
        desktop_scan::invalidate_cache();
        // A fresh scan deserves fresh icon attempts.
        FAILED_EXTRACT.with(|set| set.borrow_mut().clear());
    }
    panel.entries = build_entries(&panel.categories);
    // Re-seed the decode queue with whatever icons are already cached on disk.
    panel.pending_icons = panel
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.preloaded_icon.is_some())
        .map(|(i, _)| i)
        .collect();
    panel.extraction_keys.clear();
    panel.extracting = false;
    panel.hovered = -1;
    panel.drag = None;
    panel.relayout();
    render(panel);
    schedule_icon_load(hwnd, panel);
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

fn fit_icon(container: D2D_RECT_F, size: D2D_SIZE_F) -> D2D_RECT_F {
    if size.width <= 0.0 || size.height <= 0.0 {
        return container;
    }
    let cw = container.right - container.left;
    let ch = container.bottom - container.top;
    // Shell fallbacks can be only 16/32px. Fit them to the same visual slot as
    // high-resolution icons instead of leaving them at their source pixel size.
    let scale = (cw / size.width).min(ch / size.height);
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
            material_color(panel.material.hover_rgb, panel.material.hover_alpha * a),
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
        material_color(panel.material.text_rgb, 0.98 * alpha),
        false,
    );
}

unsafe fn render(panel: &Drawer) {
    let elapsed = panel.anim_start.elapsed().as_secs_f32();
    let motion = panel.motion_at(elapsed);
    let a = motion.alpha;
    let dpi = panel.dpi;
    let s = |v: f32| v * dpi;
    let dc = panel.glass.dc();

    // The whole panel subtly scales and rises from the dock button. We anchor the scale
    // at the button's X (not the panel centre) so a panel clamped to a monitor edge still
    // appears to grow *out of the button that summoned it*. Close reverses the same
    // transform, keeping the motion spatial instead of snapping away.
    let base = panel.base_transform(motion);
    let open_scale = base.M11;

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
    let bg_top_left = transform_point(base, bg.left, bg.top);
    let bg_bottom_right = transform_point(base, bg.right, bg.bottom);
    let _ = panel.glass.update_backdrop(
        (
            bg_top_left.0,
            bg_top_left.1,
            bg_bottom_right.0,
            bg_bottom_right.1,
        ),
        panel.material.radius * dpi * open_scale,
        if panel.material.backdrop { a } else { 0.0 },
    );
    // An update can disable a lost/unsupported compositor surface. Check after
    // updating so even that frame gets an opaque, readable fallback.
    let background_alpha = if panel.material.backdrop && !panel.glass.has_backdrop() {
        0.98
    } else {
        panel.material.alpha
    };
    fill_round(
        dc,
        &panel.brush,
        bg,
        panel.material.radius * dpi,
        material_color(panel.material.rgb, background_alpha * a),
    );
    panel.brush.SetColor(&material_color(
        panel.material.border_rgb,
        panel.material.border_alpha * a,
    ));
    dc.DrawRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: bg,
            radiusX: panel.material.radius * dpi,
            radiusY: panel.material.radius * dpi,
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
        material_color(panel.material.text_rgb, a),
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
            material_color(panel.material.muted_rgb, a),
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
        if let Some((cx, cy)) = panel.content_point(drag.cur_x, drag.cur_y, motion) {
            let m = metrics(panel.width, panel.dpi);
            if let Some((sec_idx, _)) =
                drawer_layout::drop_target(&panel.sections, &panel.layout, &m, cx, cy)
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
                        material_color(
                            panel.material.accent_rgb,
                            panel.material.selected_alpha * a,
                        ),
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
        let hdr = risen_rect(to_d2d(band.header), hp, s(HEADER_RISE));
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
            material_color(panel.material.muted_rgb, ha),
            false,
        );
        panel.brush.SetColor(&material_color(
            panel.material.muted_rgb,
            panel.material.divider_alpha * ha,
        ));
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
                material_color(panel.material.muted_rgb, 0.9 * ha),
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
        let cell_p = panel.row_progress(elapsed, cell.rect.top);
        draw_cell(
            panel,
            risen_rect(to_d2d(cell.rect), cell_p, s(TILE_RISE)),
            &panel.entries[cell.entry],
            a * cell_p,
            hovered,
            drag_src,
        );
    }

    // "new category" button at the bottom of the content (last to ride the wave in)
    let ab0 = to_d2d(panel.layout.add_button);
    if ab0.bottom >= vis_top && ab0.top <= vis_bot {
        let abp = panel.row_progress(elapsed, panel.layout.add_button.top);
        let aba = a * abp;
        let ab = risen_rect(ab0, abp, s(HEADER_RISE));
        let round = D2D1_ROUNDED_RECT {
            rect: ab,
            radiusX: s(10.0),
            radiusY: s(10.0),
        };
        panel.brush.SetColor(&material_color(
            panel.material.hover_rgb,
            panel.material.hover_alpha * 0.4 * aba,
        ));
        dc.FillRoundedRectangle(&round, &panel.brush);
        panel.brush.SetColor(&material_color(
            panel.material.border_rgb,
            panel.material.border_alpha * 0.45 * aba,
        ));
        dc.DrawRoundedRectangle(&round, &panel.brush, s(1.0), None);
        let txt = rect(ab.left + s(10.0), ab.top, ab.right - s(10.0), ab.bottom);
        draw_text(
            dc,
            &panel.brush,
            "＋  新建分类",
            &panel.section,
            txt,
            material_color(panel.material.muted_rgb, aba),
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
        fill_round(
            dc,
            &panel.brush,
            bar,
            s(1.5),
            material_color(panel.material.muted_rgb, 0.32 * a),
        );
    }

    // --- drag ghost: keep the raw client pointer centred under the base transform ---
    if let Some(drag) = panel.drag.as_ref().filter(|d| d.active) {
        if let Some((px, py)) = inverse_transform_point(base, drag.cur_x, drag.cur_y) {
            let cw = s(CELL_W);
            let ch = s(CELL_H);
            let gx = px - cw / 2.0;
            let gy = py - ch / 2.0;
            let gcell = rect(gx, gy, gx + cw, gy + ch);
            fill_round(
                dc,
                &panel.brush,
                gcell,
                12.0 * dpi,
                material_color(
                    panel.material.rgb,
                    (panel.material.alpha + 0.08).min(1.0) * a,
                ),
            );
            panel.brush.SetColor(&material_color(
                panel.material.border_rgb,
                panel.material.border_alpha * a,
            ));
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
    panel.motion_from = panel.motion();
    panel.cascade_suppressed = true;
    panel.closing = true;
    panel.hovered = -1;
    panel.drag = None;
    let _ = ReleaseCapture();
    panel.anim_start = Instant::now();
    render(panel);
    wake_owner(panel);
}

unsafe fn reopen(hwnd: HWND, panel: &mut Drawer) {
    if !panel.closing {
        return;
    }
    panel.motion_from = panel.motion();
    panel.closing = false;
    panel.anim_start = Instant::now();
    render(panel);
    let _ = SetForegroundWindow(hwnd);
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
    if panel.closing {
        return;
    }
    let elapsed = panel.anim_start.elapsed().as_secs_f32();
    let motion = panel.motion_at(elapsed);
    if let Some(ci) = panel.hit_cell(x, y, motion, elapsed) {
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
    if panel.closing {
        return;
    }
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
    let elapsed = panel.anim_start.elapsed().as_secs_f32();
    let motion = panel.motion_at(elapsed);
    let hovered = panel
        .hit_cell(x, y, motion, elapsed)
        .map(|i| i as i32)
        .unwrap_or(-1);
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
    let menu_style = crate::menu_style::style(menu, crate::dock_theme(panel.owner), hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    drop(menu_style);
    let _ = DestroyMenu(menu);

    match cmd.0 as usize {
        ID_CTX_OPEN => {
            if let Some(item) = panel.entries.get(entry) {
                desktop_scan::launch(
                    item.path.as_deref(),
                    item.pidl.as_ref().map(|p| p.as_ptr()),
                    Some(&item.key),
                );
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
    let menu_style = crate::menu_style::style(menu, crate::dock_theme(panel.owner), hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    drop(menu_style);
    let _ = DestroyMenu(menu);

    match cmd.0 as usize {
        ID_CTX_NEW_CATEGORY => add_category(hwnd, panel),
        ID_CTX_RESTORE => {
            panel.categories.restore_hidden();
            if let Err(error) = categories::save(&panel.categories) {
                crate::error_log::write("恢复抽屉隐藏项失败", &error);
            }
            reload_entries(hwnd, panel, false);
        }
        ID_CTX_REFRESH => reload_entries(hwnd, panel, true),
        _ => {}
    }
}

/// Commit a dropped tile into the section + slot under the cursor (None = uncategorized).
unsafe fn commit_drop(panel: &mut Drawer, entry: usize, x: f32, y: f32) {
    if panel.closing {
        return;
    }
    let motion = panel.motion();
    let Some((cx, cy)) = panel.content_point(x, y, motion) else {
        return;
    };
    let m = metrics(panel.width, panel.dpi);
    let Some((sec_idx, index)) =
        drawer_layout::drop_target(&panel.sections, &panel.layout, &m, cx, cy)
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
    if panel.closing || panel.max_scroll <= 0.0 {
        return;
    }
    let step = (CELL_H + GAP_Y) * panel.dpi;
    panel.scroll = (panel.scroll - (delta / 120.0) * step).clamp(0.0, panel.max_scroll);
    render(panel);
}

unsafe fn on_up(hwnd: HWND, panel: &mut Drawer, x: f32, y: f32) {
    if panel.closing {
        return;
    }
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
                Some(&entry.key),
            );
            start_close(hwnd, panel);
        }
        return;
    }
    let elapsed = panel.anim_start.elapsed().as_secs_f32();
    let motion = panel.motion_at(elapsed);
    // Not a tile press: category rename/delete controls, then the "new category" button.
    if let Some((ci, action)) = panel.hit_header_action(x, y, motion, elapsed) {
        match action {
            HeaderAction::Delete => delete_category(panel, ci),
            HeaderAction::Rename => begin_rename(hwnd, ci),
        }
        return;
    }
    if panel.hit_add_button(x, y, motion, elapsed) {
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

    // Share the dock's D3D11 + Direct2D devices so the startup-decoded icon
    // bitmaps can be drawn on this surface directly (no per-open upload).
    let shared = crate::dock_shared_gpu(dock_hwnd);
    let glass = match Glass::new(
        hwnd,
        width as u32,
        height as u32,
        shared.as_ref().map(|(d3d, d2d)| (d3d, d2d)),
    ) {
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

    // The drawer shares the dock's Direct2D device, so bitmaps decoded at startup
    // (`warm_cache` → BITMAP_CACHE) are reused directly — nothing to upload. Only
    // entries without a cached bitmap fall through to the batched decode timer.
    let mut pending_icons: Vec<usize> = Vec::new();
    for (i, entry) in entries.iter_mut().enumerate() {
        match cached_icon(&entry.key) {
            Some(icon) => entry.icon = Some(icon),
            None => {
                if entry.preloaded_icon.is_some() {
                    pending_icons.push(i);
                }
            }
        }
    }

    let panel = Box::into_raw(Box::new(Drawer {
        owner: dock_hwnd,
        material: crate::dock_theme(dock_hwnd).surface(),
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
        motion_from: PanelMotion::HIDDEN,
        cascade_suppressed: false,
        pending_icons,
        extracting: false,
        extraction_keys: Vec::new(),
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, panel as isize);
    PANEL_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    render(&*panel);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    schedule_icon_load(hwnd, &mut *panel);
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

/// Close through the normal animated path, including when its theme changes.
pub unsafe fn hide() {
    let raw = PANEL_HWND.load(Ordering::Relaxed);
    if raw != 0 {
        let hwnd = HWND(raw as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Destroy the GPU popup synchronously before its owner's devices are dropped.
/// Exit must not enqueue the normal close animation or another owner frame.
pub unsafe fn shutdown() {
    let raw = PANEL_HWND.swap(0, Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if IsWindow(hwnd).as_bool() {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Drawer;
        if !ptr.is_null() {
            (*ptr).closing = true;
            (*ptr).owner = HWND::default();
        }
        let _ = DestroyWindow(hwnd);
    }
}

/// Toggle the drawer anchored above a dock button. The click that dismisses through
/// click-away stays consumed even while the close animation is still running; only a
/// separate toggle reverses that closing window in place.
pub unsafe fn toggle(dock_hwnd: HWND, anchor_cx: i32, anchor_top: i32) {
    let existing = PANEL_HWND.load(Ordering::Relaxed);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Drawer;
            if ptr.is_null() {
                clear_reopen_guard();
                let _ = SendMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            } else if (*ptr).closing {
                let should_reopen = CLICK_AWAY_GUARD.with(|slot| {
                    reopen_closing_with_toggle(
                        &mut slot.borrow_mut(),
                        dock_hwnd.0 as isize,
                        GetTickCount(),
                    )
                });
                if should_reopen {
                    reopen(hwnd, &mut *ptr);
                }
            } else {
                clear_reopen_guard();
                start_close(hwnd, &mut *ptr);
            }
            return;
        }
    }
    if consume_reopen_guard(dock_hwnd, GetTickCount()) {
        return;
    }
    open(dock_hwnd, anchor_cx, anchor_top);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(result) = crate::menu_style::handle_message(hwnd, msg, wparam, lparam) {
            return result;
        }
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
                let elapsed = panel.anim_start.elapsed().as_secs_f32();
                let motion = panel.motion_at(elapsed);
                if let Some(ci) = panel.hit_cell(x, y, motion, elapsed) {
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
                        let panel = &mut *ptr;
                        if !panel.closing {
                            let activated = HWND(lparam.0 as *mut c_void);
                            record_click_away_guard(panel.owner, activated, GetTickCount());
                        }
                        start_close(hwnd, panel);
                    } else {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_ICON_LOAD && !ptr.is_null() => {
                let panel = &mut *ptr;
                if panel.closing {
                    let _ = KillTimer(hwnd, TIMER_ICON_LOAD);
                } else if !decode_pending_batch(
                    panel.glass.dc(),
                    &mut panel.entries,
                    &mut panel.pending_icons,
                ) {
                    // Queue drained: stop ticking and show the final frame.
                    let _ = KillTimer(hwnd, TIMER_ICON_LOAD);
                    render(panel);
                    wake_owner(panel);
                } else {
                    // More icons are coming — redraw what we have now.
                    render(panel);
                }
                LRESULT(0)
            }
            WM_APP_ICONS_READY if !ptr.is_null() => {
                // The background extraction finished: merge its BMPs into the cache,
                // queue the newly-covered entries for decode, and remember which
                // keys failed so we don't re-extract them every open.
                let results = Box::from_raw(lparam.0 as *mut Vec<(String, PathBuf)>);
                let panel = &mut *ptr;
                panel.extracting = false;
                mark_failed_jobs(&panel.extraction_keys, &results);
                panel.extraction_keys.clear();
                let mut new_pending: Vec<usize> = Vec::new();
                PRELOADED_ICONS.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    for (key, png) in results.iter() {
                        cache.insert(key.clone(), png.clone());
                    }
                });
                for (i, entry) in panel.entries.iter_mut().enumerate() {
                    if let Some((_, png)) = results.iter().find(|(key, _)| *key == entry.key) {
                        entry.preloaded_icon = Some(png.clone());
                        if entry.icon.is_none() {
                            new_pending.push(i);
                        }
                    }
                }
                if !new_pending.is_empty() {
                    panel.pending_icons.extend(new_pending);
                    let _ = SetTimer(hwnd, TIMER_ICON_LOAD, TIMER_ICON_LOAD_MS, None);
                    render(panel);
                    wake_owner(panel);
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
                let _ = KillTimer(hwnd, TIMER_ICON_LOAD);
                PANEL_HWND.store(0, Ordering::Relaxed);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        click_away_guard, content_point_from_client, fit_icon, in_rect, mul, panel_transform, rect,
        reopen_closing_with_toggle, risen_rect, take_reopen_guard, transform_point,
        transition_from, translate, PanelMotion, CLOSE_ANIM_SECS, D2D_SIZE_F, OPEN_ANIM_SECS,
        REOPEN_GUARD_MS,
    };

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.0001,
            "expected {expected}, got {actual}"
        );
    }

    fn assert_motion(actual: PanelMotion, expected: PanelMotion) {
        assert_close(actual.alpha, expected.alpha);
        assert_close(actual.e, expected.e);
    }

    #[test]
    fn small_shell_icons_fill_the_drawer_slot() {
        let dest = fit_icon(
            rect(10.0, 20.0, 66.0, 76.0),
            D2D_SIZE_F {
                width: 16.0,
                height: 16.0,
            },
        );
        assert_eq!(
            (dest.left, dest.top, dest.right, dest.bottom),
            (10.0, 20.0, 66.0, 76.0)
        );
    }

    #[test]
    fn fitting_icons_preserves_aspect_ratio_and_centers_them() {
        let dest = fit_icon(
            rect(0.0, 0.0, 56.0, 56.0),
            D2D_SIZE_F {
                width: 32.0,
                height: 16.0,
            },
        );
        assert_eq!(
            (dest.left, dest.top, dest.right, dest.bottom),
            (0.0, 14.0, 56.0, 42.0)
        );
        let dest = fit_icon(
            rect(0.0, 0.0, 56.0, 56.0),
            D2D_SIZE_F {
                width: 256.0,
                height: 256.0,
            },
        );
        assert_eq!(
            (dest.left, dest.top, dest.right, dest.bottom),
            (0.0, 0.0, 56.0, 56.0)
        );
    }

    #[test]
    fn middle_frame_render_point_inverts_to_the_same_content_hit() {
        let motion = transition_from(PanelMotion::HIDDEN, false, OPEN_ANIM_SECS * 0.43);
        assert!(motion.e > 0.0 && motion.e < 1.0);
        let base = panel_transform(motion, 117.0, 420.0, 1.5);
        let grid_top = 48.0;
        let viewport_h = 280.0;
        let scroll = 73.0;
        let content = (88.0, 146.0);
        let content_to_client = mul(translate(0.0, grid_top - scroll), base);
        let client = transform_point(content_to_client, content.0, content.1);

        let hit = content_point_from_client(base, grid_top, viewport_h, scroll, client.0, client.1)
            .expect("rendered content point must remain inside the viewport");
        assert_close(hit.0, content.0);
        assert_close(hit.1, content.1);
        assert!(in_rect(&rect(40.0, 120.0, 140.0, 200.0), hit.0, hit.1));

        let title = transform_point(base, 88.0, grid_top - 0.5);
        assert!(
            content_point_from_client(base, grid_top, viewport_h, scroll, title.0, title.1,)
                .is_none()
        );
    }

    #[test]
    fn row_rise_hit_rect_matches_the_rendered_rect() {
        let static_rect = rect(10.0, 100.0, 60.0, 140.0);
        let animated_rect = risen_rect(static_rect, 0.5, 18.0);
        assert_eq!((animated_rect.top, animated_rect.bottom), (109.0, 149.0));

        assert!(in_rect(&static_rect, 20.0, 105.0));
        assert!(!in_rect(&animated_rect, 20.0, 105.0));
        assert!(!in_rect(&static_rect, 20.0, 145.0));
        assert!(in_rect(&animated_rect, 20.0, 145.0));
    }

    #[test]
    fn only_dock_click_away_arms_a_one_shot_reopen_guard() {
        let owner = 42;
        assert_eq!(click_away_guard(owner, 77, 1_000), None);
        assert_eq!(click_away_guard(0, 0, 1_000), None);

        let mut guard = click_away_guard(owner, owner, 1_000);
        assert!(take_reopen_guard(
            &mut guard,
            owner,
            1_000 + REOPEN_GUARD_MS - 1,
        ));
        assert!(!take_reopen_guard(&mut guard, owner, 1_000));

        let mut expired = click_away_guard(owner, owner, 1_000);
        assert!(!take_reopen_guard(
            &mut expired,
            owner,
            1_000 + REOPEN_GUARD_MS,
        ));

        let mut other_owner = click_away_guard(owner, owner, 1_000);
        assert!(!take_reopen_guard(&mut other_owner, 99, 1_001));
        assert_eq!(other_owner, None);
    }

    #[test]
    fn click_away_close_consumes_only_the_original_dock_toggle() {
        let owner = 42;
        let mut click_away = click_away_guard(owner, owner, 1_000);
        assert!(!reopen_closing_with_toggle(&mut click_away, owner, 1_005));
        assert_eq!(click_away, None);

        let mut independent_toggle = None;
        assert!(reopen_closing_with_toggle(
            &mut independent_toggle,
            owner,
            1_006
        ));
    }

    #[test]
    fn reopen_guard_age_handles_get_tick_count_wrap() {
        let owner = 42;
        let tick = u32::MAX - 99;
        let mut fresh = click_away_guard(owner, owner, tick);
        assert!(take_reopen_guard(&mut fresh, owner, 50));

        let mut expired = click_away_guard(owner, owner, tick);
        assert!(!take_reopen_guard(&mut expired, owner, 150));
    }

    #[test]
    fn close_reopen_is_continuous_and_preserves_normal_curves() {
        let opening = transition_from(PanelMotion::HIDDEN, false, OPEN_ANIM_SECS * 0.5);
        assert_close(opening.alpha, 0.5);
        assert_close(opening.e, 0.96875);
        let closing = transition_from(PanelMotion::VISIBLE, true, CLOSE_ANIM_SECS * 0.5);
        assert_close(closing.alpha, 0.5);
        assert_close(closing.e, 0.875);

        let sampled = transition_from(PanelMotion::VISIBLE, true, CLOSE_ANIM_SECS * 0.37);
        assert!(sampled.alpha > 0.0 && sampled.alpha < 1.0);
        assert!(sampled.e > 0.0 && sampled.e < 1.0);
        assert_motion(transition_from(sampled, false, 0.0), sampled);
        assert_motion(
            transition_from(sampled, false, OPEN_ANIM_SECS),
            PanelMotion::VISIBLE,
        );
    }

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
