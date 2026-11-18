//! Opens the native Explorer overflow panel on demand through its taskbar control.

use std::{io, mem::size_of, thread};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
use windows::Win32::System::Com::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const PROVIDER_TIMEOUT_MS: u32 = 700;
// The Win32 notification chevron is a direct Button child of TrayNotifyWnd.
const LEGACY_CHEVRON_ID: i32 = 1502;
const CHEVRON_SELECTORS: [(UIA_PROPERTY_ID, &str); 3] = [
    (UIA_ClassNamePropertyId, "SystemTray.ChevronIconView"),
    (UIA_ClassNamePropertyId, "ChevronIconView"),
    (UIA_AutomationIdPropertyId, "ChevronIconView"),
];
const CHEVRON_NAMES: &[&str] = &[
    "Show hidden icons",
    "Hidden icons",
    "Hidden icon",
    "Notification Chevron",
    "NotificationChevron",
    "显示隐藏的图标",
    "显示隐藏图标",
    "System tray overflow",
    "系统托盘溢出",
];

pub fn request_open(dock: HWND, foreground: HWND) -> io::Result<thread::JoinHandle<()>> {
    let dock = dock.0 as isize;
    let foreground = foreground.0 as isize;
    thread::Builder::new()
        .name("featherdock-native-tray".into())
        .spawn(move || unsafe {
            let result = open_on_worker(HWND(dock as _), HWND(foreground as _));
            if let Err(error) = result {
                crate::error_log::write("无法打开系统托盘面板", &error);
            }
        })
}

unsafe fn open_on_worker(dock: HWND, foreground: HWND) -> Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let result = open_overflow(dock, foreground);
    CoUninitialize();
    result
}

unsafe fn open_overflow(dock: HWND, foreground: HWND) -> Result<()> {
    let taskbar = crate::taskbar::tray_hwnd();
    let shell_pid = window_process(taskbar);
    if shell_pid == 0
        || shell_pid != window_process(GetShellWindow())
        || !invocation_is_current(dock, foreground, taskbar, shell_pid)
    {
        return Ok(());
    }
    let automation: IUIAutomation2 = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
    automation.SetConnectionTimeout(PROVIDER_TIMEOUT_MS)?;
    automation.SetTransactionTimeout(PROVIDER_TIMEOUT_MS)?;

    let chevron = find_chevron(&automation, taskbar)?;
    if chevron.CurrentProcessId()? != shell_pid as i32 || !chevron.CurrentIsEnabled()?.as_bool() {
        return Err(Error::new(E_FAIL, "Explorer 托盘按钮不可用"));
    }
    let can_activate =
        || !overflow_visible() && invocation_is_current(dock, foreground, taskbar, shell_pid);
    if let Ok(pattern) = chevron
        .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)
    {
        if pattern.CurrentExpandCollapseState()? == ExpandCollapseState_Collapsed && can_activate()
        {
            return pattern.Expand();
        }
    } else if let Ok(pattern) =
        chevron.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
    {
        if pattern.CurrentToggleState()? == ToggleState_Off && can_activate() {
            return pattern.Toggle();
        }
    } else if let Ok(pattern) =
        chevron.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
    {
        if can_activate() {
            return pattern.Invoke();
        }
    } else {
        let pattern = chevron.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
            UIA_LegacyIAccessiblePatternId,
        )?;
        if can_activate() {
            return pattern.DoDefaultAction();
        }
    }
    Ok(())
}

unsafe fn find_chevron(automation: &IUIAutomation2, taskbar: HWND) -> Result<IUIAutomationElement> {
    if let Ok(tray) = FindWindowExW(
        taskbar,
        HWND::default(),
        w!("TrayNotifyWnd"),
        PCWSTR::null(),
    ) {
        if let Ok(button) = GetDlgItem(tray, LEGACY_CHEVRON_ID) {
            let mut name = [0; 16];
            let len = GetClassNameW(button, &mut name) as usize;
            if IsWindowVisible(button).as_bool()
                && name[..len].iter().copied().eq("Button".encode_utf16())
            {
                if let Ok(element) = automation.ElementFromHandle(button) {
                    return Ok(element);
                }
            }
        }
    }

    let identifiers = CHEVRON_SELECTORS
        .iter()
        .map(|&(property, value)| {
            automation
                .CreatePropertyCondition(property, &VARIANT::from(BSTR::from(value)))
                .map(Some)
        })
        .collect::<Result<Vec<_>>>()?;
    let names = CHEVRON_NAMES
        .iter()
        .map(|&name| {
            automation
                .CreatePropertyConditionEx(
                    UIA_NamePropertyId,
                    &VARIANT::from(BSTR::from(name)),
                    PropertyConditionFlags_IgnoreCase,
                )
                .map(Some)
        })
        .collect::<Result<Vec<_>>>()?;
    let name = automation.CreateOrConditionFromNativeArray(&names)?;
    let button = automation.CreatePropertyCondition(
        UIA_ControlTypePropertyId,
        &VARIANT::from(UIA_ButtonControlTypeId.0),
    )?;
    let button_name = automation.CreateAndCondition(&name, &button)?;
    let identifier = automation.CreateOrConditionFromNativeArray(&identifiers)?;
    let condition = automation.CreateOrCondition(&identifier, &button_name)?;

    // On Windows 11 the XAML provider exposes its subtree when addressed by
    // the bridge HWND, even when Shell_TrayWnd only exposes the Win32 children.
    if let Ok(bridge) = FindWindowExW(
        taskbar,
        HWND::default(),
        w!("Windows.UI.Composition.DesktopWindowContentBridge"),
        PCWSTR::null(),
    ) {
        if let Ok(element) = automation.ElementFromHandle(bridge) {
            if let Ok(chevron) = element.FindFirst(TreeScope_Descendants, &condition) {
                return Ok(chevron);
            }
        }
    }
    automation
        .ElementFromHandle(taskbar)?
        .FindFirst(TreeScope_Descendants, &condition)
}

unsafe fn invocation_is_current(
    dock: HWND,
    foreground: HWND,
    taskbar: HWND,
    shell_pid: u32,
) -> bool {
    IsWindowVisible(dock).as_bool()
        && GetForegroundWindow() == foreground
        && !fullscreen_foreground_present()
        && crate::taskbar::tray_hwnd() == taskbar
        && window_process(taskbar) == shell_pid
}

pub unsafe fn fullscreen_foreground_present() -> bool {
    let foreground_monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONULL);
    !foreground_monitor.is_invalid()
        && crate::windows_list::is_fullscreen_present(foreground_monitor)
}

unsafe fn window_process(window: HWND) -> u32 {
    let mut pid = 0;
    GetWindowThreadProcessId(window, Some(&mut pid));
    pid
}

pub unsafe fn overflow_visible() -> bool {
    let shell_pid = window_process(crate::taskbar::tray_hwnd());
    if shell_pid == 0 {
        return false;
    }
    for class in [
        w!("NotifyIconOverflowWindow"),
        w!("TopLevelWindowForOverflowXamlIsland"),
    ] {
        let mut after = HWND::default();
        while let Ok(window) = FindWindowExW(HWND::default(), after, class, PCWSTR::null()) {
            after = window;
            if window_process(window) != shell_pid || !IsWindowVisible(window).as_bool() {
                continue;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            );
            if cloaked == 0 {
                return true;
            }
        }
    }
    false
}
