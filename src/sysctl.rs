//! System integration for the control center: real master-volume control (Core
//! Audio), battery + clock readouts, and helpers to hand off to the native Windows
//! panels for things Win11 no longer lets a process drive directly (network,
//! Bluetooth, input-method switch). Pure-ish wrappers, all failures degrade quietly.

use windows::core::*;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_LWIN, VK_SPACE,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Master volume of the default playback endpoint. Held open while the panel is up
/// so dragging the slider doesn't re-create COM objects on every mouse move.
pub struct AudioControl {
    endpoint: IAudioEndpointVolume,
}

impl AudioControl {
    /// Bind to the current default render endpoint. `None` if there is no output
    /// device (or COM isn't ready) — the panel simply hides the volume row then.
    pub fn open() -> Option<AudioControl> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
            Some(AudioControl { endpoint })
        }
    }

    /// Current level, 0.0..=1.0.
    pub fn level(&self) -> f32 {
        unsafe { self.endpoint.GetMasterVolumeLevelScalar().unwrap_or(0.0) }
    }

    pub fn muted(&self) -> bool {
        unsafe {
            self.endpoint
                .GetMute()
                .map(|b| b.as_bool())
                .unwrap_or(false)
        }
    }

    /// Set the level (clamped). Setting a non-zero level also unmutes, matching the
    /// system slider, so dragging up from a muted state actually produces sound.
    pub fn set_level(&self, level: f32) {
        unsafe {
            let level = level.clamp(0.0, 1.0);
            let _ = self
                .endpoint
                .SetMasterVolumeLevelScalar(level, std::ptr::null());
            if level > 0.0 && self.muted() {
                let _ = self.endpoint.SetMute(false, std::ptr::null());
            }
        }
    }

    pub fn set_muted(&self, muted: bool) {
        unsafe {
            let _ = self.endpoint.SetMute(muted, std::ptr::null());
        }
    }
}

/// Battery snapshot for the status line. `percent` is None on a desktop with no
/// battery (we then just show the clock).
#[derive(Clone, Copy, Default)]
pub struct Battery {
    pub present: bool,
    pub charging: bool,
    pub percent: u8,
}

const BATTERY_FLAG_NO_BATTERY: u8 = 128;
const AC_LINE_ONLINE: u8 = 1;

pub fn battery() -> Battery {
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut status).is_err() {
            return Battery::default();
        }
        let present =
            status.BatteryFlag & BATTERY_FLAG_NO_BATTERY == 0 && status.BatteryFlag != 255;
        Battery {
            present,
            charging: status.ACLineStatus == AC_LINE_ONLINE,
            percent: if status.BatteryLifePercent <= 100 {
                status.BatteryLifePercent
            } else {
                0
            },
        }
    }
}

/// Local wall-clock, formatted as ("16:58", "6月24日") for the status line.
pub fn clock() -> (String, String) {
    unsafe {
        let t: SYSTEMTIME = GetLocalTime();
        (
            format!("{:02}:{:02}", t.wHour, t.wMinute),
            format!("{}月{}日", t.wMonth, t.wDay),
        )
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open a Windows settings page (e.g. `ms-settings:network-status`) or any shell URI.
pub fn open_uri(uri: &str) {
    unsafe {
        let target = wide(uri);
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Press Win + `vk`, then release — used to invoke a system shortcut.
unsafe fn tap_with_win(vk: VIRTUAL_KEY) {
    let inputs = [
        key_event(VK_LWIN, false),
        key_event(vk, false),
        key_event(vk, true),
        key_event(VK_LWIN, true),
    ];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

/// Cycle the input method / keyboard layout (Win+Space), the system shortcut.
pub fn switch_input_method() {
    unsafe { tap_with_win(VK_SPACE) }
}
