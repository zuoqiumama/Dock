//! Stable application keys. Shell-link metadata is resolved only in background
//! collectors; the dock consults a memory cache and never resolves a link in paint.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use windows::core::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::*;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IPropertyStore, SHGetPropertyStoreForWindow, PROPERTYKEY,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

const APP_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};
static LINKS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn normalize_source(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_lowercase()
}

pub fn command_action_id(path: &str) -> String {
    normalize_source(path)
}

pub fn normalize(path: &str) -> String {
    let path = normalize_source(path);
    // Package updates change version/architecture, but retain name + publisher.
    if let Some((_, suffix)) = path.split_once("\\windowsapps\\") {
        if let Some((package, relative)) = suffix.split_once('\\') {
            let parts: Vec<&str> = package.split('_').collect();
            if parts.len() >= 5 {
                return format!("appx:{}_{}\\{}", parts[0], parts[parts.len() - 1], relative);
            }
        }
    }
    path
}

fn expand_environment_strings(path: &str) -> String {
    let source: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let needed = ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), None);
        if needed == 0 {
            return path.to_string();
        }
        let mut buffer = vec![0u16; needed as usize];
        let written =
            ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), Some(buffer.as_mut_slice()));
        if written == 0 || written > needed {
            return path.to_string();
        }
        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }
}

fn normalize_raw_path(path: &str) -> String {
    normalize(&expand_environment_strings(path))
}

pub fn cached_launch_key(path: &str) -> String {
    let key = normalize(path);
    LINKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
        .unwrap_or(key)
}

unsafe fn store_id(store: &IPropertyStore) -> Option<String> {
    let value = store.GetValue(&APP_ID).ok()?;
    let text = BSTR::try_from(&value).ok()?.to_string();
    (!text.trim().is_empty()).then(|| format!("aumid:{}", text.to_lowercase()))
}

pub unsafe fn window_id(hwnd: HWND) -> Option<String> {
    let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
    store_id(&store)
}

/// Caller owns a COM apartment. Never calls IShellLink::Resolve (which may prompt).
pub unsafe fn resolve_launch_key(path: &str) -> String {
    let original = normalize(path);
    if !path.to_ascii_lowercase().ends_with(".lnk") {
        return original;
    }
    let resolved = (|| -> Option<String> {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        persist.Load(PCWSTR(wide.as_ptr()), STGM_READ).ok()?;
        if let Ok(store) = link.cast::<IPropertyStore>() {
            if let Some(id) = store_id(&store) {
                return Some(id);
            }
        }
        let mut buffer = [0u16; 32768];
        link.GetPath(&mut buffer, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()?;
        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        (end > 0).then(|| normalize_raw_path(&String::from_utf16_lossy(&buffer[..end])))
    })()
    .unwrap_or_else(|| original.clone());
    let mut cache = LINKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() > 2048 {
        cache.clear();
    }
    cache.insert(original, resolved.clone());
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_versions_match_without_merging_unrelated_executables() {
        assert_eq!(
            normalize(r"C:\Program Files\WindowsApps\Vendor.App_1.0.0.0_x64__abc\app.exe"),
            normalize(r"C:\Program Files\WindowsApps\Vendor.App_2.0.0.0_x64__abc\app.exe")
        );
        assert_ne!(normalize(r"C:\One\app.exe"), normalize(r"C:\Two\app.exe"));
    }

    #[test]
    fn command_actions_use_source_paths_instead_of_application_groups() {
        assert_eq!(
            command_action_id(" C:/Links/WeChat.LNK "),
            command_action_id(r"c:\links\wechat.lnk")
        );
        assert_ne!(
            command_action_id(r"C:\Links\WeChat.lnk"),
            command_action_id(r"C:\Links\WeChat Work.lnk")
        );
        assert_ne!(
            command_action_id(r"C:\WindowsApps\Vendor.App_1.0.0.0_x64__abc\app.exe"),
            command_action_id(r"C:\WindowsApps\Vendor.App_2.0.0.0_x64__abc\app.exe")
        );
    }

    #[test]
    fn raw_paths_expand_environment_before_normalization() {
        let root = std::env::var("SystemRoot").unwrap();
        assert_eq!(
            normalize_raw_path("%SystemRoot%/System32"),
            normalize(&format!("{root}\\System32"))
        );
    }
}
