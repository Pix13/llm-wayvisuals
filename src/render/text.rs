//! Embedded fonts and backend-neutral text measuring / fitting (ab_glyph), so
//! the layout is identical whatever the drawing backend is.

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont, VariableFont};

use super::canvas::{FontFace, FontId};

pub const LABEL_TTF: &[u8] = include_bytes!("../../assets/fonts/rajdhani/Rajdhani-Medium.ttf");
pub const SEMI_TTF: &[u8] = include_bytes!("../../assets/fonts/rajdhani/Rajdhani-SemiBold.ttf");
pub const DIGITS_TTF: &[u8] = include_bytes!("../../assets/fonts/oxanium/Oxanium-VF.ttf");
/// `wght` value applied to the Oxanium variable font.
pub const DIGITS_WGHT: f32 = 700.0;

pub struct Fonts {
    /// Rajdhani Medium: labels, small text.
    pub label: FontFace,
    /// Rajdhani SemiBold: titles, values.
    pub semi: FontFace,
    /// Oxanium (variable, wght 700): giant digits.
    pub digits: FontFace,
}

impl Fonts {
    pub fn load() -> Self {
        let label = FontRef::try_from_slice(LABEL_TTF).expect("Rajdhani-Medium");
        let semi = FontRef::try_from_slice(SEMI_TTF).expect("Rajdhani-SemiBold");
        let mut digits = FontRef::try_from_slice(DIGITS_TTF).expect("Oxanium");
        digits.set_variation(b"wght", DIGITS_WGHT);
        Fonts {
            label: FontFace { id: FontId::Label, font: label, data: LABEL_TTF, variation: None },
            semi: FontFace { id: FontId::Semi, font: semi, data: SEMI_TTF, variation: None },
            digits: FontFace { id: FontId::Digits, font: digits, data: DIGITS_TTF, variation: Some((*b"wght", DIGITS_WGHT)) },
        }
    }

    pub fn get(&self, id: FontId) -> &FontFace {
        match id {
            FontId::Label => &self.label,
            FontId::Semi => &self.semi,
            FontId::Digits => &self.digits,
        }
    }
}

/// Hard cap on the characters of any displayed string (server-controlled
/// text must not make a frame slow). Cuts on a char boundary.
pub const MAX_CHARS: usize = 256;

pub fn cap(s: &str) -> &str {
    match s.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub fn text_width(font: &FontRef, size: f32, s: &str, tracking: f32) -> f32 {
    let s = cap(s);
    let sc = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev = None;
    for ch in s.chars() {
        let g = sc.glyph_id(ch);
        if let Some(p) = prev {
            w += sc.kern(p, g);
        }
        w += sc.h_advance(g) + tracking;
        prev = Some(g);
    }
    (w - tracking).max(0.0)
}

/// Largest size <= `size` (down to `min`) at which `s` fits in `max_w`.
pub fn fit_size(font: &FontRef, size: f32, min: f32, s: &str, max_w: f32, tracking: f32) -> f32 {
    let w = text_width(font, size, s, tracking);
    if w <= max_w || w <= 0.0 {
        size
    } else {
        (size * max_w / w).max(min)
    }
}

/// Truncates with an ellipsis so that it fits `max_w`. Linear time: glyph
/// advances are accumulated once (strings are capped at [`MAX_CHARS`]).
pub fn clip_text(font: &FontRef, size: f32, s: &str, max_w: f32, tracking: f32) -> String {
    let capped = cap(s);
    if capped.len() == s.len() && text_width(font, size, capped, tracking) <= max_w {
        return s.to_string();
    }
    let sc = font.as_scaled(PxScale::from(size));
    let ell = sc.glyph_id('…');
    let ell_adv = sc.h_advance(ell);
    // (byte offset of the char, width of the prefix before it incl. trailing tracking, its glyph)
    let mut pre: Vec<(usize, f32, GlyphId)> = Vec::new();
    let mut cum = 0.0f32;
    let mut prev: Option<GlyphId> = None;
    for (i, ch) in capped.char_indices() {
        let g = sc.glyph_id(ch);
        if let Some(p) = prev {
            cum += sc.kern(p, g);
        }
        pre.push((i, cum, g));
        cum += sc.h_advance(g) + tracking;
        prev = Some(g);
    }
    // keep the first k chars: width = prefix(k) + kern(last, …) + adv(…)
    let n = pre.len();
    let last_k = if capped.len() == s.len() { n.saturating_sub(1) } else { n };
    for k in (0..=last_k).rev() {
        let w = if k == 0 {
            ell_adv
        } else {
            let (_, c, g) = pre[k - 1];
            // pre[k-1].1 is the prefix before char k-1; add that char's own advance
            c + sc.h_advance(g) + tracking + sc.kern(g, ell) + ell_adv
        };
        if w <= max_w {
            let end = if k == n { capped.len() } else { pre[k].0 };
            return format!("{}…", &capped[..end]);
        }
    }
    String::new()
}
