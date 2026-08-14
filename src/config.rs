//! Tiny zero-dependency config: a list of `[[item]]` tables. Not full TOML — just
//! the subset we need (item tables with quoted-string values + comments).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
};

use crate::atomic;
use crate::dock::DockItem;

#[derive(Default)]
pub struct ItemSpec {
    pub label: Option<String>,
    pub path: Option<String>, // file, folder, shortcut, or application path
    pub exe: Option<String>,  // full path to launch + pull icon from
    pub app: Option<String>,  // "App Paths" name, e.g. chrome.exe (resolved at load)
    pub icon: Option<String>, // optional separate icon source path
}

pub struct Config {
    pub items: Vec<ItemSpec>,
}

const HEADER: &str = "\
# FeatherDock config - edit then restart the dock.
# One [[item]] per icon. Provide `path` (file, folder, shortcut, or application)
# OR `app` (resolved via Windows \"App Paths\", e.g. chrome.exe).
# Legacy `exe` is still accepted. `label` and `icon` are optional.
# Backslashes in double-quoted paths must be doubled, e.g. \"C:\\\\Tools\\\\app.exe\".
";

pub fn config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|dir| config_path_from(&dir))
        .unwrap_or_else(legacy_config_path)
}

fn config_path_from(appdata: &Path) -> PathBuf {
    appdata.join("FeatherDock").join("featherdock.toml")
}

fn legacy_config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("featherdock.toml")))
        .unwrap_or_else(|| PathBuf::from("featherdock.toml"))
}

/// Normalize a path into a stable comparison key: forward slashes → backslashes,
/// trailing separators dropped, lower-cased. Shared with the drawer's category store.
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn item_path(spec: &ItemSpec) -> Option<&str> {
    spec.path.as_deref().or(spec.exe.as_deref())
}

fn write_config(path: &Path, config: &Config) -> io::Result<()> {
    let mut body = String::from(HEADER);
    for spec in &config.items {
        write_spec(&mut body, spec);
    }
    atomic::write(path, body.as_bytes())
}

fn replace_launch_path_at(path: &Path, old_path: &str, new_path: &str) -> io::Result<bool> {
    let content = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut config = parse(&content);
    let old_key = path_key(Path::new(old_path));
    let mut changed = false;
    for spec in &mut config.items {
        for field in [&mut spec.path, &mut spec.exe] {
            if field
                .as_deref()
                .is_some_and(|value| path_key(Path::new(value)) == old_key)
            {
                *field = Some(new_path.to_string());
                changed = true;
            }
        }
    }
    if changed {
        write_config(path, &config)?;
    }
    Ok(changed)
}

/// If a launch path went stale after an app update or filename encoding glitch, find the
/// current path and persist it for the next run. Returns `None` when no safe repair exists.
pub fn repair_launch_path(path: &str) -> Option<String> {
    let path_ref = Path::new(path);
    if path_ref.exists() {
        return None;
    }
    let repaired = repair_missing_path(path_ref)?;
    let _ = replace_launch_path_at(&config_path(), path, &repaired);
    Some(repaired)
}

fn repair_config_paths(config: &mut Config) -> bool {
    let mut changed = false;
    for spec in &mut config.items {
        changed |= repair_spec_field(&mut spec.path);
        changed |= repair_spec_field(&mut spec.exe);
        changed |= repair_spec_field(&mut spec.icon);
    }
    changed
}

fn repair_spec_field(field: &mut Option<String>) -> bool {
    let Some(value) = field.as_deref() else {
        return false;
    };
    if Path::new(value).exists() {
        return false;
    }
    let Some(repaired) = repair_missing_path(Path::new(value)) else {
        return false;
    };
    *field = Some(repaired);
    true
}

fn repair_missing_path(path: &Path) -> Option<String> {
    repair_windowsapps_path(path)
        .or_else(|| repair_sibling_exe_path(path))
        .map(|path| path.to_string_lossy().into_owned())
}

fn repair_windowsapps_path(path: &Path) -> Option<PathBuf> {
    windowsapps_repair_candidates(path, &registry_appx_package_names())
        .into_iter()
        .find(|candidate| candidate.exists())
}

#[derive(Clone)]
struct AppxIdentity {
    name: String,
    version: String,
    arch: String,
    publisher: String,
}

fn appx_identity(package_dir: &str) -> Option<AppxIdentity> {
    let (prefix, publisher) = package_dir.rsplit_once("__")?;
    let mut parts = prefix.split('_');
    Some(AppxIdentity {
        name: parts.next()?.to_string(),
        version: parts.next()?.to_string(),
        arch: parts.next()?.to_string(),
        publisher: publisher.to_string(),
    })
}

fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

fn appx_identity_matches(old: &AppxIdentity, current: &AppxIdentity) -> bool {
    old.name.eq_ignore_ascii_case(&current.name)
        && old.arch.eq_ignore_ascii_case(&current.arch)
        && old.publisher.eq_ignore_ascii_case(&current.publisher)
}

fn windowsapps_repair_candidates(path: &Path, package_dirs: &[String]) -> Vec<PathBuf> {
    let path_text = path.to_string_lossy();
    let lower = path_text.to_ascii_lowercase();
    let marker = "\\windowsapps\\";
    let Some(marker_start) = lower.find(marker) else {
        return Vec::new();
    };
    let package_start = marker_start + marker.len();
    let root = &path_text[..package_start];
    let after_root = &path_text[package_start..];
    let Some((old_package, rest)) = after_root.split_once('\\') else {
        return Vec::new();
    };
    let Some(old_identity) = appx_identity(old_package) else {
        return Vec::new();
    };

    let mut candidates: Vec<(Vec<u64>, PathBuf)> = package_dirs
        .iter()
        .filter_map(|package| {
            let identity = appx_identity(package)?;
            if !appx_identity_matches(&old_identity, &identity) {
                return None;
            }
            let path = PathBuf::from(format!("{root}{package}\\{rest}"));
            Some((version_key(&identity.version), path))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().map(|(_, path)| path).collect()
}

fn repair_sibling_exe_path(path: &Path) -> Option<PathBuf> {
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return None;
    }
    let parent = path.parent()?;
    if !parent.is_dir() {
        return None;
    }
    let candidates: Vec<PathBuf> = fs::read_dir(parent)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate.is_file()
                && candidate
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
                && !looks_like_uninstaller(candidate)
        })
        .collect();
    if candidates.len() == 1 {
        candidates.into_iter().next()
    } else {
        None
    }
}

fn looks_like_uninstaller(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase();
    name.contains("uninstall") || name.contains("unins") || name.contains("\u{5378}\u{8f7d}")
}

fn registry_appx_package_names() -> Vec<String> {
    unsafe {
        let subkey = "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\Repository\\Packages";
        let wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            0,
            KEY_READ,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return Vec::new();
        }

        let mut names = Vec::new();
        let mut index = 0;
        loop {
            let mut buffer = vec![0u16; 512];
            let mut len = buffer.len() as u32;
            let status = RegEnumKeyExW(
                key,
                index,
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
                None,
                PWSTR::null(),
                None,
                None,
            );
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status == ERROR_SUCCESS {
                names.push(String::from_utf16_lossy(&buffer[..len as usize]));
            }
            index += 1;
        }
        let _ = RegCloseKey(key);
        names
    }
}

fn add_item_at(path: &Path, label: &str, item_path_value: &Path) -> io::Result<bool> {
    // A missing config just means "no items yet". Any other read error — invalid
    // UTF-8, permission denied, transient I/O — must NOT fall back to a default
    // header, or the atomic write below would replace the user's unreadable file
    // with one containing only the new item. Mirrors `remove_item_at`.
    let mut content = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::from(HEADER),
        Err(error) => return Err(error),
    };
    let config = parse(&content);
    let new_key = path_key(item_path_value);
    if config
        .items
        .iter()
        .filter_map(item_path)
        .any(|existing| path_key(Path::new(existing)) == new_key)
    {
        return Ok(false);
    }

    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!(
        "\n[[item]]\nlabel = \"{}\"\npath = \"{}\"\n",
        quote(label),
        quote(&item_path_value.to_string_lossy())
    ));
    atomic::write(path, content.as_bytes())?;
    Ok(true)
}

fn migrate_legacy(legacy: &Path, target: &Path) -> io::Result<bool> {
    if target.exists() || !legacy.exists() || legacy == target {
        return Ok(false);
    }
    let bytes = fs::read(legacy)?;
    atomic::write(target, &bytes)?;
    Ok(true)
}

/// Read the config if it exists. `None` means "no config — use built-in defaults".
pub fn load() -> io::Result<Option<Config>> {
    let path = config_path();
    migrate_legacy(&legacy_config_path(), &path)?;
    match fs::read_to_string(&path) {
        Ok(text) => {
            let mut config = parse(&text);
            if repair_config_paths(&mut config) {
                let _ = write_config(&path, &config);
            }
            Ok(Some(config))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Write a starter config reflecting the resolved default apps, so it's editable.
pub fn write_default(items: &[DockItem]) -> io::Result<()> {
    let mut s = String::from(HEADER);
    for it in items {
        let Some(path) = &it.path else {
            continue;
        };
        s.push_str("\n[[item]]\n");
        s.push_str(&format!("label = \"{}\"\n", quote(&it.label)));
        s.push_str(&format!("path = \"{}\"\n", quote(path)));
    }
    atomic::write(&config_path(), s.as_bytes())
}

/// Remove the item whose launch path matches `item_path_value` (case-insensitive),
/// rewriting the config without it. Returns whether anything was removed.
pub fn remove_item(item_path_value: &str) -> io::Result<bool> {
    remove_item_at(&config_path(), item_path_value)
}

fn remove_item_at(path: &Path, item_path_value: &str) -> io::Result<bool> {
    let content = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let config = parse(&content);
    let target_key = path_key(Path::new(item_path_value));
    let mut kept = String::from(HEADER);
    let mut removed = false;
    for spec in &config.items {
        let key = item_path(spec).map(|value| path_key(Path::new(value)));
        if key.as_deref() == Some(target_key.as_str()) {
            removed = true;
            continue;
        }
    }
    if removed {
        for spec in &config.items {
            let key = item_path(spec).map(|value| path_key(Path::new(value)));
            if key.as_deref() != Some(target_key.as_str()) {
                write_spec(&mut kept, spec);
            }
        }
        atomic::write(path, kept.as_bytes())?;
    }
    Ok(removed)
}

/// Remove the `index`-th `[[item]]` (0-based, in file order), rewriting the config
/// without it. Removing by position — rather than by launch path — is what lets the
/// settings UI delete an entry written as `app = "chrome.exe"`, whose stored spec carries
/// no `path`/`exe` for a path match to hit. Out-of-range index is a no-op.
pub fn remove_item_at_index(index: usize) -> io::Result<bool> {
    remove_item_at_index_in(&config_path(), index)
}

fn remove_item_at_index_in(path: &Path, index: usize) -> io::Result<bool> {
    let content = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let config = parse(&content);
    if index >= config.items.len() {
        return Ok(false);
    }
    let mut kept = String::from(HEADER);
    for spec in config
        .items
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, spec)| spec)
    {
        write_spec(&mut kept, spec);
    }
    atomic::write(path, kept.as_bytes())?;
    Ok(true)
}

/// Serialize one `[[item]]` table the way the rewrite paths expect.
fn write_spec(out: &mut String, spec: &ItemSpec) {
    out.push_str("\n[[item]]\n");
    if let Some(label) = &spec.label {
        out.push_str(&format!("label = \"{}\"\n", quote(label)));
    }
    if let Some(value) = &spec.path {
        out.push_str(&format!("path = \"{}\"\n", quote(value)));
    } else if let Some(value) = &spec.exe {
        out.push_str(&format!("exe = \"{}\"\n", quote(value)));
    }
    if let Some(value) = &spec.app {
        out.push_str(&format!("app = \"{}\"\n", quote(value)));
    }
    if let Some(value) = &spec.icon {
        out.push_str(&format!("icon = \"{}\"\n", quote(value)));
    }
}

/// Append a new `[[item]]` to the config file (creating it with a header if needed).
pub fn add_item(label: &str, item_path_value: &str) -> io::Result<bool> {
    if !Path::new(item_path_value).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("path does not exist: {item_path_value}"),
        ));
    }
    add_item_at(&config_path(), label, Path::new(item_path_value))
}

pub fn parse(text: &str) -> Config {
    let mut items: Vec<ItemSpec> = Vec::new();
    let mut cur: Option<ItemSpec> = None;
    for raw in text.lines() {
        let line = strip_comment(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[item]]" {
            if let Some(it) = cur.take() {
                items.push(it);
            }
            cur = Some(ItemSpec::default());
            continue;
        }
        if line.starts_with('[') {
            continue; // ignore unknown tables
        }
        if let (Some((k, v)), Some(it)) = (line.split_once('='), cur.as_mut()) {
            let val = unquote(v);
            match k.trim() {
                "label" => it.label = Some(val),
                "path" => it.path = Some(val),
                "exe" => it.exe = Some(val),
                "app" => it.app = Some(val),
                "icon" => it.icon = Some(val),
                _ => {}
            }
        }
    }
    if let Some(it) = cur.take() {
        items.push(it);
    }
    Config { items }
}

fn strip_comment(s: &str) -> &str {
    if s.trim_start().starts_with('#') {
        return "";
    }

    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote == Some('"') {
            escaped = true;
            continue;
        }
        if ch == '"' || ch == '\'' {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            continue;
        }
        if ch == '#' && quote.is_none() {
            return &s[..index];
        }
    }
    s
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let mut result = String::new();
        let mut chars = s[1..s.len() - 1].chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                result.push(ch);
                continue;
            }
            match chars.next() {
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        }
        result
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("featherdock-config-{nonce}-{name}"))
    }

    #[test]
    fn add_item_fails_without_touching_unreadable_config() {
        let dir = temp_dir("unreadable-config");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("featherdock.toml");
        // Invalid UTF-8: a UTF-16 BOM followed by "AB".
        let original = [0xFFu8, 0xFE, 0x00, 0x41, 0x00, 0x42];
        fs::write(&path, original).unwrap();

        let result = add_item_at(&path, "App", Path::new(r"C:\app.exe"));

        assert!(result.is_err(), "adding to an unreadable config must fail");
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "failed add must not overwrite the original bytes"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_item_creates_config_when_file_missing() {
        let dir = temp_dir("missing-config");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("featherdock.toml");

        let result = add_item_at(&path, "My App", Path::new(r"C:\app.exe"));

        assert!(matches!(result, Ok(true)));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("label = \"My App\""));
        assert!(text.contains("path = \"C:\\\\app.exe\""));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_path_and_preserves_hash_inside_quotes() {
        let cfg = parse(
            r#"
[[item]]
label = "C # Tools"
path = "C:\\Apps\\C # Tools\\tool.exe" # trailing comment
"#,
        );

        assert_eq!(cfg.items.len(), 1);
        assert_eq!(cfg.items[0].label.as_deref(), Some("C # Tools"));
        assert_eq!(
            cfg.items[0].path.as_deref(),
            Some(r"C:\Apps\C # Tools\tool.exe")
        );
    }

    #[test]
    fn parses_escaped_quotes_and_backslashes() {
        let cfg = parse(
            r#"
[[item]]
label = "A \"quoted\" item"
path = "C:\\Tools\\app.exe"
"#,
        );
        assert_eq!(cfg.items[0].label.as_deref(), Some("A \"quoted\" item"));
        assert_eq!(cfg.items[0].path.as_deref(), Some(r"C:\Tools\app.exe"));
    }

    #[test]
    fn appdata_config_path_is_stable() {
        assert_eq!(
            config_path_from(Path::new(r"C:\Users\Test\AppData\Roaming")),
            PathBuf::from(r"C:\Users\Test\AppData\Roaming")
                .join("FeatherDock")
                .join("featherdock.toml")
        );
    }

    #[test]
    fn add_item_writes_path_and_rejects_duplicate_case_insensitively() {
        let dir = temp_dir("dedupe");
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("featherdock.toml");
        let item = dir.join("Photo.PNG");
        fs::write(&item, b"image").unwrap();

        assert!(add_item_at(&config, "Photo", &item).unwrap());
        let upper = PathBuf::from(item.to_string_lossy().to_uppercase());
        assert!(!add_item_at(&config, "Photo duplicate", &upper).unwrap());

        let text = fs::read_to_string(&config).unwrap();
        assert!(text.contains("path = "));
        assert_eq!(parse(&text).items.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_item_drops_only_the_matching_path() {
        let dir = temp_dir("remove");
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("featherdock.toml");
        let a = dir.join("A.exe");
        let b = dir.join("B.exe");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert!(add_item_at(&config, "A", &a).unwrap());
        assert!(add_item_at(&config, "B", &b).unwrap());

        // Case-insensitive match removes exactly one.
        let upper = PathBuf::from(a.to_string_lossy().to_uppercase());
        assert!(remove_item_at(&config, &upper.to_string_lossy()).unwrap());
        let remaining = parse(&fs::read_to_string(&config).unwrap());
        assert_eq!(remaining.items.len(), 1);
        assert_eq!(
            path_key(Path::new(remaining.items[0].path.as_deref().unwrap())),
            path_key(&b)
        );
        // Removing something absent is a no-op.
        assert!(!remove_item_at(&config, &a.to_string_lossy()).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_item_at_index_drops_app_entry_with_no_path() {
        let dir = temp_dir("remove-index");
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("featherdock.toml");
        // The first item is `app = "chrome.exe"` form (no path/exe) — the case that the
        // path-matching remove_item could never delete.
        fs::write(
            &config,
            "[[item]]\nlabel = \"Chrome\"\napp = \"chrome.exe\"\n\n[[item]]\nlabel = \"B\"\npath = \"C:\\\\B.exe\"\n",
        )
        .unwrap();
        assert_eq!(parse(&fs::read_to_string(&config).unwrap()).items.len(), 2);

        assert!(remove_item_at_index_in(&config, 0).unwrap());
        let remaining = parse(&fs::read_to_string(&config).unwrap());
        assert_eq!(remaining.items.len(), 1);
        assert_eq!(remaining.items[0].label.as_deref(), Some("B"));
        assert_eq!(remaining.items[0].path.as_deref(), Some(r"C:\B.exe"));

        // Out-of-range index is a no-op.
        assert!(!remove_item_at_index_in(&config, 5).unwrap());
        assert_eq!(parse(&fs::read_to_string(&config).unwrap()).items.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn windowsapps_repair_candidates_keep_subpath_on_newest_matching_package() {
        let old = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.623.9142.0_x64__2p2nqsd0c76g0\app\Codex.exe",
        );
        let packages = vec![
            "OpenAI.Codex_26.623.9142.0_x64__2p2nqsd0c76g0".to_string(),
            "OpenAI.Codex_26.623.13972.0_x64__2p2nqsd0c76g0".to_string(),
            "OpenAI.Codex_26.623.15000.0_arm64__2p2nqsd0c76g0".to_string(),
            "Other.App_99.0.0.0_x64__2p2nqsd0c76g0".to_string(),
        ];

        let candidates = windowsapps_repair_candidates(old, &packages);

        assert_eq!(
            candidates.first(),
            Some(&PathBuf::from(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.623.13972.0_x64__2p2nqsd0c76g0\app\Codex.exe"
            ))
        );
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn sibling_exe_repair_ignores_uninstaller() {
        let dir = temp_dir("sibling-exe");
        fs::create_dir_all(&dir).unwrap();
        let app = dir.join("Bilibili.exe");
        let uninstaller = dir.join("\u{5378}\u{8f7d}Bilibili.exe");
        fs::write(&app, b"app").unwrap();
        fs::write(&uninstaller, b"uninstall").unwrap();

        let repaired = repair_sibling_exe_path(&dir.join("garbled.exe"));

        assert_eq!(repaired, Some(app));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrates_legacy_config_without_overwriting_existing_target() {
        let dir = temp_dir("migration");
        fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("legacy.toml");
        let target = dir.join("data").join("featherdock.toml");
        fs::write(&legacy, "[[item]]\nexe = \"C:\\\\Tools\\\\app.exe\"\n").unwrap();

        assert!(migrate_legacy(&legacy, &target).unwrap());
        assert!(target.exists());
        fs::write(&target, "keep").unwrap();
        assert!(!migrate_legacy(&legacy, &target).unwrap());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        fs::remove_dir_all(&dir).unwrap();
    }
}
