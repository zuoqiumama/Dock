//! Scoped colors for native popup menus. HMENU keeps command routing, strings,
//! focus and accessibility; only measuring and painting are handled here.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{w, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_CHECKED, ODS_DISABLED, ODS_GRAYED, ODS_NOACCEL,
    ODS_SELECTED, ODT_MENU,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::theme::ThemePreset;

thread_local! {
    static ITEMS: RefCell<HashMap<usize, Entry>> = RefCell::new(HashMap::new());
}
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(0x4644_0000);

#[derive(Clone)]
struct Entry {
    menu: HMENU,
    label: String,
    kind: MENU_ITEM_TYPE,
    submenu: bool,
    theme: ThemePreset,
    font: HFONT,
    scale: f32,
}

struct OriginalItem {
    menu: HMENU,
    position: u32,
    kind: MENU_ITEM_TYPE,
    data: usize,
    token: usize,
}

/// Keep this guard alive until TrackPopupMenu returns; drop before DestroyMenu.
#[must_use]
#[derive(Default)]
pub struct MenuStyleGuard {
    items: Vec<OriginalItem>,
    backgrounds: Vec<(HMENU, HBRUSH)>,
    font: HFONT,
    background: HBRUSH,
}

impl Drop for MenuStyleGuard {
    fn drop(&mut self) {
        unsafe {
            for item in &self.items {
                let info = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_DATA,
                    fType: item.kind,
                    dwItemData: item.data,
                    ..Default::default()
                };
                let _ = SetMenuItemInfoW(item.menu, item.position, true, &info);
                ITEMS.with(|items| items.borrow_mut().remove(&item.token));
            }
            for &(menu, brush) in &self.backgrounds {
                let info = MENUINFO {
                    cbSize: std::mem::size_of::<MENUINFO>() as u32,
                    fMask: MIM_BACKGROUND,
                    hbrBack: brush,
                    ..Default::default()
                };
                let _ = SetMenuInfo(menu, &info);
            }
            if !self.font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            if !self.background.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.background.0));
            }
        }
    }
}

fn color((r, g, b): (f32, f32, f32)) -> COLORREF {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF(channel(r) | (channel(g) << 8) | (channel(b) << 16))
}

fn blend(a: (f32, f32, f32), b: (f32, f32, f32), alpha: f32) -> (f32, f32, f32) {
    (
        a.0 + (b.0 - a.0) * alpha,
        a.1 + (b.1 - a.1) * alpha,
        a.2 + (b.2 - a.2) * alpha,
    )
}

unsafe fn high_contrast() -> bool {
    let mut info = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    SystemParametersInfoW(
        SPI_GETHIGHCONTRAST,
        info.cbSize,
        Some(&mut info as *mut _ as *mut c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_err()
        || info.dwFlags.0 & HCF_HIGHCONTRASTON.0 != 0
}

/// Style supported light presets; system high-contrast menus remain native.
pub unsafe fn style(menu: HMENU, theme: ThemePreset, owner: HWND) -> MenuStyleGuard {
    let mut guard = MenuStyleGuard::default();
    if !matches!(theme, ThemePreset::Glass | ThemePreset::Macos) || high_contrast() {
        return guard;
    }
    let scale = GetDpiForWindow(owner).max(96) as f32 / 96.0;
    guard.font = CreateFontW(
        -(13.0 * scale).round() as i32,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        1,
        0,
        0,
        0,
        0,
        w!("Segoe UI"),
    );
    guard.background = CreateSolidBrush(color(theme.surface().rgb));
    if guard.font.0.is_null() || guard.background.0.is_null() {
        return guard;
    }
    collect(menu, theme, scale, &mut guard, &mut HashSet::new());
    guard
}

unsafe fn collect(
    menu: HMENU,
    theme: ThemePreset,
    scale: f32,
    guard: &mut MenuStyleGuard,
    visited: &mut HashSet<isize>,
) {
    if visited.len() >= 32 || !visited.insert(menu.0 as isize) {
        return;
    }
    let mut background = MENUINFO {
        cbSize: std::mem::size_of::<MENUINFO>() as u32,
        fMask: MIM_BACKGROUND,
        ..Default::default()
    };
    if GetMenuInfo(menu, &mut background).is_ok() {
        let previous = background.hbrBack;
        background.hbrBack = guard.background;
        if SetMenuInfo(menu, &background).is_ok() {
            guard.backgrounds.push((menu, previous));
        }
    }
    for position in 0..GetMenuItemCount(menu).max(0) as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE
                | MIIM_DATA
                | MIIM_STRING
                | MIIM_SUBMENU
                | MIIM_BITMAP
                | MIIM_CHECKMARKS,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, position, true, &mut info).is_err() {
            continue;
        }
        if !info.hSubMenu.0.is_null() {
            collect(info.hSubMenu, theme, scale, guard, visited);
        }
        // Do not take over another owner's drawing or custom bitmap semantics.
        if info.fType.0 & (MFT_OWNERDRAW.0 | MFT_BITMAP.0) != 0
            || !info.hbmpItem.0.is_null()
            || !info.hbmpChecked.0.is_null()
            || !info.hbmpUnchecked.0.is_null()
        {
            continue;
        }
        let mut label = vec![0u16; info.cch as usize + 1];
        info.dwTypeData = PWSTR(label.as_mut_ptr());
        info.cch = label.len() as u32;
        if GetMenuItemInfoW(menu, position, true, &mut info).is_err() {
            continue;
        }
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let entry = Entry {
            menu,
            label: String::from_utf16_lossy(&label[..info.cch as usize]),
            kind: info.fType,
            submenu: !info.hSubMenu.0.is_null(),
            theme,
            font: guard.font,
            scale,
        };
        let original = OriginalItem {
            menu,
            position,
            kind: info.fType,
            data: info.dwItemData,
            token,
        };
        // Keep MIIM_STRING intact for native menu accessibility and discovery.
        info.fMask = MIIM_FTYPE | MIIM_DATA;
        info.fType |= MFT_OWNERDRAW;
        info.dwItemData = token;
        if SetMenuItemInfoW(menu, position, true, &info).is_ok() {
            ITEMS.with(|items| items.borrow_mut().insert(token, entry));
            guard.items.push(original);
        }
    }
}

fn entry(token: usize) -> Option<Entry> {
    ITEMS.with(|items| items.borrow().get(&token).cloned())
}

fn pixels(entry: &Entry, value: f32) -> i32 {
    (value * entry.scale).round() as i32
}

unsafe fn measure(hwnd: HWND, item: &mut MEASUREITEMSTRUCT, entry: &Entry) {
    if entry.kind.0 & MFT_SEPARATOR.0 != 0 {
        item.itemHeight = pixels(entry, 9.0) as u32;
        item.itemWidth = 1;
        return;
    }
    let dc = GetDC(hwnd);
    let previous = SelectObject(dc, HGDIOBJ(entry.font.0));
    let mut bounds = RECT::default();
    let mut label: Vec<u16> = entry.label.encode_utf16().collect();
    DrawTextW(
        dc,
        &mut label,
        &mut bounds,
        DT_CALCRECT | DT_SINGLELINE | DT_EXPANDTABS,
    );
    item.itemWidth = (bounds.right - bounds.left + pixels(entry, 68.0)).max(1) as u32;
    item.itemHeight =
        (bounds.bottom - bounds.top + pixels(entry, 16.0)).max(pixels(entry, 32.0)) as u32;
    SelectObject(dc, previous);
    ReleaseDC(hwnd, dc);
}

unsafe fn paint(item: &DRAWITEMSTRUCT, entry: &Entry) {
    let surface = entry.theme.surface();
    let disabled = item.itemState.0 & (ODS_DISABLED.0 | ODS_GRAYED.0) != 0;
    let selected = item.itemState.0 & ODS_SELECTED.0 != 0 && !disabled;
    let background = if selected {
        blend(surface.rgb, surface.accent_rgb, surface.selected_alpha)
    } else {
        surface.rgb
    };
    let brush = CreateSolidBrush(color(background));
    FillRect(item.hDC, &item.rcItem, brush);
    let _ = DeleteObject(HGDIOBJ(brush.0));
    if entry.kind.0 & MFT_SEPARATOR.0 != 0 {
        let line = CreateSolidBrush(color(blend(
            surface.rgb,
            surface.muted_rgb,
            surface.divider_alpha,
        )));
        let middle = (item.rcItem.top + item.rcItem.bottom) / 2;
        let bounds = RECT {
            left: item.rcItem.left + pixels(entry, 12.0),
            top: middle,
            right: item.rcItem.right - pixels(entry, 12.0),
            bottom: middle + 1,
        };
        FillRect(item.hDC, &bounds, line);
        let _ = DeleteObject(HGDIOBJ(line.0));
        return;
    }
    let saved = SaveDC(item.hDC);
    SelectObject(item.hDC, HGDIOBJ(entry.font.0));
    SetBkMode(item.hDC, TRANSPARENT);
    SetTextColor(
        item.hDC,
        color(if disabled {
            surface.muted_rgb
        } else {
            surface.text_rgb
        }),
    );
    let mut flags = DT_SINGLELINE | DT_VCENTER;
    if item.itemState.0 & ODS_NOACCEL.0 != 0 {
        flags |= DT_HIDEPREFIX;
    }
    let (label, shortcut) = entry.label.split_once('\t').unwrap_or((&entry.label, ""));
    let mut bounds = item.rcItem;
    bounds.left += pixels(entry, 32.0);
    bounds.right -= pixels(entry, 26.0);
    let mut label: Vec<u16> = label.encode_utf16().collect();
    DrawTextW(item.hDC, &mut label, &mut bounds, flags | DT_LEFT);
    if !shortcut.is_empty() {
        let mut shortcut: Vec<u16> = shortcut.encode_utf16().collect();
        DrawTextW(
            item.hDC,
            &mut shortcut,
            &mut bounds,
            flags | DT_RIGHT | DT_NOPREFIX,
        );
    }
    if item.itemState.0 & ODS_CHECKED.0 != 0 {
        let mut check: Vec<u16> = if entry.kind.0 & MFT_RADIOCHECK.0 != 0 {
            "●"
        } else {
            "✓"
        }
        .encode_utf16()
        .collect();
        let mut check_bounds = item.rcItem;
        check_bounds.left += pixels(entry, 4.0);
        check_bounds.right = item.rcItem.left + pixels(entry, 30.0);
        DrawTextW(
            item.hDC,
            &mut check,
            &mut check_bounds,
            flags | DT_CENTER | DT_NOPREFIX,
        );
    }
    if entry.submenu {
        let mut arrow: Vec<u16> = "›".encode_utf16().collect();
        let mut arrow_bounds = item.rcItem;
        arrow_bounds.left = arrow_bounds.right - pixels(entry, 25.0);
        DrawTextW(
            item.hDC,
            &mut arrow,
            &mut arrow_bounds,
            flags | DT_CENTER | DT_NOPREFIX,
        );
    }
    let _ = RestoreDC(item.hDC, saved);
}

fn mnemonic(label: &str) -> Option<char> {
    let mut chars = label.chars();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            let next = chars.next()?;
            if next != '&' {
                return next.to_lowercase().next();
            }
        }
    }
    None
}

unsafe fn menu_char(menu: HMENU, key: u16) -> Option<LRESULT> {
    let owned = ITEMS.with(|items| items.borrow().values().any(|item| item.menu == menu));
    if !owned {
        return None;
    }
    let key = char::from_u32(key as u32)?.to_lowercase().next()?;
    let mut matches = Vec::new();
    let mut selected = None;
    for position in 0..GetMenuItemCount(menu).max(0) as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_DATA | MIIM_STATE,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, position, true, &mut info).is_err() {
            continue;
        }
        if info.fState.0 & MFS_HILITE.0 != 0 {
            selected = Some(position);
        }
        if info.fState.0 & MFS_DISABLED.0 == 0 {
            if let Some(item) = entry(info.dwItemData) {
                if mnemonic(&item.label) == Some(key) {
                    matches.push((position, item.submenu));
                }
            }
        }
    }
    let &(position, submenu) = matches
        .iter()
        .find(|(position, _)| selected.is_none_or(|current| *position > current))
        .or_else(|| matches.first())?;
    let action = if matches.len() == 1 && !submenu {
        MNC_EXECUTE
    } else {
        MNC_SELECT
    };
    Some(LRESULT((position | (action << 16)) as isize))
}

/// Call from the popup owner's window procedure before other message handlers.
pub unsafe fn handle_message(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_MEASUREITEM if lparam.0 != 0 => {
            let item = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
            if item.CtlType != ODT_MENU {
                return None;
            }
            let entry = entry(item.itemData)?;
            measure(hwnd, item, &entry);
            Some(LRESULT(1))
        }
        WM_DRAWITEM if lparam.0 != 0 => {
            let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
            if item.CtlType != ODT_MENU {
                return None;
            }
            let entry = entry(item.itemData)?;
            if entry.menu.0 != item.hwndItem.0 {
                return None;
            }
            paint(item, &entry);
            Some(LRESULT(1))
        }
        WM_MENUCHAR => menu_char(HMENU(lparam.0 as *mut c_void), (wparam.0 & 0xffff) as u16),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mnemonics_preserve_escaped_ampersands() {
        assert_eq!(mnemonic("&Open"), Some('o'));
        assert_eq!(mnemonic("Save && &Exit"), Some('e'));
        assert_eq!(mnemonic("设置"), None);
        assert_eq!(mnemonic("Rock && Roll"), None);
    }

    #[test]
    fn native_menu_strings_and_item_data_survive_styling() {
        unsafe {
            let menu = CreatePopupMenu().unwrap();
            AppendMenuW(menu, MF_STRING, 7, w!("&Open")).unwrap();
            let original = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_DATA,
                dwItemData: 123,
                ..Default::default()
            };
            SetMenuItemInfoW(menu, 0, true, &original).unwrap();
            let guard = style(menu, ThemePreset::Glass, HWND::default());
            let mut text = [0u16; 32];
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_DATA | MIIM_FTYPE | MIIM_STRING,
                dwTypeData: PWSTR(text.as_mut_ptr()),
                cch: text.len() as u32,
                ..Default::default()
            };
            GetMenuItemInfoW(menu, 0, true, &mut info).unwrap();
            assert_eq!(
                String::from_utf16_lossy(&text[..info.cch as usize]),
                "&Open"
            );
            if !guard.items.is_empty() {
                assert_ne!(info.fType.0 & MFT_OWNERDRAW.0, 0);
                assert!(entry(info.dwItemData).is_some());
                let mut measure = MEASUREITEMSTRUCT {
                    CtlType: ODT_MENU,
                    itemID: 7,
                    itemData: info.dwItemData,
                    ..Default::default()
                };
                assert_eq!(
                    handle_message(
                        HWND::default(),
                        WM_MEASUREITEM,
                        WPARAM(0),
                        LPARAM(&mut measure as *mut _ as isize),
                    ),
                    Some(LRESULT(1)),
                );
                assert!(measure.itemWidth > 32 && measure.itemHeight >= 32);
            }
            let token = info.dwItemData;
            drop(guard);
            info.fMask = MIIM_DATA | MIIM_FTYPE;
            GetMenuItemInfoW(menu, 0, true, &mut info).unwrap();
            assert_eq!(info.fType, MFT_STRING);
            assert_eq!(info.dwItemData, 123);
            assert!(entry(token).is_none());
            DestroyMenu(menu).unwrap();
        }
    }
}
