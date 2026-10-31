//! The [`Rgb`] color type and its parsing, mixing and contrast helpers.

use std::fmt;
use std::str::FromStr;

/// Error returned when a color string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseColorError {
    /// The input was empty (after trimming).
    #[error("empty color string")]
    Empty,
    /// The input looked like neither `#rrggbb`/`#rgb` nor KDE `r,g,b`.
    #[error("unrecognized color syntax: {0:?}")]
    Syntax(String),
    /// A numeric channel was outside `0..=255` or not a number.
    #[error("invalid color channel {0:?}")]
    Channel(String),
}

/// An opaque 8-bit-per-channel sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

/// Pure black; used only for contrast and mixing math.
pub(crate) const BLACK: Rgb = Rgb::new(0, 0, 0);
/// Pure white; used only for contrast and mixing math.
pub(crate) const WHITE: Rgb = Rgb::new(255, 255, 255);

impl Rgb {
    /// Builds a color from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Formats the color as lowercase `#rrggbb`.
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Parses a KDE color value: `r,g,b` with decimal channels (whitespace tolerated).
    /// A fourth alpha component (`r,g,b,a`) is accepted and ignored.
    pub fn parse_kde(s: &str) -> Option<Rgb> {
        let parts: Vec<&str> = s.trim().split(',').map(str::trim).collect();
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let mut ch = [0u8; 4];
        for (slot, part) in ch.iter_mut().zip(&parts) {
            *slot = part.parse::<u8>().ok()?;
        }
        Some(Rgb::new(ch[0], ch[1], ch[2]))
    }

    /// Parses `#rrggbb`, `rrggbb`, `#rgb` or `rgb` (case-insensitive). An 8-digit
    /// `#rrggbbaa` form is accepted and its alpha ignored.
    pub fn parse_hex(s: &str) -> Option<Rgb> {
        let s = s.trim();
        let s = s.strip_prefix('#').unwrap_or(s);
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
        match s.len() {
            6 | 8 => Some(Rgb::new(byte(0)?, byte(2)?, byte(4)?)),
            3 => {
                let nib = |i: usize| {
                    let v = u8::from_str_radix(s.get(i..=i)?, 16).ok()?;
                    Some(v * 17)
                };
                Some(Rgb::new(nib(0)?, nib(1)?, nib(2)?))
            }
            _ => None,
        }
    }

    /// Linear interpolation toward `other`: `t = 0` returns `self`, `t = 1` returns `other`.
    /// `t` is clamped to `0..=1`; a NaN `t` is treated as 0.
    pub fn mix(&self, other: Rgb, t: f32) -> Rgb {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        let lerp = |a: u8, b: u8| -> u8 {
            let v = f32::from(a) + (f32::from(b) - f32::from(a)) * t;
            // Clamped to the u8 range, so the cast cannot truncate.
            v.round().clamp(0.0, 255.0) as u8
        };
        Rgb::new(
            lerp(self.r, other.r),
            lerp(self.g, other.g),
            lerp(self.b, other.b),
        )
    }

    /// WCAG 2.x relative luminance in `0.0..=1.0`.
    pub fn relative_luminance(&self) -> f32 {
        fn lin(c: u8) -> f32 {
            let c = f32::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// WCAG contrast ratio against `other`, in `1.0..=21.0`. Symmetric.
    pub fn contrast(&self, other: Rgb) -> f32 {
        let a = self.relative_luminance();
        let b = other.relative_luminance();
        let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Returns `self` if it already reaches `min` contrast against `against`; otherwise
    /// moves it step by step toward whichever of black/white contrasts more with
    /// `against` until it does (or that extreme is reached).
    pub(crate) fn ensure_contrast(self, against: Rgb, min: f32) -> Rgb {
        if self.contrast(against) >= min {
            return self;
        }
        let target = if WHITE.contrast(against) >= BLACK.contrast(against) {
            WHITE
        } else {
            BLACK
        };
        (1..=20)
            .map(|i| self.mix(target, i as f32 * 0.05))
            .find(|c| c.contrast(against) >= min)
            .unwrap_or(target)
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl FromStr for Rgb {
    type Err = ParseColorError;

    /// Accepts either hex (`#rrggbb`, `#rgb`) or KDE (`r,g,b`) syntax.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(ParseColorError::Empty);
        }
        if t.contains(',') {
            if let Some(c) = Rgb::parse_kde(t) {
                return Ok(c);
            }
            let bad = t
                .split(',')
                .map(str::trim)
                .find(|p| p.parse::<u8>().is_err())
                .map(str::to_owned);
            return Err(match bad {
                Some(p) => ParseColorError::Channel(p),
                None => ParseColorError::Syntax(t.to_owned()),
            });
        }
        Rgb::parse_hex(t).ok_or_else(|| ParseColorError::Syntax(t.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let c = Rgb::new(61, 174, 233);
        assert_eq!(c.hex(), "#3daee9");
        assert_eq!(Rgb::parse_hex(&c.hex()), Some(c));
        assert_eq!(Rgb::parse_hex("3DAEE9"), Some(c));
        assert_eq!(Rgb::parse_hex("#3daee9ff"), Some(c));
        assert_eq!(Rgb::parse_hex("#fff"), Some(WHITE));
        assert_eq!(Rgb::parse_hex("#a1b"), Some(Rgb::new(0xaa, 0x11, 0xbb)));
        assert_eq!(c.to_string(), "#3daee9");
    }

    #[test]
    fn hex_rejects_garbage() {
        for s in [
            "", "#", "#12", "#12345", "#1234567", "#gggggg", "#+12345", "é12345", "#ééé",
        ] {
            assert_eq!(Rgb::parse_hex(s), None, "{s:?}");
        }
    }

    #[test]
    fn kde_parsing() {
        assert_eq!(Rgb::parse_kde("32,35,38"), Some(Rgb::new(32, 35, 38)));
        assert_eq!(Rgb::parse_kde(" 32 , 35 ,38 "), Some(Rgb::new(32, 35, 38)));
        assert_eq!(Rgb::parse_kde("32,35,38,255"), Some(Rgb::new(32, 35, 38)));
        assert_eq!(Rgb::parse_kde("32,35"), None);
        assert_eq!(Rgb::parse_kde("32,35,256"), None);
        assert_eq!(Rgb::parse_kde("32,35,-1"), None);
        assert_eq!(Rgb::parse_kde("a,b,c"), None);
        assert_eq!(Rgb::parse_kde("1,2,3,4,5"), None);
        assert_eq!(Rgb::parse_kde(""), None);
    }

    #[test]
    fn from_str_both_syntaxes() {
        assert_eq!("#202326".parse::<Rgb>(), Ok(Rgb::new(32, 35, 38)));
        assert_eq!("32,35,38".parse::<Rgb>(), Ok(Rgb::new(32, 35, 38)));
        assert_eq!("".parse::<Rgb>(), Err(ParseColorError::Empty));
        assert_eq!(
            "1,2,300".parse::<Rgb>(),
            Err(ParseColorError::Channel("300".into()))
        );
        assert!(matches!(
            "zz".parse::<Rgb>(),
            Err(ParseColorError::Syntax(_))
        ));
    }

    #[test]
    fn mix_endpoints_and_clamp() {
        let a = Rgb::new(0, 100, 200);
        let b = Rgb::new(200, 100, 0);
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert_eq!(a.mix(b, 0.5), Rgb::new(100, 100, 100));
        assert_eq!(a.mix(b, -3.0), a);
        assert_eq!(a.mix(b, 7.0), b);
        assert_eq!(a.mix(b, f32::NAN), a);
    }

    #[test]
    fn luminance_and_contrast() {
        assert!(BLACK.relative_luminance().abs() < 1e-6);
        assert!((WHITE.relative_luminance() - 1.0).abs() < 1e-6);
        assert!((BLACK.contrast(WHITE) - 21.0).abs() < 1e-3);
        assert!((WHITE.contrast(BLACK) - 21.0).abs() < 1e-3);
        assert!((WHITE.contrast(WHITE) - 1.0).abs() < 1e-6);
        // Known reference: #777777 on white is ~4.48.
        let gray = Rgb::new(0x77, 0x77, 0x77);
        assert!((gray.contrast(WHITE) - 4.48).abs() < 0.02);
    }

    #[test]
    fn ensure_contrast_reaches_target() {
        let bg = Rgb::new(32, 35, 38);
        let dim = Rgb::new(50, 50, 50);
        let fixed = dim.ensure_contrast(bg, 3.0);
        assert!(fixed.contrast(bg) >= 3.0);
        let fine = Rgb::new(200, 200, 200);
        assert_eq!(fine.ensure_contrast(bg, 3.0), fine);
    }
}
