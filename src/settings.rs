//! Persisted dock settings, kept in `%APPDATA%\FeatherDock\settings.toml`,
//! separate from the items config so the two parsers never interfere.
//!
//! Tiny zero-dependency `key = value` format. A real visual settings page can edit
//! this later; for now it's read at startup and writable via the menu.

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::theme::{DockStyle, ThemePreset};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub icon_size: u32,
    pub magnification: u32,
    pub bottom_gap: u32,
    pub opacity: u32,
    pub preview_delay_ms: u32,
    pub reduce_motion: bool,
}
impl Default for Layout {
    fn default() -> Self {
        Self {
            icon_size: 50,
            magnification: (crate::dock::MAX_SCALE * 100.0) as u32,
            bottom_gap: crate::dock::SHOWN_GAP as u32,
            opacity: 48,
            preview_delay_ms: 300,
            reduce_motion: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Settings {
    pub dock_mode: DockMode,
    pub layout: Layout,
    pub monitor: u32,
    pub search_hotkey: u32,
    pub follow_system: bool,
    pub intellihide: bool,
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
    /// Structural dock morphology decoupled from `theme` palette/material.
    pub dock_style: DockStyle,
    /// Dedicated right-side Agent zone + status lights / hover session card.
    pub agent_zone_enabled: bool,
    /// Very subtle slow shimmer along Running Agent icons (honours `reduce_motion`).
    pub agent_shimmer: bool,
    /// Single light bounce when an Agent session newly enters Waiting (suppressed when fullscreen).
    pub agent_bounce_waiting: bool,
}

impl Settings {
    pub fn effective_theme(&self) -> ThemePreset {
        if !self.follow_system {
            return self.theme;
        }
        unsafe {
            use windows::core::w;
            use windows::Win32::System::Registry::*;
            let mut light = 1u32;
            let mut size = 4u32;
            let _ = RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                w!("AppsUseLightTheme"),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut light as *mut _ as *mut core::ffi::c_void),
                Some(&mut size),
            );
            if light == 0 {
                ThemePreset::Compact
            } else {
                ThemePreset::Glass
            }
        }
    }
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            dock_mode: DockMode::Always,
            layout: Layout::default(),
            monitor: 0,
            search_hotkey: 1,
            follow_system: false,
            intellihide: false,
            hide_on_fullscreen: true,
            hide_on_maximized: true,
            taskbar_mode: TaskbarMode::Show,
            drawer_enabled: true,
            hide_desktop_icons: false,
            theme: ThemePreset::Glass,
            dock_style: DockStyle::Pill,
            agent_zone_enabled: true,
            agent_shimmer: true,
            agent_bounce_waiting: true,
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
# dock_style = \"pill\" | \"hairline\" | \"islands\" | \"shelf\" | \"tiles\" | \"fluent\"
# agent_zone_enabled = true | false  (group AI agents in the right-side Agent zone)
# agent_shimmer = true | false       (faint slow shimmer on active Running agent icons)
# agent_bounce_waiting = true | false (single gentle hop when an agent enters Waiting)
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
    let number = |low: u32, high: u32| value.parse::<u32>().ok().map(|v| v.clamp(low, high));
    match key.trim() {
        "icon_size" => {
            if let Some(v) = number(24, 80) {
                settings.layout.icon_size = v;
            }
        }
        "magnification" => {
            if let Some(v) = number(100, 220) {
                settings.layout.magnification = v;
            }
        }
        "bottom_gap" => {
            if let Some(v) = number(0, 32) {
                settings.layout.bottom_gap = v;
            }
        }
        "opacity" => {
            if let Some(v) = number(10, 100) {
                settings.layout.opacity = v;
            }
        }
        "preview_delay_ms" => {
            if let Some(v) = number(0, 1500) {
                settings.layout.preview_delay_ms = v;
            }
        }
        "monitor" => {
            if let Some(v) = number(0, 99) {
                settings.monitor = v;
            }
        }
        "search_hotkey" => {
            if let Some(v) = number(0, 3) {
                settings.search_hotkey = v;
            }
        }
        "follow_system" => settings.follow_system = value.eq_ignore_ascii_case("true"),
        "reduce_motion" => settings.layout.reduce_motion = value.eq_ignore_ascii_case("true"),
        "intellihide" => settings.intellihide = value.eq_ignore_ascii_case("true"),
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
        "dock_style" => {
            if let Some(style) = DockStyle::parse(value) {
                settings.dock_style = style;
            }
        }
        "agent_zone_enabled" => {
            settings.agent_zone_enabled = value.eq_ignore_ascii_case("true");
        }
        "agent_shimmer" => {
            settings.agent_shimmer = value.eq_ignore_ascii_case("true");
        }
        "agent_bounce_waiting" => {
            settings.agent_bounce_waiting = value.eq_ignore_ascii_case("true");
        }
        _ => {}
    }
}

/// Persist the current settings (creating the folder/file as needed).
pub fn save(settings: &Settings) -> io::Result<()> {
    let path = settings_path();
    let mut body = format!(
        "{HEADER}\ndock_mode = \"{}\"\nhide_on_fullscreen = {}\nhide_on_maximized = {}\ntaskbar_mode = \"{}\"\ndrawer_enabled = {}\nhide_desktop_icons = {}\ntheme = \"{}\"\ndock_style = \"{}\"\n",
        settings.dock_mode.as_str(),
        settings.hide_on_fullscreen,
        settings.hide_on_maximized,
        settings.taskbar_mode.as_str(),
        settings.drawer_enabled,
        settings.hide_desktop_icons,
        settings.theme.as_str(),
        settings.dock_style.as_str()
    );
    body.push_str(&format!(
        "agent_zone_enabled = {}\nagent_shimmer = {}\nagent_bounce_waiting = {}\n",
        settings.agent_zone_enabled, settings.agent_shimmer, settings.agent_bounce_waiting
    ));
    body.push_str(&format!("icon_size = {}\nmagnification = {}\nbottom_gap = {}\nopacity = {}\npreview_delay_ms = {}\nreduce_motion = {}\nmonitor = {}\nsearch_hotkey = {}\nfollow_system = {}\nintellihide = {}\n",settings.layout.icon_size,settings.layout.magnification,settings.layout.bottom_gap,settings.layout.opacity,settings.layout.preview_delay_ms,settings.layout.reduce_motion,settings.monitor,settings.search_hotkey,settings.follow_system,settings.intellihide));
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
        for line in
            "theme = \"compact\"\ndock_style = \"hairline\"\nhide_on_fullscreen = false".lines()
        {
            parse_line_into(&mut settings, line);
        }

        assert_eq!(settings.theme, crate::theme::ThemePreset::Compact);
        assert_eq!(settings.dock_style, DockStyle::Hairline);
        assert!(!settings.hide_on_fullscreen);
    }

    #[test]
    fn old_settings_without_dock_style_default_to_pill() {
        let mut settings = Settings::default();
        for line in "theme = \"macos\"\nopacity = 60".lines() {
            parse_line_into(&mut settings, line);
        }
        assert_eq!(settings.theme, ThemePreset::Macos);
        assert_eq!(settings.dock_style, DockStyle::Pill);
    }
}

#[cfg(test)]
mod layout_settings_regression_tests {
    use super::*;
    #[test]
    fn numeric_preferences_are_bounded_and_invalid_values_preserve_defaults() {
        let mut s = Settings::default();
        for line in [
            "icon_size = 9999",
            "magnification = 0",
            "opacity = 500",
            "bottom_gap = 99",
            "preview_delay_ms = 99999",
            "search_hotkey = 3",
            "monitor = 2",
            "reduce_motion = true",
        ] {
            parse_line_into(&mut s, line);
        }
        assert_eq!(s.layout.icon_size, 80);
        assert_eq!(s.layout.magnification, 100);
        assert_eq!(s.layout.opacity, 100);
        assert_eq!(s.layout.bottom_gap, 32);
        assert_eq!(s.layout.preview_delay_ms, 1500);
        assert_eq!(s.monitor, 2);
        assert!(s.layout.reduce_motion);
        parse_line_into(&mut s, "icon_size = oops");
        assert_eq!(s.layout.icon_size, 80);
    }
    #[test]
    fn old_settings_gain_safe_defaults_without_changing_system_policy() {
        let mut s = Settings::default();
        for line in "theme = glass\ntaskbar_mode = show\nhide_desktop_icons = false".lines() {
            parse_line_into(&mut s, line);
        }
        assert_eq!(s.layout, Layout::default());
        assert_eq!(s.monitor, 0);
        assert_eq!(s.search_hotkey, 1);
        assert!(!s.intellihide);
        assert!(!s.hide_desktop_icons);
        assert!(s.agent_zone_enabled);
        assert!(s.agent_shimmer);
        assert!(s.agent_bounce_waiting);
    }

    #[test]
    fn parses_agent_settings_flags() {
        let mut s = Settings::default();
        for line in
            "agent_zone_enabled = false\nagent_shimmer = false\nagent_bounce_waiting = false"
                .lines()
        {
            parse_line_into(&mut s, line);
        }
        assert!(!s.agent_zone_enabled);
        assert!(!s.agent_shimmer);
        assert!(!s.agent_bounce_waiting);
    }
}
