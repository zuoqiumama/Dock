//! Resolve real application executables so we can pull their icons + launch them.
//! Resolution order per app: HKCU/HKLM "App Paths" registry, then candidate paths.

use std::path::Path;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;

use crate::content::{classify_path, fallback_visual, ContentKind};
use crate::dock::{DockItem, ItemRole, RunningWindowRef};

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
        group_key: None,
        windows: Vec::new(),
    }
}

/// The far-right control button — opens the glass control center. Drawn as a custom
/// vector "sliders" glyph (see render.rs), so it needs no extracted icon.
pub fn control_item() -> DockItem {
    DockItem {
        label: "控制中心".to_string(),
        glyph: "\u{2699}", // ⚙ fallback only
        color: (0.30, 0.32, 0.38),
        path: None,
        icon: None,
        kind: ContentKind::Application,
        role: ItemRole::Control,
        hwnd: None,
        group_key: None,
        windows: Vec::new(),
    }
}

/// The app-drawer button — opens the glass drawer listing every program on the
/// desktop. Drawn as a custom 3x3 "app grid" glyph (see render.rs), so it needs no
/// extracted icon.
pub fn drawer_item() -> DockItem {
    DockItem {
        label: "应用抽屉".to_string(),
        glyph: "\u{25A6}", // ▦ fallback only
        color: (0.28, 0.30, 0.36),
        path: None,
        icon: None,
        kind: ContentKind::Application,
        role: ItemRole::Drawer,
        hwnd: None,
        group_key: None,
        windows: Vec::new(),
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
        group_key: None,
        windows: Vec::new(),
    }
}

/// A currently-open window; clicking it activates (or minimizes) that window.
pub fn running_item(group: &crate::windows_list::RunningGroup) -> DockItem {
    let (glyph, color) = fallback_visual(ContentKind::Application);
    let primary = group.windows.first().map(|window| window.hwnd);
    DockItem {
        label: group.label.clone(),
        glyph,
        color,
        path: None,
        icon: group.icon_path.clone(),
        kind: ContentKind::Application,
        role: ItemRole::Running,
        hwnd: primary,
        group_key: Some(group.key.clone()),
        windows: group
            .windows
            .iter()
            .map(|window| RunningWindowRef {
                hwnd: window.hwnd,
                title: window.title.clone(),
            })
            .collect(),
    }
}

/// Does a pinned app's launch path refer to the same executable as a running window
/// group? Matches the full normalized path first, then falls back to the executable's
/// file name — so a packaged app whose versioned WindowsApps path drifts after an
/// update (e.g. `Claude_1.15200…\app\claude.exe`) still pairs with its running process.
pub fn exe_matches(pinned_path: &str, group_key: &str) -> bool {
    let pinned = pinned_path.trim().trim_matches('"').to_ascii_lowercase();
    if pinned == group_key {
        return true;
    }
    let file_of = |s: &str| {
        Path::new(s)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_string())
    };
    match (file_of(&pinned), file_of(group_key)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Some launchers expose their real UI from a helper process whose executable does not
/// match the pinned launcher path. If the top-level window title is exactly the pinned
/// app label, merge that helper window into the pinned icon instead of treating it as an
/// unrelated running app.
pub fn title_matches_label(label: &str, group: &crate::windows_list::RunningGroup) -> bool {
    let label = label.trim();
    !label.is_empty()
        && group
            .windows
            .iter()
            .any(|window| window.title.trim().eq_ignore_ascii_case(label))
}

/// Merge a running app's open windows into its pinned dock icon, so the pinned slot
/// shows a running indicator and (on click) activates the window instead of launching
/// a duplicate — the macOS behaviour. The icon/path stay the pinned item's own.
pub fn attach_running(item: &mut DockItem, group: &crate::windows_list::RunningGroup) {
    item.hwnd = group.windows.first().map(|window| window.hwnd);
    item.group_key = Some(group.key.clone());
    item.windows = group
        .windows
        .iter()
        .map(|window| RunningWindowRef {
            hwnd: window.hwnd,
            title: window.title.clone(),
        })
        .collect();
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
        group_key: None,
        windows: Vec::new(),
    }
}

/// Resolve a config item's launch path the same way the dock does: an explicit `path`,
/// then legacy `exe`, then an `app` name looked up via Windows "App Paths". `None` means
/// the entry launches nothing (e.g. icon-only). Shared with the settings UI so a row's
/// identity matches whether it was written as `path` or `app`.
pub fn resolve_launch_path(spec: &crate::config::ItemSpec) -> Option<String> {
    spec.path
        .clone()
        .or_else(|| spec.exe.clone())
        .or_else(|| spec.app.as_deref().and_then(|a| unsafe { reg_app_path(a) }))
}

/// Build the dock from a user config: each item resolves `exe` directly or `app`
/// via "App Paths". Empty/unresolvable entries are skipped.
pub fn from_config(specs: &[crate::config::ItemSpec]) -> Vec<DockItem> {
    specs
        .iter()
        .filter_map(|s| {
            let path = resolve_launch_path(s);
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
                group_key: None,
                windows: Vec::new(),
            })
        })
        .collect()
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
                if best.as_ref().is_none_or(|(bt, _)| mt > *bt) {
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

    #[test]
    fn launcher_helper_window_can_match_pinned_label_by_title() {
        let group = crate::windows_list::RunningGroup {
            key: r"c:\program files\wegame\browser.exe".to_string(),
            label: "Browser".to_string(),
            icon_path: Some(r"C:\Program Files\WeGame\browser.exe".to_string()),
            windows: vec![crate::windows_list::RunningWindow {
                hwnd: 42,
                title: "WeGame".to_string(),
                exe_path: Some(r"C:\Program Files\WeGame\browser.exe".to_string()),
            }],
        };

        assert!(title_matches_label("WeGame", &group));
        assert!(!title_matches_label("Game", &group));
        assert!(!title_matches_label("", &group));
    }
}
