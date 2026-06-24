//! Persisted dock settings, kept in `%APPDATA%\FeatherDock\settings.toml`,
//! separate from the items config so the two parsers never interfere.
//!
//! Tiny zero-dependency `key = value` format. A real visual settings page can edit
//! this later; for now it's read at startup and writable via the menu.

use std::fs;
use std::io;
use std::path::PathBuf;

/// How the dock occupies the bottom of the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockMode {
    /// Resident at the bottom; only retracts when a fullscreen app is foreground.
    Always,
    /// Classic auto-hide: stays hidden until the cursor hits the bottom edge.
    AutoHide,
}

impl DockMode {
    fn as_str(self) -> &'static str {
        match self {
            DockMode::Always => "always",
            DockMode::AutoHide => "autohide",
        }
    }
    fn parse(value: &str) -> Option<DockMode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "always" => Some(DockMode::Always),
            "autohide" | "auto-hide" | "auto_hide" => Some(DockMode::AutoHide),
            _ => None,
        }
    }
}

/// What to do with the real Windows taskbar while FeatherDock runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskbarMode {
    /// Leave the system taskbar visible (default — non-destructive).
    Show,
    /// Set the system taskbar to auto-hide (OS-managed; reclaims space, pops on hover).
    AutoHide,
    /// Fully hide the system taskbar (most "pure dock"; restored on exit).
    Hidden,
}

impl TaskbarMode {
    fn as_str(self) -> &'static str {
        match self {
            TaskbarMode::Show => "show",
            TaskbarMode::AutoHide => "autohide",
            TaskbarMode::Hidden => "hidden",
        }
    }
    fn parse(value: &str) -> Option<TaskbarMode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "show" | "none" => Some(TaskbarMode::Show),
            "autohide" | "auto-hide" | "auto_hide" => Some(TaskbarMode::AutoHide),
            "hidden" | "hide" => Some(TaskbarMode::Hidden),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Settings {
    pub dock_mode: DockMode,
    /// In Always mode, hide the dock while a fullscreen app is in front.
    pub hide_on_fullscreen: bool,
    pub taskbar_mode: TaskbarMode,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            dock_mode: DockMode::Always,
            hide_on_fullscreen: true,
            taskbar_mode: TaskbarMode::Show,
        }
    }
}

const HEADER: &str = "\
# FeatherDock settings.
# dock_mode = \"always\"  -> resident at the bottom (retracts only for fullscreen apps)
# dock_mode = \"autohide\" -> classic auto-hide (reveal by hitting the bottom edge)
# hide_on_fullscreen = true | false
# taskbar_mode = \"show\" | \"autohide\" | \"hidden\"
";

fn settings_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("FeatherDock")
        .join("settings.toml")
}

/// Load settings, falling back to defaults for anything missing or unreadable.
pub fn load() -> Settings {
    let mut settings = Settings::default();
    let Ok(text) = fs::read_to_string(settings_path()) else {
        return settings;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "dock_mode" => {
                if let Some(mode) = DockMode::parse(value) {
                    settings.dock_mode = mode;
                }
            }
            "hide_on_fullscreen" => {
                settings.hide_on_fullscreen = value.eq_ignore_ascii_case("true");
            }
            "taskbar_mode" => {
                if let Some(mode) = TaskbarMode::parse(value) {
                    settings.taskbar_mode = mode;
                }
            }
            _ => {}
        }
    }
    settings
}

/// Persist the current settings (creating the folder/file as needed).
pub fn save(settings: &Settings) -> io::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = format!(
        "{HEADER}\ndock_mode = \"{}\"\nhide_on_fullscreen = {}\ntaskbar_mode = \"{}\"\n",
        settings.dock_mode.as_str(),
        settings.hide_on_fullscreen,
        settings.taskbar_mode.as_str()
    );
    fs::write(&path, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mode_and_flag() {
        // Defaults when empty.
        assert!(matches!(Settings::default().dock_mode, DockMode::Always));
        assert!(Settings::default().hide_on_fullscreen);

        assert_eq!(DockMode::parse("autohide"), Some(DockMode::AutoHide));
        assert_eq!(DockMode::parse("ALWAYS"), Some(DockMode::Always));
        assert_eq!(DockMode::parse("nonsense"), None);
    }
}
