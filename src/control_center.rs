//! The control center: a frosted-glass popup, GPU-composited to match the dock,
//! summoned by the dock's right-side Control button. It keeps the common controls
//! in-process (volume, audio device choice, battery entry, clock handoff) and hands
//! the parts that need native Windows credential/device flows to Settings.
//!
//! It is its own top-level window (single-instance via `PANEL_HWND`) so it can open
//! and close on demand without disturbing the dock. It activates on open and closes
//! when it loses focus (click-away), on Esc, or when its toggle button is hit again.

use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};

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
    ReleaseCapture, SetCapture, VK_BACK, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::glass::Glass;
use crate::sysctl::{self, AudioControl};

// --- layout, in logical px @96dpi (multiplied by dpi for device px) ---
const PANEL_W: f32 = 340.0;
const PANEL_H: f32 = 222.0;
#[cfg(test)]
const PANEL_AUDIO_H: f32 = 374.0;
const PANEL_BATTERY_H: f32 = 260.0;
const PANEL_BLUETOOTH_H: f32 = 270.0;
const PANEL_INPUT_H: f32 = 292.0;
const PAD: f32 = 14.0;
const RADIUS: f32 = 18.0;
const VOL_TOP: f32 = 16.0;
const VOL_H: f32 = 32.0;
const GLYPH_W: f32 = 30.0;
const AUDIO_BUTTON_TOP: f32 = 56.0;
const AUDIO_BUTTON_H: f32 = 34.0;
const BTN_TOP: f32 = 102.0;
const BTN_H: f32 = 58.0;
const BTN_GAP: f32 = 10.0;
const STATUS_H: f32 = 22.0;
const HEADER_TOP: f32 = 12.0;
const HEADER_H: f32 = 34.0;
const SECTION_H: f32 = 22.0;
const ROW_H: f32 = 38.0;
const ROW_GAP: f32 = 6.0;
const AUDIO_CONTENT_TOP: f32 = 58.0;
const AUDIO_SECTION_GAP: f32 = 14.0;
const AUDIO_SETTINGS_GAP: f32 = 18.0;
const AUDIO_MAX_ROWS: usize = 3;
const NETWORK_CONTENT_TOP: f32 = 58.0;
const NETWORK_MAX_ROWS: usize = 5;
const NETWORK_SETTINGS_GAP: f32 = 14.0;
const BLUETOOTH_CONTENT_TOP: f32 = 58.0;
const BLUETOOTH_MAX_ROWS: usize = 5;
const INPUT_CONTENT_TOP: f32 = 58.0;
const INPUT_MAX_ROWS: usize = 5;
const SIMPLE_PANEL_SETTINGS_GAP: f32 = 14.0;
const ANIM_SECS: f32 = 0.13;
const WIFI_REFRESH_TIMER: usize = 2;
const WIFI_REFRESH_INTERVAL_MS: u32 = 450;
const WIFI_REFRESH_TICKS: u8 = 8;
/// Ignore a re-open that lands right after a click-away close (avoids flicker when
/// the Control button is clicked to dismiss).
const REOPEN_GUARD_MS: u32 = 220;

const BUTTON_LABELS: [&str; 3] = ["网络", "蓝牙", "输入法"];

static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);
static LAST_CLOSED_TICK: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelView {
    Main,
    Audio,
    Battery,
    Network,
    Bluetooth,
    Input,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelAction {
    None,
    ToggleMute,
    StartVolumeDrag,
    ShowAudioPanel,
    ShowNetworkPanel,
    ShowBluetoothPanel,
    ShowInputPanel,
    ShowBatteryPanel,
    RefreshWifi,
    RefreshBluetooth,
    ConnectWifi {
        index: usize,
    },
    SelectWifiForPassword {
        index: usize,
    },
    FocusWifiPassword,
    ConnectSelectedWifi,
    OpenBluetoothDevice {
        index: usize,
    },
    SelectInputMethod {
        index: usize,
    },
    CycleInputMethod,
    OpenDateTimeSettings,
    OpenNetworkSettings,
    OpenBluetoothSettings,
    OpenInputSettings,
    BackToMain,
    OpenSoundSettings,
    OpenBatterySaverSettings,
    OpenPowerSettings,
    SelectAudioDevice {
        flow: sysctl::AudioFlow,
        index: usize,
    },
}

struct Panel {
    owner: HWND,
    glass: Glass,
    brush: ID2D1SolidColorBrush,
    emoji: IDWriteTextFormat,
    title: IDWriteTextFormat,
    label: IDWriteTextFormat,
    small: IDWriteTextFormat,
    status_left: IDWriteTextFormat,
    status_right: IDWriteTextFormat,
    view: PanelView,
    audio: Option<AudioControl>,
    audio_devices: sysctl::AudioDevices,
    wifi_networks: sysctl::WifiNetworks,
    bluetooth_devices: sysctl::BluetoothDevices,
    input_methods: sysctl::InputMethods,
    selected_wifi: Option<usize>,
    wifi_password: String,
    wifi_password_active: bool,
    wifi_message: Option<String>,
    wifi_pending_refreshes: u8,
    bluetooth_message: Option<String>,
    input_message: Option<String>,
    // Snapshot of the system state, sampled once at open (and, for volume, on
    // interaction). The open animation re-renders every frame; reading the audio endpoint /
    // power API on each of those frames risks janking the fade if a call briefly blocks, so
    // render() draws from this snapshot and never queries the system itself.
    vol_level: f32,
    vol_muted: bool,
    battery: sysctl::Battery,
    clock: (String, String),
    dpi: f32,
    width: f32,
    height: f32,
    hovered: PanelAction,
    dragging: bool,
    anim_start: Instant,
    view_anim_start: Instant,
    view_direction: f32,
}

struct Layout {
    vol_glyph: D2D_RECT_F,
    vol_row: D2D_RECT_F,
    vol_bar: D2D_RECT_F,
    audio_button: D2D_RECT_F,
    buttons: [D2D_RECT_F; 3],
    status: D2D_RECT_F,
    battery_status: D2D_RECT_F,
    clock_status: D2D_RECT_F,
    back: D2D_RECT_F,
    battery_saver: D2D_RECT_F,
    power_settings: D2D_RECT_F,
}

struct AudioPanelLayout {
    output_title: D2D_RECT_F,
    output_rows: Vec<D2D_RECT_F>,
    input_title: D2D_RECT_F,
    input_rows: Vec<D2D_RECT_F>,
    sound_settings: D2D_RECT_F,
}

struct NetworkPanelLayout {
    rows: Vec<D2D_RECT_F>,
    password: D2D_RECT_F,
    connect: D2D_RECT_F,
    refresh: D2D_RECT_F,
    status: D2D_RECT_F,
    network_settings: D2D_RECT_F,
}

struct BluetoothPanelLayout {
    rows: Vec<D2D_RECT_F>,
    refresh: D2D_RECT_F,
    status: D2D_RECT_F,
    bluetooth_settings: D2D_RECT_F,
}

struct InputPanelLayout {
    rows: Vec<D2D_RECT_F>,
    status: D2D_RECT_F,
    system_switch: D2D_RECT_F,
    input_settings: D2D_RECT_F,
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

fn panel_size(view: PanelView, dpi: f32) -> (f32, f32) {
    let h = match view {
        PanelView::Main => PANEL_H,
        PanelView::Audio => audio_panel_height(&sysctl::AudioDevices::default()),
        PanelView::Battery => PANEL_BATTERY_H,
        PanelView::Network => network_panel_height(&sysctl::WifiNetworks::default()),
        PanelView::Bluetooth => bluetooth_panel_height(&sysctl::BluetoothDevices::default()),
        PanelView::Input => input_panel_height(&sysctl::InputMethods::default()),
    };
    (PANEL_W * dpi, h * dpi)
}

fn panel_size_for_view(
    view: PanelView,
    dpi: f32,
    audio_devices: &sysctl::AudioDevices,
    wifi_networks: &sysctl::WifiNetworks,
    selected_wifi: Option<usize>,
    bluetooth_devices: &sysctl::BluetoothDevices,
    input_methods: &sysctl::InputMethods,
) -> (f32, f32) {
    let h = match view {
        PanelView::Main => PANEL_H,
        PanelView::Audio => audio_panel_height(audio_devices),
        PanelView::Battery => PANEL_BATTERY_H,
        PanelView::Network => network_panel_height_for_selection(wifi_networks, selected_wifi),
        PanelView::Bluetooth => bluetooth_panel_height(bluetooth_devices),
        PanelView::Input => input_panel_height(input_methods),
    };
    (PANEL_W * dpi, h * dpi)
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
    let audio_button = rect(
        cl,
        s(AUDIO_BUTTON_TOP),
        cr,
        s(AUDIO_BUTTON_TOP) + s(AUDIO_BUTTON_H),
    );
    let count = quick_button_count();
    let bw = (cr - cl - count.saturating_sub(1) as f32 * s(BTN_GAP)) / count as f32;
    let buttons = [0usize, 1, 2].map(|i| {
        let left = cl + i as f32 * (bw + s(BTN_GAP));
        rect(left, s(BTN_TOP), left + bw, s(BTN_TOP) + s(BTN_H))
    });
    let stop = height - s(PAD) - s(STATUS_H);
    let status = rect(cl, stop, cr, stop + s(STATUS_H));
    let clock_w = s(128.0).min(status.right - status.left);
    let clock_status = rect(
        status.right - clock_w,
        status.top,
        status.right,
        status.bottom,
    );
    let battery_status = rect(
        status.left,
        status.top,
        clock_status.left - s(8.0),
        status.bottom,
    );
    let back = rect(cl, s(HEADER_TOP), cl + s(34.0), s(HEADER_TOP) + s(HEADER_H));
    let battery_saver = rect(cl, s(86.0), cr, s(86.0) + s(ROW_H));
    let power_settings = rect(cl, s(132.0), cr, s(132.0) + s(ROW_H));
    Layout {
        vol_glyph,
        vol_row,
        vol_bar,
        audio_button,
        buttons,
        status,
        battery_status,
        clock_status,
        back,
        battery_saver,
        power_settings,
    }
}

fn view_switch_initial_animation_elapsed_secs() -> f32 {
    0.0
}

fn panel_view_depth(view: PanelView) -> i32 {
    match view {
        PanelView::Main => 0,
        PanelView::Audio
        | PanelView::Battery
        | PanelView::Network
        | PanelView::Bluetooth
        | PanelView::Input => 1,
    }
}

fn view_switch_direction(from: PanelView, to: PanelView) -> f32 {
    match panel_view_depth(to).cmp(&panel_view_depth(from)) {
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.45,
    }
}

fn view_content_alpha(elapsed_secs: f32) -> f32 {
    smoothstep(elapsed_secs / ANIM_SECS)
}

fn view_content_offset_y(elapsed_secs: f32, direction: f32, dpi: f32) -> f32 {
    (1.0 - view_content_alpha(elapsed_secs)) * 8.0 * direction * dpi
}

fn visible_audio_row_count(count: usize) -> usize {
    count.clamp(1, AUDIO_MAX_ROWS)
}

fn audio_row_rect(top: f32, width: f32, dpi: f32) -> D2D_RECT_F {
    rect(PAD * dpi, top, width - PAD * dpi, top + ROW_H * dpi)
}

fn audio_rows(start_top: f32, count: usize, width: f32, dpi: f32) -> Vec<D2D_RECT_F> {
    (0..count)
        .map(|index| {
            audio_row_rect(
                start_top + index as f32 * (ROW_H + ROW_GAP) * dpi,
                width,
                dpi,
            )
        })
        .collect()
}

fn rows_bottom(rows: &[D2D_RECT_F]) -> f32 {
    rows.last().map(|row| row.bottom).unwrap_or(0.0)
}

fn audio_panel_layout(devices: &sysctl::AudioDevices, width: f32, dpi: f32) -> AudioPanelLayout {
    let content_top = AUDIO_CONTENT_TOP * dpi;
    let section_h = SECTION_H * dpi;
    let section_gap = AUDIO_SECTION_GAP * dpi;
    let settings_gap = AUDIO_SETTINGS_GAP * dpi;

    let output_title = rect(
        PAD * dpi,
        content_top,
        width - PAD * dpi,
        content_top + section_h,
    );
    let output_rows = audio_rows(
        output_title.bottom,
        visible_audio_row_count(devices.outputs.len()),
        width,
        dpi,
    );
    let input_title_top = rows_bottom(&output_rows) + section_gap;
    let input_title = rect(
        PAD * dpi,
        input_title_top,
        width - PAD * dpi,
        input_title_top + section_h,
    );
    let input_rows = audio_rows(
        input_title.bottom,
        visible_audio_row_count(devices.inputs.len()),
        width,
        dpi,
    );
    let settings_top = rows_bottom(&input_rows) + settings_gap;
    let sound_settings = audio_row_rect(settings_top, width, dpi);

    AudioPanelLayout {
        output_title,
        output_rows,
        input_title,
        input_rows,
        sound_settings,
    }
}

fn audio_panel_height(devices: &sysctl::AudioDevices) -> f32 {
    let layout = audio_panel_layout(devices, PANEL_W, 1.0);
    (layout.sound_settings.bottom + PAD).max(PANEL_H)
}

fn visible_network_row_count(networks: &sysctl::WifiNetworks) -> usize {
    if networks.networks.is_empty() {
        1
    } else {
        networks.networks.len().min(NETWORK_MAX_ROWS)
    }
}

fn network_rows(start_top: f32, count: usize, width: f32, dpi: f32) -> Vec<D2D_RECT_F> {
    (0..count)
        .map(|index| {
            audio_row_rect(
                start_top + index as f32 * (ROW_H + ROW_GAP) * dpi,
                width,
                dpi,
            )
        })
        .collect()
}

#[cfg(test)]
fn network_panel_layout(
    networks: &sysctl::WifiNetworks,
    width: f32,
    dpi: f32,
) -> NetworkPanelLayout {
    network_panel_layout_for_selection(networks, None, width, dpi)
}

fn network_panel_layout_for_selection(
    networks: &sysctl::WifiNetworks,
    selected: Option<usize>,
    width: f32,
    dpi: f32,
) -> NetworkPanelLayout {
    let content_top = NETWORK_CONTENT_TOP * dpi;
    let rows = network_rows(content_top, visible_network_row_count(networks), width, dpi);
    let mut top = rows_bottom(&rows) + NETWORK_SETTINGS_GAP * dpi;
    let mut password = rect(0.0, 0.0, 0.0, 0.0);
    let mut connect = rect(0.0, 0.0, 0.0, 0.0);

    if selected.is_some() {
        password = audio_row_rect(top, width, dpi);
        top = password.bottom + ROW_GAP * dpi;
        connect = audio_row_rect(top, width, dpi);
        top = connect.bottom + NETWORK_SETTINGS_GAP * dpi;
    }

    let status = rect(PAD * dpi, top, width - PAD * dpi, top + STATUS_H * dpi);
    top = status.bottom + ROW_GAP * dpi;
    let network_settings = audio_row_rect(top, width, dpi);
    let refresh = rect(
        width - (PAD + 68.0) * dpi,
        HEADER_TOP * dpi,
        width - PAD * dpi,
        (HEADER_TOP + HEADER_H) * dpi,
    );

    NetworkPanelLayout {
        rows,
        password,
        connect,
        refresh,
        status,
        network_settings,
    }
}

fn network_panel_height(networks: &sysctl::WifiNetworks) -> f32 {
    network_panel_height_for_selection(networks, None)
}

fn network_panel_height_for_selection(
    networks: &sysctl::WifiNetworks,
    selected: Option<usize>,
) -> f32 {
    let layout = network_panel_layout_for_selection(networks, selected, PANEL_W, 1.0);
    (layout.network_settings.bottom + PAD).max(PANEL_H)
}

fn masked_password(password: &str) -> String {
    "\u{2022}".repeat(password.chars().count())
}

fn simple_visible_row_count(count: usize, max_rows: usize) -> usize {
    if count == 0 {
        1
    } else {
        count.min(max_rows)
    }
}

fn simple_rows(start_top: f32, count: usize, width: f32, dpi: f32) -> Vec<D2D_RECT_F> {
    (0..count)
        .map(|index| {
            audio_row_rect(
                start_top + index as f32 * (ROW_H + ROW_GAP) * dpi,
                width,
                dpi,
            )
        })
        .collect()
}

fn header_refresh_rect(width: f32, dpi: f32) -> D2D_RECT_F {
    rect(
        width - (PAD + 68.0) * dpi,
        HEADER_TOP * dpi,
        width - PAD * dpi,
        (HEADER_TOP + HEADER_H) * dpi,
    )
}

fn bluetooth_panel_layout(
    devices: &sysctl::BluetoothDevices,
    width: f32,
    dpi: f32,
) -> BluetoothPanelLayout {
    let rows = simple_rows(
        BLUETOOTH_CONTENT_TOP * dpi,
        simple_visible_row_count(devices.devices.len(), BLUETOOTH_MAX_ROWS),
        width,
        dpi,
    );
    let mut top = rows_bottom(&rows) + SIMPLE_PANEL_SETTINGS_GAP * dpi;
    let status = rect(PAD * dpi, top, width - PAD * dpi, top + STATUS_H * dpi);
    top = status.bottom + ROW_GAP * dpi;
    let bluetooth_settings = audio_row_rect(top, width, dpi);

    BluetoothPanelLayout {
        rows,
        refresh: header_refresh_rect(width, dpi),
        status,
        bluetooth_settings,
    }
}

fn bluetooth_panel_height(devices: &sysctl::BluetoothDevices) -> f32 {
    let layout = bluetooth_panel_layout(devices, PANEL_W, 1.0);
    (layout.bluetooth_settings.bottom + PAD).max(PANEL_BLUETOOTH_H)
}

fn input_panel_layout(methods: &sysctl::InputMethods, width: f32, dpi: f32) -> InputPanelLayout {
    let rows = simple_rows(
        INPUT_CONTENT_TOP * dpi,
        simple_visible_row_count(methods.methods.len(), INPUT_MAX_ROWS),
        width,
        dpi,
    );
    let mut top = rows_bottom(&rows) + SIMPLE_PANEL_SETTINGS_GAP * dpi;
    let status = rect(PAD * dpi, top, width - PAD * dpi, top + STATUS_H * dpi);
    top = status.bottom + ROW_GAP * dpi;
    let system_switch = audio_row_rect(top, width, dpi);
    top = system_switch.bottom + ROW_GAP * dpi;
    let input_settings = audio_row_rect(top, width, dpi);

    InputPanelLayout {
        rows,
        status,
        system_switch,
        input_settings,
    }
}

fn input_panel_height(methods: &sysctl::InputMethods) -> f32 {
    let layout = input_panel_layout(methods, PANEL_W, 1.0);
    (layout.input_settings.bottom + PAD).max(PANEL_INPUT_H)
}

fn main_action_at(lay: &Layout, x: f32, y: f32) -> PanelAction {
    if in_rect(&lay.vol_glyph, x, y) {
        return PanelAction::ToggleMute;
    }
    if in_rect(&lay.vol_row, x, y) {
        return PanelAction::StartVolumeDrag;
    }
    if in_rect(&lay.audio_button, x, y) {
        return PanelAction::ShowAudioPanel;
    }
    for (i, rc) in lay.buttons.iter().enumerate() {
        if in_rect(rc, x, y) {
            return match i {
                0 => PanelAction::ShowNetworkPanel,
                1 => PanelAction::ShowBluetoothPanel,
                _ => PanelAction::ShowInputPanel,
            };
        }
    }
    if in_rect(&lay.battery_status, x, y) {
        return PanelAction::ShowBatteryPanel;
    }
    if in_rect(&lay.clock_status, x, y) {
        return PanelAction::OpenDateTimeSettings;
    }
    PanelAction::None
}

fn audio_action_at(
    devices: &sysctl::AudioDevices,
    width: f32,
    height: f32,
    dpi: f32,
    x: f32,
    y: f32,
) -> PanelAction {
    let lay = layout(width, height, dpi);
    let audio_lay = audio_panel_layout(devices, width, dpi);
    if in_rect(&lay.back, x, y) {
        return PanelAction::BackToMain;
    }
    for (index, _) in devices.outputs.iter().take(AUDIO_MAX_ROWS).enumerate() {
        if in_rect(&audio_lay.output_rows[index], x, y) {
            return PanelAction::SelectAudioDevice {
                flow: sysctl::AudioFlow::Output,
                index,
            };
        }
    }
    for (index, _) in devices.inputs.iter().take(AUDIO_MAX_ROWS).enumerate() {
        if in_rect(&audio_lay.input_rows[index], x, y) {
            return PanelAction::SelectAudioDevice {
                flow: sysctl::AudioFlow::Input,
                index,
            };
        }
    }
    if in_rect(&audio_lay.sound_settings, x, y) {
        return PanelAction::OpenSoundSettings;
    }
    PanelAction::None
}

fn battery_action_at(lay: &Layout, x: f32, y: f32) -> PanelAction {
    if in_rect(&lay.back, x, y) {
        return PanelAction::BackToMain;
    }
    if in_rect(&lay.battery_saver, x, y) {
        return PanelAction::OpenBatterySaverSettings;
    }
    if in_rect(&lay.power_settings, x, y) {
        return PanelAction::OpenPowerSettings;
    }
    PanelAction::None
}

#[cfg(test)]
fn network_action_at(
    networks: &sysctl::WifiNetworks,
    width: f32,
    height: f32,
    dpi: f32,
    x: f32,
    y: f32,
) -> PanelAction {
    network_action_at_for_selection(networks, None, width, height, dpi, x, y)
}

fn network_action_at_for_selection(
    networks: &sysctl::WifiNetworks,
    selected: Option<usize>,
    width: f32,
    height: f32,
    dpi: f32,
    x: f32,
    y: f32,
) -> PanelAction {
    let lay = layout(width, height, dpi);
    let network_lay = network_panel_layout_for_selection(networks, selected, width, dpi);
    if in_rect(&lay.back, x, y) {
        return PanelAction::BackToMain;
    }
    if in_rect(&network_lay.refresh, x, y) {
        return PanelAction::RefreshWifi;
    }
    for (index, network) in networks.networks.iter().take(NETWORK_MAX_ROWS).enumerate() {
        if in_rect(&network_lay.rows[index], x, y) {
            if !network.connectable {
                return PanelAction::OpenNetworkSettings;
            }
            if network.secure && !network.has_profile {
                return PanelAction::SelectWifiForPassword { index };
            }
            return PanelAction::ConnectWifi { index };
        }
    }
    if selected.is_some() && in_rect(&network_lay.password, x, y) {
        return PanelAction::FocusWifiPassword;
    }
    if selected.is_some() && in_rect(&network_lay.connect, x, y) {
        return PanelAction::ConnectSelectedWifi;
    }
    if in_rect(&network_lay.network_settings, x, y) {
        return PanelAction::OpenNetworkSettings;
    }
    PanelAction::None
}

fn bluetooth_action_at(
    devices: &sysctl::BluetoothDevices,
    width: f32,
    height: f32,
    dpi: f32,
    x: f32,
    y: f32,
) -> PanelAction {
    let lay = layout(width, height, dpi);
    let bluetooth_lay = bluetooth_panel_layout(devices, width, dpi);
    if in_rect(&lay.back, x, y) {
        return PanelAction::BackToMain;
    }
    if in_rect(&bluetooth_lay.refresh, x, y) {
        return PanelAction::RefreshBluetooth;
    }
    for (index, _) in devices.devices.iter().take(BLUETOOTH_MAX_ROWS).enumerate() {
        if in_rect(&bluetooth_lay.rows[index], x, y) {
            return PanelAction::OpenBluetoothDevice { index };
        }
    }
    if in_rect(&bluetooth_lay.bluetooth_settings, x, y) {
        return PanelAction::OpenBluetoothSettings;
    }
    PanelAction::None
}

fn input_action_at(
    methods: &sysctl::InputMethods,
    width: f32,
    height: f32,
    dpi: f32,
    x: f32,
    y: f32,
) -> PanelAction {
    let lay = layout(width, height, dpi);
    let input_lay = input_panel_layout(methods, width, dpi);
    if in_rect(&lay.back, x, y) {
        return PanelAction::BackToMain;
    }
    for (index, _) in methods.methods.iter().take(INPUT_MAX_ROWS).enumerate() {
        if in_rect(&input_lay.rows[index], x, y) {
            return PanelAction::SelectInputMethod { index };
        }
    }
    if in_rect(&input_lay.system_switch, x, y) {
        return PanelAction::CycleInputMethod;
    }
    if in_rect(&input_lay.input_settings, x, y) {
        return PanelAction::OpenInputSettings;
    }
    PanelAction::None
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

fn default_output_name(panel: &Panel) -> String {
    panel
        .audio_devices
        .outputs
        .iter()
        .find(|device| device.is_default)
        .or_else(|| panel.audio_devices.outputs.first())
        .map(|device| device.name.clone())
        .unwrap_or_else(|| "打开声音设置".to_string())
}

fn battery_text(battery: sysctl::Battery) -> String {
    if !battery.present {
        "未检测到电池".to_string()
    } else if battery.charging {
        format!("充电中 {}%", battery.percent)
    } else {
        format!("电池 {}%", battery.percent)
    }
}

unsafe fn draw_panel_background(panel: &Panel, a: f32) {
    let dpi = panel.dpi;
    let dc = panel.glass.dc();
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
    stroke_round(
        dc,
        &panel.brush,
        bg,
        RADIUS * dpi,
        1.0 * dpi,
        rgba(1.0, 1.0, 1.0, 0.12 * a),
    );
}

unsafe fn draw_main(panel: &Panel, lay: &Layout, a: f32) {
    let dpi = panel.dpi;
    let dc = panel.glass.dc();

    let (level, muted) = (panel.vol_level, panel.vol_muted);
    let speaker = if muted || level <= 0.0001 {
        "\u{1F507}"
    } else {
        "\u{1F50A}"
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
        fill_round(
            dc,
            &panel.brush,
            rect(bar.left, bar.top, fill_right, bar.bottom),
            bar_radius,
            rgba(0.36, 0.64, 0.99, 0.95 * a),
        );
    }
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

    draw_row(
        panel,
        lay.audio_button,
        "声音设备",
        Some(default_output_name(panel).as_str()),
        false,
        panel.hovered == PanelAction::ShowAudioPanel,
        a,
    );

    for (i, &rc) in lay.buttons.iter().enumerate() {
        let action = match i {
            0 => PanelAction::ShowNetworkPanel,
            1 => PanelAction::ShowBluetoothPanel,
            _ => PanelAction::ShowInputPanel,
        };
        let hot = panel.hovered == action;
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

    if panel.hovered == PanelAction::ShowBatteryPanel {
        fill_round(
            dc,
            &panel.brush,
            lay.battery_status,
            8.0 * dpi,
            rgba(1.0, 1.0, 1.0, 0.08 * a),
        );
    }
    if panel.hovered == PanelAction::OpenDateTimeSettings {
        fill_round(
            dc,
            &panel.brush,
            lay.clock_status,
            8.0 * dpi,
            rgba(1.0, 1.0, 1.0, 0.08 * a),
        );
    }
    draw_text(
        dc,
        &panel.brush,
        &battery_text(panel.battery),
        &panel.status_left,
        lay.status,
        rgba(0.78, 0.80, 0.84, 0.92 * a),
        false,
    );
    let (hm, md) = &panel.clock;
    draw_text(
        dc,
        &panel.brush,
        &format!("{}   {}", hm, md),
        &panel.status_right,
        lay.status,
        rgba(0.78, 0.80, 0.84, 0.92 * a),
        false,
    );
}

unsafe fn draw_header(panel: &Panel, lay: &Layout, title: &str, a: f32) {
    let dpi = panel.dpi;
    let dc = panel.glass.dc();
    let hot = panel.hovered == PanelAction::BackToMain;
    fill_round(
        dc,
        &panel.brush,
        lay.back,
        9.0 * dpi,
        rgba(1.0, 1.0, 1.0, if hot { 0.14 } else { 0.07 } * a),
    );
    draw_text(
        dc,
        &panel.brush,
        "<",
        &panel.label,
        lay.back,
        rgba(0.95, 0.95, 0.97, 0.96 * a),
        false,
    );
    draw_text(
        dc,
        &panel.brush,
        title,
        &panel.title,
        rect(
            lay.back.right + 8.0 * dpi,
            lay.back.top,
            panel.width - PAD * dpi,
            lay.back.bottom,
        ),
        rgba(0.96, 0.96, 0.98, 0.97 * a),
        false,
    );
}

unsafe fn draw_section_title_rect(panel: &Panel, text: &str, rc: D2D_RECT_F, a: f32) {
    draw_text(
        panel.glass.dc(),
        &panel.brush,
        text,
        &panel.small,
        rc,
        rgba(0.62, 0.65, 0.70, 0.92 * a),
        false,
    );
}

unsafe fn draw_row(
    panel: &Panel,
    rc: D2D_RECT_F,
    title: &str,
    subtitle: Option<&str>,
    checked: bool,
    hot: bool,
    a: f32,
) {
    let dpi = panel.dpi;
    let dc = panel.glass.dc();
    let fill_color = if checked {
        if hot {
            rgba(0.18, 0.52, 0.34, 0.34 * a)
        } else {
            rgba(0.12, 0.42, 0.27, 0.25 * a)
        }
    } else {
        rgba(1.0, 1.0, 1.0, if hot { 0.15 } else { 0.08 } * a)
    };
    let stroke_color = if checked {
        rgba(0.32, 0.78, 0.50, 0.42 * a)
    } else {
        rgba(1.0, 1.0, 1.0, 0.08 * a)
    };
    fill_round(dc, &panel.brush, rc, 10.0 * dpi, fill_color);
    stroke_round(dc, &panel.brush, rc, 10.0 * dpi, 1.0 * dpi, stroke_color);

    let text_right = rc.right - 28.0 * dpi;
    let left = rc.left + 12.0 * dpi;
    if let Some(subtitle) = subtitle {
        draw_text(
            dc,
            &panel.brush,
            title,
            &panel.small,
            rect(left, rc.top + 3.0 * dpi, text_right, rc.top + 19.0 * dpi),
            rgba(0.95, 0.95, 0.97, 0.96 * a),
            false,
        );
        draw_text(
            dc,
            &panel.brush,
            subtitle,
            &panel.small,
            rect(left, rc.top + 18.0 * dpi, text_right, rc.bottom - 2.0 * dpi),
            rgba(0.66, 0.69, 0.74, 0.92 * a),
            false,
        );
    } else {
        draw_text(
            dc,
            &panel.brush,
            title,
            &panel.small,
            rect(left, rc.top, text_right, rc.bottom),
            rgba(0.95, 0.95, 0.97, 0.96 * a),
            false,
        );
    }

    draw_text(
        dc,
        &panel.brush,
        if checked { "✓" } else { ">" },
        &panel.label,
        rect(
            rc.right - 28.0 * dpi,
            rc.top,
            rc.right - 8.0 * dpi,
            rc.bottom,
        ),
        rgba(0.82, 0.86, 0.92, 0.88 * a),
        false,
    );
}

unsafe fn draw_empty_row(panel: &Panel, rc: D2D_RECT_F, text: &str, a: f32) {
    draw_text(
        panel.glass.dc(),
        &panel.brush,
        text,
        &panel.small,
        rc,
        rgba(0.62, 0.65, 0.70, 0.86 * a),
        false,
    );
}

unsafe fn draw_password_field(panel: &Panel, rc: D2D_RECT_F, active: bool, a: f32) {
    let dpi = panel.dpi;
    let dc = panel.glass.dc();
    fill_round(
        dc,
        &panel.brush,
        rc,
        10.0 * dpi,
        rgba(1.0, 1.0, 1.0, if active { 0.14 } else { 0.08 } * a),
    );
    stroke_round(
        dc,
        &panel.brush,
        rc,
        10.0 * dpi,
        1.0 * dpi,
        rgba(0.36, 0.64, 0.99, if active { 0.56 } else { 0.10 } * a),
    );
    let left = rc.left + 12.0 * dpi;
    let mid = rc.left + 70.0 * dpi;
    draw_text(
        dc,
        &panel.brush,
        "密码",
        &panel.small,
        rect(left, rc.top, mid, rc.bottom),
        rgba(0.72, 0.75, 0.80, 0.94 * a),
        false,
    );
    let shown = if panel.wifi_password.is_empty() {
        "输入网络密码".to_string()
    } else {
        masked_password(&panel.wifi_password)
    };
    draw_text(
        dc,
        &panel.brush,
        &shown,
        &panel.small,
        rect(mid, rc.top, rc.right - 12.0 * dpi, rc.bottom),
        rgba(0.95, 0.95, 0.97, 0.96 * a),
        false,
    );
}

unsafe fn draw_audio(panel: &Panel, lay: &Layout, a: f32) {
    let audio_lay = audio_panel_layout(&panel.audio_devices, panel.width, panel.dpi);
    draw_header(panel, lay, "声音", a);
    draw_section_title_rect(panel, "输出设备", audio_lay.output_title, a);
    if panel.audio_devices.outputs.is_empty() {
        draw_empty_row(panel, audio_lay.output_rows[0], "没有可用输出设备", a);
    } else {
        for (index, device) in panel
            .audio_devices
            .outputs
            .iter()
            .take(AUDIO_MAX_ROWS)
            .enumerate()
        {
            let action = PanelAction::SelectAudioDevice {
                flow: sysctl::AudioFlow::Output,
                index,
            };
            draw_row(
                panel,
                audio_lay.output_rows[index],
                &device.name,
                if device.is_default {
                    Some("当前输出")
                } else {
                    None
                },
                device.is_default,
                panel.hovered == action,
                a,
            );
        }
    }

    draw_section_title_rect(panel, "输入设备", audio_lay.input_title, a);
    if panel.audio_devices.inputs.is_empty() {
        draw_empty_row(panel, audio_lay.input_rows[0], "没有可用输入设备", a);
    } else {
        for (index, device) in panel
            .audio_devices
            .inputs
            .iter()
            .take(AUDIO_MAX_ROWS)
            .enumerate()
        {
            let action = PanelAction::SelectAudioDevice {
                flow: sysctl::AudioFlow::Input,
                index,
            };
            draw_row(
                panel,
                audio_lay.input_rows[index],
                &device.name,
                if device.is_default {
                    Some("当前输入")
                } else {
                    None
                },
                device.is_default,
                panel.hovered == action,
                a,
            );
        }
    }

    draw_row(
        panel,
        audio_lay.sound_settings,
        "更多声音设置",
        Some("应用音量、空间音效和高级设备"),
        false,
        panel.hovered == PanelAction::OpenSoundSettings,
        a,
    );
}

fn wifi_subtitle(network: &sysctl::WifiNetworkInfo) -> String {
    let state = if network.connected {
        "已连接"
    } else if !network.connectable {
        "不可连接"
    } else if network.has_profile {
        "已保存"
    } else if network.secure {
        "需要密码"
    } else {
        "开放网络"
    };
    format!("{} · 信号 {}%", state, network.signal)
}

unsafe fn draw_network(panel: &Panel, lay: &Layout, a: f32) {
    let network_lay = network_panel_layout_for_selection(
        &panel.wifi_networks,
        panel.selected_wifi,
        panel.width,
        panel.dpi,
    );
    draw_header(panel, lay, "网络", a);

    fill_round(
        panel.glass.dc(),
        &panel.brush,
        network_lay.refresh,
        9.0 * panel.dpi,
        rgba(
            1.0,
            1.0,
            1.0,
            if panel.hovered == PanelAction::RefreshWifi {
                0.14
            } else {
                0.07
            } * a,
        ),
    );
    draw_text(
        panel.glass.dc(),
        &panel.brush,
        "刷新",
        &panel.small,
        network_lay.refresh,
        rgba(0.95, 0.95, 0.97, 0.96 * a),
        false,
    );

    if !panel.wifi_networks.available {
        draw_empty_row(panel, network_lay.rows[0], "未检测到 Wi-Fi 适配器", a);
    } else if panel.wifi_networks.networks.is_empty() {
        draw_empty_row(panel, network_lay.rows[0], "未找到可用网络", a);
    } else {
        for (index, network) in panel
            .wifi_networks
            .networks
            .iter()
            .take(NETWORK_MAX_ROWS)
            .enumerate()
        {
            let action = if !network.connectable {
                PanelAction::OpenNetworkSettings
            } else if network.secure && !network.has_profile {
                PanelAction::SelectWifiForPassword { index }
            } else {
                PanelAction::ConnectWifi { index }
            };
            let subtitle = wifi_subtitle(network);
            draw_row(
                panel,
                network_lay.rows[index],
                &network.ssid,
                Some(&subtitle),
                network.connected,
                panel.hovered == action || panel.selected_wifi == Some(index),
                a,
            );
        }
    }

    if let Some(index) = panel.selected_wifi {
        if let Some(network) = panel.wifi_networks.networks.get(index) {
            draw_password_field(
                panel,
                network_lay.password,
                panel.wifi_password_active || panel.hovered == PanelAction::FocusWifiPassword,
                a,
            );
            draw_row(
                panel,
                network_lay.connect,
                "连接",
                Some(&network.ssid),
                false,
                panel.hovered == PanelAction::ConnectSelectedWifi,
                a,
            );
        }
    }

    let status = panel
        .wifi_message
        .as_deref()
        .or(panel.wifi_networks.status.as_deref())
        .or_else(|| {
            (panel.wifi_networks.networks.len() > NETWORK_MAX_ROWS)
                .then_some("还有更多网络，打开设置查看更多")
        });
    if let Some(status) = status {
        draw_text(
            panel.glass.dc(),
            &panel.brush,
            status,
            &panel.small,
            network_lay.status,
            rgba(0.62, 0.65, 0.70, 0.88 * a),
            false,
        );
    }

    draw_row(
        panel,
        network_lay.network_settings,
        "更多网络设置",
        Some("飞行模式、代理和高级网络选项"),
        false,
        panel.hovered == PanelAction::OpenNetworkSettings,
        a,
    );
}

fn bluetooth_subtitle(device: &sysctl::BluetoothDeviceInfo) -> &'static str {
    if device.connected {
        "Connected"
    } else if device.authenticated {
        "Paired"
    } else if device.remembered {
        "Saved"
    } else {
        "Discovered"
    }
}

unsafe fn draw_bluetooth(panel: &Panel, lay: &Layout, a: f32) {
    let bluetooth_lay = bluetooth_panel_layout(&panel.bluetooth_devices, panel.width, panel.dpi);
    draw_header(panel, lay, "Bluetooth", a);
    fill_round(
        panel.glass.dc(),
        &panel.brush,
        bluetooth_lay.refresh,
        9.0 * panel.dpi,
        rgba(
            1.0,
            1.0,
            1.0,
            if panel.hovered == PanelAction::RefreshBluetooth {
                0.14
            } else {
                0.07
            } * a,
        ),
    );
    draw_text(
        panel.glass.dc(),
        &panel.brush,
        "Refresh",
        &panel.small,
        bluetooth_lay.refresh,
        rgba(0.95, 0.95, 0.97, 0.96 * a),
        false,
    );

    if !panel.bluetooth_devices.available {
        draw_empty_row(
            panel,
            bluetooth_lay.rows[0],
            "No Bluetooth adapter detected",
            a,
        );
    } else if panel.bluetooth_devices.devices.is_empty() {
        draw_empty_row(
            panel,
            bluetooth_lay.rows[0],
            "No paired Bluetooth devices",
            a,
        );
    } else {
        for (index, device) in panel
            .bluetooth_devices
            .devices
            .iter()
            .take(BLUETOOTH_MAX_ROWS)
            .enumerate()
        {
            draw_row(
                panel,
                bluetooth_lay.rows[index],
                &device.name,
                Some(bluetooth_subtitle(device)),
                device.connected,
                panel.hovered == (PanelAction::OpenBluetoothDevice { index }),
                a,
            );
        }
    }

    let status = panel
        .bluetooth_message
        .as_deref()
        .or(panel.bluetooth_devices.status.as_deref())
        .or(panel.bluetooth_devices.radio_name.as_deref())
        .or_else(|| {
            (panel.bluetooth_devices.devices.len() > BLUETOOTH_MAX_ROWS)
                .then_some("More devices are available in Settings")
        });
    if let Some(status) = status {
        draw_text(
            panel.glass.dc(),
            &panel.brush,
            status,
            &panel.small,
            bluetooth_lay.status,
            rgba(0.62, 0.65, 0.70, 0.88 * a),
            false,
        );
    }

    draw_row(
        panel,
        bluetooth_lay.bluetooth_settings,
        "More Bluetooth settings",
        Some("Add devices and manage advanced options"),
        false,
        panel.hovered == PanelAction::OpenBluetoothSettings,
        a,
    );
}

unsafe fn draw_input(panel: &Panel, lay: &Layout, a: f32) {
    let input_lay = input_panel_layout(&panel.input_methods, panel.width, panel.dpi);
    draw_header(panel, lay, "Input", a);

    if panel.input_methods.methods.is_empty() {
        draw_empty_row(
            panel,
            input_lay.rows[0],
            "No switchable input methods detected",
            a,
        );
    } else {
        for (index, method) in panel
            .input_methods
            .methods
            .iter()
            .take(INPUT_MAX_ROWS)
            .enumerate()
        {
            draw_row(
                panel,
                input_lay.rows[index],
                &method.label,
                Some(&method.id),
                method.active,
                panel.hovered == (PanelAction::SelectInputMethod { index }),
                a,
            );
        }
    }

    let status = panel
        .input_message
        .as_deref()
        .or(panel.input_methods.status.as_deref())
        .or_else(|| {
            (panel.input_methods.methods.len() > INPUT_MAX_ROWS)
                .then_some("More input methods are available in Settings")
        });
    if let Some(status) = status {
        draw_text(
            panel.glass.dc(),
            &panel.brush,
            status,
            &panel.small,
            input_lay.status,
            rgba(0.62, 0.65, 0.70, 0.88 * a),
            false,
        );
    }

    draw_row(
        panel,
        input_lay.system_switch,
        "System input switch",
        Some("Use Win + Space"),
        false,
        panel.hovered == PanelAction::CycleInputMethod,
        a,
    );
    draw_row(
        panel,
        input_lay.input_settings,
        "More input settings",
        Some("Language, keyboard and typing options"),
        false,
        panel.hovered == PanelAction::OpenInputSettings,
        a,
    );
}

unsafe fn draw_battery(panel: &Panel, lay: &Layout, a: f32) {
    draw_header(panel, lay, "电池", a);
    let dpi = panel.dpi;
    draw_text(
        panel.glass.dc(),
        &panel.brush,
        &battery_text(panel.battery),
        &panel.title,
        rect(PAD * dpi, 54.0 * dpi, panel.width - PAD * dpi, 78.0 * dpi),
        rgba(0.95, 0.95, 0.97, 0.96 * a),
        false,
    );
    draw_row(
        panel,
        lay.battery_saver,
        "低电量模式",
        Some("打开 Windows 节电设置"),
        false,
        panel.hovered == PanelAction::OpenBatterySaverSettings,
        a,
    );
    draw_row(
        panel,
        lay.power_settings,
        "电源设置",
        Some("睡眠、屏幕和电源模式"),
        false,
        panel.hovered == PanelAction::OpenPowerSettings,
        a,
    );
}

unsafe fn render(panel: &Panel) {
    let open_a = smoothstep(panel.anim_start.elapsed().as_secs_f32() / ANIM_SECS);
    let view_elapsed = panel.view_anim_start.elapsed().as_secs_f32();
    let view_a = view_content_alpha(view_elapsed);
    let dpi = panel.dpi;
    let lay = layout(panel.width, panel.height, dpi);
    let dc = panel.glass.dc();

    dc.BeginDraw();
    dc.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
    let panel_ty = (1.0 - open_a) * 8.0 * dpi;
    dc.SetTransform(&Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 0.0,
        M32: panel_ty,
    });

    draw_panel_background(panel, open_a);
    let content_ty = panel_ty + view_content_offset_y(view_elapsed, panel.view_direction, dpi);
    dc.SetTransform(&Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 0.0,
        M32: content_ty,
    });
    let a = open_a * view_a;
    match panel.view {
        PanelView::Main => draw_main(panel, &lay, a),
        PanelView::Audio => draw_audio(panel, &lay, a),
        PanelView::Battery => draw_battery(panel, &lay, a),
        PanelView::Network => draw_network(panel, &lay, a),
        PanelView::Bluetooth => draw_bluetooth(panel, &lay, a),
        PanelView::Input => draw_input(panel, &lay, a),
    }

    dc.SetTransform(&Matrix3x2::identity());
    let _ = panel.glass.present();
}

unsafe fn wake_owner(panel: &Panel) {
    if !panel.owner.is_invalid() {
        let _ = PostMessageW(panel.owner, crate::WM_ANIMATION_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// Drive the open/view fade. Returns false once both animations have settled.
unsafe fn animate(panel: &Panel) -> bool {
    render(panel);
    panel.anim_start.elapsed().as_secs_f32() < ANIM_SECS
        || panel.view_anim_start.elapsed().as_secs_f32() < ANIM_SECS
}

/// Render one control-center frame from the Dock's shared, vsync-paced animation loop.
pub unsafe fn animate_frame() -> bool {
    let raw = PANEL_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return false;
    }

    let hwnd = HWND(raw as *mut c_void);
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Panel;
    if ptr.is_null() {
        return false;
    }

    animate(&*ptr)
}

unsafe fn handle_press(hwnd: HWND, panel: &mut Panel, x: f32, y: f32) {
    let lay = layout(panel.width, panel.height, panel.dpi);
    match action_at(panel, x, y) {
        PanelAction::ToggleMute => {
            if let Some(muted) = panel.audio.as_ref().map(|audio| {
                let muted = !audio.muted();
                audio.set_muted(muted);
                muted
            }) {
                panel.vol_muted = muted;
            }
            render(panel);
        }
        PanelAction::StartVolumeDrag => {
            panel.dragging = true;
            let _ = SetCapture(hwnd);
            apply_slider(panel, &lay, x);
            render(panel);
        }
        _ => {}
    }
}

unsafe fn apply_slider(panel: &mut Panel, lay: &Layout, x: f32) {
    let bar = lay.vol_bar;
    let level = ((x - bar.left) / (bar.right - bar.left)).clamp(0.0, 1.0);
    if let Some(audio) = &panel.audio {
        audio.set_level(level);
        // set_level unmutes the system when level > 0; keep the UI snapshot in
        // sync so the bar and icon reflect the real state after a mute toggle.
        if level > 0.0 {
            panel.vol_muted = false;
        }
    }
    panel.vol_level = level; // keep the render snapshot in sync with the drag
}

unsafe fn handle_release(hwnd: HWND, panel: &mut Panel, x: f32, y: f32) {
    if panel.dragging {
        panel.dragging = false;
        let _ = ReleaseCapture();
        // Ensure the final slider position is painted after the drag ends.
        apply_slider(panel, &layout(panel.width, panel.height, panel.dpi), x);
        render(panel);
        return;
    }
    let action = action_at(panel, x, y);
    run_action(hwnd, panel, action);
}

fn action_at(panel: &Panel, x: f32, y: f32) -> PanelAction {
    let lay = layout(panel.width, panel.height, panel.dpi);
    match panel.view {
        PanelView::Main => main_action_at(&lay, x, y),
        PanelView::Audio => audio_action_at(
            &panel.audio_devices,
            panel.width,
            panel.height,
            panel.dpi,
            x,
            y,
        ),
        PanelView::Battery => battery_action_at(&lay, x, y),
        PanelView::Network => network_action_at_for_selection(
            &panel.wifi_networks,
            panel.selected_wifi,
            panel.width,
            panel.height,
            panel.dpi,
            x,
            y,
        ),
        PanelView::Bluetooth => bluetooth_action_at(
            &panel.bluetooth_devices,
            panel.width,
            panel.height,
            panel.dpi,
            x,
            y,
        ),
        PanelView::Input => input_action_at(
            &panel.input_methods,
            panel.width,
            panel.height,
            panel.dpi,
            x,
            y,
        ),
    }
}

fn quick_button_count() -> usize {
    BUTTON_LABELS.len()
}

unsafe fn start_wifi_status_refresh(hwnd: HWND, panel: &mut Panel) {
    panel.wifi_pending_refreshes = WIFI_REFRESH_TICKS;
    let _ = SetTimer(hwnd, WIFI_REFRESH_TIMER, WIFI_REFRESH_INTERVAL_MS, None);
}

unsafe fn handle_wifi_status_refresh(hwnd: HWND, panel: &mut Panel) {
    if panel.view != PanelView::Network {
        panel.wifi_pending_refreshes = 0;
        let _ = KillTimer(hwnd, WIFI_REFRESH_TIMER);
        return;
    }
    if panel.wifi_pending_refreshes == 0 {
        let _ = KillTimer(hwnd, WIFI_REFRESH_TIMER);
        return;
    }
    panel.wifi_pending_refreshes -= 1;
    panel.wifi_networks = sysctl::wifi_networks();
    if panel
        .wifi_networks
        .networks
        .iter()
        .any(|network| network.connected)
    {
        panel.wifi_message = panel.wifi_networks.status.clone();
        panel.wifi_pending_refreshes = 0;
        let _ = KillTimer(hwnd, WIFI_REFRESH_TIMER);
    } else if panel.wifi_pending_refreshes == 0 {
        panel.wifi_message = Some("连接状态仍在更新，稍后可刷新确认".to_string());
        let _ = KillTimer(hwnd, WIFI_REFRESH_TIMER);
    }
    render(panel);
}

unsafe fn run_action(hwnd: HWND, panel: &mut Panel, action: PanelAction) {
    match action {
        PanelAction::None | PanelAction::ToggleMute | PanelAction::StartVolumeDrag => {}
        PanelAction::ShowAudioPanel => {
            panel.audio_devices = sysctl::audio_devices();
            set_panel_view(hwnd, panel, PanelView::Audio);
        }
        PanelAction::ShowNetworkPanel => {
            panel.wifi_networks = sysctl::wifi_networks();
            panel.selected_wifi = None;
            panel.wifi_password.clear();
            panel.wifi_password_active = false;
            panel.wifi_message = panel.wifi_networks.status.clone();
            set_panel_view(hwnd, panel, PanelView::Network);
        }
        PanelAction::ShowBluetoothPanel => {
            panel.bluetooth_devices = sysctl::bluetooth_devices();
            panel.bluetooth_message = panel.bluetooth_devices.status.clone();
            set_panel_view(hwnd, panel, PanelView::Bluetooth);
        }
        PanelAction::ShowInputPanel => {
            panel.input_methods = sysctl::input_methods();
            panel.input_message = panel.input_methods.status.clone();
            set_panel_view(hwnd, panel, PanelView::Input);
        }
        PanelAction::ShowBatteryPanel => {
            panel.battery = sysctl::battery();
            set_panel_view(hwnd, panel, PanelView::Battery);
        }
        PanelAction::RefreshWifi => {
            panel.wifi_networks = sysctl::wifi_networks();
            panel.selected_wifi = None;
            panel.wifi_password.clear();
            panel.wifi_password_active = false;
            start_wifi_status_refresh(hwnd, panel);
            panel.wifi_message = panel.wifi_networks.status.clone();
            set_panel_view(hwnd, panel, PanelView::Network);
        }
        PanelAction::RefreshBluetooth => {
            panel.bluetooth_devices = sysctl::bluetooth_devices();
            panel.bluetooth_message = panel.bluetooth_devices.status.clone();
            set_panel_view(hwnd, panel, PanelView::Bluetooth);
        }
        PanelAction::ConnectWifi { index } => {
            let Some(network) = panel.wifi_networks.networks.get(index).cloned() else {
                return;
            };
            match sysctl::connect_wifi(&network, None) {
                Ok(()) => {
                    panel.wifi_message = Some(format!("正在连接 {}", network.ssid));
                    panel.wifi_networks = sysctl::wifi_networks();
                    start_wifi_status_refresh(hwnd, panel);
                }
                Err(error) => panel.wifi_message = Some(error),
            }
            panel.selected_wifi = None;
            panel.wifi_password.clear();
            panel.wifi_password_active = false;
            set_panel_view(hwnd, panel, PanelView::Network);
        }
        PanelAction::SelectWifiForPassword { index } => {
            let Some(network) = panel.wifi_networks.networks.get(index) else {
                return;
            };
            let ssid = network.ssid.clone();
            panel.selected_wifi = Some(index);
            panel.wifi_password.clear();
            panel.wifi_password_active = true;
            panel.wifi_message = Some(format!("输入 {} 的密码", ssid));
            set_panel_view(hwnd, panel, PanelView::Network);
        }
        PanelAction::FocusWifiPassword => {
            panel.wifi_password_active = true;
            render(panel);
        }
        PanelAction::ConnectSelectedWifi => {
            let Some(index) = panel.selected_wifi else {
                return;
            };
            let Some(network) = panel.wifi_networks.networks.get(index).cloned() else {
                return;
            };
            if panel.wifi_password.trim().is_empty() {
                panel.wifi_message = Some("请输入网络密码".to_string());
                panel.wifi_password_active = true;
                render(panel);
                return;
            }
            match sysctl::connect_wifi(&network, Some(&panel.wifi_password)) {
                Ok(()) => {
                    panel.wifi_message = Some(format!("正在连接 {}", network.ssid));
                    panel.wifi_networks = sysctl::wifi_networks();
                    panel.selected_wifi = None;
                    panel.wifi_password.clear();
                    panel.wifi_password_active = false;
                    start_wifi_status_refresh(hwnd, panel);
                    set_panel_view(hwnd, panel, PanelView::Network);
                }
                Err(error) => {
                    panel.wifi_message = Some(error);
                    panel.wifi_password_active = true;
                    render(panel);
                }
            }
        }
        PanelAction::OpenBluetoothDevice { index } => {
            let name = panel
                .bluetooth_devices
                .devices
                .get(index)
                .map(|device| device.name.clone());
            if let Some(name) = name {
                panel.bluetooth_message = Some(format!("Opening Bluetooth settings for {}", name));
            }
            sysctl::open_bluetooth_settings();
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::SelectInputMethod { index } => {
            let Some(method) = panel.input_methods.methods.get(index).cloned() else {
                return;
            };
            if sysctl::select_input_method(&method) {
                panel.input_methods = sysctl::input_methods();
                panel.input_message = Some(format!("Switched to {}", method.label));
                set_panel_view(hwnd, panel, PanelView::Input);
            } else {
                panel.input_message =
                    Some("Input method switch failed; use the system switch".to_string());
                render(panel);
            }
        }
        PanelAction::CycleInputMethod => {
            sysctl::switch_input_method();
            panel.input_methods = sysctl::input_methods();
            panel.input_message = Some("Triggered system input switch".to_string());
            set_panel_view(hwnd, panel, PanelView::Input);
        }
        PanelAction::OpenDateTimeSettings => {
            sysctl::open_date_time_settings();
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::OpenNetworkSettings => {
            panel.wifi_password.clear();
            panel.wifi_password_active = false;
            sysctl::open_uri("ms-settings:network-status");
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::OpenBluetoothSettings => {
            sysctl::open_bluetooth_settings();
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::OpenInputSettings => {
            sysctl::open_input_settings();
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::BackToMain => {
            panel.wifi_password.clear();
            panel.wifi_password_active = false;
            panel.selected_wifi = None;
            set_panel_view(hwnd, panel, PanelView::Main);
        }
        PanelAction::OpenSoundSettings => {
            sysctl::open_sound_settings();
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::OpenBatterySaverSettings => {
            sysctl::open_battery_saver_settings();
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::OpenPowerSettings => {
            sysctl::open_power_settings();
            let _ = DestroyWindow(hwnd);
        }
        PanelAction::SelectAudioDevice { flow, index } => {
            let device = match flow {
                sysctl::AudioFlow::Output => panel.audio_devices.outputs.get(index),
                sysctl::AudioFlow::Input => panel.audio_devices.inputs.get(index),
            };
            let Some(device) = device else {
                return;
            };
            if sysctl::set_default_audio_device(flow, &device.id) {
                panel.audio_devices = sysctl::audio_devices();
                if flow == sysctl::AudioFlow::Output {
                    panel.audio = AudioControl::open();
                    let (vol_level, vol_muted) = panel
                        .audio
                        .as_ref()
                        .map(|audio| (audio.level(), audio.muted()))
                        .unwrap_or((panel.vol_level, panel.vol_muted));
                    panel.vol_level = vol_level;
                    panel.vol_muted = vol_muted;
                }
                render(panel);
            } else {
                sysctl::open_sound_settings();
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

unsafe fn set_panel_view(hwnd: HWND, panel: &mut Panel, view: PanelView) {
    let (width, height) = panel_size_for_view(
        view,
        panel.dpi,
        &panel.audio_devices,
        &panel.wifi_networks,
        panel.selected_wifi,
        &panel.bluetooth_devices,
        &panel.input_methods,
    );
    let width_i = width.round() as i32;
    let height_i = height.round() as i32;
    let mut wr = RECT::default();
    if GetWindowRect(hwnd, &mut wr).is_ok() {
        let mut x = wr.left;
        let mut y = wr.bottom - height_i;
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor = MonitorFromPoint(
            POINT {
                x: wr.left + (wr.right - wr.left) / 2,
                y: wr.bottom,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let margin = (8.0 * panel.dpi) as i32;
            x = x.clamp(
                info.rcWork.left + margin,
                info.rcWork.right - width_i - margin,
            );
            y = y.max(info.rcWork.top + margin);
        }
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, width_i, height_i, SWP_NOACTIVATE);
    } else {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            width_i,
            height_i,
            SWP_NOACTIVATE | SWP_NOMOVE,
        );
    }
    let _ = panel.glass.resize(width_i as u32, height_i as u32);
    panel.view_direction = view_switch_direction(panel.view, view);
    panel.view = view;
    panel.width = width_i as f32;
    panel.height = height_i as f32;
    panel.hovered = PanelAction::None;
    panel.dragging = false;
    panel.view_anim_start =
        Instant::now() - Duration::from_secs_f32(view_switch_initial_animation_elapsed_secs());
    render(panel);
    wake_owner(panel);
}

unsafe fn handle_move(panel: &mut Panel, x: f32, y: f32) {
    let lay = layout(panel.width, panel.height, panel.dpi);
    if panel.dragging {
        apply_slider(panel, &lay, x);
        render(panel);
        return;
    }
    let hovered = action_at(panel, x, y);
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
    let (logical_w, logical_h) = panel_size(PanelView::Main, dpi);
    let width = logical_w.round() as i32;
    let height = logical_h.round() as i32;

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

    let glass = match Glass::new(hwnd, width as u32, height as u32, None) {
        Ok(glass) => glass,
        Err(error) => {
            crate::error_log::write("控制中心 GPU 初始化失败", &error);
            let _ = DestroyWindow(hwnd);
            return;
        }
    };

    let (brush, emoji, title, label, small, status_left, status_right) =
        match resources(glass.dc(), dpi) {
            Ok(resources) => resources,
            Err(error) => {
                crate::error_log::write("控制中心字体初始化失败", &error);
                let _ = DestroyWindow(hwnd);
                return;
            }
        };

    let audio = AudioControl::open();
    let (vol_level, vol_muted) = audio
        .as_ref()
        .map(|a| (a.level(), a.muted()))
        .unwrap_or((0.0, false));
    let panel = Box::into_raw(Box::new(Panel {
        owner: dock_hwnd,
        glass,
        brush,
        emoji,
        title,
        label,
        small,
        status_left,
        status_right,
        view: PanelView::Main,
        audio,
        audio_devices: sysctl::audio_devices(),
        wifi_networks: sysctl::WifiNetworks::default(),
        bluetooth_devices: sysctl::BluetoothDevices::default(),
        input_methods: sysctl::InputMethods::default(),
        selected_wifi: None,
        wifi_password: String::new(),
        wifi_password_active: false,
        wifi_message: None,
        wifi_pending_refreshes: 0,
        bluetooth_message: None,
        input_message: None,
        vol_level,
        vol_muted,
        battery: sysctl::battery(),
        clock: sysctl::clock(),
        dpi,
        width: width as f32,
        height: height as f32,
        hovered: PanelAction::None,
        dragging: false,
        anim_start: Instant::now(),
        view_anim_start: Instant::now(),
        view_direction: 1.0,
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, panel as isize);
    PANEL_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    render(&*panel);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    wake_owner(&*panel);
}

/// Build the brush + text formats for the main view and subpanels.
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

    let title = make(w!("Microsoft YaHei UI"), 15.5 * dpi)?;
    title.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
    title.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let label = make(w!("Microsoft YaHei UI"), 14.0 * dpi)?;
    label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    label.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let small = make(w!("Microsoft YaHei UI"), 11.5 * dpi)?;
    small.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
    small.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let status_left = make(w!("Microsoft YaHei UI"), 12.5 * dpi)?;
    status_left.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
    status_left.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    let status_right = make(w!("Microsoft YaHei UI"), 12.5 * dpi)?;
    status_right.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
    status_right.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

    Ok((brush, emoji, title, label, small, status_left, status_right))
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

fn handle_wifi_password_char(panel: &mut Panel, ch: u32) -> bool {
    if panel.view != PanelView::Network || !panel.wifi_password_active {
        return false;
    }
    if ch < 0x20 || ch == 0x7f {
        return false;
    }
    let Some(ch) = char::from_u32(ch) else {
        return false;
    };
    if panel.wifi_password.chars().count() < 63 {
        panel.wifi_password.push(ch);
        panel.wifi_message = None;
    }
    true
}

fn handle_wifi_password_backspace(panel: &mut Panel) -> bool {
    if panel.view != PanelView::Network || !panel.wifi_password_active {
        return false;
    }
    panel.wifi_password.pop();
    panel.wifi_message = None;
    true
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Panel;
        match msg {
            WM_TIMER if wparam.0 == WIFI_REFRESH_TIMER => {
                if !ptr.is_null() {
                    handle_wifi_status_refresh(hwnd, &mut *ptr);
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
            WM_CHAR if !ptr.is_null() => {
                if handle_wifi_password_char(&mut *ptr, wparam.0 as u32) {
                    render(&*ptr);
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_KEYDOWN if !ptr.is_null() && wparam.0 == VK_BACK.0 as usize => {
                if handle_wifi_password_backspace(&mut *ptr) {
                    render(&*ptr);
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_KEYDOWN if !ptr.is_null() && wparam.0 == VK_RETURN.0 as usize => {
                if (*ptr).view == PanelView::Network && (*ptr).wifi_password_active {
                    run_action(hwnd, &mut *ptr, PanelAction::ConnectSelectedWifi);
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(hwnd, WIFI_REFRESH_TIMER);
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
    use super::*;

    #[test]
    fn control_center_uses_shared_frame_loop_instead_of_animation_timer() {
        let source = include_str!("control_center.rs");
        let production = source
            .rsplit_once("#[cfg(test)]")
            .map(|(production, _)| production)
            .expect("control-center production source");

        assert!(production.contains("pub unsafe fn animate_frame()"));
        assert!(!production.contains("SetTimer(hwnd, 1, 16, None)"));
    }

    #[test]
    fn control_center_keeps_only_clear_system_quick_actions() {
        assert_eq!(quick_button_count(), 3);
        assert_eq!(BUTTON_LABELS, ["网络", "蓝牙", "输入法"]);
        assert!(!BUTTON_LABELS.contains(&"托盘"));
    }

    #[test]
    fn main_panel_routes_first_phase_targets() {
        let (w, h) = panel_size(PanelView::Main, 1.0);
        let lay = layout(w, h, 1.0);

        assert_eq!(
            main_action_at(&lay, lay.buttons[0].left + 4.0, lay.buttons[0].top + 4.0),
            PanelAction::ShowNetworkPanel
        );
        assert_eq!(
            main_action_at(&lay, lay.buttons[1].left + 4.0, lay.buttons[1].top + 4.0),
            PanelAction::ShowBluetoothPanel
        );
        assert_eq!(
            main_action_at(&lay, lay.buttons[2].left + 4.0, lay.buttons[2].top + 4.0),
            PanelAction::ShowInputPanel
        );
        assert_eq!(
            main_action_at(
                &lay,
                lay.audio_button.left + 4.0,
                lay.audio_button.top + 4.0
            ),
            PanelAction::ShowAudioPanel
        );
        assert_eq!(
            main_action_at(
                &lay,
                lay.battery_status.left + 4.0,
                lay.battery_status.top + 4.0
            ),
            PanelAction::ShowBatteryPanel
        );
        assert_eq!(
            main_action_at(
                &lay,
                lay.clock_status.left + 4.0,
                lay.clock_status.top + 4.0
            ),
            PanelAction::OpenDateTimeSettings
        );
    }

    #[test]
    fn subpanels_expand_and_return_to_compact_main_panel() {
        let (_, main_h) = panel_size(PanelView::Main, 1.0);
        let (_, audio_h) = panel_size(PanelView::Audio, 1.0);
        let (_, battery_h) = panel_size(PanelView::Battery, 1.0);
        let (_, bluetooth_h) = panel_size(PanelView::Bluetooth, 1.0);
        let (_, input_h) = panel_size(PanelView::Input, 1.0);

        assert!(audio_h > main_h);
        assert!(battery_h > main_h);
        assert!(bluetooth_h > main_h);
        assert!(input_h > main_h);
        assert_eq!(panel_size(PanelView::Main, 1.5).0, PANEL_W * 1.5);
    }

    #[test]
    fn audio_device_rows_route_by_flow_and_index() {
        let devices = sysctl::AudioDevices {
            outputs: vec![
                sysctl::AudioDeviceInfo::test("out-1", "Speakers", true),
                sysctl::AudioDeviceInfo::test("out-2", "Headphones", false),
            ],
            inputs: vec![sysctl::AudioDeviceInfo::test("in-1", "Microphone", true)],
        };
        let w = PANEL_W;
        let h = audio_panel_height(&devices);
        let lay = audio_panel_layout(&devices, w, 1.0);

        assert_eq!(
            audio_action_at(&devices, w, h, 1.0, 20.0, lay.output_rows[1].top + 2.0),
            PanelAction::SelectAudioDevice {
                flow: sysctl::AudioFlow::Output,
                index: 1
            }
        );
        assert_eq!(
            audio_action_at(&devices, w, h, 1.0, 20.0, lay.input_rows[0].top + 2.0),
            PanelAction::SelectAudioDevice {
                flow: sysctl::AudioFlow::Input,
                index: 0
            }
        );
    }

    #[test]
    fn view_switch_uses_content_motion_without_hiding_panel_background() {
        assert_eq!(view_switch_initial_animation_elapsed_secs(), 0.0);
        assert_eq!(
            view_switch_direction(PanelView::Main, PanelView::Bluetooth),
            1.0
        );
        assert_eq!(
            view_switch_direction(PanelView::Bluetooth, PanelView::Main),
            -1.0
        );
        assert!(view_content_alpha(0.0) < view_content_alpha(ANIM_SECS / 2.0));
        assert_eq!(view_content_alpha(ANIM_SECS), 1.0);
        assert!(view_content_offset_y(0.0, 1.0, 1.0) > view_content_offset_y(ANIM_SECS, 1.0, 1.0));
    }

    #[test]
    fn audio_layout_compacts_when_only_one_output_and_input_exist() {
        let devices = sysctl::AudioDevices {
            outputs: vec![sysctl::AudioDeviceInfo::test("out-1", "Speakers", true)],
            inputs: vec![sysctl::AudioDeviceInfo::test("in-1", "Microphone", true)],
        };
        let lay = audio_panel_layout(&devices, PANEL_W, 1.0);
        let output_bottom = lay.output_rows[0].bottom;

        assert!(lay.input_title.top - output_bottom <= 20.0);
        assert!(lay.input_rows[0].bottom + 12.0 <= lay.sound_settings.top);
        assert!(audio_panel_height(&devices) < PANEL_AUDIO_H);
    }

    #[test]
    fn network_rows_route_saved_and_secure_networks_differently() {
        let networks = sysctl::WifiNetworks {
            available: true,
            networks: vec![
                sysctl::WifiNetworkInfo::test("Saved", true, true, 88, true),
                sysctl::WifiNetworkInfo::test("Needs password", true, false, 64, false),
            ],
            status: None,
        };
        let w = PANEL_W;
        let h = network_panel_height(&networks);
        let lay = network_panel_layout(&networks, w, 1.0);

        assert_eq!(
            network_action_at(&networks, w, h, 1.0, 20.0, lay.rows[0].top + 2.0),
            PanelAction::ConnectWifi { index: 0 }
        );
        assert_eq!(
            network_action_at(&networks, w, h, 1.0, 20.0, lay.rows[1].top + 2.0),
            PanelAction::SelectWifiForPassword { index: 1 }
        );
    }

    #[test]
    fn network_password_state_masks_and_routes_connect_button() {
        let networks = sysctl::WifiNetworks {
            available: true,
            networks: vec![sysctl::WifiNetworkInfo::test(
                "Needs password",
                true,
                false,
                64,
                false,
            )],
            status: None,
        };
        let w = PANEL_W;
        let selected = Some(0);
        let lay = network_panel_layout_for_selection(&networks, selected, w, 1.0);

        assert_eq!(masked_password("secret123"), "•••••••••");
        assert_eq!(
            network_action_at_for_selection(
                &networks,
                selected,
                w,
                network_panel_height_for_selection(&networks, selected),
                1.0,
                lay.password.left + 2.0,
                lay.password.top + 2.0,
            ),
            PanelAction::FocusWifiPassword
        );
        assert_eq!(
            network_action_at_for_selection(
                &networks,
                selected,
                w,
                network_panel_height_for_selection(&networks, selected),
                1.0,
                lay.connect.left + 2.0,
                lay.connect.top + 2.0,
            ),
            PanelAction::ConnectSelectedWifi
        );
    }

    #[test]
    fn bluetooth_rows_route_to_device_details_and_settings() {
        let devices = sysctl::BluetoothDevices {
            available: true,
            radio_name: Some("Adapter".to_string()),
            devices: vec![
                sysctl::BluetoothDeviceInfo::test("Keyboard", true, true),
                sysctl::BluetoothDeviceInfo::test("Mouse", false, true),
            ],
            status: None,
        };
        let w = PANEL_W;
        let h = bluetooth_panel_height(&devices);
        let lay = bluetooth_panel_layout(&devices, w, 1.0);

        assert_eq!(
            bluetooth_action_at(&devices, w, h, 1.0, 20.0, lay.rows[0].top + 2.0),
            PanelAction::OpenBluetoothDevice { index: 0 }
        );
        assert_eq!(
            bluetooth_action_at(
                &devices,
                w,
                h,
                1.0,
                lay.bluetooth_settings.left + 2.0,
                lay.bluetooth_settings.top + 2.0,
            ),
            PanelAction::OpenBluetoothSettings
        );
    }

    #[test]
    fn input_rows_route_to_specific_method_and_shortcut_fallback() {
        let methods = sysctl::InputMethods {
            methods: vec![
                sysctl::InputMethodInfo::test("00000804", "中文", true),
                sysctl::InputMethodInfo::test("00000409", "English", false),
            ],
            status: None,
        };
        let w = PANEL_W;
        let h = input_panel_height(&methods);
        let lay = input_panel_layout(&methods, w, 1.0);

        assert_eq!(
            input_action_at(&methods, w, h, 1.0, 20.0, lay.rows[1].top + 2.0),
            PanelAction::SelectInputMethod { index: 1 }
        );
        assert_eq!(
            input_action_at(
                &methods,
                w,
                h,
                1.0,
                lay.system_switch.left + 2.0,
                lay.system_switch.top + 2.0,
            ),
            PanelAction::CycleInputMethod
        );
    }
}
