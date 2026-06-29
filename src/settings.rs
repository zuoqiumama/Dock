//! Persisted dock settings, kept in `%APPDATA%\FeatherDock\settings.toml`,
//! separate from the items config so the two parsers never interfere.
//!
//! Tiny zero-dependency `key = value` format. A real visual settings page can edit
//! this later; for now it's read at startup and writable via the menu.

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::theme::ThemePreset;

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
    /// In Always mode, FULLY hide the dock while a fullscreen app (game/video) is in
    /// front — and don't let a bottom-edge hover summon it (never disturb the game).
    pub hide_on_fullscreen: bool,
    /// In Always mode, retract the dock to its reveal strip while a *maximized* window is
    /// in front, so it doesn't cover the window — but a bottom-edge hover still summons
    /// it (unlike fullscreen). Classic auto-hide, scoped to "something is maximized".
    pub hide_on_maximized: bool,
    pub taskbar_mode: TaskbarMode,
    /// Show the app-drawer button (the "app grid" launcher) on the dock. Off → the
    /// dock omits it entirely, for users who don't want the drawer.
    pub drawer_enabled: bool,
    /// Hide the Windows desktop icons (the SysListView32 toggle Explorer's right-click
    /// "Show desktop icons" flips). Reversible, non-destructive; restored on exit.
    pub hide_desktop_icons: bool,
    /// Visual preset for the dock pill and subtle cues. Kept intentionally small:
    /// presets change paint constants without starting a skin/plugin system.
    pub theme: ThemePreset,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            dock_mode: DockMode::Always,
            hide_on_fullscreen: true,
            hide_on_maximized: true,
            taskbar_mode: TaskbarMode::Show,
            drawer_enabled: true,
            hide_desktop_icons: false,
            theme: ThemePreset::Glass,
        }
    }
}

const HEADER: &str = "\
# FeatherDock settings.
# dock_mode = \"always\"  -> resident at the bottom (retracts for fullscreen/maximized)
# dock_mode = \"autohide\" -> classic auto-hide (reveal by hitting the bottom edge)
# hide_on_fullscreen = true | false  (fullscreen game: fully hidden, no hover-reveal)
# hide_on_maximized  = true | false  (maximized window: retract to strip, hover reveals)
# taskbar_mode = \"show\" | \"autohide\" | \"hidden\"
# drawer_enabled = true | false  (show the app-drawer button on the dock)
# hide_desktop_icons = true | false  (hide the Windows desktop icons while running)
# theme = \"glass\" | \"compact\" | \"solid\" | \"macos\" | \"contrast\"
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
        parse_line_into(&mut settings, line);
    }
    settings
}

fn parse_line_into(settings: &mut Settings, line: &str) {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return;
    }
    let Some((key, value)) = line.split_once('=') else {
        return;
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
        "hide_on_maximized" => {
            settings.hide_on_maximized = value.eq_ignore_ascii_case("true");
        }
        "taskbar_mode" => {
            if let Some(mode) = TaskbarMode::parse(value) {
                settings.taskbar_mode = mode;
            }
        }
        "drawer_enabled" => {
            settings.drawer_enabled = value.eq_ignore_ascii_case("true");
        }
        "hide_desktop_icons" => {
            settings.hide_desktop_icons = value.eq_ignore_ascii_case("true");
        }
        "theme" => {
            if let Some(theme) = ThemePreset::parse(value) {
                settings.theme = theme;
            }
        }
        _ => {}
    }
}

/// Persist the current settings (creating the folder/file as needed).
pub fn save(settings: &Settings) -> io::Result<()> {
    let path = settings_path();
    let body = format!(
        "{HEADER}\ndock_mode = \"{}\"\nhide_on_fullscreen = {}\nhide_on_maximized = {}\ntaskbar_mode = \"{}\"\ndrawer_enabled = {}\nhide_desktop_icons = {}\ntheme = \"{}\"\n",
        settings.dock_mode.as_str(),
        settings.hide_on_fullscreen,
        settings.hide_on_maximized,
        settings.taskbar_mode.as_str(),
        settings.drawer_enabled,
        settings.hide_desktop_icons,
        settings.theme.as_str()
    );
    crate::atomic::write(&path, body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mode_and_flag() {
        // Defaults when empty.
        assert!(matches!(Settings::default().dock_mode, DockMode::Always));
        assert!(Settings::default().hide_on_fullscreen);
        assert!(Settings::default().hide_on_maximized);
        assert_eq!(Settings::default().theme, crate::theme::ThemePreset::Glass);

        assert_eq!(DockMode::parse("autohide"), Some(DockMode::AutoHide));
        assert_eq!(DockMode::parse("ALWAYS"), Some(DockMode::Always));
        assert_eq!(DockMode::parse("nonsense"), None);
    }

    #[test]
    fn parses_theme_preset_without_disturbing_other_settings() {
        let mut settings = Settings::default();
        for line in "theme = \"compact\"\nhide_on_fullscreen = false".lines() {
            parse_line_into(&mut settings, line);
        }

        assert_eq!(settings.theme, crate::theme::ThemePreset::Compact);
        assert!(!settings.hide_on_fullscreen);
    }
}
