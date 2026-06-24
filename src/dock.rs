//! Dock model + magnification math + frame-rate-independent easing.
//! Pure logic, no Win32 — keeps the "fluid" feel in one testable place.

use std::time::Instant;

use crate::content::ContentKind;

// All sizes are LOGICAL pixels @96dpi; multiply by `dpi` for device pixels.
pub const BASE_ICON: f32 = 50.0;
pub const MAX_SCALE: f32 = 1.9;
pub const GAP: f32 = 14.0;
pub const PILL_PAD_X: f32 = 16.0;
pub const PILL_PAD_Y: f32 = 9.0;
pub const TOP_PAD: f32 = 12.0;
pub const SHOWN_GAP: f32 = 8.0; // gap above the screen bottom when revealed
pub const INFLUENCE: f32 = 130.0; // horizontal reach of the magnification, px
pub const EASE_TAU: f32 = 0.030; // seconds; smaller = snappier (tighter cursor tracking)
pub const CORNER: f32 = 0.24; // icon corner radius as a fraction of icon size
pub const BOUNCE_DUR: f32 = 0.45; // seconds for the click hop
pub const BOUNCE_AMP: f32 = 0.42; // hop height as a fraction of icon size
pub const REVEAL_TAU: f32 = 0.14; // auto-hide slide smoothing (seconds)
pub const HIDE_SLIVER: f32 = 3.0; // visible sliver when hidden (logical px)
pub const TRIGGER_PX: f32 = 4.0; // bottom hot-zone height that reveals the dock
pub const DIVIDER_W: f32 = 16.0; // width of the separator slot (logical px)

/// What a slot in the dock represents — drives layout, magnification, hit-testing
/// and what a click does. The dock row is ordered: Start, pinned…, Divider,
/// running windows… (left = pinned, right = open windows, like the user's macOS-
/// style mental model).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ItemRole {
    Start,   // opens the Windows Start menu
    Pinned,  // launches `path` via the shell
    Running, // activates the open window identified by `hwnd`
    Divider, // a vertical separator: no icon, no magnify, not clickable
}

/// Resting width of a slot in logical px (before DPI + magnification).
pub fn role_base_width(role: ItemRole) -> f32 {
    match role {
        ItemRole::Divider => DIVIDER_W,
        _ => BASE_ICON,
    }
}

pub struct DockItem {
    pub label: String,
    pub glyph: &'static str,    // fallback if the icon can't be extracted
    pub color: (f32, f32, f32), // fallback tile color
    pub path: Option<String>,   // application, shortcut, file, or folder to open
    pub icon: Option<String>,   // optional separate icon source
    pub kind: ContentKind,
    pub role: ItemRole,
    pub hwnd: Option<isize>, // for Running items: the window to activate
}

pub struct IconFrame {
    pub idx: usize,
    pub cx: f32,
    pub scale: f32,
    pub bounce_y: f32, // upward offset from the click hop
}

pub struct Frame {
    pub baseline: f32,
    pub icons: Vec<IconFrame>,
    pub pill: (f32, f32, f32, f32), // left, top, right, bottom
}

pub struct Dock {
    pub items: Vec<DockItem>,
    pub scale: Vec<f32>,
    pub bounce: Vec<f32>, // per-item click-hop phase, 1.0 -> 0.0
    pub cursor_x: Option<f32>,
    pub reveal: f32,        // 0 = fully hidden (slid down), 1 = fully shown
    pub reveal_target: f32, // where reveal is easing toward
    pub dpi: f32,
    pub win_w: f32,
    pub win_h: f32,
    last: Instant,
}

/// Window size (device px) needed to fit every slot at full magnification PLUS the
/// click hop, so a bouncing, magnified icon never gets clipped at the top. Takes
/// the actual items because slots have different widths (a divider is narrow).
pub fn window_size(items: &[DockItem], dpi: f32) -> (u32, u32) {
    let base = BASE_ICON * dpi;
    let n = items.len();
    let row: f32 = items
        .iter()
        .map(|it| role_base_width(it.role) * dpi)
        .sum::<f32>()
        + GAP * dpi * n.saturating_sub(1) as f32;
    let w = row + 2.0 * PILL_PAD_X * dpi + base * 4.0; // bulge headroom
    let h = base * MAX_SCALE + BOUNCE_AMP * base + (PILL_PAD_Y + TOP_PAD + SHOWN_GAP) * dpi;
    (w.ceil() as u32, h.ceil() as u32)
}

impl Dock {
    pub fn new(items: Vec<DockItem>, dpi: f32, win_w: f32, win_h: f32) -> Dock {
        let n = items.len();
        Dock {
            items,
            scale: vec![1.0; n],
            bounce: vec![0.0; n],
            cursor_x: None,
            // Start shown; main.rs decides the resting target from settings + the
            // fullscreen state (always-resident vs. auto-hide vs. fullscreen retract).
            reveal: 1.0,
            reveal_target: 1.0,
            dpi,
            win_w,
            win_h,
            last: Instant::now(),
        }
    }

    /// How far (device px) content slides down when hidden (leaves a small sliver).
    fn hide_slide(&self) -> f32 {
        BASE_ICON * self.dpi + (2.0 * PILL_PAD_Y + SHOWN_GAP - HIDE_SLIVER) * self.dpi
    }

    /// Resting (un-magnified) width of slot `i` in device px.
    fn item_w(&self, i: usize) -> f32 {
        role_base_width(self.items[i].role) * self.dpi
    }

    pub fn dock_span_x(&self) -> (f32, f32) {
        let base = BASE_ICON * self.dpi;
        let gap = GAP * self.dpi;
        let n = self.items.len();
        let widths: f32 = (0..n).map(|i| self.item_w(i)).sum();
        let row = widths + gap * n.saturating_sub(1) as f32 + 2.0 * PILL_PAD_X * self.dpi + base; // bulge slack
        let cx = self.win_w / 2.0;
        (cx - row / 2.0, cx + row / 2.0)
    }

    /// Client-space rectangle that should intercept the mouse. Everything outside it
    /// is click-through. When hidden it's just a thin strip at the very bottom edge.
    pub fn hit_zone(&self, shown: bool) -> (f32, f32, f32, f32) {
        let (l, r) = self.dock_span_x();
        if shown {
            let top = self.win_h - (BASE_ICON * MAX_SCALE * self.dpi + 2.0 * PILL_PAD_Y * self.dpi);
            (l, top.max(0.0), r, self.win_h)
        } else {
            (l, self.win_h - TRIGGER_PX * self.dpi, r, self.win_h)
        }
    }

    pub fn interactive_hit_zone(&self) -> (f32, f32, f32, f32) {
        self.hit_zone(self.reveal > 0.02)
    }

    /// Start the click hop for item `i`.
    pub fn bump(&mut self, i: usize) {
        if let Some(b) = self.bounce.get_mut(i) {
            *b = 1.0;
        }
    }

    fn metrics(&self) -> (f32, f32) {
        (BASE_ICON * self.dpi, GAP * self.dpi)
    }

    fn rest_centers(&self) -> Vec<f32> {
        let (_, gap) = self.metrics();
        let n = self.items.len();
        let widths: Vec<f32> = (0..n).map(|i| self.item_w(i)).collect();
        let row: f32 = widths.iter().sum::<f32>() + gap * n.saturating_sub(1) as f32;
        let mut x = (self.win_w - row) / 2.0;
        let mut centers = Vec::with_capacity(n);
        for w in &widths {
            centers.push(x + w / 2.0);
            x += w + gap;
        }
        centers
    }

    /// Cosine "bell" falloff: scale by horizontal distance from the cursor.
    fn target(&self, rest_cx: f32) -> f32 {
        match self.cursor_x {
            None => 1.0,
            Some(cx) => {
                let infl = INFLUENCE * self.dpi;
                let d = (cx - rest_cx).abs();
                if d >= infl {
                    1.0
                } else {
                    let t = d / infl;
                    1.0 + (MAX_SCALE - 1.0) * 0.5 * (1.0 + (std::f32::consts::PI * t).cos())
                }
            }
        }
    }

    /// Advance the easing one frame. Returns true while still animating.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f32().min(0.05);
        self.last = now;
        let alpha = 1.0 - (-dt / EASE_TAU).exp(); // frame-rate independent
        let rc = self.rest_centers();
        let mut moving = false;
        for i in 0..self.items.len() {
            // Dividers never magnify; everything else follows the cursor bell.
            let tg = if self.items[i].role == ItemRole::Divider {
                1.0
            } else {
                self.target(rc[i])
            };
            let d = tg - self.scale[i];
            if d.abs() > 0.002 {
                moving = true;
                self.scale[i] += d * alpha;
            } else {
                self.scale[i] = tg;
            }
            if self.bounce[i] > 0.0 {
                self.bounce[i] = (self.bounce[i] - dt / BOUNCE_DUR).max(0.0);
                moving = true;
            }
        }
        // auto-hide slide easing
        let ra = 1.0 - (-dt / REVEAL_TAU).exp();
        let dr = self.reveal_target - self.reveal;
        if dr.abs() > 0.001 {
            self.reveal += dr * ra;
            moving = true;
        } else {
            self.reveal = self.reveal_target;
        }
        moving
    }

    /// Compute the current laid-out frame (dynamic widths, centered row).
    pub fn frame(&self) -> Frame {
        let (base, gap) = self.metrics();
        let n = self.items.len();
        if n == 0 {
            return Frame {
                baseline: self.win_h,
                icons: Vec::new(),
                pill: (0.0, 0.0, 0.0, 0.0),
            };
        }
        let widths: Vec<f32> = (0..n).map(|i| self.item_w(i) * self.scale[i]).collect();
        let total: f32 = widths.iter().sum::<f32>() + gap * (n as f32 - 1.0);
        let mut x = (self.win_w - total) / 2.0;
        // auto-hide slide: shown floats SHOWN_GAP above the bottom; hidden slides down.
        let shown_baseline = self.win_h - (PILL_PAD_Y + SHOWN_GAP) * self.dpi;
        let baseline = shown_baseline + (1.0 - self.reveal) * self.hide_slide();
        let mut icons = Vec::with_capacity(n);
        for i in 0..n {
            let w = widths[i];
            // single smooth hop: 0 at phase 1 -> peak at 0.5 -> 0 at phase 0
            let bounce_y = (self.bounce[i] * std::f32::consts::PI).sin() * BOUNCE_AMP * base;
            icons.push(IconFrame {
                idx: i,
                cx: x + w / 2.0,
                scale: self.scale[i],
                bounce_y,
            });
            x += w + gap;
        }
        let pill_l = icons[0].cx - widths[0] / 2.0 - PILL_PAD_X * self.dpi;
        let pill_r = icons[n - 1].cx + widths[n - 1] / 2.0 + PILL_PAD_X * self.dpi;
        let pill_t = baseline - base - PILL_PAD_Y * self.dpi;
        let pill_b = baseline + PILL_PAD_Y * self.dpi; // tracks the slide (clipped at win_h)
        Frame {
            baseline,
            icons,
            pill: (pill_l, pill_t, pill_r, pill_b),
        }
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        let f = self.frame();
        for ic in &f.icons {
            if self.items[ic.idx].role == ItemRole::Divider {
                continue; // separators aren't clickable
            }
            let w = self.item_w(ic.idx) * ic.scale;
            if x >= ic.cx - w / 2.0
                && x <= ic.cx + w / 2.0
                && y >= f.baseline - w
                && y <= f.baseline
            {
                return Some(ic.idx);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dock() -> Dock {
        Dock::new(Vec::new(), 1.0, 400.0, 140.0)
    }

    #[test]
    fn visible_hide_animation_keeps_full_interactive_zone() {
        let mut dock = dock();
        dock.reveal = 0.5;
        assert_eq!(dock.interactive_hit_zone(), dock.hit_zone(true));
        dock.reveal = 0.0;
        assert_eq!(dock.interactive_hit_zone(), dock.hit_zone(false));
    }
}
