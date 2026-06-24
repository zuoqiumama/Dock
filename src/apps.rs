//! Resolve real application executables so we can pull their icons + launch them.
//! Resolution order per app: HKCU/HKLM "App Paths" registry, then candidate paths.

use std::path::Path;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;

use crate::content::{classify_path, fallback_visual, ContentKind};
use crate::dock::{DockItem, ItemRole};

/// The leading Start button — opens the Windows Start menu. Drawn as a custom
/// 4-pane glyph (see render.rs), so it needs no extracted icon.
pub fn start_item() -> DockItem {
    DockItem {
        label: "Start".to_string(),
        glyph: "\u{229E}", // ⊞ fallback only
        color: (0.16, 0.50, 0.95),
        path: None,
        icon: None,
        kind: ContentKind::Application,
        role: ItemRole::Start,
        hwnd: None,
    }
}

/// The vertical separator between pinned apps (left) and open windows (right).
pub fn divider_item() -> DockItem {
    DockItem {
        label: String::new(),
        glyph: "",
        color: (1.0, 1.0, 1.0),
        path: None,
        icon: None,
        kind: ContentKind::File,
        role: ItemRole::Divider,
        hwnd: None,
    }
}

/// A currently-open window; clicking it activates (or minimizes) that window.
pub fn running_item(title: &str, hwnd: isize) -> DockItem {
    let (glyph, color) = fallback_visual(ContentKind::Application);
    DockItem {
        label: title.to_string(),
        glyph,
        color,
        path: None,
        icon: None,
        kind: ContentKind::Application,
        role: ItemRole::Running,
        hwnd: Some(hwnd),
    }
}

/// Built-in default app set. Each entry falls back to a glyph/color if exe missing.
pub fn default_items() -> Vec<DockItem> {
    vec![
        item(
            "Edge",
            "\u{1F310}",
            (0.18, 0.45, 0.85),
            app_path_or(
                "msedge.exe",
                &[
                    pf("Microsoft\\Edge\\Application\\msedge.exe"),
                    pf86("Microsoft\\Edge\\Application\\msedge.exe"),
                ],
            ),
        ),
        item(
            "Chrome",
            "\u{1F30D}",
            (0.92, 0.42, 0.30),
            app_path_or(
                "chrome.exe",
                &[
                    pf("Google\\Chrome\\Application\\chrome.exe"),
                    pf86("Google\\Chrome\\Application\\chrome.exe"),
                ],
            ),
        ),
        item(
            "VS Code",
            "\u{1F4DD}",
            (0.14, 0.46, 0.78),
            app_path_or(
                "Code.exe",
                &[
                    local("Programs\\Microsoft VS Code\\Code.exe"),
                    pf("Microsoft VS Code\\Code.exe"),
                ],
            ),
        ),
        item("Claude", "\u{2728}", (0.85, 0.50, 0.28), find_claude()),
    ]
}

fn item(
    label: &'static str,
    glyph: &'static str,
    color: (f32, f32, f32),
    path: Option<String>,
) -> DockItem {
    DockItem {
        label: label.to_string(),
        glyph,
        color,
        kind: path
            .as_deref()
            .map(Path::new)
            .map(classify_path)
            .unwrap_or(ContentKind::Application),
        path,
        icon: None,
        role: ItemRole::Pinned,
        hwnd: None,
    }
}

/// Build the dock from a user config: each item resolves `exe` directly or `app`
/// via "App Paths". Empty/unresolvable entries are skipped.
pub fn from_config(specs: &[crate::config::ItemSpec]) -> Vec<DockItem> {
    specs
        .iter()
        .filter_map(|s| {
            let path = s
                .path
                .clone()
                .or_else(|| s.exe.clone())
                .or_else(|| s.app.as_deref().and_then(|a| unsafe { reg_app_path(a) }));
            if path.is_none() && s.icon.is_none() {
                return None; // nothing to show or launch
            }
            let label = s
                .label
                .clone()
                .unwrap_or_else(|| label_from(path.as_deref()));
            let kind = path
                .as_deref()
                .map(Path::new)
                .map(classify_path)
                .unwrap_or(ContentKind::File);
            let (glyph, color) = fallback_visual(kind);
            Some(DockItem {
                label,
                glyph,
                color,
                path,
                icon: s.icon.clone(),
                kind,
                role: ItemRole::Pinned,
                hwnd: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ItemSpec;

    #[test]
    fn builds_image_item_from_path_field() {
        let specs = vec![ItemSpec {
            label: Some("Photo".to_string()),
            path: Some(r"C:\Pictures\photo.png".to_string()),
            ..Default::default()
        }];

        let items = from_config(&specs);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path.as_deref(), Some(r"C:\Pictures\photo.png"));
        assert_eq!(items[0].kind, ContentKind::Image);
    }
}

fn label_from(exe: Option<&str>) -> String {
    exe.and_then(|p| Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "App".to_string())
}

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}
fn pf(rel: &str) -> String {
    format!("{}\\{}", env("ProgramFiles"), rel)
}
fn pf86(rel: &str) -> String {
    format!("{}\\{}", env("ProgramFiles(x86)"), rel)
}
fn local(rel: &str) -> String {
    format!("{}\\{}", env("LOCALAPPDATA"), rel)
}

fn first_existing(cands: &[String]) -> Option<String> {
    cands.iter().find(|p| Path::new(p).exists()).cloned()
}

fn app_path_or(exe: &str, cands: &[String]) -> Option<String> {
    unsafe { reg_app_path(exe) }.or_else(|| first_existing(cands))
}

/// Claude Code CLI lives in a versioned folder; pick the newest `claude.exe`.
fn find_claude() -> Option<String> {
    let mut bases = Vec::new();
    let home = env("USERPROFILE");
    if !home.is_empty() {
        let direct = format!("{home}\\.local\\bin\\claude.exe");
        if Path::new(&direct).exists() {
            return Some(direct);
        }
    }
    let l = env("LOCALAPPDATA");
    if !l.is_empty() {
        bases.push(format!("{l}\\AnthropicClaude"));
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for base in bases {
        let Ok(rd) = std::fs::read_dir(&base) else {
            continue;
        };
        for e in rd.flatten() {
            let exe = e.path().join("claude.exe");
            if exe.exists() {
                let mt = exe
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map_or(true, |(bt, _)| mt > *bt) {
                    best = Some((mt, exe.to_string_lossy().into_owned()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

unsafe fn reg_app_path(exe: &str) -> Option<String> {
    let sub = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}");
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Some(v) = reg_default_sz(hive, &sub) {
            let v = v.trim().trim_matches('"').to_string();
            if Path::new(&v).exists() {
                return Some(v);
            }
        }
    }
    None
}

unsafe fn reg_default_sz(hive: HKEY, subkey: &str) -> Option<String> {
    let sub: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut size: u32 = 0;
    if RegGetValueW(
        hive,
        PCWSTR(sub.as_ptr()),
        PCWSTR::null(),
        RRF_RT_REG_SZ,
        None,
        None,
        Some(&mut size),
    ) != ERROR_SUCCESS
        || size == 0
    {
        return None;
    }
    let mut buf = vec![0u16; (size as usize / 2) + 1];
    if RegGetValueW(
        hive,
        PCWSTR(sub.as_ptr()),
        PCWSTR::null(),
        RRF_RT_REG_SZ,
        None,
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        Some(&mut size),
    ) != ERROR_SUCCESS
    {
        return None;
    }
    let s: Vec<u16> = buf.into_iter().take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&s))
}
