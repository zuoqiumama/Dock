#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemePreset {
    #[default]
    Glass,
    Compact,
    Solid,
    Macos,
    Contrast,
}

#[derive(Clone, Copy, Debug)]
pub struct VisualTheme {
    pub pill_rgb: (f32, f32, f32),
    pub pill_alpha: f32,
    pub border_alpha: f32,
    pub divider_alpha: f32,
    pub dot_rgb: (f32, f32, f32),
    pub dot_alpha: f32,
    pub control_rgb: (f32, f32, f32),
    pub divider_rgb: (f32, f32, f32),
    pub pill_radius: f32,
}

/// Shared material for the drawer, control center, settings and native menus.
/// Values are straight RGB; the composition pipeline handles premultiplication.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceTheme {
    pub backdrop: bool,
    pub rgb: (f32, f32, f32),
    pub alpha: f32,
    pub text_rgb: (f32, f32, f32),
    pub muted_rgb: (f32, f32, f32),
    pub accent_rgb: (f32, f32, f32),
    pub border_rgb: (f32, f32, f32),
    pub border_alpha: f32,
    pub hover_rgb: (f32, f32, f32),
    pub hover_alpha: f32,
    pub selected_alpha: f32,
    pub divider_alpha: f32,
    pub radius: f32,
}

pub const fn rgb8(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

impl ThemePreset {
    pub const ALL: [ThemePreset; 5] = [
        ThemePreset::Glass,
        ThemePreset::Compact,
        ThemePreset::Solid,
        ThemePreset::Macos,
        ThemePreset::Contrast,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ThemePreset::Glass => "glass",
            ThemePreset::Compact => "compact",
            ThemePreset::Solid => "solid",
            ThemePreset::Macos => "macos",
            ThemePreset::Contrast => "contrast",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemePreset::Glass => "暖瓷玻璃",
            ThemePreset::Compact => "紧凑",
            ThemePreset::Solid => "纯色",
            ThemePreset::Macos => "macOS",
            ThemePreset::Contrast => "高对比",
        }
    }

    pub fn parse(value: &str) -> Option<ThemePreset> {
        match value.trim().to_ascii_lowercase().as_str() {
            "glass" | "default" | "warm-glass" => Some(ThemePreset::Glass),
            "compact" => Some(ThemePreset::Compact),
            "solid" => Some(ThemePreset::Solid),
            "macos" | "mac" => Some(ThemePreset::Macos),
            "contrast" | "high-contrast" | "high_contrast" => Some(ThemePreset::Contrast),
            _ => None,
        }
    }

    pub fn visual(self) -> VisualTheme {
        match self {
            ThemePreset::Glass => VisualTheme {
                pill_rgb: rgb8(255, 251, 246),
                pill_alpha: 0.48,
                border_alpha: 0.74,
                divider_alpha: 0.16,
                dot_rgb: rgb8(56, 84, 68),
                dot_alpha: 1.0,
                control_rgb: rgb8(56, 54, 51),
                divider_rgb: rgb8(107, 102, 94),
                pill_radius: 32.0,
            },
            ThemePreset::Compact => VisualTheme {
                pill_rgb: (0.02, 0.02, 0.03),
                pill_alpha: 0.90,
                border_alpha: 0.08,
                divider_alpha: 0.12,
                dot_rgb: (0.58, 0.96, 0.72),
                dot_alpha: 0.92,
                control_rgb: (0.92, 0.94, 0.98),
                divider_rgb: (1.0, 1.0, 1.0),
                pill_radius: 22.0,
            },
            ThemePreset::Solid => VisualTheme {
                pill_rgb: (0.18, 0.18, 0.17),
                pill_alpha: 1.0,
                border_alpha: 0.16,
                divider_alpha: 0.22,
                dot_rgb: (0.98, 0.72, 0.44),
                dot_alpha: 0.95,
                control_rgb: (0.92, 0.94, 0.98),
                divider_rgb: (1.0, 1.0, 1.0),
                pill_radius: 22.0,
            },
            ThemePreset::Macos => VisualTheme {
                pill_rgb: (0.72, 0.74, 0.78),
                pill_alpha: 0.70,
                border_alpha: 0.46,
                divider_alpha: 0.30,
                dot_rgb: (0.04, 0.05, 0.07),
                dot_alpha: 0.78,
                control_rgb: (0.12, 0.14, 0.16),
                divider_rgb: (0.12, 0.14, 0.16),
                pill_radius: 22.0,
            },
            ThemePreset::Contrast => VisualTheme {
                pill_rgb: (0.0, 0.0, 0.0),
                pill_alpha: 1.0,
                border_alpha: 0.70,
                divider_alpha: 0.58,
                dot_rgb: (1.0, 0.86, 0.18),
                dot_alpha: 1.0,
                control_rgb: (1.0, 1.0, 1.0),
                divider_rgb: (1.0, 1.0, 1.0),
                pill_radius: 22.0,
            },
        }
    }

    pub fn dock_alpha(self, backdrop_enabled: bool) -> f32 {
        if self == Self::Glass && !backdrop_enabled {
            0.92
        } else {
            self.visual().pill_alpha
        }
    }

    pub fn surface(self) -> SurfaceTheme {
        let visual = self.visual();
        let light = matches!(self, Self::Glass | Self::Macos);
        SurfaceTheme {
            backdrop: self == Self::Glass,
            rgb: visual.pill_rgb,
            alpha: if self == Self::Glass { 0.88 } else { 0.97 },
            text_rgb: if light {
                rgb8(56, 54, 51)
            } else {
                rgb8(242, 242, 242)
            },
            muted_rgb: if light {
                rgb8(107, 102, 94)
            } else {
                rgb8(190, 192, 196)
            },
            accent_rgb: if self == Self::Glass {
                rgb8(104, 136, 121)
            } else if light {
                rgb8(52, 89, 116)
            } else {
                visual.dot_rgb
            },
            border_rgb: (1.0, 1.0, 1.0),
            border_alpha: visual.border_alpha,
            hover_rgb: if light {
                (1.0, 1.0, 1.0)
            } else {
                rgb8(150, 161, 176)
            },
            hover_alpha: if light { 0.42 } else { 0.16 },
            selected_alpha: if light { 0.14 } else { 0.26 },
            divider_alpha: if light { 0.12 } else { 0.18 },
            radius: if self == Self::Glass { 32.0 } else { 18.0 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_theme_names_case_insensitively() {
        assert_eq!(ThemePreset::parse("glass"), Some(ThemePreset::Glass));
        assert_eq!(ThemePreset::parse("COMPACT"), Some(ThemePreset::Compact));
        assert_eq!(ThemePreset::parse("solid"), Some(ThemePreset::Solid));
        assert_eq!(ThemePreset::parse("macos"), Some(ThemePreset::Macos));
        assert_eq!(ThemePreset::parse("contrast"), Some(ThemePreset::Contrast));
        assert_eq!(ThemePreset::parse("unknown"), None);
    }

    #[test]
    fn compact_theme_has_smaller_visual_density_than_glass() {
        let glass = ThemePreset::Glass.visual();
        let compact = ThemePreset::Compact.visual();
        assert!(compact.pill_alpha >= glass.pill_alpha);
        assert!(compact.border_alpha <= glass.border_alpha);
    }

    #[test]
    fn glass_dock_is_more_transparent_without_weakening_fallbacks() {
        let glass = ThemePreset::Glass;
        assert_eq!(glass.visual().pill_alpha, 0.48);
        assert_eq!(glass.dock_alpha(true), 0.48);
        assert_eq!(glass.dock_alpha(false), 0.92);
        assert_eq!(glass.surface().alpha, 0.88);
        for preset in ThemePreset::ALL.into_iter().filter(|p| *p != glass) {
            assert_eq!(preset.dock_alpha(false), preset.visual().pill_alpha);
        }
    }

    #[test]
    fn adjacent_theme_pills_are_obviously_distinct() {
        for pair in ThemePreset::ALL.windows(2) {
            let a = pair[0].visual();
            let b = pair[1].visual();
            let rgb_delta = (a.pill_rgb.0 - b.pill_rgb.0).abs()
                + (a.pill_rgb.1 - b.pill_rgb.1).abs()
                + (a.pill_rgb.2 - b.pill_rgb.2).abs();
            let alpha_delta = (a.pill_alpha - b.pill_alpha).abs();
            assert!(
                rgb_delta >= 0.18 || alpha_delta >= 0.25,
                "{:?} and {:?} are too similar: rgb_delta={rgb_delta:.3}, alpha_delta={alpha_delta:.3}",
                pair[0],
                pair[1]
            );
        }
    }
}
