//! sRGB color model: transfer-function conversion and premultiplied alpha.
//!
//! Design doc §07 requires a single, explicit color path so a missing or
//! doubled sRGB conversion can be isolated by the linear-gray diagnostic:
//! rendering a linear gradient then a semi-transparent rect must produce
//! `sRGB(0.5 linear) = 188`, not `128` (double-decoded) or `255` (skipped).

/// A premultiplied sRGB color (each channel ≤ alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Premultiplied red.
    pub r: u8,
    /// Premultiplied green.
    pub g: u8,
    /// Premultiplied blue.
    pub b: u8,
    /// Alpha.
    pub a: u8,
}

impl Color {
    /// Fully transparent black.
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// From straight (non-premultiplied) sRGB 8-bit channels.
    pub fn from_srgb8(r: u8, g: u8, b: u8, a: u8) -> Self {
        let af = a as u32;
        Self {
            r: ((r as u32 * af + 127) / 255) as u8,
            g: ((g as u32 * af + 127) / 255) as u8,
            b: ((b as u32 * af + 127) / 255) as u8,
            a,
        }
    }

    /// Opaque from straight sRGB.
    pub fn srgb(r: u8, g: u8, b: u8) -> Self {
        Self::from_srgb8(r, g, b, 255)
    }

    /// Straight-channel components (un-premultiplied, rounded).
    pub fn straight(&self) -> (u8, u8, u8, u8) {
        if self.a == 0 || self.a == 255 {
            return (self.r, self.g, self.b, self.a);
        }
        let a = self.a as u32;
        (
            ((self.r as u32 * 255 + a / 2) / a).min(255) as u8,
            ((self.g as u32 * 255 + a / 2) / a).min(255) as u8,
            ((self.b as u32 * 255 + a / 2) / a).min(255) as u8,
            self.a,
        )
    }
}

/// sRGB transfer function → linear. `c` in `0..=255`.
pub fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear → sRGB transfer function. `c` in `0.0..=1.0`, returns `0..=255`.
pub fn linear_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_edges() {
        assert_eq!(linear_to_srgb(0.0), 0);
        assert_eq!(linear_to_srgb(1.0), 255);
        // §07 diagnostic anchor: 0.5 linear ≈ 188 sRGB (not 128).
        assert_eq!(linear_to_srgb(0.5), 188);
        assert_eq!(srgb_to_linear(0), 0.0);
        assert!((srgb_to_linear(255) - 1.0).abs() < 1e-6);
        // round-trip
        for &v in &[1u8, 10, 128, 188, 254] {
            assert_eq!(linear_to_srgb(srgb_to_linear(v)), v);
        }
    }

    #[test]
    fn premultiply() {
        let c = Color::from_srgb8(200, 100, 50, 128);
        assert!(c.r <= c.a && c.g <= c.a && c.b <= c.a);
        let (r, g, b, a) = c.straight();
        assert_eq!(a, 128);
        assert!((r as i32 - 200).abs() <= 1);
        assert!((g as i32 - 100).abs() <= 1);
        assert!((b as i32 - 50).abs() <= 1);
    }
}
