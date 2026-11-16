//! Dock model + magnification math + frame-rate-independent easing.
//! Pure logic, no Win32 — keeps the "fluid" feel in one testable place.

use std::time::Instant;

use crate::agent_state::AgentState;
use crate::content::ContentKind;
use crate::theme::ThemePreset;

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
pub const AGENT_DIVIDER_W: f32 = 18.0; // subtle double-line gap before the Agent Zone
pub const ENTER_TAU: f32 = 0.17; // appear easing — slot opens + icon scales/fades in
pub const EXIT_TAU: f32 = 0.13; // disappear easing — a touch quicker than the open
pub const PRESENCE_GONE: f32 = 0.015; // below this, a fully-exited slot is dropped
pub const ENTER_RISE: f32 = 0.16; // how far (fraction of icon) a new icon rises into place
const RUNNING_TAU: f32 = 0.11;
/// Seconds for the cursor low-pass AT REST: `cursor_smooth` eases toward the raw
/// pointer with this time constant, filtering hand micro-jitter (±1px) while slow
/// deliberate drift still tracks closely.
const BLEND_TAU: f32 = 0.045;
/// Low-pass time constant while the pointer sweeps FAST. A fixed 45ms filter put
/// the magnification bell ~70px behind a fast cursor (45ms × ~1500px/s), so during
/// a quick sweep the bulge visibly chased the pointer — the animation read as
/// laggy and stiff. The effective constant tightens from `BLEND_TAU` to this as
/// the pointer accelerates past `BLEND_TRACK_PX_S` (see `adaptive_blend_tau`).
const BLEND_TAU_FAST: f32 = 0.010;
/// Raw pointer speed (px/s) where the low-pass starts tightening, reaching
/// `BLEND_TAU_FAST` at twice this speed. Below it the filter behaves exactly as
/// the original 45ms constant, so rest-time micro-jitter stays filtered.
const BLEND_TRACK_PX_S: f32 = 300.0;
const MAX_FRAME_DT: f32 = 1.0 / 30.0;
const IDLE_GAP: f32 = 0.10;

/// The low-pass time constant for the measured raw-pointer speed: the full rest
/// filter up to `BLEND_TRACK_PX_S`, tightening linearly to `BLEND_TAU_FAST` at
/// twice that speed. Speed-adaptive smoothing keeps two properties at once —
/// jitter suppression when the hand is still, tight tracking when it sweeps.
fn adaptive_blend_tau(speed: f32) -> f32 {
    let t = (speed / BLEND_TRACK_PX_S - 1.0).clamp(0.0, 1.0);
    BLEND_TAU + (BLEND_TAU_FAST - BLEND_TAU) * t
}

fn animation_dt(elapsed: f32) -> f32 {
    if elapsed >= IDLE_GAP {
        // Never turn time spent blocked or idle into a visible animation jump. The next
        // presented frame establishes the real monitor cadence again.
        0.0
    } else {
        elapsed.min(MAX_FRAME_DT)
    }
}

/// What a slot in the dock represents — drives layout, magnification, hit-testing
/// and what a click does. The dock row is ordered: Start, pinned…, Divider,
/// running windows… (left = pinned, right = open windows, like the user's macOS-
/// style mental model).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSlotInfo {
    pub agent_id: String,
    pub state: AgentState,
    pub badge: Option<String>,
    pub active_sessions: usize,
    pub total_sessions: usize,
    pub waiting_sessions: usize,
}

impl AgentSlotInfo {
    pub fn idle(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            state: AgentState::Idle,
            badge: None,
            active_sessions: 0,
            total_sessions: 0,
            waiting_sessions: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ItemRole {
    Start,        // opens the Windows Start menu
    Pinned,       // launches `path` via the shell
    Running,      // activates the open window identified by `hwnd`
    Divider,      // a vertical separator: no icon, no magnify, not clickable
    AgentDivider, // subtle zone separator before the dedicated Agent Zone
    Control,      // far-right button: opens the glass control center
    Drawer,       // opens the glass app drawer (all desktop programs)
    Overflow,     // reachable applications that do not fit the current monitor
}

impl ItemRole {
    pub fn is_divider(self) -> bool {
        matches!(self, ItemRole::Divider | ItemRole::AgentDivider)
    }
}

/// A stable identity for a slot, so the dock can be *reconciled* across rebuilds
/// (match surviving items, animate new ones in and closed ones out) instead of
/// being thrown away and recreated. Two items are "the same slot" iff keys match.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ItemKey {
    Start,
    Divider,
    AgentDivider,
    Control,
    Drawer,
    Overflow,
    Pinned(String),  // by launch path (falls back to icon source, then label)
    Running(String), // by application group key
    Agent(String),   // by normalized agent id ("antigravity", "codex", "dsh", ...)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningWindowRef {
    pub hwnd: isize,
    pub title: String,
}

/// Resting width of a slot in logical px (before DPI + magnification).
pub fn gap_for(theme: ThemePreset) -> f32 {
    if theme == ThemePreset::Compact {
        8.0
    } else {
        GAP
    }
}

pub fn role_base_width(role: ItemRole) -> f32 {
    match role {
        ItemRole::Divider => DIVIDER_W,
        ItemRole::AgentDivider => AGENT_DIVIDER_W,
        _ => BASE_ICON,
    }
}

#[derive(Clone)]
pub struct DockItem {
    pub label: String,
    pub glyph: &'static str,    // fallback if the icon can't be extracted
    pub color: (f32, f32, f32), // fallback tile color
    pub path: Option<String>,   // application, shortcut, file, or folder to open
    pub icon: Option<String>,   // optional separate icon source
    pub kind: ContentKind,
    pub role: ItemRole,
    pub hwnd: Option<isize>, // for Running items: the window to activate
    pub group_key: Option<String>,
    pub windows: Vec<RunningWindowRef>,
    pub agent: Option<AgentSlotInfo>,
}

impl DockItem {
    pub fn is_agent(&self) -> bool {
        self.agent.is_some()
    }

    pub fn has_active_indicator(&self) -> bool {
        !self.windows.is_empty()
            || self
                .agent
                .as_ref()
                .is_some_and(|a| a.state != AgentState::Idle)
    }

    /// Stable identity used to reconcile the dock across rebuilds (see `ItemKey`).
    pub fn key(&self) -> ItemKey {
        if let Some(agent) = &self.agent {
            if matches!(self.role, ItemRole::Pinned | ItemRole::Running) {
                return ItemKey::Agent(agent.agent_id.clone());
            }
        }
        match self.role {
            ItemRole::Start => ItemKey::Start,
            ItemRole::Divider => ItemKey::Divider,
            ItemRole::AgentDivider => ItemKey::AgentDivider,
            ItemRole::Control => ItemKey::Control,
            ItemRole::Drawer => ItemKey::Drawer,
            ItemRole::Overflow => ItemKey::Overflow,
            ItemRole::Running => ItemKey::Running(
                self.group_key
                    .clone()
                    .unwrap_or_else(|| format!("hwnd:{}", self.hwnd.unwrap_or(0))),
            ),
            ItemRole::Pinned => ItemKey::Pinned(
                self.path
                    .clone()
                    .or_else(|| self.icon.clone())
                    .unwrap_or_else(|| self.label.clone()),
            ),
        }
    }
}

/// Per-slot animation state. `presence` (0..1) drives the appear/disappear of a
/// slot: its layout width and its icon's scale/opacity all scale by it, so a slot
/// smoothly grows open when added and collapses shut when removed.
#[derive(Clone, Copy)]
struct Slot {
    scale: f32,           // magnification, eased toward the cursor bell
    bounce: f32,          // click-hop phase, 1.0 -> 0.0
    presence: f32,        // 0 = absent (collapsed), 1 = fully present
    presence_target: f32, // 1 while alive, 0 once removed (then eased out + dropped)
    running_presence: f32,
    running_target: f32,
}

impl Slot {
    fn present(running: bool) -> Slot {
        let running = if running { 1.0 } else { 0.0 };
        Slot {
            scale: 1.0,
            bounce: 0.0,
            presence: 1.0,
            presence_target: 1.0,
            running_presence: running,
            running_target: running,
        }
    }
    fn entering(running: bool) -> Slot {
        Slot {
            scale: 1.0,
            bounce: 0.0,
            presence: 0.0,
            presence_target: 1.0,
            running_presence: 0.0,
            running_target: if running { 1.0 } else { 0.0 },
        }
    }
}

pub struct IconFrame {
    pub idx: usize,
    pub cx: f32,
    pub scale: f32,
    pub bounce_y: f32,         // upward offset from the click hop
    pub presence: f32,         // 0..1 appear/disappear factor (icon scale + opacity)
    pub running_presence: f32, // independently eases the running-state indicator
}

pub struct Frame {
    pub baseline: f32,
    pub icons: Vec<IconFrame>,
    pub pill: (f32, f32, f32, f32), // left, top, right, bottom
}

pub struct Dock {
    pub items: Vec<DockItem>,
    pub foreground: Option<isize>,
    pub preferences: crate::settings::Layout,
    pub insertion_x: Option<f32>,
    slots: Vec<Slot>, // per-item animation state, kept in lockstep with `items`
    pub cursor_x: Option<f32>,
    /// Low-passed cursor position (px) that `frame` actually lays out against.
    /// `tick` eases it toward the raw pointer so ±1px hand micro-jitter — which
    /// the instantaneous anchor blend amplified ~0.7px of row shift per pointer
    /// pixel into a visible dock tremble — is filtered like every other dock
    /// animation. Reset to the raw position whenever the cursor re-enters.
    cursor_smooth: f32,
    /// Raw pointer position at the previous tick; with `tick`'s dt it yields the
    /// pointer speed that drives `adaptive_blend_tau`.
    raw_prev: f32,
    /// Measured raw-pointer speed (px/s) at the last tick.
    cursor_speed: f32,
    /// Whether `cursor_x` was `Some` at the previous tick (drives the re-entry
    /// reset of `cursor_smooth`).
    had_cursor: bool,
    pub reveal: f32,        // 0 = fully hidden (slid down), 1 = fully shown
    pub reveal_target: f32, // where reveal is easing toward
    pub theme: ThemePreset,
    pub dpi: f32,
    pub win_w: f32,
    pub win_h: f32,
    last: Instant,
    resume_pending: bool,
    pub shimmer_phase: Option<f32>,
}

/// Window size (device px) needed to fit every slot at full magnification PLUS the
/// click hop, so a bouncing, magnified icon never gets clipped at the top. Takes
/// the actual items because slots have different widths (a divider is narrow).
pub fn window_size_with(
    items: &[DockItem],
    dpi: f32,
    preferences: crate::settings::Layout,
    theme: ThemePreset,
) -> (u32, u32) {
    let base = preferences.icon_size as f32 * dpi;
    let n = items.len();
    let row: f32 = items
        .iter()
        .map(|it| role_base_width(it.role) * preferences.icon_size as f32 / BASE_ICON * dpi)
        .sum::<f32>()
        + gap_for(theme) * dpi * n.saturating_sub(1) as f32;
    let w = row + 2.0 * PILL_PAD_X * dpi + base * 4.0; // bulge headroom
    let h = base * preferences.magnification as f32 / 100.0
        + BOUNCE_AMP * base
        + (PILL_PAD_Y + TOP_PAD + preferences.bottom_gap as f32) * dpi;
    (w.ceil() as u32, h.ceil() as u32)
}

fn overflow_tier(item: &DockItem) -> Option<u8> {
    match (&item.role, &item.agent) {
        (ItemRole::Running, None) => Some(0),
        (ItemRole::Pinned, None) => Some(1),
        (ItemRole::Pinned | ItemRole::Running, Some(agent)) => Some(match agent.state {
            AgentState::Idle if item.windows.is_empty() => 2,
            AgentState::Idle => 3,
            AgentState::Done => 4,
            AgentState::Running => 5,
            AgentState::Error => 6,
            AgentState::Waiting => 7,
        }),
        _ => None,
    }
}

fn overflow_drop_index(items: &[DockItem]) -> Option<usize> {
    (0..=7).find_map(|tier| {
        items
            .iter()
            .rposition(|item| overflow_tier(item) == Some(tier))
    })
}

fn prune_dangling_dividers(items: &mut Vec<DockItem>) {
    if let Some(index) = items.iter().position(|i| i.role == ItemRole::Divider) {
        let has_regular_running = items[index + 1..]
            .iter()
            .any(|i| i.role == ItemRole::Running && !i.is_agent());
        let has_regular_pinned = items[..index]
            .iter()
            .any(|i| i.role == ItemRole::Pinned && !i.is_agent());
        if !has_regular_running || !has_regular_pinned {
            items.remove(index);
        }
    }
    if let Some(index) = items.iter().position(|i| i.role == ItemRole::AgentDivider) {
        let has_regular_before = items[..index]
            .iter()
            .any(|i| matches!(i.role, ItemRole::Pinned | ItemRole::Running) && !i.is_agent());
        let has_agent_after = items[index + 1..].iter().any(|i| i.is_agent());
        if !has_regular_before || !has_agent_after {
            items.remove(index);
        }
    }
}

/// Keep every displaced app accessible through the overflow menu. System buttons
/// and active/waiting Agents are retained longest; remove dangling separators
/// rather than spending slots on them.
pub fn limit_items(
    mut items: Vec<DockItem>,
    width: u32,
    dpi: f32,
    preferences: crate::settings::Layout,
    theme: ThemePreset,
) -> (Vec<DockItem>, Vec<DockItem>) {
    let mut overflow = Vec::new();
    if window_size_with(&items, dpi, preferences, theme).0 <= width
        || !items
            .iter()
            .any(|i| matches!(i.role, ItemRole::Pinned | ItemRole::Running))
    {
        return (items, overflow);
    }
    let at = items
        .iter()
        .position(|i| i.role == ItemRole::Control)
        .unwrap_or(items.len());
    items.insert(at, crate::apps::overflow_item());
    while window_size_with(&items, dpi, preferences, theme).0 > width {
        let Some(index) = overflow_drop_index(&items) else {
            break;
        };
        overflow.insert(0, items.remove(index));
        prune_dangling_dividers(&mut items);
    }
    prune_dangling_dividers(&mut items);
    (items, overflow)
}

impl Dock {
    /// The full animation envelope also fits during reconciliation (old slots
    /// remain briefly while fading out). Rendering and hit testing share this DPI.
    pub fn layout_dpi(&self) -> f32 {
        let natural =
            window_size_with(&self.items, self.dpi, self.preferences, self.theme).0 as f32;
        self.dpi * (self.win_w / natural.max(1.0)).min(1.0)
    }
    pub fn base_icon(&self) -> f32 {
        self.preferences.icon_size as f32 * self.layout_dpi()
    }

    pub fn new(items: Vec<DockItem>, dpi: f32, win_w: f32, win_h: f32, theme: ThemePreset) -> Dock {
        let slots = items
            .iter()
            .map(|item| Slot::present(item.has_active_indicator()))
            .collect();
        Dock {
            items,
            foreground: None,
            preferences: crate::settings::Layout::default(),
            insertion_x: None,
            // Whatever is present at construction starts fully shown (no intro animation).
            slots,
            cursor_x: None,
            cursor_smooth: 0.0,
            raw_prev: 0.0,
            cursor_speed: 0.0,
            had_cursor: false,
            // Start shown; main.rs decides the resting target from settings + the
            // fullscreen state (always-resident vs. auto-hide vs. fullscreen retract).
            reveal: 1.0,
            reveal_target: 1.0,
            theme,
            dpi,
            win_w,
            win_h,
            last: Instant::now(),
            resume_pending: false,
            shimmer_phase: None,
        }
    }

    pub fn has_running_agent(&self) -> bool {
        self.items.iter().enumerate().any(|(i, item)| {
            self.slots.get(i).is_some_and(|s| s.presence_target > 0.0)
                && item
                    .agent
                    .as_ref()
                    .is_some_and(|a| a.state == AgentState::Running)
        })
    }

    pub fn sync_active_indicators(&mut self) {
        for (i, item) in self.items.iter().enumerate() {
            if let Some(slot) = self.slots.get_mut(i) {
                slot.running_target = if item.has_active_indicator() {
                    1.0
                } else {
                    0.0
                };
            }
        }
    }

    /// How far (device px) content slides down when hidden (leaves a small sliver).
    fn hide_slide(&self) -> f32 {
        self.preferences.icon_size as f32 * self.layout_dpi()
            + (2.0 * PILL_PAD_Y + self.preferences.bottom_gap as f32 - HIDE_SLIVER)
                * self.layout_dpi()
    }

    /// Resting (un-magnified) width of slot `i` in device px.
    fn item_w(&self, i: usize) -> f32 {
        role_base_width(self.items[i].role) * self.preferences.icon_size as f32 / BASE_ICON
            * self.layout_dpi()
    }

    pub fn dock_span_x(&self) -> (f32, f32) {
        let base = self.preferences.icon_size as f32 * self.layout_dpi();
        let gap = gap_for(self.theme) * self.layout_dpi();
        let n = self.items.len();
        let widths: f32 = (0..n).map(|i| self.item_w(i)).sum();
        let row =
            widths + gap * n.saturating_sub(1) as f32 + 2.0 * PILL_PAD_X * self.layout_dpi() + base; // bulge slack
        let cx = self.win_w / 2.0;
        (cx - row / 2.0, cx + row / 2.0)
    }

    /// Client-space rectangle that should intercept the mouse. Everything outside it
    /// is click-through. When hidden it's just a thin strip at the very bottom edge.
    pub fn hit_zone(&self, shown: bool) -> (f32, f32, f32, f32) {
        let (l, r) = self.dock_span_x();
        if shown {
            let frame = self.frame();
            let base = self.base_icon();
            let pad = PILL_PAD_Y * self.layout_dpi();
            // Use the drawing baseline so bottom_gap and reveal stay in sync.
            // A click hop or a settling old scale can exceed the configured envelope.
            let top = frame.icons.iter().fold(
                frame.baseline - base * self.preferences.magnification as f32 / 100.0 - pad,
                |top, icon| {
                    let rise = (1.0 - icon.presence) * ENTER_RISE * base;
                    let width = self.item_w(icon.idx) * icon.scale * icon.presence;
                    top.min(frame.baseline - icon.bounce_y + rise - width - pad)
                },
            );
            (l, top.clamp(0.0, self.win_h), r, self.win_h)
        } else {
            (
                l,
                self.win_h - TRIGGER_PX * self.layout_dpi(),
                r,
                self.win_h,
            )
        }
    }

    pub fn interactive_hit_zone(&self) -> (f32, f32, f32, f32) {
        self.hit_zone(self.reveal > 0.02)
    }

    /// The client-space rectangle that should actually intercept the mouse, sized
    /// dynamically by whether the cursor is currently over the dock:
    ///
    /// * **Hovering** (`cursor_x` set) → the full magnification envelope, so the pointer
    ///   stays captured as icons bulge up *above* the pill and the row widens.
    /// * **At rest** (`cursor_x` clear) → just the visible pill, so the tall head-room and
    ///   side bulge that the envelope reserves don't swallow clicks on whatever sits just
    ///   above or beside the dock (e.g. a Send button overlapping the old oversized box).
    ///
    /// Entering works because the pill itself is interactive: the moment the cursor
    /// lands on it, `WM_MOUSEMOVE` sets `cursor_x` and the zone expands on the next hit
    /// test; leaving fires `WM_MOUSELEAVE`, clearing `cursor_x` and shrinking it back.
    pub fn pointer_hit_zone(&self) -> (f32, f32, f32, f32) {
        if self.cursor_x.is_some() {
            self.interactive_hit_zone()
        } else {
            self.frame().pill
        }
    }

    /// Start the click hop for item `i`.
    pub fn bump(&mut self, i: usize) {
        if let Some(s) = self.slots.get_mut(i) {
            s.bounce = 1.0;
        }
    }

    /// Start a bounce on the live item whose `ItemKey` matches `key`.
    pub fn bump_key(&mut self, key: &ItemKey) {
        if let Some(i) = self
            .items
            .iter()
            .enumerate()
            .position(|(idx, item)| self.slots[idx].presence_target > 0.0 && &item.key() == key)
        {
            self.bump(i);
        }
    }

    /// Reset the frame clock when the blocking message loop wakes an idle Dock.
    pub fn wake_animation_clock(&mut self) {
        self.last = Instant::now();
        self.resume_pending = true;
    }

    fn metrics(&self) -> (f32, f32) {
        (
            self.preferences.icon_size as f32 * self.layout_dpi(),
            gap_for(self.theme) * self.layout_dpi(),
        )
    }

    fn rest_centers(&self) -> Vec<f32> {
        let (_, gap) = self.metrics();
        let n = self.items.len();
        // Un-magnified widths, but still collapsed by `presence` so an opening or
        // closing slot's gap shrinks with it (matches `frame`'s geometry at scale=1).
        let widths: Vec<f32> = (0..n)
            .map(|i| self.item_w(i) * self.slots[i].presence)
            .collect();
        let row: f32 =
            widths.iter().sum::<f32>() + (1..n).map(|i| gap * self.slots[i].presence).sum::<f32>();
        let mut x = (self.win_w - row) / 2.0;
        let mut centers = Vec::with_capacity(n);
        for (i, &w) in widths.iter().enumerate() {
            if i > 0 {
                x += gap * self.slots[i].presence;
            }
            centers.push(x + w / 2.0);
            x += w;
        }
        centers
    }

    /// Cosine "bell" falloff: scale by horizontal distance from the cursor.
    fn target(&self, rest_cx: f32) -> f32 {
        match self.cursor_x {
            None => 1.0,
            Some(cx) => {
                let infl = INFLUENCE * self.layout_dpi();
                let d = (cx - rest_cx).abs();
                if d >= infl {
                    1.0
                } else {
                    let t = d / infl;
                    1.0 + (self.preferences.magnification as f32 / 100.0 - 1.0)
                        * 0.5
                        * (1.0 + (std::f32::consts::PI * t).cos())
                }
            }
        }
    }

    /// Advance the easing one frame. Returns true while still animating.
    pub fn tick(&mut self) -> bool {
        if self.preferences.reduce_motion {
            self.reveal = self.reveal_target;
            for slot in &mut self.slots {
                slot.scale = 1.0;
                slot.bounce = 0.0;
                slot.presence = slot.presence_target;
                slot.running_presence = slot.running_target;
            }
            return false;
        }
        let now = Instant::now();
        let dt = if std::mem::take(&mut self.resume_pending) {
            0.0
        } else {
            animation_dt((now - self.last).as_secs_f32())
        };
        self.last = now;
        let alpha = 1.0 - (-dt / EASE_TAU).exp(); // frame-rate independent
        let rc = self.rest_centers();
        let mut moving = false;
        // Range loop: the body mutates `self.slots[i]` while also calling `self.target`
        // (an `&self` method), so an iterator over `slots` can't co-exist with it.
        #[allow(clippy::needless_range_loop)]
        for i in 0..self.items.len() {
            // Dividers never magnify; everything else follows the cursor bell.
            let tg = if self.items[i].role.is_divider() {
                1.0
            } else {
                self.target(rc[i])
            };
            let d = tg - self.slots[i].scale;
            if d.abs() > 0.002 {
                moving = true;
                self.slots[i].scale += d * alpha;
            } else {
                self.slots[i].scale = tg;
            }
            if self.slots[i].bounce > 0.0 {
                self.slots[i].bounce = (self.slots[i].bounce - dt / BOUNCE_DUR).max(0.0);
                moving = true;
            }
            let running_delta = self.slots[i].running_target - self.slots[i].running_presence;
            if running_delta.abs() > 0.0005 {
                self.slots[i].running_presence += running_delta * (1.0 - (-dt / RUNNING_TAU).exp());
                moving = true;
            } else {
                self.slots[i].running_presence = self.slots[i].running_target;
            }
            // appear/disappear: open a bit slower than we close, both frame-rate independent.
            let pt = self.slots[i].presence_target;
            let dp = pt - self.slots[i].presence;
            if dp.abs() > 0.0005 {
                let tau = if dp > 0.0 { ENTER_TAU } else { EXIT_TAU };
                self.slots[i].presence += dp * (1.0 - (-dt / tau).exp());
                moving = true;
            } else {
                self.slots[i].presence = pt;
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
        // Cursor low-pass: ease `cursor_smooth` toward the raw pointer so the
        // layout (magnification bell + anchor blend) derives from a filtered
        // position. Tracking the raw pointer directly moved the row ~0.7px per
        // pointer pixel, amplifying ±1px hand micro-jitter into a visible dock
        // tremble. The time constant is SPEED-ADAPTIVE (see `adaptive_blend_tau`):
        // heavy at rest for jitter suppression, tight during fast sweeps so the
        // magnification bell stays under the pointer instead of chasing it.
        // Re-entry resets to the raw position so the layout never starts from a
        // stale cursor.
        if let Some(cx) = self.cursor_x {
            if self.had_cursor {
                // dt = 0 wake frames freeze the blend entirely (their easing factor
                // is 0), so only measured frames update the speed estimate.
                if dt > 0.0 {
                    self.cursor_speed = (cx - self.raw_prev).abs() / dt;
                }
                self.raw_prev = cx;
                let blend_tau = adaptive_blend_tau(self.cursor_speed);
                let dcx = cx - self.cursor_smooth;
                if dcx.abs() > 0.001 {
                    self.cursor_smooth += dcx * (1.0 - (-dt / blend_tau).exp());
                    moving = true;
                } else {
                    self.cursor_smooth = cx;
                }
            } else {
                self.cursor_smooth = cx;
                self.raw_prev = cx;
                self.cursor_speed = 0.0;
            }
        }
        self.had_cursor = self.cursor_x.is_some();
        moving
    }

    /// The cursor position `frame` lays out against: the raw pointer right after
    /// entry, then the low-passed `cursor_smooth` (see `tick`).
    fn smooth_cursor(&self) -> Option<f32> {
        self.cursor_x.map(|_| self.cursor_smooth)
    }

    /// Index of the anchor for the current cursor: the icon whose resting centre
    /// is the LEFT edge of the cursor's segment. (Not the *closest* centre: at the
    /// midpoint between two icons the closest flips, which would reset the blend
    /// and snap the row — using the left edge keeps the blend continuous.)
    fn anchor_index(&self, rest_cx: &[f32]) -> usize {
        match self.smooth_cursor() {
            Some(cx) => {
                let mut j = 0;
                while j + 1 < rest_cx.len() && cx >= rest_cx[j + 1] {
                    j += 1;
                }
                j
            }
            None => rest_cx.len() / 2,
        }
    }

    /// Compute the current laid-out frame (dynamic widths, cursor-anchored).
    ///
    /// The icon closest to the cursor is pinned at its **resting** centre and
    /// neighbours are pushed outward using magnified widths.  This prevents the
    /// hovered (and therefore clicked) icon from drifting sideways when adjacent
    /// icons also magnify — which made the click-bounce and panel-open animations
    /// appear to originate from the wrong screen position.
    ///
    /// The anchor is *continuous*, not discrete: as the cursor travels between two
    /// neighbouring resting centres, the layout eases linearly from the "anchored
    /// at the left icon" pose to the "anchored at the right icon" pose.  The two
    /// poses differ by a single constant row shift (the two icons' combined
    /// magnification extras, halved), so blending just slides the whole row by
    /// `f * shift` — the old discrete anchor snapped the row by that amount in
    /// one frame every time the cursor crossed an icon centre, which read as
    /// left/right trembling while sweeping across the dock.
    ///
    /// When the cursor is absent the layout falls back to a symmetric expand about
    /// the row centre, which is mathematically identical to the old re-centre
    /// algorithm at every scale value.
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
        // Magnified widths (with scale and presence) — used for spacing so that
        // a magnified icon pushes its neighbours apart.
        let widths: Vec<f32> = (0..n)
            .map(|i| self.item_w(i) * self.slots[i].scale * self.slots[i].presence)
            .collect();
        // Resting widths (scale = 1, with presence) — used to compute the resting
        // centres that the cursor-anchored layout pivots on.
        let rest_widths: Vec<f32> = (0..n)
            .map(|i| self.item_w(i) * self.slots[i].presence)
            .collect();
        let rest_total: f32 = rest_widths.iter().sum::<f32>()
            + (1..n).map(|i| gap * self.slots[i].presence).sum::<f32>();

        // Resting centres: where each icon would sit at scale = 1.
        let mut rest_cx = Vec::with_capacity(n);
        {
            let mut x = (self.win_w - rest_total) / 2.0;
            for (i, &w) in rest_widths.iter().enumerate() {
                if i > 0 {
                    x += gap * self.slots[i].presence;
                }
                rest_cx.push(x + w / 2.0);
                x += w;
            }
        }

        let mut centers = vec![0.0f32; n];
        // Pin the anchor at its resting centre, then lay out outward using the
        // magnified widths.  Icons to the right extend left→right; icons to the
        // left extend right→left.  Gap before slot `i` is `gap * presence[i]`,
        // matching the original left-to-right sweep.
        let anchor = self.anchor_index(&rest_cx);
        centers[anchor] = rest_cx[anchor];
        // Right of anchor
        let mut edge = centers[anchor] + widths[anchor] / 2.0;
        for i in (anchor + 1)..n {
            edge += gap * self.slots[i].presence;
            centers[i] = edge + widths[i] / 2.0;
            edge += widths[i];
        }
        // Left of anchor
        let mut edge = centers[anchor] - widths[anchor] / 2.0;
        for i in (0..anchor).rev() {
            edge -= gap * self.slots[i + 1].presence;
            centers[i] = edge - widths[i] / 2.0;
            edge -= widths[i];
        }

        // Continuous-anchor blend: the "anchored at `anchor`" and "anchored at
        // `anchor + 1`" poses differ by exactly one constant row shift — half of
        // the two icons' combined magnification extras.  Slide the whole row
        // toward the next pose by the cursor's fractional progress past the
        // anchor's resting centre, so crossing an icon's centre slides the row
        // smoothly instead of snapping it (the old discrete anchor jumped the
        // whole dock ~38px at every crossing — the "trembling" on hover). The
        // cursor here is the LOW-PASSED position, so the blend no longer tracks
        // the raw pointer pixel-for-pixel (that amplified ±1px hand micro-jitter
        // into a residual tremble).
        if let Some(cx) = self.smooth_cursor() {
            if anchor + 1 < n {
                let span = (rest_cx[anchor + 1] - rest_cx[anchor]).max(1e-4);
                let f = ((cx - rest_cx[anchor]) / span).clamp(0.0, 1.0);
                let shift = (widths[anchor] - rest_widths[anchor] + widths[anchor + 1]
                    - rest_widths[anchor + 1])
                    / 2.0;
                if f > 0.0 {
                    for center in &mut centers {
                        *center -= f * shift;
                    }
                }
            }
        }

        // auto-hide slide: shown floats SHOWN_GAP above the bottom; hidden slides down.
        let shown_baseline =
            self.win_h - (PILL_PAD_Y + self.preferences.bottom_gap as f32) * self.layout_dpi();
        let baseline = shown_baseline + (1.0 - self.reveal) * self.hide_slide();
        let mut icons = Vec::with_capacity(n);
        for (i, &cx) in centers.iter().enumerate() {
            // single smooth hop: 0 at phase 1 -> peak at 0.5 -> 0 at phase 0
            let bounce_y = (self.slots[i].bounce * std::f32::consts::PI).sin() * BOUNCE_AMP * base;
            icons.push(IconFrame {
                idx: i,
                cx,
                scale: self.slots[i].scale,
                bounce_y,
                presence: self.slots[i].presence,
                running_presence: self.slots[i].running_presence,
            });
        }
        let pill_l = icons[0].cx - widths[0] / 2.0 - PILL_PAD_X * self.layout_dpi();
        let pill_r = icons[n - 1].cx + widths[n - 1] / 2.0 + PILL_PAD_X * self.layout_dpi();
        let pill_t = baseline - base - PILL_PAD_Y * self.layout_dpi();
        let pill_b = baseline + PILL_PAD_Y * self.layout_dpi(); // tracks the slide (clipped at win_h)
        Frame {
            baseline,
            icons,
            pill: (pill_l, pill_t, pill_r, pill_b),
        }
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        let f = self.frame();
        let (base, _) = self.metrics();
        for ic in &f.icons {
            if self.items[ic.idx].role.is_divider() {
                continue; // separators aren't clickable
            }
            // A slot that's collapsing away (its window already closed) isn't a target.
            if self.slots[ic.idx].presence_target == 0.0 {
                continue;
            }
            let w = self.item_w(ic.idx) * ic.scale * ic.presence;
            // The icon's visual bottom follows the same animation offsets the renderer
            // applies: bounce up (`-bounce_y`) and enter rise (`+rise`).  The hit zone
            // must track that movement so a click on the bouncing icon still registers.
            let rise = (1.0 - ic.presence) * ENTER_RISE * base;
            let icon_bottom = f.baseline - ic.bounce_y + rise;
            if x >= ic.cx - w / 2.0
                && x <= ic.cx + w / 2.0
                && y >= icon_bottom - w
                && y <= icon_bottom
            {
                return Some(ic.idx);
            }
        }
        None
    }

    /// Reconcile the live item set with a freshly-composed desired list. Items that
    /// persist keep their animation state (and icon); items only in the new list are
    /// inserted *entering* (presence 0 -> 1); items only in the old list are kept but
    /// marked *exiting* (presence -> 0) so they collapse before being dropped.
    ///
    /// Returns, for each merged slot, `Some(old_index)` if its icon can be reused or
    /// `None` if a fresh icon must be loaded — letting the GPU layer realign its
    /// bitmaps without re-extracting everything (and without blanking a closing icon).
    pub fn reconcile(&mut self, desired: Vec<DockItem>) -> Vec<Option<usize>> {
        let old_items = std::mem::take(&mut self.items);
        let old_slots = std::mem::take(&mut self.slots);
        let (m, d) = (old_items.len(), desired.len());

        let old_keys: Vec<ItemKey> = old_items.iter().map(DockItem::key).collect();
        let desired_keys: Vec<ItemKey> = desired.iter().map(DockItem::key).collect();

        let mut old_used = vec![false; m];
        let mut match_for_desired = vec![None; d];
        for (new_index, key) in desired_keys.iter().enumerate() {
            if let Some(old_index) = old_keys
                .iter()
                .enumerate()
                .find_map(|(old_index, old_key)| {
                    (!old_used[old_index] && old_key == key).then_some(old_index)
                })
            {
                old_used[old_index] = true;
                match_for_desired[new_index] = Some(old_index);
            }
        }

        // Move-out buffers so we can take ownership of each side by index.
        let mut old_items: Vec<Option<DockItem>> = old_items.into_iter().map(Some).collect();
        let mut desired: Vec<Option<DockItem>> = desired.into_iter().map(Some).collect();

        let mut items = Vec::with_capacity(m + d);
        let mut slots = Vec::with_capacity(m + d);
        let mut remap = Vec::with_capacity(m + d);

        // Do not use the old positional assumption:
        // Old order was Start, pinned items, divider, then running groups by hwnd.
        // Match by ItemKey so bitmap reuse stays aligned with click/preview targets.
        let mut next_old_exit = 0;
        for (new_index, old_match) in match_for_desired.into_iter().enumerate() {
            if let Some(limit) = old_match {
                while next_old_exit < limit {
                    if !old_used[next_old_exit] {
                        items.push(old_items[next_old_exit].take().unwrap());
                        let mut s = old_slots[next_old_exit];
                        s.presence_target = 0.0; // exiting: collapse, then get dropped
                        s.running_target = 0.0;
                        slots.push(s);
                        remap.push(Some(next_old_exit));
                    }
                    next_old_exit += 1;
                }
            }

            match old_match {
                Some(old_index) => {
                    let running = desired[new_index].as_ref().unwrap().has_active_indicator();
                    items.push(desired[new_index].take().unwrap());
                    let mut s = old_slots[old_index];
                    s.presence_target = 1.0; // revive if it had been mid-exit
                    s.running_target = if running { 1.0 } else { 0.0 };
                    slots.push(s);
                    remap.push(Some(old_index));
                }
                None => {
                    let running = desired[new_index].as_ref().unwrap().has_active_indicator();
                    items.push(desired[new_index].take().unwrap());
                    slots.push(Slot::entering(running));
                    remap.push(None);
                }
            }
        }

        while next_old_exit < m {
            if !old_used[next_old_exit] {
                items.push(old_items[next_old_exit].take().unwrap());
                let mut s = old_slots[next_old_exit];
                s.presence_target = 0.0;
                s.running_target = 0.0;
                slots.push(s);
                remap.push(Some(next_old_exit));
            }
            next_old_exit += 1;
        }

        self.items = items;
        self.slots = slots;
        remap
    }

    /// Drop slots that have finished exiting (collapsed to nothing). Returns their
    /// indices — ascending — so the GPU layer can drop the matching icon bitmaps and
    /// stay in lockstep. Call once per frame after `tick`.
    pub fn take_finished_exits(&mut self) -> Vec<usize> {
        let removed: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.presence_target == 0.0 && s.presence <= PRESENCE_GONE)
            .map(|(i, _)| i)
            .collect();
        if removed.is_empty() {
            return removed;
        }
        let mut keep = removed.iter().copied().peekable();
        let mut idx = 0;
        self.items.retain(|_| {
            let drop = keep.peek() == Some(&idx);
            if drop {
                keep.next();
            }
            idx += 1;
            !drop
        });
        let mut keep = removed.iter().copied().peekable();
        let mut idx = 0;
        self.slots.retain(|_| {
            let drop = keep.peek() == Some(&idx);
            if drop {
                keep.next();
            }
            idx += 1;
            !drop
        });
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dock() -> Dock {
        Dock::new(Vec::new(), 1.0, 400.0, 140.0, ThemePreset::Glass)
    }

    pub(super) fn item(role: ItemRole, hwnd: Option<isize>, path: Option<&str>) -> DockItem {
        DockItem {
            label: String::new(),
            glyph: "",
            color: (0.0, 0.0, 0.0),
            path: path.map(str::to_string),
            icon: None,
            kind: ContentKind::Application,
            role,
            hwnd,
            group_key: hwnd.map(|value| format!("hwnd:{value}")),
            windows: hwnd
                .map(|value| {
                    vec![RunningWindowRef {
                        hwnd: value,
                        title: String::new(),
                    }]
                })
                .unwrap_or_default(),
            agent: None,
        }
    }

    pub(super) fn start() -> DockItem {
        item(ItemRole::Start, None, None)
    }
    pub(super) fn divider() -> DockItem {
        item(ItemRole::Divider, None, None)
    }
    pub(super) fn running(hwnd: isize) -> DockItem {
        item(ItemRole::Running, Some(hwnd), None)
    }

    fn running_group(key: &str, hwnd: isize) -> DockItem {
        let mut item = running(hwnd);
        item.group_key = Some(key.to_string());
        item
    }

    pub(super) fn pinned(path: &str, hwnd: Option<isize>) -> DockItem {
        let mut item = item(ItemRole::Pinned, None, Some(path));
        item.hwnd = hwnd;
        item.windows = hwnd
            .map(|value| {
                vec![RunningWindowRef {
                    hwnd: value,
                    title: "Pinned app".to_string(),
                }]
            })
            .unwrap_or_default();
        item
    }

    fn keys(dock: &Dock) -> Vec<ItemKey> {
        dock.items.iter().map(DockItem::key).collect()
    }

    pub(super) fn seeded(items: Vec<DockItem>) -> Dock {
        // Start with the items already present (presence == 1), like a fresh launch.
        Dock::new(items, 1.0, 1200.0, 140.0, ThemePreset::Glass)
    }

    #[test]
    fn visible_hide_animation_keeps_full_interactive_zone() {
        let mut dock = dock();
        dock.reveal = 0.5;
        assert_eq!(dock.interactive_hit_zone(), dock.hit_zone(true));
        dock.reveal = 0.0;
        assert_eq!(dock.interactive_hit_zone(), dock.hit_zone(false));
    }

    #[test]
    fn stale_idle_time_never_becomes_animation_motion() {
        let mut dock = seeded(vec![start()]);
        dock.bump(0);
        dock.last = Instant::now() - Duration::from_secs(1);

        assert!(dock.tick());
        assert_eq!(dock.slots[0].bounce, 1.0);
    }

    #[test]
    fn explicit_animation_wake_does_not_assume_60hz() {
        let mut dock = seeded(vec![start()]);
        dock.bump(0);
        dock.last = Instant::now() - Duration::from_millis(80);

        dock.wake_animation_clock();
        assert!(dock.tick());

        assert_eq!(dock.slots[0].bounce, 1.0);
    }

    #[test]
    fn high_refresh_frame_after_wake_uses_measured_elapsed_time() {
        let mut dock = seeded(vec![start()]);
        dock.bump(0);
        dock.wake_animation_clock();
        assert!(dock.tick());

        dock.last = Instant::now() - Duration::from_millis(7);
        assert!(dock.tick());

        assert!(
            dock.slots[0].bounce > 0.98 && dock.slots[0].bounce < 1.0,
            "144 Hz-sized frame should advance smoothly: {}",
            dock.slots[0].bounce
        );
    }

    #[test]
    fn closing_a_pinned_app_fades_its_running_indicator() {
        let path = r"C:\Apps\Pinned.exe";
        let mut dock = seeded(vec![pinned(path, Some(42))]);

        dock.reconcile(vec![pinned(path, None)]);
        let before = dock.frame().icons[0].running_presence;
        dock.last = Instant::now() - Duration::from_millis(16);
        assert!(dock.tick());
        let after = dock.frame().icons[0].running_presence;

        assert_eq!(before, 1.0);
        assert!(
            after > 0.0 && after < before,
            "indicator snapped to {after}"
        );
    }

    #[test]
    fn hit_zone_tightens_to_the_pill_at_rest_and_expands_on_hover() {
        let mut dock = seeded(vec![start(), running(10)]);
        dock.reveal = 1.0;
        // At rest the interceptable area is exactly the visible pill.
        dock.cursor_x = None;
        assert_eq!(dock.pointer_hit_zone(), dock.frame().pill);
        // While hovering it expands to the full magnification envelope.
        dock.cursor_x = Some(dock.win_w / 2.0);
        assert_eq!(dock.pointer_hit_zone(), dock.interactive_hit_zone());
        // The resting zone's top edge sits strictly lower than the hover envelope's:
        // the head-room reserved above the pill for bulging icons is gone when unused.
        dock.cursor_x = None;
        let rest_top = dock.pointer_hit_zone().1;
        dock.cursor_x = Some(dock.win_w / 2.0);
        let hover_top = dock.pointer_hit_zone().1;
        assert!(
            rest_top > hover_top,
            "rest_top {rest_top} should be below hover_top {hover_top}"
        );
    }

    #[test]
    fn shown_hit_zone_uses_each_configured_icon_size_without_expanding_click_through() {
        fn contains(zone: (f32, f32, f32, f32), x: f32, y: f32) -> bool {
            x >= zone.0 && x <= zone.2 && y >= zone.1 && y <= zone.3
        }

        for icon_size in [24, 50, 80] {
            let mut dock = Dock::new(vec![start()], 1.0, 1200.0, 300.0, ThemePreset::Glass);
            dock.preferences.icon_size = icon_size;
            dock.preferences.magnification = 100;
            let zone = dock.hit_zone(true);
            let expected_top =
                300.0 - icon_size as f32 - 2.0 * PILL_PAD_Y - dock.preferences.bottom_gap as f32;
            assert!((zone.1 - expected_top).abs() < 1e-4, "size {icon_size}");
            assert!(zone.0 > 0.0 && zone.2 < dock.win_w);

            let cx = (zone.0 + zone.2) / 2.0;
            assert!(!contains(zone, cx, zone.1 - 1.0));
            assert!(contains(zone, cx, zone.1 + 1.0));
            assert!(!contains(zone, zone.0 - 1.0, zone.1 + 1.0));
        }
    }

    #[test]
    fn shown_hit_zone_tracks_the_magnification_envelope() {
        let mut dock = Dock::new(vec![start()], 1.0, 1200.0, 300.0, ThemePreset::Glass);
        dock.preferences.icon_size = 50;
        dock.preferences.magnification = 100;
        assert!((dock.hit_zone(true).1 - 224.0).abs() < 1e-4);
        dock.preferences.magnification = 180;
        assert!((dock.hit_zone(true).1 - 184.0).abs() < 1e-4);
    }

    #[test]
    fn shown_hit_zone_uses_the_automatically_scaled_layout_dpi() {
        let mut dock = Dock::new(vec![start()], 1.0, 141.0, 300.0, ThemePreset::Glass);
        dock.preferences.icon_size = 50;
        dock.preferences.magnification = 160;
        assert!((dock.layout_dpi() - 0.5).abs() < 1e-4);
        assert!((dock.base_icon() - 25.0).abs() < 1e-4);
        assert!((dock.hit_zone(true).1 - 247.0).abs() < 1e-4);
    }

    #[test]
    fn hover_keeps_visible_icons_interactive_with_bottom_gap_and_click_hops() {
        for dpi in [1.0, 1.5, 2.0] {
            for icon_size in [24, 50, 80] {
                for magnification in [100, 180, 220] {
                    for bottom_gap in [0, 8, 32] {
                        for width_ratio in [0.5, 1.0] {
                            let preferences = crate::settings::Layout {
                                icon_size,
                                magnification,
                                bottom_gap,
                                ..Default::default()
                            };
                            let items = vec![start()];
                            let (width, height) =
                                window_size_with(&items, dpi, preferences, ThemePreset::Glass);
                            let mut dock = Dock::new(
                                items,
                                dpi,
                                width as f32 * width_ratio,
                                height as f32,
                                ThemePreset::Glass,
                            );
                            dock.preferences = preferences;
                            let frame = dock.frame();
                            let x = frame.icons[0].cx;
                            let y = frame.baseline - dock.base_icon() + 1.0;
                            let rest = dock.pointer_hit_zone();
                            assert!(y >= rest.1 && y <= rest.3);
                            assert_eq!(dock.hit_test(x, y), Some(0));
                            dock.cursor_x = Some(x);
                            let hovered = dock.pointer_hit_zone();
                            assert!(y >= hovered.1 && y <= hovered.3);

                            // Settings may lower magnification while the old scale
                            // and the click hop are still easing out.
                            dock.slots[0].scale = 2.2;
                            dock.slots[0].bounce = 0.5;
                            let frame = dock.frame();
                            let top =
                                frame.baseline - frame.icons[0].bounce_y - dock.base_icon() * 2.2;
                            let visible_y = top.max(0.0) + 0.1;
                            let hovered = dock.pointer_hit_zone();
                            assert!(visible_y >= hovered.1 && visible_y <= hovered.3);
                            assert_eq!(dock.hit_test(x, visible_y), Some(0));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn opening_a_window_inserts_an_entering_slot_keeping_others() {
        let mut dock = seeded(vec![start(), running(10)]);
        let remap = dock.reconcile(vec![start(), divider(), running(10), running(20)]);
        assert_eq!(
            keys(&dock),
            vec![
                ItemKey::Start,
                ItemKey::Divider,
                ItemKey::Running("hwnd:10".to_string()),
                ItemKey::Running("hwnd:20".to_string())
            ]
        );
        // Start + running(10) reuse their icons; divider + running(20) are new.
        assert_eq!(remap, vec![Some(0), None, Some(1), None]);
        // The pre-existing window stays fully present; the new one starts collapsed.
        assert_eq!(dock.slots[2].presence, 1.0);
        assert_eq!(dock.slots[3].presence, 0.0);
        assert_eq!(dock.slots[3].presence_target, 1.0);
    }

    #[test]
    fn closing_a_window_keeps_it_exiting_then_drops_it() {
        let mut dock = seeded(vec![start(), divider(), running(10), running(20)]);
        dock.reconcile(vec![start(), divider(), running(10)]);
        // running(20) is retained, collapsing out — not gone yet, so neighbours glide.
        assert_eq!(keys(&dock).len(), 4);
        let exiting = keys(&dock)
            .iter()
            .position(|k| *k == ItemKey::Running("hwnd:20".to_string()))
            .unwrap();
        assert_eq!(dock.slots[exiting].presence_target, 0.0);
        assert!(dock.take_finished_exits().is_empty()); // still visible -> kept

        // Drive presence to ~0 and confirm it (and only it) gets dropped.
        dock.slots[exiting].presence = 0.0;
        assert_eq!(dock.take_finished_exits(), vec![exiting]);
        assert_eq!(
            keys(&dock),
            vec![
                ItemKey::Start,
                ItemKey::Divider,
                ItemKey::Running("hwnd:10".to_string())
            ]
        );
    }

    #[test]
    fn reordering_running_groups_reuses_icons_by_key() {
        let mut dock = seeded(vec![
            start(),
            divider(),
            running_group("explorer.exe", 10),
            running_group("mihoyo.exe", 20),
        ]);

        let remap = dock.reconcile(vec![
            start(),
            divider(),
            running_group("mihoyo.exe", 20),
            running_group("explorer.exe", 10),
        ]);

        assert_eq!(
            keys(&dock),
            vec![
                ItemKey::Start,
                ItemKey::Divider,
                ItemKey::Running("mihoyo.exe".to_string()),
                ItemKey::Running("explorer.exe".to_string())
            ]
        );
        assert_eq!(remap, vec![Some(0), Some(1), Some(3), Some(2)]);
    }

    #[test]
    fn middle_insertion_lands_between_the_right_neighbours() {
        let mut dock = seeded(vec![start(), divider(), running(10), running(30)]);
        // A new window whose handle sorts between the two existing ones.
        let remap = dock.reconcile(vec![
            start(),
            divider(),
            running(10),
            running(20),
            running(30),
        ]);
        assert_eq!(
            keys(&dock),
            vec![
                ItemKey::Start,
                ItemKey::Divider,
                ItemKey::Running("hwnd:10".to_string()),
                ItemKey::Running("hwnd:20".to_string()),
                ItemKey::Running("hwnd:30".to_string())
            ]
        );
        assert_eq!(remap, vec![Some(0), Some(1), Some(2), None, Some(3)]);
    }

    #[test]
    fn cursor_anchored_icon_stays_at_resting_centre() {
        // Five icons so the middle one (index 2) has neighbours on both sides
        // that also magnify when the cursor is nearby.
        let mut dock = seeded(vec![
            pinned("a", None),
            pinned("b", None),
            pinned("c", None),
            pinned("d", None),
            pinned("e", None),
        ]);

        // Resting centres with no cursor.
        let rest = dock.rest_centers();
        let hovered = 2;
        dock.cursor_x = Some(rest[hovered]);

        // Tick until the magnification eases to its target.
        for _ in 0..200 {
            if !dock.tick() {
                break;
            }
        }

        let frame = dock.frame();
        let magnified_cx = frame.icons.iter().find(|ic| ic.idx == hovered).unwrap().cx;

        // The hovered icon must stay at its resting centre — this is the fix.
        // Before the fix the entire row was re-centred on magnified widths, which
        // shifted the hovered icon sideways when neighbours also magnified.
        assert!(
            (magnified_cx - rest[hovered]).abs() < 0.5,
            "hovered icon drifted from resting centre: rest={} mag={}",
            rest[hovered],
            magnified_cx
        );

        // Neighbours should have been pushed *outward* (away from the cursor).
        let left_cx = frame.icons.iter().find(|ic| ic.idx == 0).unwrap().cx;
        let right_cx = frame.icons.iter().find(|ic| ic.idx == 4).unwrap().cx;
        assert!(
            left_cx < rest[0],
            "left neighbour should shift left: rest={} got={}",
            rest[0],
            left_cx
        );
        assert!(
            right_cx > rest[4],
            "right neighbour should shift right: rest={} got={}",
            rest[4],
            right_cx
        );
    }

    #[test]
    fn frame_lays_out_against_the_smoothed_cursor_not_the_raw_pointer() {
        // `frame` must use the low-passed cursor (`cursor_smooth`), not the raw
        // `cursor_x`: tracking the raw pointer directly moved the whole row
        // ~0.7px per pointer pixel, which made the dock tremble while the cursor
        // swept across icons.
        let mut dock = seeded(vec![
            pinned("a", None),
            pinned("b", None),
            pinned("c", None),
        ]);
        for s in &mut dock.slots {
            s.scale = 1.5; // deterministic magnified widths (no real-time easing)
        }
        let rest = dock.rest_centers();
        // Raw cursor mid-way between icon 0 and 1 (raw progress f = 0.5).
        dock.cursor_x = Some((rest[0] + rest[1]) / 2.0);

        dock.cursor_smooth = rest[0]; // smoothed cursor at icon 0's centre: f = 0
        let anchor_pose: Vec<f32> = dock.frame().icons.iter().map(|ic| ic.cx).collect();
        dock.cursor_smooth = rest[1]; // smoothed cursor at icon 1's centre: pose(1)
        let shifted_pose: Vec<f32> = dock.frame().icons.iter().map(|ic| ic.cx).collect();
        let total: f32 = anchor_pose
            .iter()
            .zip(&shifted_pose)
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(
            total > 1.0,
            "cursor_smooth must drive the row shift, got {total}px"
        );

        // The mid-point cursor must land exactly half-way between the two poses.
        // If `frame` still read the raw cursor it would use f = 0.5 in all three
        // poses above, pinning them together.
        dock.cursor_smooth = (rest[0] + rest[1]) / 2.0;
        let mid_pose: Vec<f32> = dock.frame().icons.iter().map(|ic| ic.cx).collect();
        for ((m, a), b) in mid_pose.iter().zip(&anchor_pose).zip(&shifted_pose) {
            assert!(
                (m - (a + b) / 2.0).abs() < 1e-3,
                "mid-point cursor must interpolate half-way between the poses"
            );
        }
    }

    #[test]
    fn blend_tau_tightens_with_pointer_speed_and_keeps_rest_filtering() {
        // At rest and during slow drift the filter keeps its full 45ms strength —
        // that is what suppresses ±1px hand micro-jitter.
        assert_eq!(adaptive_blend_tau(0.0), BLEND_TAU);
        assert_eq!(adaptive_blend_tau(60.0), BLEND_TAU);
        // It tightens progressively between 1x and 2x the tracking threshold…
        let mid = adaptive_blend_tau(BLEND_TRACK_PX_S * 1.5);
        assert!(mid > BLEND_TAU_FAST && mid < BLEND_TAU, "mid tau {mid}");
        // …and clamps at the fast constant beyond that.
        let fast = adaptive_blend_tau(BLEND_TRACK_PX_S * 2.0);
        assert!((fast - BLEND_TAU_FAST).abs() < 1e-6, "tau {fast}");
        assert_eq!(
            adaptive_blend_tau(BLEND_TRACK_PX_S * 20.0).total_cmp(&fast),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn fast_sweep_keeps_the_smoothed_cursor_on_the_pointer() {
        // 1200 px/s in 60 fps steps (20 px/frame). The old fixed 45ms low-pass
        // trailed the pointer by ~54px in steady state, so the magnification
        // bell visibly chased the cursor — the "laggy, stiff" fast sweep.
        let mut dock = seeded(vec![
            pinned("a", None),
            pinned("b", None),
            pinned("c", None),
            pinned("d", None),
            pinned("e", None),
        ]);
        let start = dock.rest_centers()[0];
        dock.cursor_x = Some(start);
        dock.tick(); // entry reset: cursor_smooth = raw position

        let dt = 1.0 / 60.0;
        let mut worst_lag = 0.0f32;
        for step in 1..=90 {
            dock.last = Instant::now() - Duration::from_secs_f32(dt);
            dock.cursor_x = Some(start + 20.0 * step as f32);
            dock.tick();
            let lag = (dock.cursor_x.unwrap() - dock.cursor_smooth).abs();
            worst_lag = worst_lag.max(lag);
        }
        assert!(
            worst_lag < 25.0,
            "fast sweep lagged {worst_lag}px behind the pointer (was ~54px with the fixed 45ms filter)"
        );
    }

    #[test]
    fn slow_pointer_keeps_the_heavy_jitter_filter() {
        // A 1px-per-frame drift (60 px/s) is far below the tighten threshold, so
        // the blend must keep its heavy 45ms strength: the smoothed cursor holds
        // a ~2.2px trailing offset (alpha ≈ 0.31 per frame). A wrongly tightened
        // filter (10ms) would collapse that to ~0.2px and re-expose hand tremble.
        let mut dock = seeded(vec![pinned("a", None), pinned("b", None)]);
        let start = dock.rest_centers()[0];
        dock.cursor_x = Some(start);
        dock.tick(); // entry reset

        let dt = 1.0 / 60.0;
        for step in 1..=10 {
            dock.last = Instant::now() - Duration::from_secs_f32(dt);
            dock.cursor_x = Some(start + step as f32);
            dock.tick();
        }
        let lag = dock.cursor_x.unwrap() - dock.cursor_smooth;
        assert!(
            lag > 1.5 && lag < 3.0,
            "slow drift lag {lag}px — the rest-time jitter filter changed strength"
        );
    }

    #[test]
    fn wake_frame_does_not_snap_the_row_to_the_raw_cursor() {
        // Slow slides wake the idle message loop once per 1px pointer step, and
        // the wake frame ticks with dt = 0. The raw blend used to apply the new
        // cursor position instantly on that frame — a one-frame row snap on every
        // step. The low-passed cursor must not move at dt = 0.
        let mut dock = seeded(vec![
            pinned("a", None),
            pinned("b", None),
            pinned("c", None),
        ]);
        for s in &mut dock.slots {
            s.scale = 1.5; // deterministic magnified widths (no real-time easing)
        }
        let rest = dock.rest_centers();
        let mid = (rest[0] + rest[1]) / 2.0;
        dock.cursor_x = Some(mid);
        dock.tick(); // entry reset: cursor_smooth jumps to the raw position

        let before: Vec<f32> = dock.frame().icons.iter().map(|ic| ic.cx).collect();
        // 1px pointer step, then the dt = 0 wake tick.
        dock.cursor_x = Some(mid + 1.0);
        dock.wake_animation_clock();
        dock.tick();
        let after: Vec<f32> = dock.frame().icons.iter().map(|ic| ic.cx).collect();
        for (a, b) in before.iter().zip(&after) {
            assert!(
                (a - b).abs() < 1e-3,
                "dt=0 wake frame moved the row: {a} -> {b}"
            );
        }
    }

    #[test]
    fn cursor_sweep_never_snaps_the_row_sideways() {
        // Seven icons so several anchor crossings fit inside one sweep. The
        // discrete anchor used to jump the WHOLE row by ~38px every time the
        // cursor crossed an icon's centre — the "trembling" while sweeping.
        let mut dock = seeded(vec![
            pinned("a", None),
            pinned("b", None),
            pinned("c", None),
            pinned("d", None),
            pinned("e", None),
            pinned("f", None),
            pinned("g", None),
        ]);
        let rest = dock.rest_centers();
        let span = rest[1] - rest[0];

        // Sweep left → right in fine steps, easing magnification fully at each
        // cursor position (like a slow hover) and tracking the biggest per-step
        // jump of any icon. A discontinuous anchor flips somewhere in the middle
        // of the row with a jump far larger than the step size.
        let mut max_jump = 0.0f32;
        let mut prev: Vec<f32> = dock.frame().icons.iter().map(|ic| ic.cx).collect();
        let steps = 400;
        for step in 1..=steps {
            let cx = rest[0] - span * 0.5 + span * 1.5 * (step as f32 / steps as f32);
            dock.cursor_x = Some(cx);
            dock.cursor_smooth = cx;
            // Settle magnification directly: this is a geometry regression, not a
            // wall-clock timing test. Parallel test load must not change the result.
            let rest_now = dock.rest_centers();
            let targets: Vec<f32> = (0..dock.items.len())
                .map(|i| {
                    if dock.items[i].role.is_divider() {
                        1.0
                    } else {
                        dock.target(rest_now[i])
                    }
                })
                .collect();
            for (slot, target) in dock.slots.iter_mut().zip(targets) {
                slot.scale = target;
            }
            let now: Vec<f32> = dock.frame().icons.iter().map(|ic| ic.cx).collect();
            if step > 1 {
                // The first sample establishes the initial hover magnification; only
                // subsequent samples measure continuity during the actual sweep.
                for (a, b) in now.iter().zip(&prev) {
                    max_jump = max_jump.max((a - b).abs());
                }
            }
            prev = now;
        }

        // A sweep that crosses ~6 icon centres in 400 steps moves the row at
        // most ~1px per step (magnified bell tracking). The old discrete anchor
        // produced ~38px snaps — orders of magnitude above any reasonable bound.
        assert!(
            max_jump < 2.0,
            "row snapped by {max_jump}px during a cursor sweep"
        );
    }

    #[test]
    fn reopening_a_closing_window_revives_it() {
        let mut dock = seeded(vec![start(), divider(), running(10)]);
        dock.reconcile(vec![start()]); // divider + running(10) start exiting
        let r = keys(&dock)
            .iter()
            .position(|k| *k == ItemKey::Running("hwnd:10".to_string()))
            .unwrap();
        dock.slots[r].presence = 0.4; // mid-collapse
        dock.reconcile(vec![start(), divider(), running(10)]); // it's back
        let r = keys(&dock)
            .iter()
            .position(|k| *k == ItemKey::Running("hwnd:10".to_string()))
            .unwrap();
        assert_eq!(dock.slots[r].presence_target, 1.0);
        assert_eq!(dock.slots[r].presence, 0.4); // eases back up from where it was
    }
}

#[cfg(test)]
mod layout_regression_tests {
    use super::tests::{divider, item, pinned, running, seeded, start};
    use super::*;
    fn row(count: usize) -> Vec<DockItem> {
        let mut row = vec![crate::apps::start_item(), crate::apps::drawer_item()];
        for index in 0..count {
            let mut item = crate::apps::control_item();
            item.role = ItemRole::Pinned;
            item.path = Some(format!(r"C:\Apps\{index}.exe"));
            row.push(item);
        }
        row.push(crate::apps::control_item());
        row
    }
    #[test]
    fn overflow_preserves_every_application_and_keeps_controls() {
        for dpi in [1.0, 1.5, 2.0] {
            let (visible, hidden) =
                limit_items(row(40), 900, dpi, Default::default(), ThemePreset::Glass);
            assert!(visible.iter().any(|i| i.role == ItemRole::Overflow));
            assert!(visible.iter().any(|i| i.role == ItemRole::Control));
            let mut paths: Vec<_> = visible
                .iter()
                .chain(&hidden)
                .filter_map(|i| i.path.clone())
                .collect();
            paths.sort();
            paths.dedup();
            assert_eq!(paths.len(), 40);
            let model = Dock::new(visible, dpi, 900.0, 300.0, ThemePreset::Glass);
            let frame = model.frame();
            assert!(frame.pill.0 >= 0.0 && frame.pill.2 <= 900.0);
            assert!(model.layout_dpi() <= dpi);
        }
    }
    #[test]
    fn empty_application_rows_do_not_create_an_empty_more_menu() {
        let (visible, hidden) =
            limit_items(row(0), 400, 2.0, Default::default(), ThemePreset::Glass);
        assert!(hidden.is_empty());
        assert!(!visible.iter().any(|i| i.role == ItemRole::Overflow));
    }
    #[test]
    fn roomy_monitors_have_no_unnecessary_overflow() {
        let (visible, hidden) =
            limit_items(row(3), 1920, 1.0, Default::default(), ThemePreset::Glass);
        assert!(hidden.is_empty());
        assert!(!visible.iter().any(|i| i.role == ItemRole::Overflow));
    }
    #[test]
    fn custom_magnification_and_reduced_motion_share_model_geometry() {
        let mut dock = Dock::new(row(2), 1.0, 800.0, 160.0, ThemePreset::Glass);
        dock.preferences.magnification = 130;
        dock.cursor_x = Some(200.0);
        assert!((dock.target(200.0) - 1.3).abs() < 0.001);
        dock.preferences.reduce_motion = true;
        dock.reveal_target = 0.0;
        dock.bump(2);
        assert!(!dock.tick());
        assert_eq!(dock.reveal, 0.0);
        assert!(dock.slots.iter().all(|slot| slot.bounce == 0.0));
    }
    #[test]
    fn compact_preset_reduces_actual_row_width() {
        let row = row(8);
        assert!(
            window_size_with(&row, 1.0, Default::default(), ThemePreset::Compact).0
                < window_size_with(&row, 1.0, Default::default(), ThemePreset::Glass).0
        );
    }

    #[test]
    fn limit_items_drops_regular_items_before_waiting_and_running_agents() {
        let mut codex_waiting = pinned("C:\\Codex.exe", None);
        codex_waiting.agent = Some(AgentSlotInfo {
            agent_id: "codex".to_string(),
            state: AgentState::Waiting,
            badge: Some("!".to_string()),
            active_sessions: 0,
            total_sessions: 1,
            waiting_sessions: 1,
        });
        let mut dsh_idle = pinned("C:\\dsh.exe", None);
        dsh_idle.agent = Some(AgentSlotInfo::idle("dsh"));
        let agent_div = item(ItemRole::AgentDivider, None, None);
        let control = item(ItemRole::Control, None, None);

        let all = vec![
            start(),
            pinned("C:\\App1.exe", None),
            pinned("C:\\App2.exe", None),
            divider(),
            running(101),
            agent_div,
            codex_waiting,
            dsh_idle,
            control,
        ];
        // Minimum width for `[start, codex_waiting, overflow, control]` at default 50px + headroom
        // is 474 px; 500 px keeps `codex_waiting` while evicting regular items + idle Agent.
        let (visible, hidden) = limit_items(
            all.clone(),
            500,
            1.0,
            Default::default(),
            ThemePreset::Glass,
        );
        assert!(visible
            .iter()
            .any(|i| i.agent.as_ref().is_some_and(|a| a.agent_id == "codex")));
        assert!(hidden
            .iter()
            .any(|i| i.role == ItemRole::Running && !i.is_agent()));
        assert!(!visible.iter().any(|i| i.role == ItemRole::Divider));
        assert!(!visible.iter().any(|i| i.role == ItemRole::AgentDivider));

        // Even below minimum slot width, system buttons remain reachable.
        let (extreme, _) = limit_items(all, 300, 1.0, Default::default(), ThemePreset::Glass);
        assert!(extreme.iter().any(|i| i.role == ItemRole::Start));
        assert!(extreme.iter().any(|i| i.role == ItemRole::Overflow));
        assert!(extreme.iter().any(|i| i.role == ItemRole::Control));
    }

    #[test]
    fn overflow_keeps_running_agent_before_idle_gui_agent() {
        let mut codex_running = pinned("C:\\Codex.exe", None);
        codex_running.agent = Some(AgentSlotInfo {
            agent_id: "codex".to_string(),
            state: AgentState::Running,
            badge: Some("1".to_string()),
            active_sessions: 1,
            total_sessions: 1,
            waiting_sessions: 0,
        });
        let mut dsh_idle_gui = pinned("C:\\dsh.exe", Some(42));
        dsh_idle_gui.agent = Some(AgentSlotInfo::idle("dsh"));
        let all = vec![
            start(),
            item(ItemRole::AgentDivider, None, None),
            codex_running,
            dsh_idle_gui,
            item(ItemRole::Control, None, None),
        ];
        let (visible, hidden) = limit_items(all, 500, 1.0, Default::default(), ThemePreset::Glass);
        assert!(visible
            .iter()
            .any(|i| i.agent.as_ref().is_some_and(|a| a.agent_id == "codex")));
        assert!(hidden
            .iter()
            .any(|i| i.agent.as_ref().is_some_and(|a| a.agent_id == "dsh")));
    }

    #[test]
    fn reconcile_tracks_agent_active_indicator_and_running_presence() {
        let mut agent_pinned = pinned("C:\\Antigravity.exe", None);
        agent_pinned.agent = Some(AgentSlotInfo::idle("antigravity"));
        let mut dock = seeded(vec![
            start(),
            item(ItemRole::AgentDivider, None, None),
            agent_pinned.clone(),
        ]);
        assert!(!dock.has_running_agent());
        assert_eq!(dock.slots[2].running_target, 0.0);

        agent_pinned.agent = Some(AgentSlotInfo {
            agent_id: "antigravity".to_string(),
            state: AgentState::Running,
            badge: Some("1".to_string()),
            active_sessions: 1,
            total_sessions: 1,
            waiting_sessions: 0,
        });
        dock.reconcile(vec![
            start(),
            item(ItemRole::AgentDivider, None, None),
            agent_pinned,
        ]);
        assert!(dock.has_running_agent());
        assert_eq!(dock.slots[2].running_target, 1.0);
        assert_eq!(
            dock.items[2].key(),
            ItemKey::Agent("antigravity".to_string())
        );
    }
}
