//! Run-at-login toggle via the HKCU "Run" registry key.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE: &str = "FeatherDock";

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub unsafe fn is_enabled() -> bool {
    let sub = wide(RUN_KEY);
    let val = wide(VALUE);
    let mut size = 0u32;
    if RegGetValueW(
        HKEY_CURRENT_USER,
        PCWSTR(sub.as_ptr()),
        PCWSTR(val.as_ptr()),
        RRF_RT_REG_SZ,
        None,
        None,
        Some(&mut size),
    ) != ERROR_SUCCESS
        || size == 0
    {
        return false;
    }
    let mut data = vec![0u16; size as usize / 2 + 1];
    if RegGetValueW(
        HKEY_CURRENT_USER,
        PCWSTR(sub.as_ptr()),
        PCWSTR(val.as_ptr()),
        RRF_RT_REG_SZ,
        None,
        Some(data.as_mut_ptr() as *mut core::ffi::c_void),
        Some(&mut size),
    ) != ERROR_SUCCESS
    {
        return false;
    }
    let stored = String::from_utf16_lossy(
        &data
            .into_iter()
            .take_while(|value| *value != 0)
            .collect::<Vec<_>>(),
    );
    stored
        .trim()
        .trim_matches('"')
        .eq_ignore_ascii_case(&exe_path())
}

pub unsafe fn set(enable: bool) -> Result<()> {
    let sub = wide(RUN_KEY);
    let val = wide(VALUE);
    if enable {
        let data = wide(&format!("\"{}\"", exe_path()));
        let bytes = (data.len() * 2) as u32;
        let status = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
            REG_SZ.0,
            Some(data.as_ptr() as *const core::ffi::c_void),
            bytes,
        );
        if status != ERROR_SUCCESS {
            return Err(HRESULT::from_win32(status.0).into());
        }
    } else {
        let status = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
        );
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            return Err(HRESULT::from_win32(status.0).into());
        }
    }
    Ok(())
}
