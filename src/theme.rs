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
            ThemePreset::Glass => "玻璃",
            ThemePreset::Compact => "紧凑",
            ThemePreset::Solid => "纯色",
            ThemePreset::Macos => "macOS",
            ThemePreset::Contrast => "高对比",
        }
    }

    pub fn parse(value: &str) -> Option<ThemePreset> {
        match value.trim().to_ascii_lowercase().as_str() {
            "glass" | "default" => Some(ThemePreset::Glass),
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
                pill_rgb: (0.10, 0.12, 0.18),
                pill_alpha: 0.54,
                border_alpha: 0.18,
                divider_alpha: 0.22,
                dot_rgb: (0.74, 0.86, 1.0),
                dot_alpha: 0.82,
            },
            ThemePreset::Compact => VisualTheme {
                pill_rgb: (0.02, 0.02, 0.03),
                pill_alpha: 0.90,
                border_alpha: 0.08,
                divider_alpha: 0.12,
                dot_rgb: (0.58, 0.96, 0.72),
                dot_alpha: 0.92,
            },
            ThemePreset::Solid => VisualTheme {
                pill_rgb: (0.18, 0.18, 0.17),
                pill_alpha: 1.0,
                border_alpha: 0.16,
                divider_alpha: 0.22,
                dot_rgb: (0.98, 0.72, 0.44),
                dot_alpha: 0.95,
            },
            ThemePreset::Macos => VisualTheme {
                pill_rgb: (0.72, 0.74, 0.78),
                pill_alpha: 0.70,
                border_alpha: 0.46,
                divider_alpha: 0.30,
                dot_rgb: (0.04, 0.05, 0.07),
                dot_alpha: 0.78,
            },
            ThemePreset::Contrast => VisualTheme {
                pill_rgb: (0.0, 0.0, 0.0),
                pill_alpha: 1.0,
                border_alpha: 0.70,
                divider_alpha: 0.58,
                dot_rgb: (1.0, 0.86, 0.18),
                dot_alpha: 1.0,
            },
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
