//! Enumerate the Windows desktop *shell namespace* — everything the desktop shows:
//! the file shortcuts sitting on the user + public Desktop AND the virtual items
//! (此电脑 / 回收站 / 网络 / 控制面板 / the user's folder…). The old scan only walked
//! the Desktop *folders*, so it missed the namespace junctions; enumerating the
//! desktop `IShellFolder` instead picks up both in one pass.
//!
//! Each entry carries a stable `key` (so a category assignment survives rescans and
//! reboots), an optional filesystem `path`, and — for the virtual items, which have
//! no path — its absolute `Pidl`, so we can still fetch its icon and launch it.

use core::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::config::path_key;
use crate::content::{classify_path, ContentKind};

const CACHE_TTL_SECS: u64 = 300;

/// An absolute PIDL we own; freed on drop. Held only for virtual (non-file) items.
pub struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    pub fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { ILFree(Some(self.0)) }
    }
}

/// One thing shown on the desktop.
pub struct DesktopEntry {
    pub label: String,
    pub key: String,
    pub path: Option<String>, // filesystem path, when it is one
    pub pidl: Option<Pidl>,   // absolute PIDL, for virtual items
    pub kind: ContentKind,
}

/// Drawer-facing scan: only program-like filesystem items, backed by a small cache.
/// The cache avoids a full Shell namespace walk on every drawer open, while the short
/// TTL plus a cheap Desktop directory signature keeps normal shortcut changes visible.
pub unsafe fn scan_programs_cached() -> Vec<DesktopEntry> {
    let signature = desktop_signature();
    let now = SystemTime::now();
    if let Some(mut entries) = load_cache(now, &signature) {
        append_required_virtual_entries(&mut entries);
        sort_entries(&mut entries);
        return entries;
    }

    let mut entries: Vec<DesktopEntry> = scan().into_iter().filter(is_drawer_program).collect();
    append_required_virtual_entries(&mut entries);
    sort_entries(&mut entries);
    let _ = save_cache(now, &signature, &entries);
    entries
}

pub fn invalidate_cache() {
    let _ = fs::remove_file(cache_path());
}

/// Scan the desktop namespace. Returns entries sorted case-insensitively by label.
/// On any COM failure it returns whatever it gathered (possibly empty) — never panics.
pub unsafe fn scan() -> Vec<DesktopEntry> {
    let mut out: Vec<DesktopEntry> = Vec::new();
    let Ok(desktop) = SHGetDesktopFolder() else {
        return out;
    };
    let Ok(desktop_abs) = SHGetKnownFolderIDList(&FOLDERID_Desktop, 0, HANDLE::default()) else {
        return out;
    };

    let mut enumr: Option<IEnumIDList> = None;
    let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32;
    let hr = desktop.EnumObjects(HWND::default(), flags, &mut enumr);
    if hr.is_ok() {
        if let Some(enumr) = enumr {
            let mut one: [*mut ITEMIDLIST; 1] = [std::ptr::null_mut()];
            let mut fetched = 0u32;
            // Next returns S_OK per item, S_FALSE (with fetched == 0) at the end.
            while enumr.Next(&mut one, Some(&mut fetched)) == S_OK && fetched == 1 {
                let child = one[0];
                if child.is_null() {
                    continue;
                }
                let abs = ILCombine(Some(desktop_abs as *const _), Some(child as *const _));
                ILFree(Some(child));
                if abs.is_null() {
                    continue;
                }
                match entry_from_pidl(abs) {
                    Some(entry) => out.push(entry),
                    None => ILFree(Some(abs)),
                }
            }
        }
    }
    ILFree(Some(desktop_abs));
    out.sort_by_key(|a| a.label.to_lowercase());
    out
}

fn is_drawer_program(entry: &DesktopEntry) -> bool {
    entry.path.is_some() && matches!(entry.kind, ContentKind::Application | ContentKind::Shortcut)
}

fn sort_entries(entries: &mut [DesktopEntry]) {
    entries.sort_by_key(|a| a.label.to_lowercase());
}

unsafe fn append_required_virtual_entries(entries: &mut Vec<DesktopEntry>) {
    append_missing_entries(entries, required_virtual_entries());
}

fn append_missing_entries(entries: &mut Vec<DesktopEntry>, additions: Vec<DesktopEntry>) {
    for entry in additions {
        if !entries.iter().any(|existing| existing.key == entry.key) {
            entries.push(entry);
        }
    }
}

unsafe fn required_virtual_entries() -> Vec<DesktopEntry> {
    [
        (&FOLDERID_ComputerFolder, "knownfolder:computer", "此电脑"),
        (
            &FOLDERID_RecycleBinFolder,
            "knownfolder:recycle-bin",
            "回收站",
        ),
        (
            &FOLDERID_ControlPanelFolder,
            "knownfolder:control-panel",
            "控制面板",
        ),
    ]
    .into_iter()
    .filter_map(|(folder_id, fallback_key, fallback_label)| {
        required_virtual_entry(folder_id, fallback_key, fallback_label)
    })
    .collect()
}

unsafe fn required_virtual_entry(
    folder_id: &GUID,
    fallback_key: &str,
    fallback_label: &str,
) -> Option<DesktopEntry> {
    let abs = SHGetKnownFolderIDList(folder_id, 0, HANDLE::default()).ok()?;
    if abs.is_null() {
        return None;
    }
    let label = name_of(abs, SIGDN_NORMALDISPLAY).unwrap_or_else(|| fallback_label.to_string());
    let key =
        name_of(abs, SIGDN_DESKTOPABSOLUTEPARSING).unwrap_or_else(|| fallback_key.to_string());
    Some(DesktopEntry {
        label,
        key,
        path: None,
        pidl: Some(Pidl(abs)),
        kind: ContentKind::Folder,
    })
}

fn cache_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("FeatherDock")
        .join("drawer-cache.tsv")
}

fn load_cache(now: SystemTime, current_signature: &str) -> Option<Vec<DesktopEntry>> {
    let text = fs::read_to_string(cache_path()).ok()?;
    let mut written: Option<SystemTime> = None;
    let mut signature: Option<String> = None;
    let mut entries = Vec::new();

    for line in text.lines() {
        let mut parts = line.split('\t');
        match parts.next() {
            Some("written") => {
                let secs = parts.next()?.parse::<u64>().ok()?;
                written = Some(UNIX_EPOCH + Duration::from_secs(secs));
            }
            Some("signature") => {
                signature = parts.next().map(unescape);
            }
            Some("entry") => {
                let label = parts.next().map(unescape)?;
                let key = parts.next().map(unescape)?;
                let path = parts.next().map(unescape)?;
                let kind = parts.next().and_then(kind_from_code)?;
                if Path::new(&path).exists() {
                    entries.push(DesktopEntry {
                        label,
                        key,
                        path: Some(path),
                        pidl: None,
                        kind,
                    });
                }
            }
            _ => {}
        }
    }

    if cache_is_fresh(written?, now, signature.as_deref()?, current_signature) {
        Some(entries)
    } else {
        None
    }
}

fn save_cache(now: SystemTime, signature: &str, entries: &[DesktopEntry]) -> std::io::Result<()> {
    let path = cache_path();
    let mut body = String::new();
    body.push_str("# FeatherDock drawer scan cache\n");
    body.push_str(&format!("written\t{}\n", secs_since_epoch(now)));
    body.push_str(&format!("signature\t{}\n", escape(signature)));
    for entry in entries {
        let Some(path) = &entry.path else {
            continue;
        };
        body.push_str(&format!(
            "entry\t{}\t{}\t{}\t{}\n",
            escape(&entry.label),
            escape(&entry.key),
            escape(path),
            kind_code(entry.kind)
        ));
    }
    crate::atomic::write(&path, body.as_bytes())
}

fn cache_is_fresh(
    written: SystemTime,
    now: SystemTime,
    cached_signature: &str,
    current_signature: &str,
) -> bool {
    cached_signature == current_signature
        && now
            .duration_since(written)
            .is_ok_and(|age| age <= Duration::from_secs(CACHE_TTL_SECS))
}

/// A cheap change signal for the desktop folders, computed on every drawer open to
/// validate the cache. We deliberately stamp ONLY each desktop directory's own
/// modified-time + size — NOT every entry inside it. NTFS bumps a directory's
/// modified-time when an item is added, removed, or renamed within it, so this still
/// catches the changes a user actually makes to their desktop, while avoiding a
/// per-entry `fs::metadata` storm. That per-entry walk was the expensive part: on a
/// OneDrive desktop full of cloud-placeholder files, each stat can hydrate/block, so a
/// hot cache hit could still stall the drawer open. (Trade-off: an in-place content edit
/// of an existing shortcut won't bump the dir time and is only picked up when the TTL
/// lapses — acceptable, since that doesn't change how the entry looks in the drawer.)
fn desktop_signature() -> String {
    desktop_dirs()
        .iter()
        .map(|dir| {
            let dir_key = dir.to_string_lossy().to_ascii_lowercase();
            format!("dir:{dir_key}:{}", metadata_stamp(dir))
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE") {
        dirs.push(PathBuf::from(home).join("Desktop"));
    }
    if let Some(one_drive) = std::env::var_os("OneDrive") {
        dirs.push(PathBuf::from(one_drive).join("Desktop"));
    }
    if let Some(public) = std::env::var_os("PUBLIC") {
        dirs.push(PathBuf::from(public).join("Desktop"));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

fn metadata_stamp(path: &Path) -> String {
    let Ok(meta) = fs::metadata(path) else {
        return "missing".to_string();
    };
    let modified = meta.modified().map(secs_since_epoch).unwrap_or(0);
    format!("{}:{modified}", meta.len())
}

fn secs_since_epoch(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn kind_code(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Application => "app",
        ContentKind::Shortcut => "shortcut",
        ContentKind::Folder => "folder",
        ContentKind::Image => "image",
        ContentKind::File => "file",
    }
}

fn kind_from_code(code: &str) -> Option<ContentKind> {
    match code {
        "app" => Some(ContentKind::Application),
        "shortcut" => Some(ContentKind::Shortcut),
        "folder" => Some(ContentKind::Folder),
        "image" => Some(ContentKind::Image),
        "file" => Some(ContentKind::File),
        _ => None,
    }
}

/// Build an entry from an absolute PIDL, taking ownership of it: filesystem items
/// keep their path (and the PIDL is freed); virtual items keep the PIDL.
unsafe fn entry_from_pidl(abs: *mut ITEMIDLIST) -> Option<DesktopEntry> {
    let label = name_of(abs, SIGDN_NORMALDISPLAY)?;
    match path_of(abs) {
        Some(path) => {
            let kind = classify_path(Path::new(&path));
            let key = path_key(Path::new(&path));
            ILFree(Some(abs)); // file items launch + icon straight from the path
            Some(DesktopEntry {
                label,
                key,
                path: Some(path),
                pidl: None,
                kind,
            })
        }
        None => {
            // Virtual item (此电脑 / 回收站 / …): key it by its parsing name so the
            // assignment is stable, and keep the PIDL for icon + launch.
            let key = name_of(abs, SIGDN_DESKTOPABSOLUTEPARSING)
                .unwrap_or_else(|| format!("virt:{label}"));
            Some(DesktopEntry {
                label,
                key,
                path: None,
                pidl: Some(Pidl(abs)),
                kind: ContentKind::Folder,
            })
        }
    }
}

/// Display / parsing name of a shell item; frees the shell-allocated string.
unsafe fn name_of(pidl: *const ITEMIDLIST, sigdn: SIGDN) -> Option<String> {
    let pw = SHGetNameFromIDList(pidl, sigdn).ok()?;
    if pw.is_null() {
        return None;
    }
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

/// The filesystem path of a shell item, or None for a virtual (non-file) item.
/// Uses the shell-allocated `SIGDN_FILESYSPATH` name rather than `SHGetPathFromIDListW`,
/// whose caller-supplied buffer is capped at `MAX_PATH` (260) and would truncate — or
/// fail — on long paths. Virtual (non-filesystem) items return `None` here, as before.
unsafe fn path_of(pidl: *const ITEMIDLIST) -> Option<String> {
    name_of(pidl, SIGDN_FILESYSPATH).filter(|path| !path.is_empty())
}

/// Launch a desktop item: a filesystem item via the shell's "open" (like the rest of
/// the dock); the drawer's known virtual folders through `explorer.exe shell:…` URIs —
/// which never activates the item's context-menu handlers in OUR process; and any
/// other virtual item by invoking its default verb through its PIDL.
pub unsafe fn launch(path: Option<&str>, pidl: Option<*const ITEMIDLIST>, key: Option<&str>) {
    if let Some(path) = path {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    } else if let Some(uri) = key.and_then(known_virtual_shell_uri) {
        let exe: Vec<u16> = "explorer.exe"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let arg: Vec<u16> = uri.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(exe.as_ptr()),
            PCWSTR(arg.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    } else if let Some(pidl) = pidl {
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            nShow: SW_SHOWNORMAL.0,
            lpIDList: pidl as *mut c_void,
            ..Default::default()
        };
        let _ = ShellExecuteExW(&mut info);
    }
}

/// The drawer's virtual desktop entries (此电脑 / 回收站 / 控制面板) map to `explorer.exe`
/// `shell:` URIs. Explorer performs the actual opening, so the shell's per-item
/// context-menu handlers — including buggy third-party ones — are never loaded into
/// FeatherDock's process just to launch a folder.
fn known_virtual_shell_uri(key: &str) -> Option<&'static str> {
    match key {
        "knownfolder:recycle-bin" => Some("shell:RecycleBinFolder"),
        "knownfolder:computer" => Some("shell:MyComputerFolder"),
        "knownfolder:control-panel" => Some("shell:ControlPanelFolder"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn scan_cache_is_reused_only_when_fresh_and_signature_matches() {
        let written = UNIX_EPOCH + Duration::from_secs(1_000);
        let now = UNIX_EPOCH + Duration::from_secs(1_120);

        assert!(cache_is_fresh(written, now, "desktop-a", "desktop-a"));
        assert!(!cache_is_fresh(written, now, "desktop-a", "desktop-b"));
        assert!(!cache_is_fresh(
            written,
            UNIX_EPOCH + Duration::from_secs(1_000 + CACHE_TTL_SECS + 1),
            "desktop-a",
            "desktop-a"
        ));
    }

    #[test]
    fn required_virtual_items_are_appended_without_duplicates() {
        let mut entries = vec![DesktopEntry {
            label: "此电脑".to_string(),
            key: "knownfolder:computer".to_string(),
            path: None,
            pidl: None,
            kind: ContentKind::Folder,
        }];
        let virtuals = vec![
            DesktopEntry {
                label: "此电脑".to_string(),
                key: "knownfolder:computer".to_string(),
                path: None,
                pidl: None,
                kind: ContentKind::Folder,
            },
            DesktopEntry {
                label: "回收站".to_string(),
                key: "knownfolder:recycle-bin".to_string(),
                path: None,
                pidl: None,
                kind: ContentKind::Folder,
            },
            DesktopEntry {
                label: "控制面板".to_string(),
                key: "knownfolder:control-panel".to_string(),
                path: None,
                pidl: None,
                kind: ContentKind::Folder,
            },
        ];

        append_missing_entries(&mut entries, virtuals);

        let keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"knownfolder:computer"));
        assert!(keys.contains(&"knownfolder:recycle-bin"));
        assert!(keys.contains(&"knownfolder:control-panel"));
    }
}
