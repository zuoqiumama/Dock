//! Tiny zero-dependency config: a list of `[[item]]` tables. Not full TOML — just
//! the subset we need (item tables with quoted-string values + comments).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

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

fn path_key(path: &Path) -> String {
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

fn add_item_at(path: &Path, label: &str, item_path_value: &Path) -> io::Result<bool> {
    let mut content = fs::read_to_string(path).unwrap_or_else(|_| String::from(HEADER));
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
    atomic_write(path, content.as_bytes())?;
    Ok(true)
}

fn migrate_legacy(legacy: &Path, target: &Path) -> io::Result<bool> {
    if target.exists() || !legacy.exists() || legacy == target {
        return Ok(false);
    }
    let bytes = fs::read(legacy)?;
    atomic_write(target, &bytes)?;
    Ok(true)
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "config has no parent"))?;
    fs::create_dir_all(parent)?;

    let temp = parent.join(format!(
        ".featherdock-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    let from = wide_path(&temp);
    let to = wide_path(path);
    let result = unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(io::Error::new(io::ErrorKind::Other, error.to_string()));
    }
    let _ = File::open(parent).and_then(|dir| dir.sync_all());
    Ok(())
}

/// Read the config if it exists. `None` means "no config — use built-in defaults".
pub fn load() -> io::Result<Option<Config>> {
    let path = config_path();
    migrate_legacy(&legacy_config_path(), &path)?;
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(parse(&text))),
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
    atomic_write(&config_path(), s.as_bytes())
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
        kept.push_str("\n[[item]]\n");
        if let Some(label) = &spec.label {
            kept.push_str(&format!("label = \"{}\"\n", quote(label)));
        }
        if let Some(value) = &spec.path {
            kept.push_str(&format!("path = \"{}\"\n", quote(value)));
        } else if let Some(value) = &spec.exe {
            kept.push_str(&format!("exe = \"{}\"\n", quote(value)));
        }
        if let Some(value) = &spec.app {
            kept.push_str(&format!("app = \"{}\"\n", quote(value)));
        }
        if let Some(value) = &spec.icon {
            kept.push_str(&format!("icon = \"{}\"\n", quote(value)));
        }
    }
    if removed {
        atomic_write(path, kept.as_bytes())?;
    }
    Ok(removed)
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
