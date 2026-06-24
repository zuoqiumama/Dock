//! Application icon loading from the executable resource, with a system fallback.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const APP_ICON_ID: u16 = 1;

fn resource_id(id: u16) -> PCWSTR {
    PCWSTR(id as usize as *const u16)
}

pub unsafe fn load() -> HICON {
    if let Ok(module) = GetModuleHandleW(None) {
        let instance: HINSTANCE = module.into();
        if let Ok(icon) = LoadIconW(instance, resource_id(APP_ICON_ID)) {
            return icon;
        }
    }
    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}
