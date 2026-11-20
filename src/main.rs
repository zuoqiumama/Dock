#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// FeatherDock — an ultra-light, GPU-composited macOS-style dock for Windows.
// NOTE: console subsystem kept ON during early dev so panics are visible.
// Flip to `#![windows_subsystem = "windows"]` once stable.

use std::time::{Duration, Instant};

mod agent_popup;
mod agent_state;
mod app_icon;
mod app_identity;
mod apps;
mod atomic;
mod autostart;
mod backdrop;
mod categories;
mod command_palette;
mod config;
mod content;
mod control_center;
mod desktop_icons;
mod desktop_scan;
mod dock;
mod drawer;
mod drawer_icons;
mod drawer_input;
mod drawer_layout;
mod error_log;
mod folder_stack;
mod geometry;
mod glass;
mod graphics;
mod icons;
mod menu_style;
mod native_tray;
mod preferences;
mod render;
mod search;
mod settings;
mod settings_window;
mod single_instance;
mod sysctl;
mod taskbar;
mod theme;
mod tray;
mod watchdog;
mod window_preview;
mod windows_list;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Device;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Gdi::{
    CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromWindow, PtInRect, SetWindowRgn,
    HGDIOBJ, HRGN, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, DragQueryPoint, FileOpenDialog, IFileOpenDialog,
    ShellExecuteW, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
    HDROP, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use dock::Dock;
use graphics::Gpu;
use tray::{Tray, WM_TRAY};

// Not exported by windows 0.58 under our features; define the Win32 value.
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_MOUSEHOVER: u32 = 0x02A1;
const HRESULT_CANCELLED: HRESULT = HRESULT(0x800704C7u32 as i32);

// Coalescing timer: bursts of window create/destroy/show/hide events (apps churn
// hidden helper windows) collapse into one EnumWindows rescan instead of one each.
const TIMER_WINDOWS: usize = 1;
const TIMER_WINDOWS_MS: u32 = 180;
const TIMER_TASKBAR_REHIDE: usize = 2;
const TIMER_TASKBAR_REHIDE_MS: u32 = 400;
const TIMER_AGENT_SHIMMER: usize = 3;
const SHIMMER_REST_MS: u32 = 2400;
const SHIMMER_TICK_MS: u32 = 33;
const SHIMMER_SWEEP_MS: f32 = 1200.0;
const TASKBAR_INVOCATION_GRACE: Duration = Duration::from_secs(8);
const ANIMATION_MESSAGE_BUDGET: usize = 4;
const QUIT_WAIT_MS: u32 = 5_000;
const ID_SEARCH_HOTKEY: i32 = 3001;
const QUIT_FLAG: &str = "--quit";
const SETTINGS_FLAG: &str = "--settings";
const RESTORE_SYSTEM_FLAG: &str = "--restore-system";
const WM_SHOW_EXISTING: u32 = 0x8033;
pub(crate) const WM_ANIMATION_WAKE: u32 = 0x8034;
const WM_OPEN_SETTINGS: u32 = 0x8035;
pub(crate) const WM_DOCK_ICONS_READY: u32 = 0x8036;
pub(crate) const WM_POPUP_RENDER_FAILED: u32 = WM_APP + 0x38;

fn animation_message_batch_has_capacity(dispatched: usize) -> bool {
    dispatched < ANIMATION_MESSAGE_BUDGET
}

fn animation_sources_active(dock: bool, drawer: bool, control: bool) -> bool {
    dock || drawer || control
}

fn settled_region_should_stay_full(cursor_over_dock: bool) -> bool {
    cursor_over_dock
}

fn dock_geometry_changed(current: (i32, i32, i32, i32), requested: (i32, i32, i32, i32)) -> bool {
    current != requested
}

// Right-click context-menu command ids (kept clear of the tray ids in tray.rs).
const ID_WIN_CLOSE: usize = 2001;
const ID_WIN_MINIMIZE: usize = 2002;
const ID_WIN_MAXIMIZE: usize = 2003;
const ID_WIN_PIN: usize = 2004;
const ID_PIN_OPEN: usize = 2101;
const ID_PIN_LOCATION: usize = 2102;
const ID_PIN_REMOVE: usize = 2103;

#[derive(Clone, Copy, PartialEq, Eq)]
struct WatchdogPlan {
    restore_taskbar: bool,
    restore_desktop_icons: bool,
}

impl WatchdogPlan {
    fn needed(self) -> bool {
        self.restore_taskbar || self.restore_desktop_icons
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShimmerTimerMode {
    Off,
    RestWait,
    SweepTick,
}

struct DockDrag {
    source: String,
    is_agent: bool,
    start: (f32, f32),
    active: bool,
    target: Option<(String, bool)>,
}

struct App {
    _instance: single_instance::SingleInstance,
    gpu: Gpu,
    dock: Dock,
    overflow_items: Vec<dock::DockItem>,
    drag: Option<DockDrag>,
    shutting_down: bool,
    preview_candidate: Option<usize>,
    overlap_active: bool,
    tray: Tray,
    hooks: Vec<HWINEVENTHOOK>,    // WinEvent hooks tracking open windows
    running_sig: Vec<RunningSig>, // last seen open-window topology
    window_titles: Vec<(isize, String)>,
    title_matches: Vec<(String, String)>,
    agent_registry: agent_state::AgentRegistry,
    agent_watcher: agent_state::SessionDirWatcher,
    agent_pids: agent_state::PidMonitor,
    shimmer_timer_mode: ShimmerTimerMode,
    shimmer_sweep_start: Instant,
    settings: settings::Settings, // effective settings, including a transient layout preview
    saved_settings: settings::Settings, // latest settings committed to disk
    fullscreen_active: bool,      // a fullscreen (borderless) app is in front on our monitor
    maximized_active: bool,       // a maximized (captioned) window is in front on our monitor
    animating: bool,
    tracking: bool,
    full: (i32, i32, i32, i32), // full window rect (x, y, w, h)
    strip_h: i32,               // window height when collapsed to the reveal strip
    expanded: bool,             // true = full window, false = thin bottom strip
    window_hidden: bool,        // fully SW_HIDE'd because a fullscreen app is in front
    region_full: bool,          // window/input clip: true = full, false = expanded pill
    pending_relayout: bool,     // shrink the window to fit once exit animations settle
    pending_rebuild: bool, // a window-set/display change deferred while a fullscreen app owned the screen
    taskbar_invoked_at: Option<Instant>,
    native_tray_request: Option<std::thread::JoinHandle<()>>,
    watchdog: Option<std::process::Child>, // sibling that restores system state if we crash
    watchdog_plan: Option<WatchdogPlan>,
}

struct AppGuard(*mut App);

impl Drop for AppGuard {
    fn drop(&mut self) {
        unsafe { cleanup(self.0) }
    }
}

fn pending_rebuild_after_attempt(pending: bool, succeeded: bool) -> bool {
    pending && !succeeded
}

/// True while a fullscreen app (game / video) is in front and we're set to yield to it.
/// In this state the dock is fully hidden — not merely collapsed to the reveal strip —
/// and ignores bottom-edge hover, so it can never pop over a game and kick an exclusive-
/// fullscreen title back to the desktop.
fn fullscreen_suppressed(app: &App) -> bool {
    app.settings.hide_on_fullscreen && app.fullscreen_active
}

/// True while a *maximized* window is in front and we're set to yield to it: the dock
/// retracts to its reveal strip so it doesn't cover the window, but a bottom-edge hover
/// STILL summons it — unlike `fullscreen_suppressed`, which fully hides it and ignores
/// hover. (A window can't be both: maximized has a caption, fullscreen does not.)
fn maximized_retracted(app: &App) -> bool {
    if app.settings.intellihide {
        app.overlap_active
    } else {
        app.settings.hide_on_maximized && app.maximized_active
    }
}

/// True if a borderless monitor-filling app (a fullscreen game / video) is in front on
/// the dock's monitor — the cue to keep the dock COMPLETELY passive: no `SetWindowPos`
/// on our window, no window reflow. Any z-order churn at that moment knocks such an app
/// out of exclusive fullscreen and bounces it to the desktop (the "can't enter the game"
/// bug). Checked fresh (not from cached state) since a display-mode change can arrive
/// before the foreground event that updates `fullscreen_active`.
unsafe fn fullscreen_app_present(hwnd: HWND) -> bool {
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    windows_list::is_fullscreen_present(monitor)
}

/// Where `reveal` should ease to when the cursor is NOT over the dock: hidden in auto-hide
/// mode, while a fullscreen app is in front, or while a maximized window is in front;
/// otherwise resident.
fn resting_target(app: &App) -> f32 {
    let hide = app.settings.dock_mode == settings::DockMode::AutoHide
        || fullscreen_suppressed(app)
        || maximized_retracted(app);
    if hide {
        0.0
    } else {
        1.0
    }
}

/// The dock's D3D11 + Direct2D devices, for popup surfaces to share. This is what
/// lets the drawer reuse the icon bitmaps decoded at startup instead of
/// re-uploading them on every open. Returns owned clones (the popup keeps them
/// alive for as long as its own surface exists).
pub(crate) unsafe fn dock_shared_gpu(hwnd: HWND) -> Option<(ID3D11Device, ID2D1Device)> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return None;
    }
    Some((
        (*ptr).gpu.d3d_device().clone(),
        (*ptr).gpu.d2d_device().clone(),
    ))
}

/// Snapshot the owner's preset without manufacturing another mutable App borrow.
pub(crate) unsafe fn dock_theme(hwnd: HWND) -> theme::ThemePreset {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
    if ptr.is_null() {
        theme::ThemePreset::default()
    } else {
        (*ptr).dock.theme
    }
}

pub(crate) unsafe fn dock_layout(hwnd: HWND) -> settings::Layout {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
    if ptr.is_null() {
        settings::Layout::default()
    } else {
        (*ptr).dock.preferences
    }
}

pub(crate) unsafe fn dock_contains_cursor(hwnd: HWND) -> bool {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return false;
    }
    let mut wr = RECT::default();
    IsWindow(hwnd).as_bool() && GetWindowRect(hwnd, &mut wr).is_ok() && PtInRect(&wr, pt).as_bool()
}

fn io_error(context: &str, error: std::io::Error) -> Error {
    Error::new(E_FAIL, format!("{context}: {error}"))
}

/// The pinned (left-of-divider) items, from the user's config or built-in defaults.
fn pinned_items() -> Result<Vec<dock::DockItem>> {
    match config::load_with_maintenance().map_err(|error| io_error("读取配置失败", error))? {
        Some(cfg) => Ok(apps::from_config(&cfg.items)),
        None => {
            let items = apps::default_items();
            config::write_default(&items).map_err(|error| io_error("创建默认配置失败", error))?;
            Ok(items)
        }
    }
}

/// Assemble the full dock row: Start, pinned apps, then (if any open windows that
/// AREN'T already pinned) a divider followed by one slot per such window group.
///
/// A running window whose executable matches a pinned app is *merged into that pinned
/// icon* (it gains a running dot and activates on click) rather than shown a second
/// time on the right — so the dock never carries the same app twice (macOS-style).
fn compose_items(
    running: &[windows_list::RunningWindow],
    settings: &settings::Settings,
    registry: &agent_state::AgentRegistry,
) -> Result<Vec<dock::DockItem>> {
    let pinned = pinned_items()?;
    Ok(compose_items_with_pinned(
        pinned, running, settings, registry,
    ))
}

fn compose_items_with_pinned(
    mut pinned: Vec<dock::DockItem>,
    running: &[windows_list::RunningWindow],
    settings: &settings::Settings,
    registry: &agent_state::AgentRegistry,
) -> Vec<dock::DockItem> {
    let unpinned = merge_running_groups(&mut pinned, windows_list::group_by_application(running));
    if !settings.agent_zone_enabled {
        for item in &mut pinned {
            item.agent = None;
        }
        let mut items = Vec::with_capacity(pinned.len() + unpinned.len() + 5);
        items.push(apps::start_item());
        if settings.drawer_enabled {
            items.push(apps::drawer_item());
        }
        items.extend(pinned);
        if !unpinned.is_empty() {
            items.push(apps::divider_item());
            for group in &unpinned {
                items.push(apps::running_item(group));
            }
        }
        items.extend([apps::system_tray_item(), apps::control_item()]);
        return items;
    }

    let mut regular_pinned = Vec::new();
    let mut agent_pinned: Vec<dock::DockItem> = Vec::new();
    for item in pinned {
        if let Some(agent_id) = item.agent.as_ref().map(|a| a.agent_id.clone()) {
            if let Some(primary) = agent_pinned.iter_mut().find(|existing| {
                existing
                    .agent
                    .as_ref()
                    .is_some_and(|a| a.agent_id == agent_id)
            }) {
                merge_duplicate_pinned_item(primary, item);
            } else {
                agent_pinned.push(item);
            }
        } else {
            regular_pinned.push(item);
        }
    }

    let mut regular_running = Vec::new();
    let mut unpinned_agent_groups: Vec<(String, Vec<windows_list::RunningGroup>)> = Vec::new();
    for group in unpinned {
        if let Some(agent_id) = agent_state::infer_agent_id_for_group(&group) {
            if let Some(pinned_slot) = agent_pinned
                .iter_mut()
                .find(|item| item.agent.as_ref().is_some_and(|a| a.agent_id == agent_id))
            {
                merge_additional_group(pinned_slot, &group);
            } else if let Some((_, groups)) = unpinned_agent_groups
                .iter_mut()
                .find(|(id, _)| id == &agent_id)
            {
                groups.push(group);
            } else {
                unpinned_agent_groups.push((agent_id, vec![group]));
            }
        } else {
            regular_running.push(group);
        }
    }

    let mut seen_agents: Vec<String> = Vec::new();
    for item in &mut agent_pinned {
        if let Some(agent_id) = item.agent.as_ref().map(|a| a.agent_id.clone()) {
            seen_agents.push(agent_id.clone());
            let summary = registry.summarize_agent(&agent_id, &item.label, &item.windows);
            item.agent = Some(agent_slot_info_from_summary(&summary));
        }
    }

    let mut agent_unpinned = Vec::new();
    for (agent_id, groups) in unpinned_agent_groups {
        seen_agents.push(agent_id.clone());
        if let Some(item) = build_unpinned_agent_item(&agent_id, &groups, registry) {
            agent_unpinned.push(item);
        }
    }
    for agent_id in registry.track_b_agent_ids() {
        if seen_agents.iter().any(|existing| existing == &agent_id) {
            continue;
        }
        seen_agents.push(agent_id.clone());
        let summary = registry.summarize_agent(&agent_id, "", &[]);
        if summary.aggregate_state != agent_state::AgentState::Idle {
            agent_unpinned.push(apps::agent_running_item(
                &summary,
                None,
                apps::known_agent_launch_hint(&agent_id),
            ));
        }
    }

    let mut items = Vec::with_capacity(
        regular_pinned.len()
            + regular_running.len()
            + agent_pinned.len()
            + agent_unpinned.len()
            + 6,
    );
    items.push(apps::start_item());
    if settings.drawer_enabled {
        items.push(apps::drawer_item());
    }
    items.extend(regular_pinned);
    if !regular_running.is_empty() {
        items.push(apps::divider_item());
        for group in &regular_running {
            items.push(apps::running_item(group));
        }
    }
    let has_regular_content = items
        .iter()
        .any(|i| matches!(i.role, dock::ItemRole::Pinned | dock::ItemRole::Running));
    if !agent_pinned.is_empty() || !agent_unpinned.is_empty() {
        if has_regular_content {
            items.push(apps::agent_divider_item());
        }
        items.extend(agent_pinned);
        items.extend(agent_unpinned);
    }
    items.extend([apps::system_tray_item(), apps::control_item()]);
    items
}

fn agent_slot_info_from_summary(summary: &agent_state::AgentDockSummary) -> dock::AgentSlotInfo {
    dock::AgentSlotInfo {
        agent_id: summary.agent_id.clone(),
        state: summary.aggregate_state,
        badge: summary.badge_text.clone(),
        active_sessions: summary.active_count,
        total_sessions: summary.total_sessions,
        waiting_sessions: summary.waiting_count,
    }
}

fn merge_duplicate_pinned_item(primary: &mut dock::DockItem, extra: dock::DockItem) {
    for window in extra.windows {
        if !primary
            .windows
            .iter()
            .any(|existing| existing.hwnd == window.hwnd)
        {
            primary.windows.push(window);
        }
    }
    if primary.hwnd.is_none() {
        primary.hwnd = extra.hwnd;
    }
    if primary.group_key.is_none() {
        primary.group_key = extra.group_key;
    }
    if primary.path.is_none() {
        primary.path = extra.path;
    }
    if primary.icon.is_none() {
        primary.icon = extra.icon;
    }
}

fn merge_additional_group(item: &mut dock::DockItem, group: &windows_list::RunningGroup) {
    for window in &group.windows {
        if !item
            .windows
            .iter()
            .any(|existing| existing.hwnd == window.hwnd)
        {
            item.windows.push(dock::RunningWindowRef {
                hwnd: window.hwnd,
                title: window.title.clone(),
            });
        }
    }
    if item.hwnd.is_none() {
        item.hwnd = group.windows.first().map(|w| w.hwnd);
    }
    if item.group_key.is_none() {
        item.group_key = Some(group.key.clone());
    }
    if item.icon.is_none() && item.path.is_none() {
        item.icon = group.icon_path.clone();
    }
}

fn build_unpinned_agent_item(
    agent_id: &str,
    groups: &[windows_list::RunningGroup],
    registry: &agent_state::AgentRegistry,
) -> Option<dock::DockItem> {
    let first = groups.first()?;
    let mut combined = first.clone();
    for extra in groups.iter().skip(1) {
        if combined.icon_path.is_none() {
            combined.icon_path = extra.icon_path.clone();
        }
        for window in &extra.windows {
            if !combined.windows.iter().any(|w| w.hwnd == window.hwnd) {
                combined.windows.push(window.clone());
            }
        }
    }
    let window_refs: Vec<dock::RunningWindowRef> = combined
        .windows
        .iter()
        .map(|w| dock::RunningWindowRef {
            hwnd: w.hwnd,
            title: w.title.clone(),
        })
        .collect();
    let display_label = if combined.label.trim().is_empty() {
        agent_state::display_name(agent_id)
    } else {
        combined.label.clone()
    };
    let summary = registry.summarize_agent(agent_id, &display_label, &window_refs);
    Some(apps::agent_running_item(
        &summary,
        Some(&combined),
        apps::known_agent_launch_hint(agent_id),
    ))
}

fn item_has_hover_popup(item: &dock::DockItem, agent_zone_enabled: bool) -> bool {
    !item.windows.is_empty() || (agent_zone_enabled && item.is_agent())
}

fn pinned_exactly_matches_group(item: &dock::DockItem, group: &windows_list::RunningGroup) -> bool {
    item.path
        .as_deref()
        .is_some_and(|path| crate::app_identity::cached_launch_key(path) == group.key)
}

fn merge_running_groups(
    pinned: &mut [dock::DockItem],
    groups: Vec<windows_list::RunningGroup>,
) -> Vec<windows_list::RunningGroup> {
    let fallback_claims: Vec<std::collections::HashSet<String>> = pinned
        .iter()
        .map(|item| {
            groups
                .iter()
                .filter(|group| {
                    pinned_matches_group(item, group) && !pinned_exactly_matches_group(item, group)
                })
                .map(|group| group.key.clone())
                .collect()
        })
        .collect();
    let mut unpinned = Vec::new();
    for group in groups {
        let exact = pinned
            .iter()
            .position(|item| pinned_exactly_matches_group(item, &group));
        let matched = exact.or_else(|| {
            let mut candidates = pinned.iter().enumerate().filter(|(index, item)| {
                fallback_claims[*index].len() == 1 && pinned_matches_group(item, &group)
            });
            let first = candidates.next();
            if candidates.next().is_none() {
                first.map(|(index, _)| index)
            } else {
                None
            }
        });
        if let Some(index) = matched {
            apps::attach_running(&mut pinned[index], &group);
        } else {
            unpinned.push(group);
        }
    }
    unpinned
}

fn pinned_matches_group(item: &dock::DockItem, group: &windows_list::RunningGroup) -> bool {
    item.windows.is_empty()
        && pinned_identity_matches_group(&item.label, item.path.as_deref(), group)
}

fn unique_group_for_pinned(
    item: &dock::DockItem,
    groups: &[windows_list::RunningGroup],
) -> Option<windows_list::RunningGroup> {
    let mut exact = groups
        .iter()
        .filter(|group| pinned_exactly_matches_group(item, group));
    if let Some(group) = exact.next() {
        return if exact.next().is_none() {
            Some(group.clone())
        } else {
            None
        };
    }
    let mut fallback = groups.iter().filter(|group| {
        pinned_identity_matches_group(&item.label, item.path.as_deref(), group)
            && !pinned_exactly_matches_group(item, group)
    });
    let group = fallback.next()?;
    if fallback.next().is_none() {
        Some(group.clone())
    } else {
        None
    }
}

fn pinned_identity_matches_group(
    label: &str,
    path: Option<&str>,
    group: &windows_list::RunningGroup,
) -> bool {
    path.is_some_and(|path| {
        apps::exe_matches(path, &group.key)
            || (!app_identity::cached_launch_key(path).starts_with("aumid:")
                && group
                    .icon_path
                    .as_deref()
                    .is_some_and(|exe| apps::exe_matches(path, &app_identity::normalize(exe))))
    }) || pinned_title_matches_group(label, path, group)
}

fn pinned_title_matches_group(
    label: &str,
    path: Option<&str>,
    group: &windows_list::RunningGroup,
) -> bool {
    apps::title_matches_label(label, group)
        && path.zip(group.icon_path.as_deref()).is_some_and(|(p, g)| {
            std::path::Path::new(p).parent().map(config::path_key)
                == std::path::Path::new(g).parent().map(config::path_key)
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningSig {
    hwnd: isize,
    exe_path: Option<String>,
    app_id: Option<String>,
}

fn running_sig(running: &[windows_list::RunningWindow]) -> Vec<RunningSig> {
    running
        .iter()
        .map(|window| RunningSig {
            hwnd: window.hwnd,
            exe_path: window.exe_path.clone(),
            app_id: window.app_id.clone(),
        })
        .collect()
}

fn window_titles(running: &[windows_list::RunningWindow]) -> Vec<(isize, String)> {
    running
        .iter()
        .map(|window| (window.hwnd, window.title.clone()))
        .collect()
}

fn title_group_matches(
    items: &[dock::DockItem],
    running: &[windows_list::RunningWindow],
) -> Vec<(String, String)> {
    let groups = windows_list::group_by_application(running);
    let mut matches = Vec::new();
    for item in items
        .iter()
        .filter(|item| item.role == dock::ItemRole::Pinned)
    {
        for group in &groups {
            if pinned_title_matches_group(&item.label, item.path.as_deref(), group) {
                matches.push((
                    item.path.clone().unwrap_or_else(|| item.label.clone()),
                    group.key.clone(),
                ));
            }
        }
    }
    matches.sort();
    matches.dedup();
    matches
}

fn title_group_matches_with_overflow(
    items: &[dock::DockItem],
    overflow: &[dock::DockItem],
    running: &[windows_list::RunningWindow],
) -> Vec<(String, String)> {
    let mut matches = title_group_matches(items, running);
    matches.extend(title_group_matches(overflow, running));
    matches.sort();
    matches.dedup();
    matches
}

fn record_window_state(app: &mut App, running: &[windows_list::RunningWindow]) {
    app.running_sig = running_sig(running);
    app.window_titles = window_titles(running);
    app.title_matches =
        title_group_matches_with_overflow(&app.dock.items, &app.overflow_items, running);
}

unsafe fn update_window_titles(hwnd: HWND, app: &mut App, running: &[windows_list::RunningWindow]) {
    let groups = windows_list::group_by_application(running);
    let open_agent = agent_popup::active_agent();
    let mut popup_refresh: Option<(agent_state::AgentDockSummary, Option<String>)> = None;

    for item in app
        .dock
        .items
        .iter_mut()
        .chain(app.overflow_items.iter_mut())
    {
        if app.settings.agent_zone_enabled {
            if let Some(agent_id) = item.agent.as_ref().map(|a| a.agent_id.clone()) {
                let mut merged = Vec::new();
                for group in groups.iter().filter(|g| {
                    agent_state::infer_agent_id_for_group(g).as_deref() == Some(agent_id.as_str())
                        || item.group_key.as_deref() == Some(g.key.as_str())
                        || g.windows
                            .iter()
                            .any(|w| item.windows.iter().any(|ew| ew.hwnd == w.hwnd))
                }) {
                    for window in &group.windows {
                        if !merged
                            .iter()
                            .any(|w: &dock::RunningWindowRef| w.hwnd == window.hwnd)
                        {
                            merged.push(dock::RunningWindowRef {
                                hwnd: window.hwnd,
                                title: window.title.clone(),
                            });
                        }
                    }
                }
                item.windows = merged;
                let summary =
                    app.agent_registry
                        .summarize_agent(&agent_id, &item.label, &item.windows);
                item.agent = Some(agent_slot_info_from_summary(&summary));
                if open_agent.as_deref() == Some(agent_id.as_str()) {
                    popup_refresh = Some((summary, item.path.clone()));
                }
                continue;
            }
        }
        let Some(group_key) = item.group_key.clone() else {
            continue;
        };
        let Some(group) = groups.iter().find(|group| group.key == group_key) else {
            continue;
        };
        item.windows = group
            .windows
            .iter()
            .map(|window| dock::RunningWindowRef {
                hwnd: window.hwnd,
                title: window.title.clone(),
            })
            .collect();
        let preview_key = item.group_key.as_deref().unwrap_or(item.label.as_str());
        window_preview::update_titles(preview_key, &item.windows);
    }

    if let Some((summary, launch_path)) = popup_refresh {
        agent_popup::refresh_if_open(hwnd, &summary, launch_path.as_deref());
    }
}

unsafe fn monitor_layout(
    _hwnd: HWND,
    items: &[dock::DockItem],
    prefs: settings::Settings,
) -> Result<(f32, i32, i32, u32, u32)> {
    primary_monitor_layout(items, prefs)
}

unsafe fn primary_monitor_layout(
    items: &[dock::DockItem],
    prefs: settings::Settings,
) -> Result<(f32, i32, i32, u32, u32)> {
    let monitor = geometry::selected_monitor(prefs.monitor);
    let mut x = 96;
    let mut y = 96;
    let dpi = if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() {
        x.max(96) as f32 / 96.0
    } else {
        GetDpiForSystem().max(96) as f32 / 96.0
    };
    monitor_geometry(monitor, dpi, items, prefs)
}

/// Bottom-centre the dock on `monitor` at `dpi`. Anchored to the monitor's true bottom
/// edge (not the work area) so the auto-hide reveal trigger sits at the very edge.
unsafe fn monitor_geometry(
    monitor: windows::Win32::Graphics::Gdi::HMONITOR,
    dpi: f32,
    items: &[dock::DockItem],
    prefs: settings::Settings,
) -> Result<(f32, i32, i32, u32, u32)> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return Err(Error::from_win32());
    }
    let (width, height) = dock::window_size_with_style(
        items,
        dpi,
        prefs.layout,
        prefs.effective_theme(),
        prefs.dock_style,
    );
    let width =
        width.min((info.rcMonitor.right - info.rcMonitor.left - (16.0 * dpi) as i32).max(1) as u32);
    let x = info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - width as i32) / 2;
    let y = info.rcMonitor.bottom - height as i32;
    Ok((dpi, x, y, width, height))
}

unsafe fn show_error(owner: HWND, context: &str, error: impl std::fmt::Display) {
    error_log::write(context, &error);
    let text: Vec<u16> = format!("{context}\n\n{error}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = MessageBoxW(
        owner,
        PCWSTR(text.as_ptr()),
        w!("FeatherDock"),
        MB_OK | MB_ICONERROR,
    );
}

/// Grow the window to full size (to show the dock) or shrink it to a thin bottom
/// strip (so the area it used to cover becomes genuinely click-through).
/// Where the dock should sit in the z-order. Normally the top of the topmost band;
/// while the user has explicitly revealed the system taskbar (Start click), just
/// *below* the taskbar so they can operate it on top of the dock.
unsafe fn dock_z_insert_after(app: &App) -> HWND {
    if app.taskbar_invoked_at.is_some() {
        let tray = taskbar::tray_hwnd();
        if !tray.is_invalid() {
            return tray;
        }
    }
    HWND_TOPMOST
}

/// Drop the dock just below the (revealed) taskbar so the taskbar is operable; both
/// stay topmost. Called when a Start click reveals the taskbar.
unsafe fn place_dock_behind_taskbar(dock: HWND) {
    let tray = taskbar::tray_hwnd();
    if tray.is_invalid() {
        return;
    }
    let _ = SetWindowPos(
        tray,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    let _ = SetWindowPos(
        dock,
        tray,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

unsafe fn reveal_taskbar_for_start(hwnd: HWND, app: &mut App) {
    if !taskbar::should_reveal_for_start_invocation(app.settings.taskbar_mode)
        || SetTimer(hwnd, TIMER_TASKBAR_REHIDE, TIMER_TASKBAR_REHIDE_MS, None) == 0
    {
        return;
    }
    app.taskbar_invoked_at = Some(Instant::now());
    taskbar::reveal_for_start_invocation();
    place_dock_behind_taskbar(hwnd);
}

unsafe fn open_native_tray(hwnd: HWND, app: &mut App) {
    if native_tray::fullscreen_foreground_present()
        || app
            .native_tray_request
            .as_ref()
            .is_some_and(|request| !request.is_finished())
        || native_tray::overflow_visible()
    {
        return;
    }
    hide_running_preview(app);
    drawer::hide();
    control_center::hide();
    // The notification chevron remains callable while the taskbar is transparent.
    let foreground = GetForegroundWindow();
    match native_tray::request_open(hwnd, foreground) {
        Ok(request) => app.native_tray_request = Some(request),
        Err(error) => error_log::write("启动系统托盘操作失败", &error),
    }
}

/// Restore the dock to the top of the topmost band (its normal resting z-order).
unsafe fn raise_dock_topmost(dock: HWND) {
    let _ = SetWindowPos(
        dock,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

unsafe fn set_expanded(hwnd: HWND, app: &mut App, expand: bool) -> Result<()> {
    if app.expanded == expand {
        return Ok(());
    }
    let (x, y, w, h) = app.full;
    let z = dock_z_insert_after(app);
    if expand {
        SetWindowPos(hwnd, z, x, y, w, h, SWP_NOACTIVATE)
    } else {
        SetWindowPos(
            hwnd,
            z,
            x,
            y + h - app.strip_h,
            w,
            app.strip_h,
            SWP_NOACTIVATE,
        )
    }?;
    app.expanded = expand;
    Ok(())
}

/// Fully show or hide the dock window for fullscreen suppression — distinct from the
/// auto-hide *strip*. A hidden window is out of the z-order entirely, so it can neither
/// kick an exclusive-fullscreen game to the desktop nor be summoned by a bottom-edge
/// hover. On re-show we leave the geometry at full size and let `reveal` animate the
/// dock back in.
unsafe fn set_window_hidden(hwnd: HWND, app: &mut App, hide: bool) {
    if app.window_hidden == hide {
        return;
    }
    app.window_hidden = hide;
    let _ = ShowWindow(hwnd, if hide { SW_HIDE } else { SW_SHOWNOACTIVATE });
}

/// Geometry for the resting window region. The margin keeps the hard, aliased GDI
/// boundary outside Direct2D's stroke and antialiased pixels instead of cutting
/// dark notches into the two curved ends. `right` / `bottom` stay exclusive.
fn resting_region(pill: (f32, f32, f32, f32), radius: f32, dpi: f32) -> (i32, i32, i32, i32, i32) {
    let (l, t, r, b) = pill;
    let margin = (1.5 * dpi).ceil().max(2.0) as i32;
    let ellipse = (2.0 * (radius + margin as f32)).ceil() as i32;
    (
        l.floor() as i32 - margin,
        t.floor() as i32 - margin,
        r.ceil() as i32 + margin + 1,
        b.ceil() as i32 + margin + 1,
        ellipse,
    )
}

fn style_resting_bounds(dock: &Dock) -> ((f32, f32, f32, f32), f32) {
    let (l, t, r, b) = dock.frame().pill;
    let radius = dock.style.corner_radius(&dock.theme.visual()) * dock.dpi;
    let dpi = dock.dpi;
    match dock.style {
        theme::DockStyle::Pill | theme::DockStyle::Hairline => ((l, t, r, b), radius),
        theme::DockStyle::Shelf => ((l, t - 3.0 * dpi, r, b + radius), radius),
        theme::DockStyle::Islands | theme::DockStyle::Tiles | theme::DockStyle::Fluent => (
            (l - 3.0 * dpi, t - 2.0 * dpi, r + 3.0 * dpi, b + 5.0 * dpi),
            radius,
        ),
    }
}

/// At rest, clip to a slightly expanded pill so transparent head-room and side slack
/// remain click-through. Hovering / animating removes the clip for magnified icons.
/// `HTTRANSPARENT` alone does not pass these clicks through this composition window.
/// The region is only an input-shape approximation: the GPU material supplies the
/// exact antialiased curve, so the GDI boundary must not double as its visible edge.
unsafe fn set_region_full(hwnd: HWND, app: &mut App, full: bool) {
    if app.region_full == full {
        return;
    }
    if full {
        if SetWindowRgn(hwnd, HRGN::default(), BOOL(1)) != 0 {
            app.region_full = true;
        }
        return;
    }
    let (bounds, radius) = style_resting_bounds(&app.dock);
    let (l, t, r, b, ellipse) = resting_region(bounds, radius, app.dock.dpi);
    let rgn = CreateRoundRectRgn(l, t, r, b, ellipse, ellipse);
    // On success the system owns the region; on failure we still own it -> free it.
    if SetWindowRgn(hwnd, rgn, BOOL(1)) == 0 {
        let _ = DeleteObject(HGDIOBJ(rgn.0));
    } else {
        app.region_full = false;
    }
}

/// Rebuild the dock from a fresh scan of open windows (e.g. after a config edit).
unsafe fn reload_app(hwnd: HWND, app: &mut App) -> Result<()> {
    let running = windows_list::enumerate_sorted();
    reload_with(hwnd, app, &running)
}

/// Rebuild the dock from an already-enumerated window set. Reconciles the desired
/// items against the live ones in place, so surviving slots keep their animation
/// state and icon while new windows ease *in* and closed ones collapse *out* — no
/// wholesale rebuild, no snap. The window is sized to the merged row at its widest
/// (entering + exiting both counted) so nothing clips; the slack is reclaimed once
/// the exit animations settle (`relayout_to_fit`).
unsafe fn reload_with(
    hwnd: HWND,
    app: &mut App,
    running: &[windows_list::RunningWindow],
) -> Result<()> {
    reload_with_options(hwnd, app, running, false)
}

unsafe fn reload_with_options(
    hwnd: HWND,
    app: &mut App,
    running: &[windows_list::RunningWindow],
    preserve_agent_popup: bool,
) -> Result<()> {
    let open_agent = if preserve_agent_popup {
        window_preview::hide();
        agent_popup::active_agent()
    } else {
        hide_running_preview(app);
        None
    };
    // The pill width is about to change as slots ease in/out — drop any pill clip so the
    // reconcile animation isn't cut off; the settle re-clips to the new pill at rest.
    set_region_full(hwnd, app, true);
    let desired = compose_items(running, &app.settings, &app.agent_registry)?;
    let (dpi, _, _, budget, _) = monitor_layout(hwnd, &desired, app.settings)?;
    let (desired, overflow) = dock::limit_items(
        desired,
        budget,
        dpi,
        app.settings.layout,
        app.settings.effective_theme(),
    );
    app.overflow_items = overflow;
    let remap = app.dock.reconcile(desired);
    let (dpi, x, y, width, height) = monitor_layout(hwnd, &app.dock.items, app.settings)?;
    let requested_geometry = (x, y, width as i32, height as i32);
    let needs_window_layout = !app.expanded || dock_geometry_changed(app.full, requested_geometry);
    app.gpu.resize(width, height)?;
    app.gpu.remap_icons(&remap, &app.dock.items);
    // Update geometry on the (preserved) dock; reveal stays put so a list change
    // never flashes the dock in or out — ease toward what the mode currently wants.
    app.dock.dpi = dpi;
    app.dock.win_w = width as f32;
    app.dock.win_h = height as f32;
    app.dock.reveal_target = resting_target(app);
    app.full = requested_geometry;
    app.strip_h = ((6.0 * dpi).round() as i32).max(4);
    app.expanded = true;
    app.pending_relayout = true;
    if needs_window_layout {
        let z = dock_z_insert_after(app);
        SetWindowPos(hwnd, z, x, y, width as i32, height as i32, SWP_NOACTIVATE)?;
    }
    app.gpu.render(&app.dock)?;
    record_window_state(app, running);
    if let Some(agent_id) = open_agent {
        if let Some(item) = app
            .dock
            .items
            .iter()
            .find(|it| it.agent.as_ref().is_some_and(|a| a.agent_id == agent_id))
        {
            let summary = app
                .agent_registry
                .summarize_agent(&agent_id, &item.label, &item.windows);
            agent_popup::refresh_if_open(hwnd, &summary, item.path.as_deref());
        } else {
            agent_popup::hide();
        }
    }
    sync_agent_shimmer_timer(hwnd, app);
    app.animating = true;
    Ok(())
}

/// Resize the window to exactly fit the current item row, recentred on the monitor.
/// Called once the appear/disappear animations settle: we hold the window wide while
/// slots collapse so they don't clip, then reclaim the slack here. No-op when already
/// the right size, so it's safe to call on every settle.
unsafe fn relayout_to_fit(hwnd: HWND, app: &mut App) -> Result<()> {
    let (dpi, x, y, width, height) = monitor_layout(hwnd, &app.dock.items, app.settings)?;
    if width as i32 == app.full.2 && height as i32 == app.full.3 {
        return Ok(());
    }
    app.gpu.resize(width, height)?;
    app.dock.dpi = dpi;
    app.dock.win_w = width as f32;
    app.dock.win_h = height as f32;
    app.full = (x, y, width as i32, height as i32);
    app.strip_h = ((6.0 * dpi).round() as i32).max(4);
    let z = dock_z_insert_after(app);
    SetWindowPos(hwnd, z, x, y, width as i32, height as i32, SWP_NOACTIVATE)?;
    app.gpu.render(&app.dock)?;
    Ok(())
}

unsafe fn recover_gpu(hwnd: HWND, app: &mut App) -> Result<()> {
    drawer::shutdown();
    control_center::shutdown();
    agent_popup::shutdown();
    hide_running_preview(app);
    drawer::invalidate_icon_cache();
    app.gpu.release_window_target();
    let (_, _, width, height) = app.full;
    let mut gpu = Gpu::new(hwnd, width as u32, height as u32, app.dock.dpi)?;
    gpu.load_icons(&app.dock.items, app.dock.dpi);
    gpu.render(&app.dock)?;
    app.gpu = gpu;
    Ok(())
}

fn main() {
    // Watchdog mode: a tiny sibling copy of ourselves that restores the taskbar if the
    // dock dies abnormally. Handle it before any GUI / single-instance / COM setup.
    if let Some(args) = watchdog::parse_args() {
        watchdog::run(args);
        return;
    }
    // Drawer-icon helper mode: a sibling copy that performs the drawer's shell icon
    // extraction in isolation, so a third-party shell-extension crash can never take
    // the dock down. Also handled before any GUI / single-instance setup.
    if let Some((job, outdir)) = drawer_icons::parse_args() {
        drawer_icons::run(&job, &outdir);
        return;
    }
    if let Some(code) = unsafe { agent_state::handle_cli_agent_event() } {
        std::process::exit(code);
    }
    if handle_control_args() {
        return;
    }
    if let Err(error) = run() {
        unsafe {
            show_error(HWND::default(), "FeatherDock 启动失败", error);
        }
    }
}

fn handle_control_args() -> bool {
    match std::env::args().nth(1).as_deref() {
        Some(QUIT_FLAG) => {
            unsafe { request_graceful_quit() };
            true
        }
        Some(SETTINGS_FLAG) => unsafe {
            let hwnd = dock_window();
            if hwnd.is_invalid() {
                false // Start the dock, then open settings once it is ready.
            } else {
                let _ = PostMessageW(hwnd, WM_OPEN_SETTINGS, WPARAM(0), LPARAM(0));
                true
            }
        },
        Some(RESTORE_SYSTEM_FLAG) => {
            unsafe { restore_stranded_system_state() };
            true
        }
        _ => false,
    }
}

unsafe fn acquire_instance_or_reveal_existing() -> Result<Option<single_instance::SingleInstance>> {
    if let Some(instance) = single_instance::SingleInstance::acquire()? {
        return Ok(Some(instance));
    }
    for _ in 0..20 {
        if reveal_existing_instance() {
            return Ok(None);
        }
        // The other instance may still be creating its window. Never guess that it
        // is orphaned and kill every process with the same image name.
        if let Some(instance) = single_instance::SingleInstance::acquire()? {
            return Ok(Some(instance));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    show_existing_instance_unavailable();
    Ok(None)
}

unsafe fn dock_window() -> HWND {
    FindWindowW(w!("FeatherDockWindow"), PCWSTR::null()).unwrap_or_default()
}

unsafe fn reveal_existing_instance() -> bool {
    let hwnd = dock_window();
    if hwnd.is_invalid() {
        return false;
    }
    let _ = PostMessageW(hwnd, WM_SHOW_EXISTING, WPARAM(0), LPARAM(0));
    true
}

unsafe fn show_existing_instance_unavailable() {
    let text: Vec<u16> = "检测到另一个 FeatherDock 实例，但暂时无法连接它的主窗口。它可能仍在启动或暂时无响应。\n\n请稍后再试。如需恢复任务栏或桌面图标，请显式运行 FeatherDock.exe --restore-system。"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = MessageBoxW(
        HWND::default(),
        PCWSTR(text.as_ptr()),
        w!("FeatherDock"),
        MB_OK | MB_ICONWARNING,
    );
}

unsafe fn request_graceful_quit() {
    let hwnd = dock_window();
    if hwnd.is_invalid() {
        restore_stranded_system_state();
        return;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let process = if pid == 0 {
        None
    } else {
        OpenProcess(PROCESS_SYNCHRONIZE, BOOL(0), pid).ok()
    };
    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
    if let Some(process) = process {
        if !process.is_invalid() {
            WaitForSingleObject(process, QUIT_WAIT_MS);
            let _ = CloseHandle(process);
            return;
        }
    }
    for _ in 0..50 {
        if !IsWindow(hwnd).as_bool() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

unsafe fn restore_stranded_system_state() {
    taskbar::restore_recorded_or_visible();
    desktop_icons::set_hidden(false);
}

/// Install a panic hook that restores the taskbar before the default hook runs (and, in
/// release builds, the process aborts without unwinding). Without this, a panic after we
/// hid the taskbar would leave the user with no visible taskbar.
fn install_taskbar_panic_guard() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        unsafe { taskbar::restore() };
        previous(info);
    }));
}

fn desired_watchdog_plan(app: &App) -> Option<WatchdogPlan> {
    let plan = WatchdogPlan {
        restore_taskbar: app.settings.taskbar_mode != settings::TaskbarMode::Show,
        restore_desktop_icons: app.settings.hide_desktop_icons,
    };
    plan.needed().then_some(plan)
}

/// Keep the abnormal-exit watchdog aligned with the system state FeatherDock currently
/// owns. Rebuild it when settings change so a stale helper never misses a new side effect
/// such as desktop-icon hiding, and stop it once there is nothing left to recover.
unsafe fn reconcile_watchdog(app: &mut App) {
    let desired = desired_watchdog_plan(app);
    if app.watchdog_plan == desired {
        if let Some(child) = app.watchdog.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                return;
            }
        }
        app.watchdog = None;
        app.watchdog_plan = None;
    }
    stop_watchdog(app);
    if let Some(plan) = desired {
        app.watchdog = watchdog::spawn(
            taskbar::original_autohide(),
            plan.restore_taskbar,
            plan.restore_desktop_icons,
        );
        if app.watchdog.is_some() {
            app.watchdog_plan = Some(plan);
        }
    }
}

unsafe fn stop_watchdog(app: &mut App) {
    if let Some(mut watchdog) = app.watchdog.take() {
        let _ = watchdog.kill();
        let _ = watchdog.wait();
    }
    app.watchdog_plan = None;
}

fn run() -> Result<()> {
    unsafe {
        let Some(instance) = acquire_instance_or_reveal_existing()? else {
            return Ok(());
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
        let large_icon = app_icon::load();
        let small_icon = app_icon::load();
        let class_name = w!("FeatherDockWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hIcon: large_icon,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIconSm: small_icon,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(Error::from_win32());
        }

        let running = windows_list::enumerate_sorted();
        let dock_settings = settings::load();
        let mut agent_registry = agent_state::AgentRegistry::default();
        agent_registry.set_history_window(dock_settings.agent_history_window_secs());
        let restored_dismissals = match agent_state::load_session_dismissals() {
            Ok(records) => records,
            Err(error) => {
                show_error(
                    HWND::default(),
                    "读取会话清除标记失败，本次启动已中止以避免旧会话复活",
                    &error,
                );
                return Err(Error::new(
                    HRESULT_CANCELLED,
                    "session dismissal restore failed",
                ));
            }
        };
        agent_registry.restore_dismissals(restored_dismissals);
        let session_dir = agent_state::sessions_dir();
        let _ = std::fs::create_dir_all(&session_dir);
        let initial_snapshot =
            agent_state::load_sessions_preserving(&session_dir).unwrap_or_default();
        let initial_sessions =
            sanitize_incoming_sessions(initial_snapshot.1.clone(), Some(&running), &[]);
        let init_fg = Some(GetForegroundWindow().0 as isize);
        let init_fg_agents = foreground_agents_for(init_fg, &[]);
        let (_, _, initial_requests) = agent_registry.replace_file_sessions_with_meta(
            initial_sessions,
            init_fg,
            &init_fg_agents,
        );
        let items = compose_items(&running, &dock_settings, &agent_registry)?;
        // Anchor to the primary monitor's bottom-centre using that monitor's own DPI and
        // bounds (see `primary_monitor_layout`) so the auto-hide reveal trigger sits at the
        // very edge and the first frame is already sized for the right monitor.
        let (dpi, _, _, budget, _) = primary_monitor_layout(&items, dock_settings)?;
        let (items, overflow_items) = dock::limit_items(
            items,
            budget,
            dpi,
            dock_settings.layout,
            dock_settings.effective_theme(),
        );
        let (dpi, x, y, win_w, win_h) = primary_monitor_layout(&items, dock_settings)?;

        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("FeatherDock"),
            WS_POPUP,
            x,
            y,
            win_w as i32,
            win_h as i32,
            None,
            None,
            hinstance,
            None,
        )?;

        let mut agent_pids = agent_state::PidMonitor::default();
        let mut pruned_initial_pid = false;
        for request in initial_requests {
            let pid = request.pid;
            match agent_pids.ensure_watching(hwnd, pid) {
                agent_state::PidWatchResult::Watching {
                    proc_start,
                    generation,
                } => {
                    agent_registry.bind_pid_watch(&request, proc_start, generation);
                }
                agent_state::PidWatchResult::Exited => {
                    if agent_registry.prune_pid_with_start(pid, None) {
                        pruned_initial_pid = true;
                    }
                }
                agent_state::PidWatchResult::AccessDenied => {}
            }
        }
        let (items, overflow_items, dpi, x, y, win_w, win_h) = if pruned_initial_pid {
            let fresh = compose_items(&running, &dock_settings, &agent_registry)?;
            let (d, _, _, b, _) = primary_monitor_layout(&fresh, dock_settings)?;
            let (vis, ov) = dock::limit_items(
                fresh,
                b,
                d,
                dock_settings.layout,
                dock_settings.effective_theme(),
            );
            let (d, nx, ny, nw, nh) = primary_monitor_layout(&vis, dock_settings)?;
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                nx,
                ny,
                nw as i32,
                nh as i32,
                SWP_NOACTIVATE,
            );
            (vis, ov, d, nx, ny, nw, nh)
        } else {
            (items, overflow_items, dpi, x, y, win_w, win_h)
        };

        let mut gpu = Gpu::new(hwnd, win_w, win_h, dpi)?;
        let mut dock = Dock::new(
            items,
            dpi,
            win_w as f32,
            win_h as f32,
            dock_settings.effective_theme(),
        );
        dock.preferences = dock_settings.layout;
        dock.style = dock_settings.dock_style;
        dock.foreground = Some(GetForegroundWindow().0 as isize);
        gpu.load_icons(&dock.items, dpi);
        let titles = window_titles(&running);
        let title_matches = title_group_matches(&dock.items, &running);
        let tray = Tray::new(hwnd);
        // Track open windows event-driven (no polling) so the right zone stays live.
        let hooks = windows_list::install_hooks(hwnd);
        let agent_watcher = agent_state::SessionDirWatcher::start(hwnd, initial_snapshot);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let fullscreen_active = windows_list::is_fullscreen_present(monitor);
        let maximized_active = windows_list::is_maximized_present(monitor);
        // If a previous run died without restoring the taskbar (power loss, or both the
        // dock and its watchdog killed at once), put it back BEFORE we read the "original"
        // state — otherwise we'd capture the stranded, invisible bar as the new baseline.
        taskbar::recover_if_stranded();
        // Capture the user's taskbar auto-hide preference before we touch it, then
        // apply the saved mode (so a "hidden" choice persists across launches).
        taskbar::capture_original();
        // A panic (release builds abort without unwinding) must still put the bar back.
        install_taskbar_panic_guard();
        taskbar::apply(dock_settings.taskbar_mode);
        let app_ptr = Box::into_raw(Box::new(App {
            _instance: instance,
            gpu,
            dock,
            overflow_items,
            drag: None,
            shutting_down: false,
            preview_candidate: None,
            overlap_active: false,
            tray,
            hooks,
            running_sig: running_sig(&running),
            window_titles: titles,
            title_matches,
            agent_registry,
            agent_watcher,
            agent_pids,
            shimmer_timer_mode: ShimmerTimerMode::Off,
            shimmer_sweep_start: Instant::now(),
            settings: dock_settings,
            saved_settings: dock_settings,
            fullscreen_active,
            maximized_active,
            animating: true, // ease to the resting state (resident or hidden)
            tracking: false,
            full: (x, y, win_w as i32, win_h as i32),
            strip_h: ((6.0 * dpi).round() as i32).max(4),
            expanded: true,
            window_hidden: false,
            region_full: true,
            pending_relayout: false,
            pending_rebuild: false,
            taskbar_invoked_at: None,
            native_tray_request: None,
            watchdog: None,
            watchdog_plan: None,
        }));
        let _app_guard = AppGuard(app_ptr);
        // Ease toward the resting state: resident at the bottom by default, or
        // hidden if in auto-hide mode / a fullscreen app is already in front.
        (*app_ptr).dock.reveal_target = resting_target(&*app_ptr);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
        sync_agent_shimmer_timer(hwnd, &mut *app_ptr);
        // Arm the external safety net if we modified system state: a sibling process that
        // restores it should we die abnormally (Task-Manager kill, crash, panic).
        reconcile_watchdog(&mut *app_ptr);
        // If we start hidden, grow any pre-maximized windows into the freed work area
        // (taskbar::apply already made the bar transparent + auto-hide above).
        if (*app_ptr).settings.taskbar_mode == settings::TaskbarMode::Hidden {
            taskbar::reflow_maximized_windows();
        }
        // Apply the saved "hide desktop icons" choice (restored on exit in `cleanup`).
        if (*app_ptr).settings.hide_desktop_icons {
            desktop_icons::set_hidden(true);
        }
        DragAcceptFiles(hwnd, BOOL(1)); // files, folders, shortcuts, and applications
        configure_search_hotkey(hwnd, dock_settings.search_hotkey);

        (*app_ptr).gpu.render(&(*app_ptr).dock)?;
        // Don't flash the dock on screen if a fullscreen app is already in front
        // (e.g. the dock was relaunched while a game is running) — stay hidden.
        if fullscreen_suppressed(&*app_ptr) {
            (*app_ptr).window_hidden = true;
        } else {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        desktop_scan::refresh_async(hwnd, false);
        if std::env::args().nth(1).as_deref() == Some(SETTINGS_FLAG) {
            let _ = PostMessageW(hwnd, WM_OPEN_SETTINGS, WPARAM(0), LPARAM(0));
        }

        // Event-driven loop: block in GetMessage when idle (0% CPU); when
        // animating, drain input then render one vsync-paced frame.
        let mut msg = MSG::default();
        loop {
            if (*app_ptr).animating {
                let mut dispatched = 0;
                while animation_message_batch_has_capacity(dispatched)
                    && PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool()
                {
                    if msg.message == WM_QUIT {
                        return Ok(());
                    }
                    if preferences::route_dialog(&msg) {
                        dispatched += 1;
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                    dispatched += 1;
                }
                if !(*app_ptr).animating {
                    continue;
                }
                // Grow back to full size whenever any part of the dock should show —
                // covers reveals not driven by the mouse (fullscreen exit, mode
                // toggle), not just WM_MOUSEMOVE.
                if !(*app_ptr).expanded
                    && ((*app_ptr).dock.reveal > 0.01 || (*app_ptr).dock.reveal_target > 0.01)
                {
                    if let Err(error) = set_expanded(hwnd, &mut *app_ptr, true) {
                        show_error(hwnd, "展开 Dock 窗口失败", error);
                    }
                }
                let moving = (*app_ptr).dock.tick();
                // Drop slots that finished collapsing, keeping GPU icons in lockstep.
                let removed = (*app_ptr).dock.take_finished_exits();
                if !removed.is_empty() {
                    (*app_ptr).gpu.drop_icons(&removed);
                }
                let icons_loading = (*app_ptr).gpu.decode_pending_icons(&(*app_ptr).dock.items);
                // Popup surfaces submit without their own vsync wait. The Dock's swapchain
                // is the one frame clock, so all three surfaces advance exactly once per
                // display refresh instead of serially blocking the UI thread.
                let drawer_moving = drawer::animate_frame();
                let control_moving = control_center::animate_frame();
                if let Err(render_error) = (*app_ptr).gpu.render(&(*app_ptr).dock) {
                    recover_gpu(hwnd, &mut *app_ptr).map_err(|recovery_error| {
                        Error::new(
                            recovery_error.code(),
                            format!(
                                "渲染失败且无法恢复：{render_error}; recovery: {recovery_error}"
                            ),
                        )
                    })?;
                }
                // Any future mouse move wakes the message loop and requests another
                // frame. Once all easing settles, stop presenting duplicate frames even
                // when the pointer is resting over the Dock. Background icon extraction
                // wakes this loop only when a result is ready; queued decodes stay bounded.
                if !animation_sources_active(moving, drawer_moving, control_moving)
                    && !icons_loading
                {
                    // Exit animations are done — reclaim the width we held for the
                    // collapsing slots, shrinking the window back to fit the row.
                    if (*app_ptr).pending_relayout {
                        (*app_ptr).pending_relayout = false;
                        if let Err(error) = relayout_to_fit(hwnd, &mut *app_ptr) {
                            show_error(hwnd, "重新布局 Dock 失败", error);
                        }
                    }
                    // Eased fully out. If a fullscreen app owns the screen, hide the
                    // window ENTIRELY (no strip, no z-order presence) so it can't disturb
                    // an exclusive-fullscreen game; otherwise collapse to the reveal strip
                    // so the area it covered becomes click-through. Then go idle.
                    if (*app_ptr).dock.reveal <= 0.01 {
                        if fullscreen_suppressed(&*app_ptr) {
                            set_window_hidden(hwnd, &mut *app_ptr, true);
                        } else if let Err(error) = set_expanded(hwnd, &mut *app_ptr, false) {
                            show_error(hwnd, "收起 Dock 窗口失败", error);
                        }
                        // Strip is a thin sliver and a hidden window has no surface — no
                        // head-room to clip, so the whole (tiny/absent) window takes input.
                        set_region_full(hwnd, &mut *app_ptr, true);
                    } else {
                        // Keep the magnification envelope interactive while hovered even
                        // though identical frames no longer need presenting. Once the
                        // pointer leaves, clip back to the visible pill at rest.
                        set_region_full(
                            hwnd,
                            &mut *app_ptr,
                            settled_region_should_stay_full((*app_ptr).dock.cursor_x.is_some()),
                        );
                    }
                    (*app_ptr).animating = false;
                    sync_agent_shimmer_timer(hwnd, &mut *app_ptr);
                }
            } else {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if r.0 <= 0 {
                    break;
                }
                if preferences::route_dialog(&msg) {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if (*app_ptr).animating {
                    (*app_ptr).dock.wake_animation_clock();
                }
            }
        }
    }
    Ok(())
}

unsafe fn cleanup(app_ptr: *mut App) {
    if !app_ptr.is_null() {
        // Stop the watchdog first (while it's still blocked on us) so it can't also fire
        // a redundant restore, then put the taskbar back ourselves. Never strand the user.
        stop_watchdog(&mut *app_ptr);
        windows_list::remove_hooks(std::mem::take(&mut (*app_ptr).hooks));
        // Dispatcher shutdown still pumps native messages. Destroy popups and
        // detach the App pointer first so no queued callback can use freed state.
        drawer::shutdown();
        control_center::shutdown();
        agent_popup::shutdown();
        hide_running_preview(&mut *app_ptr);
        (*app_ptr).agent_pids.clear();
        (*app_ptr).shutting_down = true;
        preferences::close();
        command_palette::close();
        settings_window::close();
        let hwnd = dock_window();
        if !hwnd.is_invalid() && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == app_ptr as isize {
            let _ = KillTimer(hwnd, TIMER_AGENT_SHIMMER);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(hwnd);
        }
        taskbar::restore();
        // Never leave the user with a blank desktop: put the icons back if we hid them.
        if (*app_ptr).settings.hide_desktop_icons {
            desktop_icons::set_hidden(false);
        }
        (*app_ptr).tray.remove();
        drawer_icons::cleanup(); // per-dock icon cache temp dir
        drop(Box::from_raw(app_ptr));
    }
    backdrop::shutdown();
}

/// (Re-)enter the hidden resting state: make the taskbar transparent + auto-hide
/// (freeing its row) and grow any maximized windows into the freed work area. Used on
/// the deliberate hidden-entry paths (Explorer restart, settings switch).
unsafe fn enter_hidden(_hwnd: HWND, _app: &mut App) {
    taskbar::apply(settings::TaskbarMode::Hidden);
    taskbar::reflow_maximized_windows();
}

fn should_launch_new_instance(kind: content::ContentKind, shift: bool, has_path: bool) -> bool {
    shift
        && has_path
        && matches!(
            kind,
            content::ContentKind::Application | content::ContentKind::Shortcut
        )
}

/// Open an application, shortcut, file, or folder with the Windows Shell.
unsafe fn open_content(path: &str) -> Result<()> {
    let launch_path = system_launcher_for_path(path).unwrap_or(path);
    match open_content_raw(launch_path) {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(repaired) = config::repair_launch_path(path) {
                open_content_raw(&repaired)
            } else {
                Err(error)
            }
        }
    }
}

fn system_launcher_for_path(path: &str) -> Option<&'static str> {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    if normalized.contains("\\windowsapps\\openai.codex_")
        && normalized.ends_with("\\app\\codex.exe")
    {
        Some("shell:AppsFolder\\OpenAI.Codex_2p2nqsd0c76g0!App")
    } else {
        None
    }
}

unsafe fn open_content_raw(path: &str) -> Result<()> {
    let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let result = ShellExecuteW(
        HWND::default(),
        w!("open"),
        PCWSTR(w.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
    if result.0 as isize <= 32 {
        Err(Error::new(
            E_FAIL,
            format!(
                "Windows 无法打开此内容（ShellExecute={}）",
                result.0 as isize
            ),
        ))
    } else {
        Ok(())
    }
}

pub(crate) unsafe fn pick_content(owner: HWND, folder: bool) -> Result<Option<String>> {
    let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
    let mut options = dialog.GetOptions()?;
    options |= FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
    if folder {
        options |= FOS_PICKFOLDERS;
        dialog.SetTitle(w!("添加文件夹到 FeatherDock"))?;
    } else {
        options |= FOS_FILEMUSTEXIST;
        dialog.SetTitle(w!("添加文件或应用到 FeatherDock"))?;
    }
    dialog.SetOptions(options)?;
    if let Err(error) = dialog.Show(owner) {
        return if error.code() == HRESULT_CANCELLED {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let item = dialog.GetResult()?;
    let raw = item.GetDisplayName(SIGDN_FILESYSPATH)?;
    let path = raw.to_string()?;
    CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void));
    Ok(Some(path))
}

unsafe fn add_content(hwnd: HWND, app: &mut App, path: &str) -> Result<bool> {
    let paths = [path.to_string()];
    Ok(add_contents(hwnd, app, &paths)? > 0)
}

unsafe fn add_contents(hwnd: HWND, app: &mut App, paths: &[String]) -> Result<usize> {
    let mut added_count = 0;
    for path in paths {
        let path_ref = std::path::Path::new(path);
        if !path_ref.exists() {
            return Err(Error::new(E_FAIL, format!("路径不存在：{path}")));
        }
        let label = path_ref
            .file_stem()
            .or_else(|| path_ref.file_name())
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Item".to_string());
        if config::add_item(&label, path).map_err(|error| io_error("保存 Dock 内容失败", error))?
        {
            added_count += 1;
        }
    }
    if added_count > 0 {
        reload_app(hwnd, app)?;
    }
    Ok(added_count)
}

unsafe fn append_item(menu: HMENU, id: usize, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide.as_ptr()));
}

/// Right-click on a running window: window actions, plus pinning it to the dock.
/// `windows` are all open windows in this slot's app group (a running icon can stand
/// for several windows); per-window closing is also available from the hover preview.
unsafe fn show_window_menu(owner: HWND, app: &mut App, windows: &[isize]) {
    let Some(&primary) = windows.first() else {
        return;
    };
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let Ok(menu) = CreatePopupMenu() else { return };
    if windows.len() == 1 {
        append_item(menu, ID_WIN_MAXIMIZE, "最大化 / 还原");
        append_item(menu, ID_WIN_MINIMIZE, "最小化");
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, ID_WIN_PIN, "固定在 Dock");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let close_label = if windows.len() > 1 {
        "关闭全部窗口"
    } else {
        "关闭窗口"
    };
    append_item(menu, ID_WIN_CLOSE, close_label);
    let _ = SetForegroundWindow(owner); // so the menu dismisses on click-away
    let menu_style = menu_style::style(menu, app.dock.theme, owner);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        owner,
        None,
    );
    drop(menu_style);
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        ID_WIN_CLOSE => windows.iter().for_each(|&w| windows_list::close_window(w)),
        ID_WIN_MINIMIZE => windows_list::minimize_window(primary),
        ID_WIN_MAXIMIZE => windows_list::toggle_maximize(primary),
        ID_WIN_PIN => pin_running_window(owner, app, primary),
        _ => {}
    }
}

unsafe fn pin_running_window(owner: HWND, app: &mut App, target: isize) {
    let Some(path) = windows_list::process_exe_path_for_window(target) else {
        show_error(
            owner,
            "固定运行程序失败",
            Error::new(E_FAIL, "无法读取此窗口的程序路径"),
        );
        return;
    };
    if let Err(error) = add_content(owner, app, &path) {
        show_error(owner, "固定运行程序失败", error);
    }
}

/// Right-click on a pinned item: open it, reveal it in Explorer, or unpin it. When the
/// pinned app is running, its open windows (`windows`) also get window actions —
/// minimize / maximize / close — so a launched pinned app can be closed from the dock.
unsafe fn show_pinned_menu(owner: HWND, app: &mut App, path: &str, windows: &[isize]) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let Ok(menu) = CreatePopupMenu() else { return };
    append_item(
        menu,
        ID_PIN_OPEN,
        if windows.is_empty() {
            "打开"
        } else {
            "打开新实例"
        },
    );
    append_item(menu, ID_PIN_LOCATION, "打开文件所在位置");
    if !windows.is_empty() {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if windows.len() == 1 {
            append_item(menu, ID_WIN_MAXIMIZE, "最大化 / 还原");
            append_item(menu, ID_WIN_MINIMIZE, "最小化");
            append_item(menu, ID_WIN_CLOSE, "关闭窗口");
        } else {
            append_item(menu, ID_WIN_CLOSE, "关闭全部窗口");
        }
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, ID_PIN_REMOVE, "从 Dock 移除");
    let _ = SetForegroundWindow(owner);
    let menu_style = menu_style::style(menu, app.dock.theme, owner);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        owner,
        None,
    );
    drop(menu_style);
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        ID_PIN_OPEN => {
            if let Err(error) = open_content(path) {
                show_error(owner, "打开内容失败", error);
            }
        }
        ID_PIN_LOCATION => reveal_in_explorer(path),
        ID_WIN_CLOSE => windows.iter().for_each(|&w| windows_list::close_window(w)),
        ID_WIN_MINIMIZE => {
            if let Some(&w) = windows.first() {
                windows_list::minimize_window(w);
            }
        }
        ID_WIN_MAXIMIZE => {
            if let Some(&w) = windows.first() {
                windows_list::toggle_maximize(w);
            }
        }
        ID_PIN_REMOVE => match config::remove_item(path) {
            Ok(true) => {
                if let Err(error) = reload_app(owner, app) {
                    show_error(owner, "移除后刷新失败", error);
                }
            }
            Ok(false) => {}
            Err(error) => show_error(owner, "从配置移除失败", io_error("移除内容失败", error)),
        },
        _ => {}
    }
}

/// Open Explorer with the file selected.
unsafe fn reveal_in_explorer(path: &str) {
    let file: Vec<u16> = "explorer.exe"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args: Vec<u16> = format!("/select,\"{path}\"")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = ShellExecuteW(
        HWND::default(),
        w!("open"),
        PCWSTR(file.as_ptr()),
        PCWSTR(args.as_ptr()),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn open_task_manager() -> Result<()> {
    let mut directory = [0; MAX_PATH as usize];
    let length = GetSystemDirectoryW(Some(&mut directory)) as usize;
    if length == 0 {
        return Err(Error::from_win32());
    }
    if length >= directory.len() {
        return Err(Error::new(E_FAIL, "Windows 系统目录路径超出长度限制"));
    }
    let directory = String::from_utf16_lossy(&directory[..length]);
    open_content_raw(&format!("{directory}\\Taskmgr.exe"))
}

/// Run a command from the Dock Start button or notification icon menu.
unsafe fn run_tray_menu(hwnd: HWND, app: &mut App, source: tray::MenuSource) {
    let always = app.settings.dock_mode == settings::DockMode::Always;
    match app.tray.show_menu(hwnd, always, source) {
        Some(tray::ID_TASK_MANAGER) if !native_tray::fullscreen_foreground_present() => {
            if let Err(error) = open_task_manager() {
                show_error(hwnd, "打开任务管理器失败", error);
            }
        }
        Some(tray::ID_EXIT) => PostQuitMessage(0),
        Some(tray::ID_AUTOSTART) => {
            if let Err(error) = autostart::set(!autostart::is_enabled()) {
                show_error(hwnd, "更新开机自启失败", error);
            }
        }
        Some(tray::ID_ADD) => match pick_content(hwnd, false) {
            Ok(Some(path)) => {
                if let Err(error) = add_content(hwnd, app, &path) {
                    show_error(hwnd, "添加内容失败", error);
                }
            }
            Ok(None) => {}
            Err(error) => show_error(hwnd, "打开文件选择器失败", error),
        },
        Some(tray::ID_ADD_FOLDER) => match pick_content(hwnd, true) {
            Ok(Some(path)) => {
                if let Err(error) = add_content(hwnd, app, &path) {
                    show_error(hwnd, "添加文件夹失败", error);
                }
            }
            Ok(None) => {}
            Err(error) => show_error(hwnd, "打开文件夹选择器失败", error),
        },
        Some(tray::ID_SETTINGS) => settings_window::open(hwnd),
        Some(tray::ID_TOGGLE_DOCK_MODE) => {
            let mut saved = app.saved_settings;
            saved.dock_mode = match saved.dock_mode {
                settings::DockMode::Always => settings::DockMode::AutoHide,
                settings::DockMode::AutoHide => settings::DockMode::Always,
            };
            match settings::save(&saved) {
                Ok(()) => {
                    app.saved_settings = saved;
                    app.settings.dock_mode = saved.dock_mode;
                    let target = if app.dock.cursor_x.is_some() {
                        1.0
                    } else {
                        resting_target(app)
                    };
                    app.dock.reveal_target = target;
                    app.animating = true;
                }
                Err(error) => show_error(hwnd, "保存设置失败", io_error("保存设置", error)),
            }
        }
        _ => {}
    }
}

/// Screen-space anchor (center-x, top-y) just above dock item `i`'s icon, used to
/// pop a glass panel (control center / app drawer) centered over the button.
unsafe fn anchor_above(hwnd: HWND, app: &App, i: usize) -> (i32, i32) {
    let frame = app.dock.frame();
    let cx = frame
        .icons
        .iter()
        .find(|ic| ic.idx == i)
        .map(|ic| ic.cx)
        .unwrap_or(0.0);
    let top = frame.pill.1;
    let mut wr = RECT::default();
    let _ = GetWindowRect(hwnd, &mut wr);
    (wr.left + cx as i32, wr.top + top as i32)
}

unsafe fn hide_running_preview(app: &mut App) {
    app.preview_candidate = None;
    use crate::window_preview as preview;
    preview::hide();
    agent_popup::hide();
}

unsafe fn show_running_preview(hwnd: HWND, app: &mut App, index: usize) {
    let Some(item) = app.dock.items.get(index) else {
        hide_running_preview(app);
        return;
    };
    let is_agent_popup = app.settings.agent_zone_enabled && item.is_agent();
    // Any dock item with open windows (or any Agent item in the Agent Zone) previews them.
    if !is_agent_popup && item.windows.is_empty() {
        hide_running_preview(app);
        return;
    }
    let frame = app.dock.frame();
    let Some(icon) = frame.icons.iter().find(|icon| icon.idx == index) else {
        hide_running_preview(app);
        return;
    };
    let mut wr = RECT::default();
    if GetWindowRect(hwnd, &mut wr).is_err() {
        hide_running_preview(app);
        return;
    }
    let anchor_x = wr.left + icon.cx as i32;
    let anchor_y = wr.top + frame.pill.1 as i32;
    if let Some(agent) = item
        .agent
        .as_ref()
        .filter(|_| app.settings.agent_zone_enabled)
    {
        window_preview::hide();
        let summary =
            app.agent_registry
                .summarize_agent(&agent.agent_id, &item.label, &item.windows);
        agent_popup::show(
            hwnd,
            &summary,
            item.path.as_deref(),
            anchor_x,
            anchor_y,
            app.dock.dpi,
        );
        return;
    }
    agent_popup::hide();
    let key = item.group_key.as_deref().unwrap_or(item.label.as_str());
    window_preview::show(hwnd, key, &item.windows, anchor_x, anchor_y, app.dock.dpi);
}

unsafe fn foreground_agents_for(
    foreground: Option<isize>,
    items: &[dock::DockItem],
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(fg) = foreground.filter(|&h| h != 0) else {
        return out;
    };
    for item in items {
        if let Some(agent) = &item.agent {
            if foreground_window_belongs_to_item(item, fg)
                && !out.iter().any(|existing| existing == &agent.agent_id)
            {
                out.push(agent.agent_id.clone());
            }
        }
    }
    let fg_hwnd = HWND(fg as *mut core::ffi::c_void);
    let exe = windows_list::process_exe_path_for_window(fg);
    let aumid = app_identity::window_id(fg_hwnd);
    if let Some(id) = agent_state::infer_agent_from_exe_or_aumid(exe.as_deref(), aumid.as_deref()) {
        if !out.iter().any(|existing| existing == &id) {
            out.push(id);
        }
    }
    out
}

fn should_run_shimmer(app: &App) -> bool {
    app.settings.agent_zone_enabled
        && app.settings.agent_shimmer
        && !app.settings.layout.reduce_motion
        && !sysctl::battery_saver_active()
        && app.dock.has_running_agent()
        && !fullscreen_suppressed(app)
        && !app.window_hidden
        && app.dock.reveal > 0.05
}

fn should_bounce_agent(app: &App) -> bool {
    app.settings.agent_zone_enabled
        && app.settings.agent_bounce_waiting
        && !app.settings.layout.reduce_motion
        && !fullscreen_suppressed(app)
        && !app.window_hidden
}

unsafe fn sync_agent_shimmer_timer(hwnd: HWND, app: &mut App) {
    if !should_run_shimmer(app) {
        if app.shimmer_timer_mode != ShimmerTimerMode::Off {
            let _ = KillTimer(hwnd, TIMER_AGENT_SHIMMER);
            app.shimmer_timer_mode = ShimmerTimerMode::Off;
        }
        if app.dock.shimmer_phase.take().is_some() && !app.animating {
            let _ = app.gpu.render(&app.dock);
        }
        return;
    }
    if app.shimmer_timer_mode == ShimmerTimerMode::Off {
        app.shimmer_timer_mode = ShimmerTimerMode::RestWait;
        let _ = SetTimer(hwnd, TIMER_AGENT_SHIMMER, SHIMMER_REST_MS, None);
    }
}

unsafe fn advance_agent_shimmer(hwnd: HWND, app: &mut App) {
    if !should_run_shimmer(app) {
        sync_agent_shimmer_timer(hwnd, app);
        return;
    }
    match app.shimmer_timer_mode {
        ShimmerTimerMode::Off => {
            app.shimmer_timer_mode = ShimmerTimerMode::RestWait;
            let _ = SetTimer(hwnd, TIMER_AGENT_SHIMMER, SHIMMER_REST_MS, None);
        }
        ShimmerTimerMode::RestWait => {
            app.shimmer_sweep_start = Instant::now();
            app.shimmer_timer_mode = ShimmerTimerMode::SweepTick;
            app.dock.shimmer_phase = Some(0.0);
            let _ = SetTimer(hwnd, TIMER_AGENT_SHIMMER, SHIMMER_TICK_MS, None);
            if !app.animating {
                let _ = app.gpu.render(&app.dock);
            }
        }
        ShimmerTimerMode::SweepTick => {
            let progress =
                app.shimmer_sweep_start.elapsed().as_secs_f32() * 1000.0 / SHIMMER_SWEEP_MS;
            if progress >= 1.0 {
                app.dock.shimmer_phase = None;
                app.shimmer_timer_mode = ShimmerTimerMode::RestWait;
                let _ = SetTimer(hwnd, TIMER_AGENT_SHIMMER, SHIMMER_REST_MS, None);
            } else {
                app.dock.shimmer_phase = Some(progress);
            }
            if !app.animating {
                let _ = app.gpu.render(&app.dock);
            }
        }
    }
}

unsafe fn hwnd_matches_expected_process(
    target: HWND,
    expected_pid: Option<u32>,
    expected_proc_start: Option<u64>,
) -> bool {
    let mut actual_pid = 0u32;
    GetWindowThreadProcessId(target, Some(&mut actual_pid));
    if actual_pid == 0 || expected_pid.is_some_and(|expected| expected != actual_pid) {
        return false;
    }
    if let Some(expected_start) = expected_proc_start {
        if agent_state::process_creation_time_for_pid(actual_pid) != Some(expected_start) {
            return false;
        }
    }
    true
}

unsafe fn hwnd_is_trusted_agent(
    raw: isize,
    agent_id: &str,
    expected_pid: Option<u32>,
    expected_proc_start: Option<u64>,
    trusted_windows: Option<&[windows_list::RunningWindow]>,
    trusted_items: &[dock::DockItem],
) -> bool {
    if raw == 0 {
        return false;
    }
    let target = HWND(raw as *mut core::ffi::c_void);
    if !IsWindow(target).as_bool() {
        return false;
    }
    if !hwnd_matches_expected_process(target, expected_pid, expected_proc_start) {
        return false;
    }
    if trusted_windows.is_some_and(|list| {
        list.iter().any(|w| {
            w.hwnd == raw
                && agent_state::infer_agent_from_exe_or_aumid(
                    w.exe_path.as_deref(),
                    w.app_id.as_deref(),
                )
                .as_deref()
                    == Some(agent_id)
        })
    }) {
        return true;
    }
    let live_exe = windows_list::process_exe_path(target);
    let live_aumid = app_identity::window_id(target);
    if agent_state::infer_agent_from_exe_or_aumid(live_exe.as_deref(), live_aumid.as_deref())
        .as_deref()
        == Some(agent_id)
    {
        return true;
    }
    if let Some(item) = trusted_items.iter().find(|it| {
        it.agent.as_ref().is_some_and(|a| a.agent_id == agent_id)
            && it.windows.iter().any(|w| w.hwnd == raw)
    }) {
        if let (Some(item_path), Some(exe)) = (item.path.as_deref(), live_exe.as_deref()) {
            if apps::exe_matches(item_path, exe) {
                return true;
            }
        }
    }
    false
}

unsafe fn sanitize_incoming_sessions(
    mut sessions: Vec<(agent_state::AgentSession, agent_state::EventMeta)>,
    trusted_windows: Option<&[windows_list::RunningWindow]>,
    trusted_items: &[dock::DockItem],
) -> Vec<(agent_state::AgentSession, agent_state::EventMeta)> {
    for (session, _) in &mut sessions {
        if let Some(raw_hwnd) = session.hwnd {
            if !hwnd_is_trusted_agent(
                raw_hwnd,
                &session.agent,
                session.pid,
                None,
                trusted_windows,
                trusted_items,
            ) {
                session.hwnd = None;
            }
        }
    }
    sessions
}

unsafe fn watch_agent_pids(hwnd: HWND, app: &mut App, requests: Vec<agent_state::PidWatchRequest>) {
    for request in requests {
        let pid = request.pid;
        match app.agent_pids.ensure_watching(hwnd, pid) {
            agent_state::PidWatchResult::Watching {
                proc_start,
                generation,
            } => {
                app.agent_registry
                    .bind_pid_watch(&request, proc_start, generation);
            }
            agent_state::PidWatchResult::Exited => {
                app.agent_registry.prune_pid_with_start(pid, None);
            }
            agent_state::PidWatchResult::AccessDenied => {}
        }
    }
}

fn needs_agent_topology_reload(app: &App) -> bool {
    if !app.settings.agent_zone_enabled {
        return app.dock.items.iter().any(dock::DockItem::is_agent);
    }
    for agent_id in app.agent_registry.track_b_agent_ids() {
        let summary = app.agent_registry.summarize_agent(&agent_id, "", &[]);
        if summary.aggregate_state == agent_state::AgentState::Idle {
            continue;
        }
        let in_visible_dock = app
            .dock
            .items
            .iter()
            .any(|it| it.agent.as_ref().is_some_and(|a| a.agent_id == agent_id));
        if !in_visible_dock {
            return true;
        }
    }
    for item in &app.dock.items {
        if item.role == dock::ItemRole::Running {
            if let Some(agent) = &item.agent {
                if item.windows.is_empty() {
                    let summary = app.agent_registry.summarize_agent(&agent.agent_id, "", &[]);
                    if summary.aggregate_state == agent_state::AgentState::Idle {
                        return true;
                    }
                }
            }
        }
    }
    false
}

unsafe fn sync_agent_slots_in_place(hwnd: HWND, app: &mut App) {
    let open_agent = agent_popup::active_agent();
    let mut popup_refresh: Option<(agent_state::AgentDockSummary, Option<String>)> = None;

    for item in app
        .dock
        .items
        .iter_mut()
        .chain(app.overflow_items.iter_mut())
    {
        if let Some(agent_id) = item.agent.as_ref().map(|a| a.agent_id.clone()) {
            let summary = app
                .agent_registry
                .summarize_agent(&agent_id, &item.label, &item.windows);
            item.agent = Some(agent_slot_info_from_summary(&summary));
            if open_agent.as_deref() == Some(agent_id.as_str()) {
                popup_refresh = Some((summary, item.path.clone()));
            }
        }
    }
    app.dock.sync_active_indicators();
    app.animating = true;
    if let Some((summary, launch_path)) = popup_refresh {
        agent_popup::refresh_if_open(hwnd, &summary, launch_path.as_deref());
    }
    let _ = app.gpu.render(&app.dock);
}

unsafe fn refresh_agent_views(
    hwnd: HWND,
    app: &mut App,
    bounces: &[String],
    watch_requests: Vec<agent_state::PidWatchRequest>,
    running: Option<&[windows_list::RunningWindow]>,
) {
    watch_agent_pids(hwnd, app, watch_requests);
    if fullscreen_app_present(hwnd) {
        app.pending_rebuild = true;
        return;
    }
    if needs_agent_topology_reload(app) {
        let owned_running;
        let running_ref = match running {
            Some(slice) => slice,
            None => {
                owned_running = windows_list::enumerate_sorted();
                &owned_running
            }
        };
        if let Err(error) = reload_with_options(hwnd, app, running_ref, true) {
            show_error(hwnd, "更新 Agent 状态失败", error);
            return;
        }
    } else {
        sync_agent_slots_in_place(hwnd, app);
    }
    if should_bounce_agent(app) {
        for agent_id in bounces {
            app.dock.bump_key(&dock::ItemKey::Agent(agent_id.clone()));
            app.animating = true;
        }
    }
    sync_agent_shimmer_timer(hwnd, app);
}

unsafe fn activate_popup_session(hwnd: HWND, app: &mut App, act: agent_popup::PopupActivation) {
    if act.intent != agent_popup::PopupIntent::Activate {
        let Some(session_id) = act.session_id.as_deref() else {
            return;
        };
        let Some(expected) = act.expected_version.as_ref() else {
            return;
        };
        if app
            .agent_registry
            .session_version(&act.agent_id, session_id)
            .as_ref()
            != Some(expected)
        {
            // The card referred to an older version. Do not let a delayed click
            // acknowledge or dismiss a newer producer state with the same key.
            return;
        }
        match act.intent {
            agent_popup::PopupIntent::Acknowledge => {
                if app
                    .agent_registry
                    .acknowledge_session(&act.agent_id, session_id)
                {
                    refresh_agent_views(hwnd, app, &[], Vec::new(), None);
                }
            }
            agent_popup::PopupIntent::Dismiss => {
                let Some(record) = app
                    .agent_registry
                    .dismissal_record(&act.agent_id, session_id)
                else {
                    return;
                };
                if let Err(error) = agent_state::persist_session_dismissal(&record) {
                    show_error(hwnd, "保存会话清除标记失败", error);
                    return;
                }
                let (changed, released_pids) = app
                    .agent_registry
                    .dismiss_session(&act.agent_id, session_id);
                for pid in released_pids {
                    app.agent_pids.unwatch(pid);
                }
                if changed {
                    refresh_agent_views(hwnd, app, &[], Vec::new(), None);
                }
            }
            agent_popup::PopupIntent::Activate => unreachable!(),
        }
        return;
    }

    let changed = if let Some(sid) = act.session_id.as_deref() {
        app.agent_registry.mark_session_read(&act.agent_id, sid)
    } else {
        app.agent_registry.mark_agent_read(&act.agent_id)
    };
    let mut activated = false;
    if let Some(link) = act
        .deep_link
        .as_deref()
        .and_then(agent_state::validate_deep_link)
    {
        if open_content_raw(&link).is_ok() {
            activated = true;
        }
    }
    if !activated {
        if let Some(raw) = act.hwnd.filter(|&h| {
            hwnd_is_trusted_agent(h, &act.agent_id, act.pid, None, None, &app.dock.items)
        }) {
            activated = windows_list::activate_from_dock(raw);
        }
    }
    if !activated {
        if let Some(item) = app.dock.items.iter().find(|it| {
            it.agent
                .as_ref()
                .is_some_and(|a| a.agent_id == act.agent_id)
        }) {
            if activate_item_windows(item) {
                activated = true;
            }
        }
    }
    if !activated {
        if let Some(path) = act.launch_path.as_deref().or_else(|| {
            app.dock
                .items
                .iter()
                .find(|it| {
                    it.agent
                        .as_ref()
                        .is_some_and(|a| a.agent_id == act.agent_id)
                })
                .and_then(|it| it.path.as_deref())
        }) {
            match open_content(path) {
                Ok(()) => activated = true,
                Err(error) => show_error(hwnd, "启动 Agent 失败", error),
            }
        }
    }
    if activated {
        app.dock
            .bump_key(&dock::ItemKey::Agent(act.agent_id.clone()));
        app.animating = true;
    }
    if changed {
        refresh_agent_views(hwnd, app, &[], Vec::new(), None);
    }
}

unsafe fn focus_trusted_agent_session_or_item(
    item: &dock::DockItem,
    summary: &agent_state::AgentDockSummary,
    trusted_items: &[dock::DockItem],
) -> bool {
    if activate_item_windows(item) {
        return true;
    }
    summary
        .sessions
        .iter()
        .find_map(|s| {
            s.hwnd.filter(|&h| {
                hwnd_is_trusted_agent(h, &summary.agent_id, s.pid, None, None, trusted_items)
            })
        })
        .is_some_and(|raw| windows_list::activate_from_dock(raw))
}

unsafe fn execute_agent_click_for_item(
    hwnd: HWND,
    app: &mut App,
    item: &dock::DockItem,
    index: Option<usize>,
    shift: bool,
) {
    let Some(agent) = item.agent.as_ref() else {
        return;
    };
    let agent_id = agent.agent_id.clone();
    let summary = app
        .agent_registry
        .summarize_agent(&agent_id, &item.label, &item.windows);

    if shift {
        if let Some(path) = item.path.as_deref() {
            match open_content(path) {
                Ok(()) => {
                    if let Some(i) = index {
                        app.dock.bump(i);
                        app.animating = true;
                    }
                }
                Err(error) => show_error(hwnd, "打开新窗口失败", error),
            }
        }
        return;
    }

    match agent_state::select_agent_click_action(&summary) {
        agent_state::AgentClickAction::JumpSession(session_idx) => {
            if let Some(session) = summary.sessions.get(session_idx) {
                hide_running_preview(app);
                activate_popup_session(
                    hwnd,
                    app,
                    agent_popup::PopupActivation {
                        agent_id,
                        session_id: Some(session.session_id.clone()),
                        hwnd: session.hwnd,
                        pid: session.pid,
                        deep_link: session.deep_link.clone(),
                        launch_path: item.path.clone(),
                        intent: agent_popup::PopupIntent::Activate,
                        expected_version: None,
                    },
                );
            }
        }
        agent_state::AgentClickAction::TogglePopup => {
            if let Some(slot_idx) = index {
                if agent_popup::active_agent().as_deref() == Some(agent_id.as_str()) {
                    let focused =
                        focus_trusted_agent_session_or_item(item, &summary, &app.dock.items);
                    hide_running_preview(app);
                    let cleared = if focused {
                        app.agent_registry.mark_agent_read(&agent_id)
                    } else {
                        false
                    };
                    if focused {
                        app.dock.bump(slot_idx);
                        app.animating = true;
                    }
                    if cleared {
                        refresh_agent_views(hwnd, app, &[], Vec::new(), None);
                    }
                } else if let Some(idx) = app
                    .dock
                    .items
                    .iter()
                    .position(|it| it.agent.as_ref().is_some_and(|a| a.agent_id == agent_id))
                {
                    show_running_preview(hwnd, app, idx);
                }
            } else {
                let cleared = app.agent_registry.mark_agent_read(&agent_id);
                let focused = focus_trusted_agent_session_or_item(item, &summary, &app.dock.items);
                if !focused {
                    if let Some(path) = item.path.as_deref() {
                        if let Err(error) = open_content(path) {
                            show_error(hwnd, "打开内容失败", error);
                        }
                    }
                }
                if cleared {
                    refresh_agent_views(hwnd, app, &[], Vec::new(), None);
                }
            }
        }
        agent_state::AgentClickAction::FocusOrLaunch => {
            let cleared = app.agent_registry.mark_agent_read(&agent_id);
            hide_running_preview(app);
            if activate_item_windows(item) {
                if let Some(i) = index {
                    app.dock.bump(i);
                    app.animating = true;
                }
            } else if index.is_some_and(|i| match item.role {
                dock::ItemRole::Pinned => activate_live_pinned_window(hwnd, app, i),
                dock::ItemRole::Running => activate_live_running_window(hwnd, app, i),
                _ => false,
            }) {
            } else if let Some(path) = item.path.as_deref() {
                match open_content(path) {
                    Ok(()) => {
                        if let Some(i) = index {
                            app.dock.bump(i);
                            app.animating = true;
                        }
                    }
                    Err(error) => show_error(hwnd, "打开内容失败", error),
                }
            }
            if cleared {
                refresh_agent_views(hwnd, app, &[], Vec::new(), None);
            }
        }
    }
}

unsafe fn handle_agent_left_click(hwnd: HWND, app: &mut App, index: usize, shift: bool) {
    let Some(item) = app.dock.items.get(index).cloned() else {
        return;
    };
    execute_agent_click_for_item(hwnd, app, &item, Some(index), shift);
}

#[cfg(test)]
fn activation_target(item: &dock::DockItem) -> Option<isize> {
    activation_order_with_foreground(item, None)
        .into_iter()
        .next()
}

#[cfg(test)]
fn activation_order_with_foreground(
    item: &dock::DockItem,
    foreground: Option<isize>,
) -> Vec<isize> {
    activation_order_with_foreground_owned(item, foreground, false)
}

fn activation_order_with_foreground_owned(
    item: &dock::DockItem,
    foreground: Option<isize>,
    foreground_owned_by_item: bool,
) -> Vec<isize> {
    let label = item.label.trim();
    let mut order = Vec::new();
    let mut push = |raw: isize| {
        if !order.contains(&raw) {
            order.push(raw);
        }
    };

    if let Some(raw) = foreground {
        if foreground_owned_by_item
            || item.hwnd == Some(raw)
            || item.windows.iter().any(|window| window.hwnd == raw)
        {
            push(raw);
        }
    }
    for window in &item.windows {
        if title_equals_label(&window.title, label) {
            push(window.hwnd);
        }
    }
    for window in &item.windows {
        if title_starts_with_label(&window.title, label) {
            push(window.hwnd);
        }
    }
    if let Some(primary) = item.hwnd {
        push(primary);
    }
    for window in &item.windows {
        push(window.hwnd);
    }

    order
}

fn title_equals_label(title: &str, label: &str) -> bool {
    !label.is_empty() && title.trim().eq_ignore_ascii_case(label)
}

fn title_starts_with_label(title: &str, label: &str) -> bool {
    !label.is_empty()
        && title
            .trim()
            .to_ascii_lowercase()
            .starts_with(&format!("{} ", label.to_ascii_lowercase()))
}

unsafe fn activate_item_windows(item: &dock::DockItem) -> bool {
    let foreground = GetForegroundWindow();
    let foreground = (!foreground.is_invalid()).then_some(foreground.0 as isize);
    let foreground_owned =
        foreground.is_some_and(|raw| foreground_window_belongs_to_item(item, raw));
    for raw in activation_order_with_foreground_owned(item, foreground, foreground_owned) {
        if windows_list::activate_from_dock(raw) {
            return true;
        }
    }
    false
}

unsafe fn foreground_window_belongs_to_item(item: &dock::DockItem, raw: isize) -> bool {
    if item.hwnd == Some(raw) || item.windows.iter().any(|window| window.hwnd == raw) {
        return true;
    }
    if !windows_list::is_running_window(raw) {
        return false;
    }
    let Some(exe_path) = windows_list::process_exe_path_for_window(raw) else {
        return false;
    };
    if item
        .group_key
        .as_deref()
        .is_some_and(|key| apps::exe_matches(&exe_path, key))
    {
        return true;
    }
    item.kind == content::ContentKind::Application
        && item
            .path
            .as_deref()
            .is_some_and(|path| apps::exe_matches(path, &exe_path))
}

unsafe fn activate_live_pinned_window(owner: HWND, app: &mut App, index: usize) -> bool {
    let running = windows_list::enumerate_sorted();
    let groups = windows_list::group_by_application(&running);
    let Some(group) = unique_group_for_pinned(&app.dock.items[index], &groups) else {
        return false;
    };

    apps::attach_running(&mut app.dock.items[index], &group);
    record_window_state(app, &running);
    if activate_item_windows(&app.dock.items[index]) {
        hide_running_preview(app);
    } else {
        show_running_preview(owner, app, index);
    }
    app.dock.bump(index);
    app.animating = true;
    true
}

unsafe fn activate_live_running_window(owner: HWND, app: &mut App, index: usize) -> bool {
    let label = app.dock.items[index].label.clone();
    let group_key = app.dock.items[index].group_key.clone();
    let running = windows_list::enumerate_sorted();
    let Some(group) = windows_list::group_by_application(&running)
        .into_iter()
        .find(|group| {
            group_key.as_deref() == Some(group.key.as_str())
                || pinned_identity_matches_group(
                    &label,
                    app.dock.items[index].path.as_deref(),
                    group,
                )
        })
    else {
        return false;
    };

    let prev_agent = app.dock.items[index].agent.clone();
    app.dock.items[index] = apps::running_item(&group);
    app.dock.items[index].agent = prev_agent;
    record_window_state(app, &running);
    if activate_item_windows(&app.dock.items[index]) {
        hide_running_preview(app);
    } else {
        show_running_preview(owner, app, index);
    }
    app.dock.bump(index);
    app.animating = true;
    true
}

unsafe fn dock_overlapped(app: &App) -> bool {
    let foreground = GetForegroundWindow();
    let mut pid = 0;
    GetWindowThreadProcessId(foreground, Some(&mut pid));
    if pid == std::process::id() || foreground.is_invalid() || IsIconic(foreground).as_bool() {
        return false;
    }
    let mut rect = RECT::default();
    if GetWindowRect(foreground, &mut rect).is_err() {
        return false;
    }
    let (left, top, right, bottom) = app.dock.frame().pill;
    let shift = (1.0 - app.dock.reveal)
        * (app.dock.base_icon()
            + (2.0 * dock::PILL_PAD_Y
                + dock::effective_bottom_gap(app.dock.style, app.settings.layout)
                - dock::HIDE_SLIVER)
                * app.dock.layout_dpi());
    let top = top - shift;
    let bottom = bottom - shift;
    let x = app.full.0;
    let y = app.full.1;
    rect.left < x + right as i32
        && rect.right > x + left as i32
        && rect.top < y + bottom as i32
        && rect.bottom > y + top as i32
}

unsafe fn configure_search_hotkey(hwnd: HWND, choice: u32) {
    let _ = UnregisterHotKey(hwnd, ID_SEARCH_HOTKEY);
    settings_window::set_hotkey_conflict(false);
    if choice == 0 {
        return;
    }
    let (modifiers, key) = match choice {
        2 => (MOD_ALT | MOD_NOREPEAT, VK_SPACE),
        3 => (MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VIRTUAL_KEY(0x51)),
        _ => (MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VK_SPACE),
    };
    let result = RegisterHotKey(hwnd, ID_SEARCH_HOTKEY, modifiers, key.0 as u32);
    settings_window::set_hotkey_conflict(result.is_err());
    if let Err(error) = result {
        error_log::write("搜索快捷键被占用，请在布局与快捷键设置中更换", &error);
    }
}

fn quote_argument(value: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
        } else {
            if ch == '"' {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
            } else {
                out.push_str(&"\\".repeat(slashes));
            }
            slashes = 0;
            out.push(ch);
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

unsafe fn handle_drop_on_app(
    hwnd: HWND,
    app: &mut App,
    item: &dock::DockItem,
    paths: &[String],
) -> bool {
    let Some(application) = item.path.as_deref() else {
        return false;
    };
    let Ok(menu) = CreatePopupMenu() else {
        return true;
    };
    let _ = AppendMenuW(menu, MF_STRING, 1, w!("用此应用打开"));
    let icon = paths.len() == 1
        && std::path::Path::new(&paths[0])
            .extension()
            .is_some_and(|ext| {
                ["ico", "png", "bmp"]
                    .iter()
                    .any(|e| ext.eq_ignore_ascii_case(e))
            });
    if icon {
        let _ = AppendMenuW(menu, MF_STRING, 2, w!("替换此应用图标"));
    }
    let _ = AppendMenuW(menu, MF_STRING, 3, w!("固定为新项目"));
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(hwnd);
    let guard = menu_style::style(menu, app.dock.theme, hwnd);
    let choice = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY,
        point.x,
        point.y,
        0,
        hwnd,
        None,
    )
    .0;
    drop(guard);
    let _ = DestroyMenu(menu);
    match choice {
        1 => {
            let target: Vec<u16> = application.encode_utf16().chain(Some(0)).collect();
            let args = paths
                .iter()
                .map(|p| quote_argument(p))
                .collect::<Vec<_>>()
                .join(" ");
            let args: Vec<u16> = args.encode_utf16().chain(Some(0)).collect();
            let result = ShellExecuteW(
                hwnd,
                w!("open"),
                PCWSTR(target.as_ptr()),
                PCWSTR(args.as_ptr()),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
            if result.0 as isize <= 32 {
                show_error(hwnd, "用应用打开文件失败", result.0 as isize);
            }
            true
        }
        2 if icon => {
            match config::set_item_icon(application, &paths[0]) {
                Ok(true) => {
                    let sources = [application.to_string(), paths[0].clone()];
                    app.gpu.invalidate_icon_sources(&sources);
                    let _ = reload_app(hwnd, app);
                }
                Ok(false) => {}
                Err(error) => show_error(hwnd, "保存应用图标失败", error),
            }
            true
        }
        3 => false,
        _ => true,
    }
}

unsafe fn show_overflow(hwnd: HWND, app: &mut App) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let items = app.overflow_items.clone();
    for (index, item) in items.iter().enumerate() {
        let label_text = match item.agent.as_ref().map(|a| a.state) {
            Some(state) if state != agent_state::AgentState::Idle => {
                format!("{} · {}", item.label, state.label_zh())
            }
            _ => item.label.clone(),
        };
        let label: Vec<u16> = label_text
            .replace('&', "&&")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let _ = AppendMenuW(menu, MF_STRING, index + 1, PCWSTR(label.as_ptr()));
    }
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(hwnd);
    let guard = menu_style::style(menu, app.dock.theme, hwnd);
    let selected = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY,
        point.x,
        point.y,
        0,
        hwnd,
        None,
    )
    .0 as usize;
    drop(guard);
    let _ = DestroyMenu(menu);
    if let Some(item) = selected.checked_sub(1).and_then(|i| items.get(i)) {
        if item.is_agent() {
            execute_agent_click_for_item(hwnd, app, item, None, false);
        } else if !activate_item_windows(item) {
            if let Some(path) = &item.path {
                if let Err(error) = open_content(path) {
                    show_error(hwnd, "打开内容失败", error);
                }
            }
        }
    }
}

unsafe fn update_running_preview(hwnd: HWND, app: &mut App, x: f32, y: f32) {
    if let Some(index) = app.dock.hit_test(x, y) {
        if item_has_hover_popup(&app.dock.items[index], app.settings.agent_zone_enabled) {
            show_running_preview(hwnd, app, index);
            return;
        }
    }
    hide_running_preview(app);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(result) = menu_style::handle_message(hwnd, msg, wparam, lparam) {
            return result;
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *ptr;
        if msg == WM_POPUP_RENDER_FAILED {
            let source = HWND(wparam.0 as *mut core::ffi::c_void);
            if drawer::owns_window(source)
                || control_center::owns_window(source)
                || agent_popup::owns_window(source)
            {
                if let Err(error) = recover_gpu(hwnd, app) {
                    show_error(hwnd, "GPU 恢复失败", error);
                    PostQuitMessage(0);
                } else {
                    app.animating = true;
                }
            }
            return LRESULT(0);
        }
        if msg != 0 && msg == app.tray.taskbar_created_message() {
            if let Err(error) = app.tray.restore() {
                show_error(hwnd, "恢复托盘图标失败", error);
            }
            // Explorer restarted: the taskbar is back, visible and reserving its row.
            // Re-apply the hidden state so the dock stays the only bar at the bottom.
            if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                && app.taskbar_invoked_at.is_none()
            {
                enter_hidden(hwnd, app);
            }
            return LRESULT(0);
        }
        match msg {
            desktop_scan::WM_DESKTOP_READY => {
                drawer::desktop_ready();
                command_palette::desktop_ready();
                if fullscreen_app_present(hwnd) {
                    app.pending_rebuild = true;
                } else if let Err(error) = reload_app(hwnd, app) {
                    error_log::write("刷新应用身份失败", &error);
                }
                LRESULT(0)
            }
            WM_DOCK_ICONS_READY => {
                app.gpu.icons_ready(&app.dock.items, lparam.0);
                app.animating = true;
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                if !fullscreen_suppressed(app) {
                    let (x, y) = (
                        (lparam.0 & 0xffff) as i16 as f32,
                        ((lparam.0 >> 16) & 0xffff) as i16 as f32,
                    );
                    let hit = app.dock.hit_test(x, y);
                    app.dock.pressed_idx = hit;
                    if hit.is_some() {
                        app.animating = true;
                    }
                    if let Some(item) = hit
                        .and_then(|i| app.dock.items.get(i))
                        .filter(|i| i.role == dock::ItemRole::Pinned)
                    {
                        if let Some(source) = item.path.clone() {
                            app.drag = Some(DockDrag {
                                source,
                                is_agent: item.is_agent(),
                                start: (x, y),
                                active: false,
                                target: None,
                            });
                            SetCapture(hwnd);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_CANCELMODE | WM_CAPTURECHANGED => {
                app.drag = None;
                app.dock.insertion_x = None;
                app.dock.pressed_idx = None;
                app.animating = true;
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                // While a fullscreen app owns the screen, never let a bottom-edge hover
                // summon the dock — popping it topmost would kick an exclusive-fullscreen
                // game straight back to the desktop. This is the core fix.
                if fullscreen_suppressed(app) {
                    return LRESULT(0);
                }
                agent_popup::cancel_leave_check();
                // Cursor is on the dock: take input across the whole window again so the
                // magnification can bulge icons up above the pill without losing the mouse.
                set_region_full(hwnd, app, true);
                let _ = set_expanded(hwnd, app, true); // grow back to full so it can slide in
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                if let Some(drag) = &mut app.drag {
                    if (x - drag.start.0).hypot(y - drag.start.1) >= 6.0 * app.dock.dpi {
                        drag.active = true;
                    }
                    if drag.active {
                        let frame = app.dock.frame();
                        let nearest = frame
                            .icons
                            .iter()
                            .filter(|ic| {
                                let candidate = &app.dock.items[ic.idx];
                                candidate.role == dock::ItemRole::Pinned
                                    && candidate.is_agent() == drag.is_agent
                            })
                            .min_by(|a, b| (a.cx - x).abs().total_cmp(&(b.cx - x).abs()));
                        if let Some(icon) = nearest {
                            let item = &app.dock.items[icon.idx];
                            let after = x > icon.cx;
                            drag.target = item.path.clone().map(|p| (p, after));
                            app.dock.insertion_x = Some(
                                icon.cx
                                    + if after {
                                        app.dock.base_icon() * 0.6
                                    } else {
                                        -app.dock.base_icon() * 0.6
                                    },
                            );
                        }
                        hide_running_preview(app);
                    }
                }
                app.dock.cursor_x = Some(x);
                app.dock.hovered_idx = if app.drag.as_ref().is_some_and(|d| d.active) {
                    None
                } else {
                    app.dock.hit_test(x, y)
                };
                app.dock.reveal_target = 1.0; // mouse is over the dock zone -> reveal
                app.animating = true;
                if !app.drag.as_ref().is_some_and(|d| d.active) {
                    if app.settings.layout.preview_delay_ms == 0 {
                        update_running_preview(hwnd, app, x, y);
                    } else {
                        let candidate = app.dock.hit_test(x, y).filter(|i| {
                            item_has_hover_popup(
                                &app.dock.items[*i],
                                app.settings.agent_zone_enabled,
                            )
                        });
                        if candidate != app.preview_candidate {
                            hide_running_preview(app);
                            app.preview_candidate = candidate;
                            let mut hover = TRACKMOUSEEVENT {
                                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_HOVER | TME_LEAVE,
                                hwndTrack: hwnd,
                                dwHoverTime: app.settings.layout.preview_delay_ms,
                            };
                            let _ = TrackMouseEvent(&mut hover);
                        }
                    }
                }
                if !app.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    app.tracking = true;
                }
                LRESULT(0)
            }
            WM_MOUSEHOVER => {
                if !fullscreen_suppressed(app) && !app.drag.as_ref().is_some_and(|d| d.active) {
                    update_running_preview(
                        hwnd,
                        app,
                        (lparam.0 & 0xffff) as i16 as f32,
                        ((lparam.0 >> 16) & 0xffff) as i16 as f32,
                    );
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                app.preview_candidate = None;
                app.dock.cursor_x = None;
                app.dock.hovered_idx = None;
                app.dock.pressed_idx = None;
                let over_preview =
                    window_preview::contains_cursor() || agent_popup::contains_cursor();
                if !over_preview {
                    window_preview::hide();
                    if agent_popup::active_agent().is_some() {
                        agent_popup::schedule_leave_check();
                    }
                }
                // Resident by default; slide away only in auto-hide mode or fullscreen.
                let keep_revealed = over_preview || agent_popup::active_agent().is_some();
                app.dock.reveal_target = if keep_revealed {
                    1.0
                } else {
                    resting_target(app)
                };
                app.animating = true;
                app.tracking = false;
                LRESULT(0)
            }
            WM_NCHITTEST => {
                // Fully click-through while a fullscreen app owns the screen, so the game
                // gets every bit of input at the bottom edge and we generate no hover.
                if fullscreen_suppressed(app) {
                    return LRESULT(HTTRANSPARENT as isize);
                }
                // Only the dock zone grabs the mouse; everything else is click-through.
                // When hidden, that zone shrinks to a thin strip at the screen's edge.
                let sx = (lparam.0 & 0xFFFF) as i16 as i32;
                let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let cx = (sx - wr.left) as f32;
                let cy = (sy - wr.top) as f32;
                let (l, t, r, b) = if app.expanded {
                    // Dynamic: just the visible pill at rest, the full magnification
                    // envelope only while the cursor is actually over the dock — so the
                    // dock's box stops swallowing clicks above/beside it when unused.
                    app.dock.pointer_hit_zone()
                } else {
                    // collapsed strip: only the dock's horizontal span at the very edge
                    let (sl, sr) = app.dock.dock_span_x();
                    (sl, 0.0, sr, app.strip_h as f32)
                };
                if cx >= l && cx <= r && cy >= t && cy <= b {
                    LRESULT(HTCLIENT as isize)
                } else {
                    LRESULT(HTTRANSPARENT as isize)
                }
            }
            WM_LBUTTONUP => {
                let drag = app.drag.take();
                let _ = ReleaseCapture();
                app.dock.insertion_x = None;
                app.dock.pressed_idx = None;
                app.animating = true;
                if let Some(drag) = drag.filter(|d| d.active) {
                    let (_, y) = (
                        (lparam.0 & 0xffff) as i16 as f32,
                        ((lparam.0 >> 16) & 0xffff) as i16 as f32,
                    );
                    let x = (lparam.0 & 0xffff) as i16 as f32;
                    if x >= 0.0 && x <= app.dock.win_w && y >= 0.0 && y <= app.dock.win_h {
                        if let Some((target, after)) = drag.target {
                            match config::reorder_items(&drag.source, &target, after) {
                                Ok(true) => {
                                    let _ = reload_app(hwnd, app);
                                }
                                Ok(false) => {}
                                Err(error) => show_error(hwnd, "保存图标顺序失败", error),
                            }
                        }
                    }
                    app.animating = true;
                    return LRESULT(0);
                }
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                if let Some(i) = app.dock.hit_test(x, y) {
                    // Copy the bits we need so we can mutate the dock (bump) after.
                    let role = app.dock.items[i].role;
                    let item_hwnd = app.dock.items[i].hwnd;
                    let path = app.dock.items[i].path.clone();
                    let kind = app.dock.items[i].kind;
                    let is_agent = app.settings.agent_zone_enabled && app.dock.items[i].is_agent();
                    if is_agent && matches!(role, dock::ItemRole::Running | dock::ItemRole::Pinned)
                    {
                        handle_agent_left_click(hwnd, app, i, GetKeyState(VK_SHIFT.0 as i32) < 0);
                        return LRESULT(0);
                    }
                    match role {
                        dock::ItemRole::Start => {
                            reveal_taskbar_for_start(hwnd, app);
                            if taskbar::should_open_start_menu_for_start_invocation(
                                app.settings.taskbar_mode,
                            ) {
                                windows_list::open_start_menu();
                            }
                            app.dock.bump(i);
                            app.animating = true;
                        }
                        dock::ItemRole::Running => {
                            if should_launch_new_instance(
                                kind,
                                GetKeyState(VK_SHIFT.0 as i32) < 0,
                                path.is_some(),
                            ) {
                                if let Err(error) = open_content(path.as_deref().unwrap()) {
                                    show_error(hwnd, "打开新窗口失败", error);
                                }
                            } else if activate_item_windows(&app.dock.items[i]) {
                                hide_running_preview(app);
                                app.dock.bump(i);
                                app.animating = true;
                            } else if activate_live_running_window(hwnd, app, i) {
                            } else if let Some(raw) = item_hwnd {
                                hide_running_preview(app);
                                windows_list::activate_from_dock(raw);
                                app.dock.bump(i);
                                app.animating = true;
                            } else {
                                show_running_preview(hwnd, app, i);
                            }
                        }
                        dock::ItemRole::Pinned => {
                            // A pinned app that's running activates its window(s) instead
                            // of launching a duplicate (macOS-style). Right-click → 打开
                            // still launches a fresh instance.
                            if should_launch_new_instance(
                                kind,
                                GetKeyState(VK_SHIFT.0 as i32) < 0,
                                path.is_some(),
                            ) {
                                if let Some(path) = path {
                                    match open_content(&path) {
                                        Ok(()) => {
                                            app.dock.bump(i);
                                            app.animating = true;
                                        }
                                        Err(error) => show_error(hwnd, "打开新窗口失败", error),
                                    }
                                }
                            } else if activate_item_windows(&app.dock.items[i]) {
                                hide_running_preview(app);
                                app.dock.bump(i);
                                app.animating = true;
                            } else if activate_live_pinned_window(hwnd, app, i) {
                            } else if let Some(path) = path {
                                if kind == content::ContentKind::Folder {
                                    let (acx, atop) = anchor_above(hwnd, app, i);
                                    folder_stack::show(hwnd, &path, acx, atop);
                                    app.dock.bump(i);
                                    app.animating = true;
                                } else {
                                    match open_content(&path) {
                                        Ok(()) => {
                                            app.dock.bump(i);
                                            app.animating = true;
                                        }
                                        Err(error) => show_error(hwnd, "打开内容失败", error),
                                    }
                                }
                            }
                        }
                        dock::ItemRole::Control => {
                            // Anchor the panel above this button's center, then toggle.
                            let (acx, atop) = anchor_above(hwnd, app, i);
                            control_center::toggle(hwnd, acx, atop);
                            app.dock.bump(i);
                            app.animating = true;
                        }
                        dock::ItemRole::SystemTray => {
                            open_native_tray(hwnd, app);
                            app.dock.bump(i);
                            app.animating = true;
                        }
                        dock::ItemRole::Drawer => {
                            // Anchor the drawer above this button's center, then toggle.
                            let (acx, atop) = anchor_above(hwnd, app, i);
                            drawer::toggle(hwnd, acx, atop);
                            app.dock.bump(i);
                            app.animating = true;
                        }
                        dock::ItemRole::Overflow => show_overflow(hwnd, app),
                        dock::ItemRole::Divider | dock::ItemRole::AgentDivider => {}
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                if let Some(i) = app.dock.hit_test(x, y) {
                    let role = app.dock.items[i].role;
                    let path = app.dock.items[i].path.clone();
                    // All open windows behind this dock slot (a running app can have many).
                    let windows: Vec<isize> =
                        app.dock.items[i].windows.iter().map(|w| w.hwnd).collect();
                    match role {
                        dock::ItemRole::Running => {
                            show_window_menu(hwnd, app, &windows);
                        }
                        dock::ItemRole::Pinned => {
                            if let Some(path) = path {
                                show_pinned_menu(hwnd, app, &path, &windows);
                            }
                        }
                        dock::ItemRole::Start => {
                            if !native_tray::fullscreen_foreground_present() {
                                run_tray_menu(hwnd, app, tray::MenuSource::StartButton);
                            }
                        }
                        // Keep settings reachable directly from the dock even
                        // when Explorer has put its notification icon in overflow.
                        dock::ItemRole::Control => settings_window::open(hwnd),
                        dock::ItemRole::Drawer | dock::ItemRole::SystemTray => {}
                        dock::ItemRole::Overflow => show_overflow(hwnd, app),
                        dock::ItemRole::Divider | dock::ItemRole::AgentDivider => {}
                    }
                }
                LRESULT(0)
            }
            WM_TRAY => {
                if Tray::is_menu_event(lparam) {
                    run_tray_menu(hwnd, app, tray::MenuSource::NotificationIcon);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == tray::TIMER_TRAY_RETRY => {
                let _ = app.tray.restore();
                LRESULT(0)
            }
            WM_HOTKEY if wparam.0 == ID_SEARCH_HOTKEY as usize => {
                if windows_list::is_fullscreen_present(MonitorFromWindow(
                    GetForegroundWindow(),
                    MONITOR_DEFAULTTONEAREST,
                )) {
                    return LRESULT(0);
                }
                command_palette::toggle(hwnd);
                LRESULT(0)
            }
            WM_OPEN_SETTINGS => {
                settings_window::open(hwnd);
                LRESULT(0)
            }
            WM_SHOW_EXISTING => {
                if app.window_hidden {
                    set_window_hidden(hwnd, app, false);
                }
                if let Err(error) = set_expanded(hwnd, app, true) {
                    show_error(hwnd, "Show existing FeatherDock failed", error);
                }
                app.dock.reveal_target = 1.0;
                app.animating = true;
                raise_dock_topmost(hwnd);
                LRESULT(0)
            }
            WM_ANIMATION_WAKE => {
                app.animating = true;
                LRESULT(0)
            }
            WM_DROPFILES => {
                let hdrop = HDROP(wparam.0 as *mut core::ffi::c_void);
                let mut point = POINT::default();
                let _ = DragQueryPoint(hdrop, &mut point);
                let target = app
                    .dock
                    .hit_test(point.x as f32, point.y as f32)
                    .and_then(|i| app.dock.items.get(i))
                    .filter(|i| {
                        matches!(
                            i.kind,
                            content::ContentKind::Application | content::ContentKind::Shortcut
                        )
                    })
                    .cloned();
                let count = DragQueryFileW(hdrop, u32::MAX, None);
                let mut paths = Vec::with_capacity(count as usize);
                for index in 0..count {
                    let len = DragQueryFileW(hdrop, index, None);
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len as usize + 1];
                    let written = DragQueryFileW(hdrop, index, Some(&mut buf));
                    if written > 0 {
                        paths.push(String::from_utf16_lossy(&buf[..written as usize]));
                    }
                }
                DragFinish(hdrop);
                if let Some(target) = target.filter(|i| i.path.is_some()) {
                    if handle_drop_on_app(hwnd, app, &target, &paths) {
                        return LRESULT(0);
                    }
                }
                if let Err(error) = add_contents(hwnd, app, &paths) {
                    show_error(hwnd, "拖放内容失败", error);
                }
                LRESULT(0)
            }
            windows_list::WM_WINDOWS_CHANGED => {
                let foreground = Some(GetForegroundWindow().0 as isize);
                if app.dock.foreground != foreground {
                    app.dock.foreground = foreground;
                    app.animating = true;
                }
                let fg_agents = foreground_agents_for(foreground, &app.dock.items);
                let fg_cleared_agent = app
                    .agent_registry
                    .on_foreground_changed(foreground, &fg_agents);
                // Cheap on every event: re-check whether a fullscreen (borderless) app or a
                // maximized (captioned) window is in front and retract / restore the dock.
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let fullscreen = windows_list::is_fullscreen_present(monitor);
                let maximized = windows_list::is_maximized_present(monitor);
                let overlap = dock_overlapped(app);
                if fullscreen != app.fullscreen_active
                    || maximized != app.maximized_active
                    || overlap != app.overlap_active
                {
                    app.overlap_active = overlap;
                    app.fullscreen_active = fullscreen;
                    app.maximized_active = maximized;
                    // No longer fully suppressed (game closed, or fullscreen→maximized):
                    // re-show the fully-hidden window so the reveal/strip can take over.
                    if !fullscreen_suppressed(app) && app.window_hidden {
                        set_window_hidden(hwnd, app, false);
                    }
                    // Fullscreen-suppressed -> force out (target 0) regardless of any stale
                    // hover; otherwise honour the cursor (maximized stays hover-revealable).
                    let target = if app.dock.cursor_x.is_some() && !fullscreen_suppressed(app) {
                        1.0
                    } else {
                        resting_target(app)
                    };
                    app.dock.reveal_target = target;
                    app.animating = true;
                }
                // A fullscreen app just left the foreground: apply any dock rebuild we
                // deferred while it owned the screen (we keep the dock fully passive during
                // fullscreen so we never kick a game out of exclusive mode).
                if app.pending_rebuild && !fullscreen {
                    app.gpu.refresh_backdrop(hwnd);
                    drawer::hide();
                    control_center::hide();
                    let result = reload_app(hwnd, app);
                    app.pending_rebuild =
                        pending_rebuild_after_attempt(app.pending_rebuild, result.is_ok());
                    if let Err(error) = result {
                        show_error(hwnd, "刷新窗口列表失败", error);
                    }
                    if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                        && app.taskbar_invoked_at.is_none()
                    {
                        taskbar::reassert_hidden();
                    }
                } else if fg_cleared_agent {
                    refresh_agent_views(hwnd, app, &[], Vec::new(), None);
                }
                sync_agent_shimmer_timer(hwnd, app);
                // Expensive (wParam == 0 only): the window SET may have changed, so
                // re-scan and rebuild if what we display actually differs.
                if wparam.0 == 0 {
                    // Debounce: (re)arm a one-shot timer instead of enumerating now.
                    // A burst of events just keeps resetting it; we scan once it settles.
                    SetTimer(hwnd, TIMER_WINDOWS, TIMER_WINDOWS_MS, None);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_WINDOWS => {
                let _ = KillTimer(hwnd, TIMER_WINDOWS);
                let running = windows_list::enumerate_sorted();
                let live_hwnds: std::collections::HashSet<isize> =
                    running.iter().map(|w| w.hwnd).collect();
                let agent_pruned = app.agent_registry.prune_dead_windows(|h| {
                    if live_hwnds.contains(&h) {
                        return true;
                    }
                    let target = HWND(h as *mut core::ffi::c_void);
                    if !IsWindow(target).as_bool() {
                        return false;
                    }
                    let exe = windows_list::process_exe_path(target);
                    let aumid = app_identity::window_id(target);
                    agent_state::infer_agent_from_exe_or_aumid(exe.as_deref(), aumid.as_deref())
                        .is_some()
                });
                let topology = running_sig(&running);
                let titles = window_titles(&running);
                let matches = title_group_matches_with_overflow(
                    &app.dock.items,
                    &app.overflow_items,
                    &running,
                );
                if topology != app.running_sig || matches != app.title_matches || agent_pruned {
                    // Never re-stack the dock (reload_with does SetWindowPos) while a
                    // fullscreen app owns the screen — it kicks an exclusive-fullscreen
                    // game to the desktop. Defer the rebuild until the game exits.
                    if fullscreen_app_present(hwnd) {
                        app.running_sig = topology;
                        app.window_titles = titles;
                        app.title_matches = matches;
                        app.pending_rebuild = true;
                    } else if let Err(error) = reload_with(hwnd, app, &running) {
                        show_error(hwnd, "刷新窗口列表失败", error);
                    }
                } else if titles != app.window_titles {
                    app.window_titles = titles;
                    update_window_titles(hwnd, app, &running);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_AGENT_SHIMMER => {
                advance_agent_shimmer(hwnd, app);
                LRESULT(0)
            }
            WM_COPYDATA => {
                if let Some((mut payload, meta)) = agent_state::decode_copydata_with_meta(lparam) {
                    if let Some(raw_hwnd) = payload.hwnd {
                        if !hwnd_is_trusted_agent(
                            raw_hwnd,
                            &payload.agent,
                            payload.pid,
                            None,
                            None,
                            &app.dock.items,
                        ) {
                            payload.hwnd = None;
                        }
                    }
                    let fg = Some(GetForegroundWindow().0 as isize);
                    let fg_agents = foreground_agents_for(fg, &app.dock.items);
                    let effect = app
                        .agent_registry
                        .apply_event_with_meta(payload, meta, fg, &fg_agents);
                    if effect.changed || effect.bounce_agent.is_some() || effect.watch.is_some() {
                        let bounces: Vec<String> = effect.bounce_agent.into_iter().collect();
                        let requests: Vec<agent_state::PidWatchRequest> =
                            effect.watch.into_iter().collect();
                        refresh_agent_views(hwnd, app, &bounces, requests, None);
                    }
                    LRESULT(1)
                } else if agent_state::is_agent_copydata(lparam) {
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            agent_state::WM_AGENT_SESSIONS_READY => {
                let snapshot = sanitize_incoming_sessions(
                    app.agent_watcher.take_snapshot_with_meta(),
                    None,
                    &app.dock.items,
                );
                let fg = Some(GetForegroundWindow().0 as isize);
                let fg_agents = foreground_agents_for(fg, &app.dock.items);
                let (changed, bounces, watch_requests) = app
                    .agent_registry
                    .replace_file_sessions_with_meta(snapshot, fg, &fg_agents);
                if changed || !bounces.is_empty() || !watch_requests.is_empty() {
                    refresh_agent_views(hwnd, app, &bounces, watch_requests, None);
                }
                LRESULT(0)
            }
            agent_state::WM_AGENT_PROCESS_EXITED => {
                let pid = wparam.0 as u32;
                let generation = lparam.0 as usize;
                if app
                    .agent_pids
                    .complete_and_prune(&mut app.agent_registry, pid, generation)
                {
                    refresh_agent_views(hwnd, app, &[], Vec::new(), None);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_TASKBAR_REHIDE => {
                if let Some(invoked_at) = app.taskbar_invoked_at {
                    let grace_elapsed = invoked_at.elapsed() >= TASKBAR_INVOCATION_GRACE;
                    if taskbar::should_rehide_after_start_invocation(
                        app.settings.taskbar_mode,
                        grace_elapsed,
                        grace_elapsed && taskbar::pointer_over_taskbar(),
                    ) {
                        let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                        app.taskbar_invoked_at = None;
                        taskbar::rehide_after_start_invocation(app.settings.taskbar_mode);
                        if !native_tray::fullscreen_foreground_present() {
                            raise_dock_topmost(hwnd);
                        }
                    } else if app.settings.taskbar_mode != settings::TaskbarMode::Hidden {
                        let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                        app.taskbar_invoked_at = None;
                        if !native_tray::fullscreen_foreground_present() {
                            raise_dock_topmost(hwnd);
                        }
                    }
                } else {
                    let _ = KillTimer(hwnd, TIMER_TASKBAR_REHIDE);
                }
                LRESULT(0)
            }
            settings_window::WM_SETTINGS_CHANGED | preferences::WM_PREVIEW_SETTINGS => {
                // The settings window changed dock_mode / fullscreen / taskbar mode and
                // applied the taskbar mode itself; re-read and ease to the new resting
                // state. If it switched us to hidden, defeat the auto-hide entry slide
                // and grow maximized windows into the freed work area.
                let previous_theme = app.dock.theme;
                let previous_style = app.dock.style;
                let settings = if msg == preferences::WM_PREVIEW_SETTINGS {
                    let preview = *Box::from_raw(lparam.0 as *mut settings::Settings);
                    preferences::merge_preview(app.saved_settings, preview)
                } else {
                    let saved = settings::load();
                    app.saved_settings = saved;
                    saved
                };
                if app.shutting_down {
                    return LRESULT(0);
                }
                app.settings = settings;
                app.agent_registry
                    .set_history_window(app.settings.agent_history_window_secs());
                app.dock.theme = app.settings.effective_theme();
                app.dock.style = app.settings.dock_style;
                app.dock.preferences = app.settings.layout;
                if previous_theme != app.dock.theme || previous_style != app.dock.style {
                    // Popups snapshot their owner's palette. Reopen them on the new
                    // preset rather than leaving mixed materials on screen.
                    if previous_theme != app.dock.theme {
                        drawer::hide();
                        control_center::hide();
                        hide_running_preview(app);
                        command_palette::close();
                        settings_window::sync_theme(app.dock.theme);
                    }
                    set_region_full(hwnd, app, true);
                }
                if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                    && app.taskbar_invoked_at.is_none()
                {
                    enter_hidden(hwnd, app);
                }
                // Re-assert the desktop-icon visibility to match the (possibly toggled)
                // setting — idempotent, in case the settings window didn't apply it.
                desktop_icons::set_hidden(app.settings.hide_desktop_icons);
                // Settings can change which system effects need abnormal-exit recovery.
                reconcile_watchdog(app);
                configure_search_hotkey(hwnd, app.settings.search_hotkey);
                if fullscreen_app_present(hwnd) {
                    app.pending_rebuild = true;
                } else if let Err(error) = reload_app(hwnd, app) {
                    error_log::write("应用布局设置失败", &error);
                }
                let target = if app.dock.cursor_x.is_some() {
                    1.0
                } else {
                    resting_target(app)
                };
                app.dock.reveal_target = target;
                app.animating = true;
                sync_agent_shimmer_timer(hwnd, app);
                LRESULT(0)
            }
            window_preview::WM_PREVIEW_CLOSED | agent_popup::WM_AGENT_POPUP_CLOSED => {
                // The hover preview was dismissed by the user. The dock's own WM_MOUSELEAVE
                // already fired (and kept it revealed) when the cursor crossed onto the
                // preview; now that the preview is gone and the cursor isn't back on the
                // dock, ease to the resting state so an auto-hide dock actually retracts.
                if app.dock.cursor_x.is_none() {
                    app.dock.reveal_target = resting_target(app);
                    app.animating = true;
                }
                LRESULT(0)
            }
            agent_popup::WM_AGENT_POPUP_ACTIVATED => {
                if let Some(activation) = agent_popup::take_activation() {
                    activate_popup_session(hwnd, app, activation);
                }
                if app.dock.cursor_x.is_none() {
                    app.dock.reveal_target = resting_target(app);
                    app.animating = true;
                }
                LRESULT(0)
            }
            settings_window::WM_PINS_CHANGED => {
                // Pinned apps (or the drawer-button toggle) changed in the settings window.
                // Re-read settings FIRST so `compose_items` sees the current
                // `drawer_enabled` (this path doesn't go through WM_SETTINGS_CHANGED), then
                // rebuild the dock.
                app.saved_settings = settings::load();
                app.settings = preferences::merge_preview(app.saved_settings, app.settings);
                app.agent_registry
                    .set_history_window(app.settings.agent_history_window_secs());
                app.dock.theme = app.settings.effective_theme();
                app.dock.style = app.settings.dock_style;
                app.dock.preferences = app.settings.layout;
                if let Err(error) = reload_app(hwnd, app) {
                    show_error(hwnd, "刷新常驻应用失败", error);
                }
                LRESULT(0)
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_THEMECHANGED => {
                // A fullscreen game switching display mode broadcasts WM_DISPLAYCHANGE.
                // Reacting now — re-stacking the dock topmost (reload_app) or reflowing
                // windows — knocks the game out of exclusive fullscreen and bounces it to
                // the desktop. Stay completely passive while a fullscreen app owns the
                // screen; rebuild once it exits (see WM_WINDOWS_CHANGED).
                if fullscreen_app_present(hwnd) {
                    app.pending_rebuild = true;
                    return LRESULT(0);
                }
                app.dock.theme = app.settings.effective_theme();
                app.dock.style = app.settings.dock_style;
                app.gpu.refresh_backdrop(hwnd);
                drawer::hide();
                control_center::hide();
                if let Err(error) = reload_app(hwnd, app) {
                    show_error(hwnd, "重新布局 Dock 失败", error);
                }
                // A display / work-area change can let the shell repaint the taskbar or
                // re-reserve its row; re-assert the transparent auto-hide state (cheap
                // and idempotent). On a resolution change also regrow maximized windows
                // into the freed work area.
                if app.settings.taskbar_mode == settings::TaskbarMode::Hidden
                    && app.taskbar_invoked_at.is_none()
                {
                    taskbar::reassert_hidden();
                    if msg == WM_DISPLAYCHANGE {
                        taskbar::reflow_maximized_windows();
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = UnregisterHotKey(hwnd, ID_SEARCH_HOTKEY);
                PostQuitMessage(0);
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
    fn expected_window_pid_is_enforced_without_process_start() {
        unsafe {
            let desktop = GetDesktopWindow();
            let mut actual_pid = 0u32;
            GetWindowThreadProcessId(desktop, Some(&mut actual_pid));
            assert_ne!(actual_pid, 0);
            assert!(hwnd_matches_expected_process(
                desktop,
                Some(actual_pid),
                None
            ));
            let wrong_pid = if actual_pid == u32::MAX {
                actual_pid - 1
            } else {
                actual_pid + 1
            };
            assert!(!hwnd_matches_expected_process(
                desktop,
                Some(wrong_pid),
                None
            ));
        }
    }

    #[test]
    fn animation_message_batch_yields_before_input_can_starve_a_frame() {
        assert_eq!(ANIMATION_MESSAGE_BUDGET, 4);
        assert!(animation_message_batch_has_capacity(0));
        assert!(animation_message_batch_has_capacity(
            ANIMATION_MESSAGE_BUDGET - 1
        ));
        assert!(!animation_message_batch_has_capacity(
            ANIMATION_MESSAGE_BUDGET
        ));
    }

    #[test]
    fn popup_animation_keeps_shared_frame_loop_alive() {
        assert!(!animation_sources_active(false, false, false));
        assert!(animation_sources_active(true, false, false));
        assert!(animation_sources_active(false, true, false));
        assert!(animation_sources_active(false, false, true));
    }

    #[test]
    fn shared_frame_loop_samples_every_animation_source() {
        let source = include_str!("main.rs");
        let frame_loop = source
            .split("if (*app_ptr).animating")
            .nth(1)
            .and_then(|tail| tail.split("} else {").next())
            .expect("animation frame loop");

        assert!(frame_loop.contains("drawer::animate_frame()"));
        assert!(frame_loop.contains("control_center::animate_frame()"));
    }

    #[test]
    fn settled_hover_keeps_the_magnified_icon_hit_region() {
        assert!(settled_region_should_stay_full(true));
        assert!(!settled_region_should_stay_full(false));
    }

    #[test]
    fn resting_region_does_not_cut_the_antialiased_stroke_at_any_dpi() {
        for dpi in [1.0, 1.25, 1.5, 2.0] {
            let pill = (10.2, 20.7, 130.4, 89.1);
            let radius = 32.0 * dpi;
            let (left, top, right, bottom, ellipse) = resting_region(pill, radius, dpi);
            let margin = (1.5 * dpi).ceil().max(2.0) as i32;

            assert!(margin as f32 >= dpi * 0.5 + 1.0);
            assert_eq!(left, 10 - margin);
            assert_eq!(top, 20 - margin);
            assert_eq!(right, 131 + margin + 1);
            assert_eq!(bottom, 90 + margin + 1);
            assert_eq!(ellipse, (2.0 * (radius + margin as f32)).ceil() as i32);
        }
    }

    #[test]
    fn unchanged_dock_geometry_does_not_restack_the_window() {
        let current = (10, 20, 1280, 180);
        assert!(!dock_geometry_changed(current, current));
        assert!(dock_geometry_changed(current, (9, 20, 1280, 180)));
        assert!(dock_geometry_changed(current, (10, 20, 1440, 180)));
    }

    fn running_ref(hwnd: isize, title: &str) -> dock::RunningWindowRef {
        dock::RunningWindowRef {
            hwnd,
            title: title.to_string(),
        }
    }

    fn item_with_windows(label: &str, windows: Vec<dock::RunningWindowRef>) -> dock::DockItem {
        dock::DockItem {
            label: label.to_string(),
            glyph: "",
            color: (0.0, 0.0, 0.0),
            path: None,
            icon: None,
            kind: content::ContentKind::Application,
            role: dock::ItemRole::Running,
            hwnd: windows.first().map(|window| window.hwnd),
            group_key: None,
            windows,
            agent: None,
        }
    }

    #[test]
    fn system_tray_stays_next_to_control_when_apps_overflow() {
        for agent_zone_enabled in [false, true] {
            let settings = settings::Settings {
                agent_zone_enabled,
                ..Default::default()
            };
            let pinned = (0..20)
                .map(|index| pinned_item(&format!("App {index}"), &format!("C:/Apps/{index}.exe")))
                .collect();
            let items = compose_items_with_pinned(
                pinned,
                &[],
                &settings,
                &agent_state::AgentRegistry::default(),
            );
            let (visible, overflow) =
                dock::limit_items(items, 500, 1.0, settings.layout, theme::ThemePreset::Glass);
            assert!(!overflow.is_empty());
            let suffix: Vec<_> = visible[visible.len() - 3..]
                .iter()
                .map(|item| item.label.as_str())
                .collect();
            assert_eq!(suffix, ["更多应用", "系统托盘", "控制中心"]);
        }
    }

    fn pinned_item(label: &str, path: &str) -> dock::DockItem {
        dock::DockItem {
            label: label.to_string(),
            glyph: "",
            color: (0.0, 0.0, 0.0),
            path: Some(path.to_string()),
            icon: None,
            kind: content::ContentKind::Application,
            role: dock::ItemRole::Pinned,
            hwnd: None,
            group_key: None,
            windows: Vec::new(),
            agent: None,
        }
    }

    fn running_group(
        key: &str,
        label: &str,
        windows: Vec<windows_list::RunningWindow>,
    ) -> windows_list::RunningGroup {
        windows_list::RunningGroup {
            key: key.to_string(),
            label: label.to_string(),
            icon_path: Some(key.to_string()),
            windows,
        }
    }

    fn running_window(hwnd: isize, title: &str, exe_path: &str) -> windows_list::RunningWindow {
        windows_list::RunningWindow {
            hwnd,
            title: title.to_string(),
            exe_path: Some(exe_path.to_string()),
            app_id: None,
        }
    }

    #[test]
    fn title_changes_are_metadata_but_app_id_changes_update_topology() {
        let before = vec![running_window(
            42,
            "Loading",
            r"C:\Program Files\WeGame\browser.exe",
        )];
        let after = vec![running_window(
            42,
            "WeGame",
            r"C:\Program Files\WeGame\browser.exe",
        )];
        assert_eq!(running_sig(&before), running_sig(&after));
        assert_ne!(window_titles(&before), window_titles(&after));

        let mut changed = after.clone();
        changed[0].app_id = Some("aumid:vendor.app".into());
        assert_ne!(running_sig(&before), running_sig(&changed));
    }

    #[test]
    fn title_changes_still_rebuild_when_they_change_pinned_matching() {
        let pinned = vec![pinned_item("WeGame", r"C:\Program Files\WeGame\wegame.exe")];
        let before = vec![running_window(
            42,
            "Loading",
            r"C:\Program Files\WeGame\browser.exe",
        )];
        let after = vec![running_window(
            42,
            "WeGame",
            r"C:\Program Files\WeGame\browser.exe",
        )];
        assert!(title_group_matches(&pinned, &before).is_empty());
        assert_eq!(title_group_matches(&pinned, &after).len(), 1);
        assert!(title_group_matches_with_overflow(&[], &pinned, &before).is_empty());
        assert_eq!(
            title_group_matches_with_overflow(&[], &pinned, &after).len(),
            1
        );
    }

    #[test]
    fn app_guard_and_gpu_recovery_cover_error_paths() {
        let source = include_str!("main.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .map(|(head, _)| head)
            .expect("main production source");
        assert!(production.contains("impl Drop for AppGuard"));
        let run = production
            .split("let _app_guard = AppGuard(app_ptr);")
            .nth(1)
            .and_then(|tail| tail.split("unsafe fn cleanup").next())
            .expect("guarded run body");
        assert!(!run.contains("cleanup(app_ptr)"));

        let recovery = production
            .split("unsafe fn recover_gpu")
            .nth(1)
            .and_then(|tail| tail.split("fn main").next())
            .expect("GPU recovery body");
        let drawer = recovery.find("drawer::shutdown()").unwrap();
        let control = recovery.find("control_center::shutdown()").unwrap();
        let release = recovery.find("app.gpu.release_window_target()").unwrap();
        let create = recovery.find("Gpu::new").unwrap();
        assert!(drawer < control && control < release && release < create);
        assert!(!recovery.contains("reload_icon_cache"));
    }

    #[test]
    fn deferred_rebuild_failure_keeps_the_retry_pending() {
        assert!(pending_rebuild_after_attempt(true, false));
        assert!(!pending_rebuild_after_attempt(true, true));
        assert!(!pending_rebuild_after_attempt(false, false));
    }

    #[test]
    fn custom_window_message_ids_are_distinct() {
        let mut messages = vec![
            WM_SHOW_EXISTING,
            WM_ANIMATION_WAKE,
            WM_OPEN_SETTINGS,
            WM_DOCK_ICONS_READY,
            WM_POPUP_RENDER_FAILED,
            desktop_scan::WM_DESKTOP_READY,
            windows_list::WM_WINDOWS_CHANGED,
            window_preview::WM_PREVIEW_CLOSED,
            settings_window::WM_SETTINGS_CHANGED,
            settings_window::WM_PINS_CHANGED,
            preferences::WM_PREVIEW_SETTINGS,
            preferences::WM_SAVED,
            settings_window::WM_HOTKEY_STATUS,
            command_palette::WM_ENTRIES_READY,
            drawer::WM_APP_ICONS_READY,
            tray::WM_TRAY,
            agent_state::WM_AGENT_SESSIONS_READY,
            agent_state::WM_AGENT_PROCESS_EXITED,
            agent_popup::WM_AGENT_POPUP_CLOSED,
            agent_popup::WM_AGENT_POPUP_ACTIVATED,
        ];
        let count = messages.len();
        messages.sort_unstable();
        messages.dedup();
        assert_eq!(messages.len(), count);
    }

    #[test]
    fn instance_recovery_never_kills_by_image_name_or_restores_implicitly() {
        let source = include_str!("main.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .map(|(head, _)| head)
            .expect("main production source");
        assert!(!production.contains("TerminateProcess"));
        let recovery = production
            .split("unsafe fn acquire_instance_or_reveal_existing")
            .nth(1)
            .and_then(|tail| tail.split("unsafe fn dock_window").next())
            .expect("instance recovery body");
        assert!(!recovery.contains("restore_stranded_system_state"));
    }

    #[test]
    fn existing_empty_pin_config_does_not_regenerate_defaults() {
        let source = include_str!("main.rs");
        let pinned = source
            .split("fn pinned_items()")
            .nth(1)
            .and_then(|tail| tail.split("fn compose_items").next())
            .expect("pinned config loader");
        assert!(pinned.contains("Some(cfg) => Ok(apps::from_config(&cfg.items))"));
        assert!(!pinned.contains("items.is_empty()"));
    }

    #[test]
    fn pinned_launcher_matches_helper_window_by_exact_title() {
        let item = pinned_item("WeGame", r"C:\Program Files\WeGame\wegame.exe");
        let group = running_group(
            r"c:\program files\wegame\browser.exe",
            "Browser",
            vec![running_window(
                42,
                "WeGame",
                r"C:\Program Files\WeGame\browser.exe",
            )],
        );

        assert!(pinned_matches_group(&item, &group));
    }

    #[test]
    fn codex_windowsapps_path_launches_through_system_app_entry() {
        assert_eq!(
            system_launcher_for_path(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.623.13972.0_x64__2p2nqsd0c76g0\app\Codex.exe"
            ),
            Some(r"shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App")
        );
        assert_eq!(
            system_launcher_for_path(
                r"C:\Program Files\WindowsApps\Other.App_1.0.0.0_x64__abc\app\Codex.exe"
            ),
            None
        );
    }

    #[test]
    fn pinned_identity_match_ignores_stale_cached_windows() {
        let group = running_group(
            r"c:\program files\wegame\browser.exe",
            "Browser",
            vec![running_window(
                42,
                "WeGame",
                r"C:\Program Files\WeGame\browser.exe",
            )],
        );

        assert!(pinned_identity_matches_group(
            "WeGame",
            Some(r"C:\Program Files\WeGame\wegame.exe"),
            &group
        ));
    }

    #[test]
    fn activation_prefers_the_window_whose_title_matches_the_app_label() {
        let item = item_with_windows(
            "WeGame",
            vec![
                running_ref(10, "WeGame Helper"),
                running_ref(20, "WeGame"),
                running_ref(30, "Settings"),
            ],
        );

        assert_eq!(activation_target(&item), Some(20));
    }

    #[test]
    fn activation_falls_back_to_the_primary_running_window() {
        let item = item_with_windows(
            "Explorer",
            vec![running_ref(10, "Downloads"), running_ref(20, "Desktop")],
        );

        assert_eq!(activation_target(&item), Some(10));
    }

    #[test]
    fn activation_prefers_foreground_window_in_same_running_group() {
        let item = item_with_windows(
            "Browser",
            vec![running_ref(10, "Docs"), running_ref(20, "Mail")],
        );

        assert_eq!(activation_order_with_foreground(&item, Some(20))[0], 20);
    }

    #[test]
    fn activation_prefers_live_foreground_window_owned_by_same_app() {
        let item = item_with_windows(
            "Explorer",
            vec![running_ref(10, "Downloads"), running_ref(20, "Desktop")],
        );

        assert_eq!(
            activation_order_with_foreground_owned(&item, Some(99), true)[0],
            99
        );
    }
}

#[cfg(test)]
mod interaction_regression_tests {
    use super::*;
    #[test]
    fn dropped_arguments_preserve_spaces_quotes_and_trailing_slashes() {
        assert_eq!(
            quote_argument(r"C:\My Files\a.txt"),
            r#""C:\My Files\a.txt""#
        );
        assert_eq!(quote_argument("C:\\Folder\\"), "\"C:\\Folder\\\\\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
    }
    #[test]
    fn direct_executable_pins_can_match_a_window_with_an_app_id() {
        let group = windows_list::RunningGroup {
            key: "aumid:vendor.app".into(),
            label: "App".into(),
            icon_path: Some(r"C:\Tools\App.exe".into()),
            windows: Vec::new(),
        };
        assert!(pinned_identity_matches_group(
            "App",
            Some(r"C:\Tools\App.exe"),
            &group
        ));
        assert!(!pinned_identity_matches_group(
            "App",
            Some(r"C:\Other\App.exe"),
            &group
        ));
    }
    #[test]
    fn unrelated_full_paths_do_not_match_by_executable_basename() {
        assert!(!apps::exe_matches(r"C:\One\app.exe", r"c:\two\app.exe"));
        assert!(apps::exe_matches("app.exe", r"c:\two\app.exe"));
    }

    fn make_pinned(path: &str) -> dock::DockItem {
        apps::from_config(&[crate::config::ItemSpec {
            label: Some("App".into()),
            path: Some(path.into()),
            ..Default::default()
        }])
        .remove(0)
    }

    fn group(key: &str, executable: &str) -> windows_list::RunningGroup {
        windows_list::RunningGroup {
            key: key.into(),
            label: "App".into(),
            icon_path: Some(executable.into()),
            windows: vec![windows_list::RunningWindow {
                hwnd: 1,
                title: "App".into(),
                exe_path: Some(executable.into()),
                app_id: None,
            }],
        }
    }

    #[test]
    fn ambiguous_executable_fallback_does_not_pick_the_first_app_id() {
        let exe = r"C:\Tools\App.exe";
        let groups = vec![
            group("aumid:vendor.one", exe),
            group("aumid:vendor.two", exe),
        ];
        let item = make_pinned(exe);
        assert!(unique_group_for_pinned(&item, &groups).is_none());
        let mut pinned = vec![item];
        let unpinned = merge_running_groups(&mut pinned, groups);
        assert_eq!(unpinned.len(), 2);
        assert!(pinned[0].group_key.is_none());

        let item = make_pinned(exe);
        let unique = group("aumid:vendor.one", exe);
        assert_eq!(
            unique_group_for_pinned(&item, std::slice::from_ref(&unique)).map(|group| group.key),
            Some("aumid:vendor.one".into())
        );
        let mut pinned = vec![item];
        let unpinned = merge_running_groups(&mut pinned, vec![unique]);
        assert!(unpinned.is_empty());
        assert_eq!(pinned[0].group_key.as_deref(), Some("aumid:vendor.one"));
    }

    #[test]
    fn shift_click_requires_an_application_launch_path() {
        assert!(should_launch_new_instance(
            content::ContentKind::Application,
            true,
            true
        ));
        assert!(!should_launch_new_instance(
            content::ContentKind::Application,
            false,
            true
        ));
        assert!(!should_launch_new_instance(
            content::ContentKind::Application,
            true,
            false
        ));
        assert!(should_launch_new_instance(
            content::ContentKind::Shortcut,
            true,
            true
        ));
        assert!(!should_launch_new_instance(
            content::ContentKind::Folder,
            true,
            true
        ));
        let item = apps::running_item(&group("aumid:vendor.one", r"C:\Tools\App.exe"));
        assert_eq!(item.path.as_deref(), Some(r"C:\Tools\App.exe"));

        let mut mixed = group("aumid:vendor.suite", r"C:\Tools\App.exe");
        mixed.windows.push(windows_list::RunningWindow {
            hwnd: 2,
            title: "Helper".into(),
            exe_path: Some(r"C:\Tools\Helper.exe".into()),
            app_id: None,
        });
        assert!(apps::running_item(&mixed).path.is_none());
    }

    #[test]
    fn three_zone_composition_separates_agents_and_aggregates_sessions() {
        let mut antigravity_pin = make_pinned(r"C:\Apps\Antigravity\Antigravity.exe");
        antigravity_pin.label = "Antigravity".to_string();
        antigravity_pin.agent = Some(dock::AgentSlotInfo::idle("antigravity"));
        let pinned = vec![make_pinned(r"C:\Windows\notepad.exe"), antigravity_pin];

        let running = vec![
            windows_list::RunningWindow {
                hwnd: 101,
                title: "Edge".into(),
                exe_path: Some(
                    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe".into(),
                ),
                app_id: None,
            },
            windows_list::RunningWindow {
                hwnd: 202,
                title: "Codex".into(),
                exe_path: Some(
                    r"C:\Program Files\WindowsApps\OpenAI.Codex_1.0\app\Codex.exe".into(),
                ),
                app_id: Some("OpenAI.Codex_2p2nqsd0c76g0!App".into()),
            },
        ];

        let mut registry = agent_state::AgentRegistry::default();
        registry.apply_event(
            agent_state::AgentEventPayload {
                agent: "codex".into(),
                session_id: "s1".into(),
                title: Some("Dock refactor".into()),
                detail: Some("Waiting approval".into()),
                cwd: None,
                state: agent_state::ParsedEventState::State(agent_state::AgentState::Waiting),
                hwnd: Some(202),
                pid: None,
                deep_link: None,
                updated_at: Some(10),
            },
            None,
            &[],
        );

        let settings = settings::Settings::default();
        let row = compose_items_with_pinned(pinned.clone(), &running, &settings, &registry);
        let roles: Vec<dock::ItemRole> = row.iter().map(|i| i.role).collect();
        assert!(roles.contains(&dock::ItemRole::Divider));
        assert!(roles.contains(&dock::ItemRole::AgentDivider));

        let ag_div = roles
            .iter()
            .position(|&r| r == dock::ItemRole::AgentDivider)
            .unwrap();
        // Regular items live before AgentDivider; Agent items live after AgentDivider.
        assert!(row[..ag_div].iter().all(|i| !i.is_agent()));
        let agent_slots: Vec<&dock::DockItem> = row[ag_div + 1..]
            .iter()
            .take_while(|item| item.role != dock::ItemRole::SystemTray)
            .collect();
        assert_eq!(agent_slots.len(), 2);
        assert_eq!(
            agent_slots[0].agent.as_ref().unwrap().agent_id,
            "antigravity"
        );
        assert_eq!(agent_slots[1].agent.as_ref().unwrap().agent_id, "codex");
        assert_eq!(
            agent_slots[1].agent.as_ref().unwrap().state,
            agent_state::AgentState::Waiting
        );
        assert_eq!(
            agent_slots[1].agent.as_ref().unwrap().badge.as_deref(),
            Some("!")
        );

        // Disabling `agent_zone_enabled` cleanly falls back to the classic two-zone dock.
        let disabled = settings::Settings {
            agent_zone_enabled: false,
            ..Default::default()
        };
        let classic = compose_items_with_pinned(pinned, &running, &disabled, &registry);
        assert!(!classic
            .iter()
            .any(|i| i.role == dock::ItemRole::AgentDivider));
        assert!(!classic.iter().any(|i| i.is_agent()));
    }

    #[test]
    fn duplicate_agent_pins_produce_one_slot() {
        let mut codex_a = make_pinned(r"C:\Apps\Codex\Codex.exe");
        codex_a.label = "Codex".into();
        codex_a.agent = Some(dock::AgentSlotInfo::idle("codex"));

        let mut codex_b = make_pinned(r"C:\Program Files\WindowsApps\OpenAI.Codex\Codex.exe");
        codex_b.label = "Codex Nightly".into();
        codex_b.agent = Some(dock::AgentSlotInfo::idle("codex"));

        let settings = settings::Settings::default();
        let registry = agent_state::AgentRegistry::default();
        let row = compose_items_with_pinned(vec![codex_a, codex_b], &[], &settings, &registry);
        let codex_count = row
            .iter()
            .filter(|it| it.agent.as_ref().is_some_and(|a| a.agent_id == "codex"))
            .count();
        assert_eq!(codex_count, 1);
    }
}
