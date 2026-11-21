//! Deterministic CPU raster surface: premultiplied RGBA buffer with
//! source-over compositing, clip stack and the draw ops standard widgets need
//! (solid/rounded rects, lines, pixel blits). Same ops → same pixels.

use crate::color::Color;
use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::HostCrash, Phase::Ui, cause)
}

/// Integer rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge (inclusive).
    pub x: i32,
    /// Top edge (inclusive).
    pub y: i32,
    /// Right edge (exclusive).
    pub x1: i32,
    /// Bottom edge (exclusive).
    pub y1: i32,
}

impl Rect {
    /// New rect; `w`/`h` are extents, not coordinates.
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            x,
            y,
            x1: x + w,
            y1: y + h,
        }
    }
    /// Intersection; may be empty.
    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect {
            x: self.x.max(o.x),
            y: self.y.max(o.y),
            x1: self.x1.min(o.x1),
            y1: self.y1.min(o.y1),
        }
    }
    /// Non-empty?
    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x || self.y1 <= self.y
    }
    /// Contains point.
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x1 && py >= self.y && py < self.y1
    }
}

/// Premultiplied-RGBA8 raster buffer in sRGB space.
pub struct Surface {
    w: usize,
    h: usize,
    px: Vec<u8>,
    clips: Vec<Rect>,
}

/// `Surface::new`'s size rule, checked without allocating.
pub(crate) fn check_size(w: usize, h: usize) -> Result<()> {
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(err("ui.surface.size"));
    }
    Ok(())
}

impl Surface {
    /// New transparent `w`×`h` surface.
    pub fn new(w: usize, h: usize) -> Result<Self> {
        check_size(w, h)?;
        Ok(Self {
            w,
            h,
            px: vec![0; w * h * 4],
            clips: vec![Rect::new(0, 0, w as i32, h as i32)],
        })
    }

    /// Width in px.
    pub fn width(&self) -> usize {
        self.w
    }
    /// Height in px.
    pub fn height(&self) -> usize {
        self.h
    }
    /// Raw premultiplied RGBA bytes (deterministic).
    pub fn pixels(&self) -> &[u8] {
        &self.px
    }

    /// Push an intersected clip; `pop_clip` restores.
    pub fn clip(&mut self, r: Rect) {
        let c = self.clips.last().copied().unwrap().intersect(&r);
        self.clips.push(c);
    }
    /// Pop the last pushed clip.
    pub fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }

    /// Depth of the clip stack (base level = 1).
    pub(crate) fn clip_depth(&self) -> usize {
        self.clips.len()
    }

    /// Drop clip levels above `depth` (clamped to the base level).
    pub(crate) fn truncate_clips(&mut self, depth: usize) {
        self.clips.truncate(depth.max(1));
    }

    /// Premultiplied source-over at (x,y) — the only blend Android widgets use.
    fn src_over(&mut self, x: i32, y: i32, src: Color) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        if !self.clips.last().unwrap().contains(x, y) {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 4;
        let sa = src.a as u32;
        if sa == 0 {
            return;
        }
        let inv = 255 - sa;
        self.px[i] = (src.r as u32 + (self.px[i] as u32 * inv + 127) / 255) as u8;
        self.px[i + 1] = (src.g as u32 + (self.px[i + 1] as u32 * inv + 127) / 255) as u8;
        self.px[i + 2] = (src.b as u32 + (self.px[i + 2] as u32 * inv + 127) / 255) as u8;
        self.px[i + 3] = (sa + (self.px[i + 3] as u32 * inv + 127) / 255) as u8;
    }

    /// Clear to a color.
    pub fn clear(&mut self, c: Color) {
        let clip = *self.clips.last().unwrap();
        for y in clip.y..clip.y1 {
            for x in clip.x..clip.x1 {
                if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
                    continue;
                }
                let i = (y as usize * self.w + x as usize) * 4;
                self.px[i..i + 4].copy_from_slice(&[c.r, c.g, c.b, c.a]);
            }
        }
    }

    /// Solid axis-aligned rect (source-over).
    pub fn fill_rect(&mut self, r: Rect, c: Color) {
        let clip = self.clips.last().unwrap().intersect(&r);
        for y in clip.y..clip.y1 {
            for x in clip.x..clip.x1 {
                self.src_over(x, y, c);
            }
        }
    }

    /// Rounded rect: rect minus quarter-circle corners of radius `rad`.
    /// Corners are coverage-quantized (inside/outside) — deterministic.
    pub fn fill_rounded_rect(&mut self, r: Rect, rad: i32, c: Color) {
        let rad = rad.max(0);
        let clip = self.clips.last().unwrap().intersect(&r);
        let r2 = rad * rad;
        for y in clip.y..clip.y1 {
            for x in clip.x..clip.x1 {
                let mut dx = 0;
                let mut dy = 0;
                if rad > 0 {
                    if x < r.x + rad && y < r.y + rad {
                        dx = r.x + rad - x - 1;
                        dy = r.y + rad - y - 1;
                    } else if x >= r.x1 - rad && y < r.y + rad {
                        dx = x - (r.x1 - rad);
                        dy = r.y + rad - y - 1;
                    } else if x < r.x + rad && y >= r.y1 - rad {
                        dx = r.x + rad - x - 1;
                        dy = y - (r.y1 - rad) + 1;
                    } else if x >= r.x1 - rad && y >= r.y1 - rad {
                        dx = x - (r.x1 - rad);
                        dy = y - (r.y1 - rad) + 1;
                    }
                }
                if dx * dx + dy * dy <= r2 {
                    self.src_over(x, y, c);
                }
            }
        }
    }

    /// Thick line via distance-to-segment, `w` px wide, source-over.
    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, w: i32, c: Color) {
        let w = w.max(1) as f32;
        let half = w / 2.0;
        let clip = *self.clips.last().unwrap();
        let (dx, dy) = ((x1 - x0) as f32, (y1 - y0) as f32);
        let len2 = dx * dx + dy * dy;
        for y in clip.y..clip.y1 {
            for x in clip.x..clip.x1 {
                let t = if len2 == 0.0 {
                    0.0
                } else {
                    (((x - x0) as f32 * dx + (y - y0) as f32 * dy) / len2).clamp(0.0, 1.0)
                };
                let px = x0 as f32 + t * dx;
                let py = y0 as f32 + t * dy;
                let d = ((x as f32 - px).powi(2) + (y as f32 - py).powi(2)).sqrt();
                if d <= half {
                    self.src_over(x, y, c);
                }
            }
        }
    }

    /// Blit premultiplied pixels (RGBA8, `src_w`×`src_h`) at (dx,dy).
    pub fn blit(&mut self, dx: i32, dy: i32, src_w: usize, src_h: usize, src: &[u8]) -> Result<()> {
        if src.len() != src_w * src_h * 4 {
            return Err(err("ui.blit.size"));
        }
        for sy in 0..src_h {
            for sx in 0..src_w {
                let i = (sy * src_w + sx) * 4;
                self.src_over(
                    dx + sx as i32,
                    dy + sy as i32,
                    Color {
                        r: src[i],
                        g: src[i + 1],
                        b: src[i + 2],
                        a: src[i + 3],
                    },
                );
            }
        }
        Ok(())
    }

    /// Blend one coverage-weighted source pixel (`src-over`, premul space).
    /// `cov` scales `c`'s alpha (0..=255).
    pub(crate) fn blend_pixel(&mut self, x: i32, y: i32, c: Color, cov: u8) {
        if !self.clips.last().unwrap().contains(x, y) || cov == 0 {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 4;
        let sc = [c.r, c.g, c.b, c.a];
        let sa = (sc[3] as u32 * cov as u32 + 127) / 255;
        if sa == 0 {
            return;
        }
        let d = &mut self.px[i..i + 4];
        let inv = 255 - sa;
        for (k, sv) in sc.iter().enumerate() {
            let sv = (*sv as u32 * cov as u32 + 127) / 255;
            d[k] = (sv + d[k] as u32 * inv / 255).min(255) as u8;
        }
    }

    /// Read back one pixel (for tests/diagnostics).
    pub fn pixel(&self, x: i32, y: i32) -> Color {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return Color::TRANSPARENT;
        }
        let i = (y as usize * self.w + x as usize) * 4;
        Color {
            r: self.px[i],
            g: self.px[i + 1],
            b: self.px[i + 2],
            a: self.px[i + 3],
        }
    }
}

/// §07 diagnostic: paint a left→right linear gray gradient, then a 50%-
/// transparent rect over the middle. Returns the center pixel; the expected
/// result is ~226 sRGB (linear 0.5 → 188 gray, over-blended by 0.5 linear gray
/// ≈ 0.75 linear → ~226), isolating a missing/double conversion.
pub fn gray_gradient_diagnostic(s: &mut Surface) -> Color {
    let w = s.w as i32;
    for x in 0..w {
        let lin = x as f32 / (w - 1) as f32;
        let v = crate::color::linear_to_srgb(lin);
        s.fill_rect(Rect::new(x, 0, 1, s.h as i32), Color::srgb(v, v, v));
    }
    s.fill_rect(
        Rect::new(w / 4, 0, w / 2, s.h as i32),
        Color::from_srgb8(128, 128, 128, 128),
    );
    s.pixel(w / 2, s.h as i32 / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rect_and_clip() {
        let mut s = Surface::new(8, 8).unwrap();
        let red = Color::srgb(255, 0, 0);
        s.fill_rect(Rect::new(0, 0, 8, 8), red);
        assert_eq!(s.pixel(4, 4), red);
        s.clip(Rect::new(0, 0, 4, 4));
        let blue = Color::srgb(0, 0, 255);
        s.fill_rect(Rect::new(0, 0, 8, 8), blue);
        assert_eq!(s.pixel(1, 1), blue);
        assert_eq!(s.pixel(5, 5), red);
        s.pop_clip();
        s.fill_rect(Rect::new(6, 6, 2, 2), blue);
        assert_eq!(s.pixel(7, 7), blue);
    }

    #[test]
    fn alpha_over_golden() {
        let mut s = Surface::new(2, 2).unwrap();
        s.clear(Color::srgb(255, 255, 255));
        // 50% black over white => ~128 straight
        s.fill_rect(Rect::new(0, 0, 2, 2), Color::from_srgb8(0, 0, 0, 128));
        let (r, _, _, a) = s.pixel(0, 0).straight();
        assert!((r as i32 - 128).abs() <= 1);
        assert_eq!(a, 255);
    }

    #[test]
    fn rounded_and_line() {
        let mut s = Surface::new(20, 20).unwrap();
        let g = Color::srgb(0, 255, 0);
        s.fill_rounded_rect(Rect::new(0, 0, 20, 20), 8, g);
        assert_eq!(s.pixel(10, 10), g);
        assert_eq!(s.pixel(0, 0), Color::TRANSPARENT); // corner cut
        let mut s2 = Surface::new(20, 20).unwrap();
        s2.draw_line(0, 10, 19, 10, 3, g);
        assert_eq!(s2.pixel(10, 10), g);
        assert_eq!(s2.pixel(10, 19), Color::TRANSPARENT);
    }

    #[test]
    fn diagnostic_deterministic() {
        let mut a = Surface::new(64, 8).unwrap();
        let mut b = Surface::new(64, 8).unwrap();
        let pa = gray_gradient_diagnostic(&mut a);
        let pb = gray_gradient_diagnostic(&mut b);
        assert_eq!(pa, pb);
        assert_eq!(a.pixels(), b.pixels()); // same ops → same pixels
        let (r, ..) = pa.straight();
        // 0.5-lin gray under 50% 128-gray: between 128 and 188, not extremes.
        assert!(r > 140 && r < 200, "center {r}");
    }
}
