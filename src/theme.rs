//! Colour themes of the panel (accent colours sampled from the reference).

use tiny_skia::Color;

/// Red is reserved for the bottleneck highlight.
pub const THEMES: [&str; 4] = ["green", "gold", "purple", "blue"];

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// Main colour: lit bars, strokes.
    pub accent: Color,
    /// Light tint of the accent: digit gradient top, corner accents, glow cores.
    pub bright: Color,
    /// Card borders (accent at ~45 %).
    pub border: Color,
    /// Unlit blocks and ticks (accent at ~15 %).
    pub empty: Color,
    /// Matrix rain colour (alpha is applied by the renderer).
    pub rain: Color,
    /// Labels and secondary text.
    pub text: Color,
    /// Values (near white, faintly tinted).
    pub value: Color,
    /// Dimmed text (units, small captions).
    pub dim: Color,
}

fn rgb(hex: u32) -> Color {
    Color::from_rgba8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, 255)
}

pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Color::from_rgba(
        l(a.red(), b.red()).clamp(0.0, 1.0),
        l(a.green(), b.green()).clamp(0.0, 1.0),
        l(a.blue(), b.blue()).clamp(0.0, 1.0),
        l(a.alpha(), b.alpha()).clamp(0.0, 1.0),
    )
    .unwrap_or(a)
}

pub fn alpha(c: Color, a: f32) -> Color {
    let mut c = c;
    c.set_alpha((c.alpha() * a).clamp(0.0, 1.0));
    c
}

impl Theme {
    fn derive(name: &'static str, accent: Color) -> Self {
        let white = Color::WHITE;
        let black = Color::BLACK;
        let text = mix(white, accent, 0.10);
        Theme {
            name,
            accent,
            bright: mix(accent, white, 0.55),
            border: alpha(accent, 0.55),
            empty: alpha(accent, 0.16),
            rain: accent,
            text: alpha(text, 0.92),
            value: mix(white, accent, 0.04),
            dim: mix(text, black, 0.24),
        }
    }
}

pub fn theme(name: &str) -> Option<Theme> {
    let accent = match name.to_ascii_lowercase().as_str() {
        "green" => (0x55cb5d, "green"),
        "gold" => (0xd1c398, "gold"),
        "purple" => (0x9d47bd, "purple"),
        // Sampled on frames 011-013 (bars ~ #2068b0..#3070c8), brightened a bit.
        "blue" => (0x2f80dc, "blue"),
        _ => return None,
    };
    Some(Theme::derive(accent.1, rgb(accent.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_themes_resolve() {
        for n in THEMES {
            assert_eq!(theme(n).unwrap().name, n);
        }
        assert!(theme("GREEN").is_some());
        assert!(theme("pink").is_none());
        assert!(theme("red").is_none());
    }
}
