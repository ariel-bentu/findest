use findest_core::config::{Appearance, ThemeMode};
use gpui::{Hsla, Rgba, WindowAppearance, rgb};

#[derive(Clone, Debug)]
pub struct Theme {
    pub background: Hsla,
    pub sidebar: Hsla,
    pub surface: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub selection: Hsla,
    pub selection_text: Hsla,
    pub hover: Hsla,
    pub danger: Hsla,
    pub font_size: f32,
    pub row_height: f32,
    pub font_family: Option<String>,
}

impl Theme {
    pub fn resolve(appearance: &Appearance, window: WindowAppearance) -> (Theme, Vec<String>) {
        let dark = match appearance.theme {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => matches!(window, WindowAppearance::Dark | WindowAppearance::VibrantDark),
        };
        let mut theme = if dark { Self::dark() } else { Self::light() };
        theme.font_size = appearance.font_size.clamp(8.0, 32.0);
        theme.row_height = appearance.row_height.max(theme.font_size + 6.0);
        theme.font_family = Some(appearance.font_family.clone()).filter(|f| !f.trim().is_empty());

        let mut errors = Vec::new();
        for (key, value) in &appearance.colors {
            let Some(color) = parse_hex(value) else {
                errors.push(format!("appearance.colors.{key}: \"{value}\" is not a hex color"));
                continue;
            };
            let slot = match key.as_str() {
                "background" => &mut theme.background,
                "sidebar" => &mut theme.sidebar,
                "surface" => &mut theme.surface,
                "border" => &mut theme.border,
                "text" => &mut theme.text,
                "text_muted" => &mut theme.text_muted,
                "accent" => &mut theme.accent,
                "selection" => &mut theme.selection,
                "selection_text" => &mut theme.selection_text,
                "hover" => &mut theme.hover,
                "danger" => &mut theme.danger,
                _ => {
                    errors.push(format!("appearance.colors: unknown color \"{key}\""));
                    continue;
                }
            };
            *slot = color;
        }
        (theme, errors)
    }

    fn dark() -> Theme {
        Theme {
            background: rgb(0x1e1f22).into(),
            sidebar: rgb(0x26272b).into(),
            surface: rgb(0x2b2d31).into(),
            border: rgb(0x3a3c42).into(),
            text: rgb(0xe6e6e6).into(),
            text_muted: rgb(0x9a9ca3).into(),
            accent: rgb(0x4c8dff).into(),
            selection: rgb(0x2f5fb3).into(),
            selection_text: rgb(0xffffff).into(),
            hover: rgb(0x2c2e33).into(),
            danger: rgb(0xff6b6b).into(),
            font_size: 13.0,
            row_height: 26.0,
            font_family: None,
        }
    }

    fn light() -> Theme {
        Theme {
            background: rgb(0xffffff).into(),
            sidebar: rgb(0xf3f3f5).into(),
            surface: rgb(0xf7f7f8).into(),
            border: rgb(0xdedfe3).into(),
            text: rgb(0x1d1d1f).into(),
            text_muted: rgb(0x6e6e73).into(),
            accent: rgb(0x0a64ff).into(),
            selection: rgb(0x0a64ff).into(),
            selection_text: rgb(0xffffff).into(),
            hover: rgb(0xf0f1f4).into(),
            danger: rgb(0xd70015).into(),
            font_size: 13.0,
            row_height: 26.0,
            font_family: None,
        }
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
fn parse_hex(value: &str) -> Option<Hsla> {
    let hex = value.trim().strip_prefix('#')?;
    let expanded: String = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).chain("ff".chars()).collect(),
        6 => format!("{hex}ff"),
        8 => hex.to_string(),
        _ => return None,
    };
    let n = u32::from_str_radix(&expanded, 16).ok()?;
    let c = |shift: u32| ((n >> shift) & 0xff) as f32 / 255.0;
    Some(Rgba { r: c(24), g: c(16), b: c(8), a: c(0) }.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#fff"), parse_hex("#ffffff"));
        assert!(parse_hex("#12345").is_none());
        assert!(parse_hex("red").is_none());
        let c: Rgba = parse_hex("#ff000080").unwrap().into();
        assert!((c.r - 1.0).abs() < 0.01 && (c.a - 0.5).abs() < 0.01);
    }

    #[test]
    fn overrides_and_errors() {
        let mut appearance = Appearance::default();
        appearance.theme = ThemeMode::Light;
        appearance.colors.insert("accent".into(), "#ff8800".into());
        appearance.colors.insert("nope".into(), "#000".into());
        let (theme, errors) = Theme::resolve(&appearance, WindowAppearance::Dark);
        assert_eq!(theme.accent, parse_hex("#ff8800").unwrap());
        assert_eq!(theme.background, rgb(0xffffff).into());
        assert_eq!(errors.len(), 1);
    }
}
