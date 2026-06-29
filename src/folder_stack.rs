use std::fs;
use std::path::{Path, PathBuf};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const MAX_STACK_ITEMS: usize = 24;

const ID_ENTRY_BASE: usize = 4100;
const ID_OPEN_FOLDER: usize = 4197;
const ID_REVEAL_FOLDER: usize = 4198;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackEntry {
    pub label: String,
    pub is_dir: bool,
    pub path: String,
}

impl StackEntry {
    pub fn new(label: impl Into<String>, is_dir: bool, path: impl Into<String>) -> StackEntry {
        StackEntry {
            label: label.into(),
            is_dir,
            path: path.into(),
        }
    }
}

pub fn normalize_entries_for_menu(mut entries: Vec<StackEntry>) -> Vec<StackEntry> {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
    });
    entries.truncate(MAX_STACK_ITEMS);
    entries
}

fn read_entries(folder: &str) -> Vec<StackEntry> {
    let Ok(read_dir) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let entries = read_dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let label = path.file_name()?.to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
            Some(StackEntry::new(label, is_dir, path.to_string_lossy()))
        })
        .collect();
    normalize_entries_for_menu(entries)
}

fn wide_nul(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn append(menu: HMENU, id: usize, text: &str, enabled: bool) {
    let wide = wide_nul(text);
    let flags = MF_STRING
        | if enabled {
            MENU_ITEM_FLAGS(0)
        } else {
            MF_GRAYED
        };
    let _ = AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr()));
}

unsafe fn shell_open(path: &str) {
    let target = wide_nul(path);
    let _ = ShellExecuteW(
        HWND::default(),
        w!("open"),
        PCWSTR(target.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn reveal_folder(path: &str) {
    let explorer = wide_nul("explorer.exe");
    let args = wide_nul(&format!("/select,\"{path}\""));
    let _ = ShellExecuteW(
        HWND::default(),
        w!("open"),
        PCWSTR(explorer.as_ptr()),
        PCWSTR(args.as_ptr()),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Open a small on-demand stack menu for a pinned folder. Directory contents are read
/// only while the user opens the stack, so idle resource usage stays unchanged.
pub unsafe fn show(owner: HWND, folder: &str, anchor_cx: i32, anchor_top: i32) {
    if !Path::new(folder).is_dir() {
        shell_open(folder);
        return;
    }
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let entries = read_entries(folder);
    if entries.is_empty() {
        append(menu, ID_ENTRY_BASE, "（空文件夹）", false);
    } else {
        for (index, entry) in entries.iter().enumerate() {
            let prefix = if entry.is_dir { "▸ " } else { "  " };
            append(
                menu,
                ID_ENTRY_BASE + index,
                &format!("{prefix}{}", entry.label),
                true,
            );
        }
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append(menu, ID_OPEN_FOLDER, "打开文件夹", true);
    append(menu, ID_REVEAL_FOLDER, "在资源管理器中显示", true);

    let _ = SetForegroundWindow(owner);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        anchor_cx,
        anchor_top,
        0,
        owner,
        None,
    );
    let _ = DestroyMenu(menu);

    let id = cmd.0 as usize;
    if (ID_ENTRY_BASE..ID_ENTRY_BASE + entries.len()).contains(&id) {
        shell_open(&entries[id - ID_ENTRY_BASE].path);
    } else if id == ID_OPEN_FOLDER {
        shell_open(folder);
    } else if id == ID_REVEAL_FOLDER {
        reveal_folder(folder);
    }
}

#[allow(dead_code)]
fn _pathbuf_from_string(path: &str) -> PathBuf {
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_entries_sort_folders_first_then_names() {
        let entries = normalize_entries_for_menu(vec![
            StackEntry::new("zeta.txt", false, "C:\\zeta.txt"),
            StackEntry::new("Alpha", true, "C:\\Alpha"),
            StackEntry::new("beta.txt", false, "C:\\beta.txt"),
            StackEntry::new("Tools", true, "C:\\Tools"),
        ]);

        let labels: Vec<&str> = entries.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(labels, vec!["Alpha", "Tools", "beta.txt", "zeta.txt"]);
    }

    #[test]
    fn stack_entries_are_limited_for_fast_popups() {
        let entries: Vec<StackEntry> = (0..40)
            .map(|i| StackEntry::new(format!("file-{i:02}.txt"), false, format!("C:\\{i}.txt")))
            .collect();

        assert_eq!(normalize_entries_for_menu(entries).len(), MAX_STACK_ITEMS);
    }
}
