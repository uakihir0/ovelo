//! `android.graphics.Canvas`-subset over a [`Surface`]: clip stack,
//! translate, and the draw calls a View tree needs (`drawColor`,
//! `drawRect`, `drawText`). Coordinates are device px; text draws in
//! sRGB with coverage-blended glyph bitmaps (fontdue raster).
//!
//! A `Canvas` borrows the surface and font set; offsets accumulate
//! through [`Canvas::translate`], matching how `ViewGroup.dispatchDraw`
//! shifts each child's origin.

use crate::color::Color;
use crate::font::FontSet;
use crate::surface::{Rect, Surface};

/// A paint bundle: color only for now (style/AA flags come with the
/// Paint class surface).
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    /// Fill color.
    pub color: Color,
    /// Text em size in px (only for `draw_text`).
    pub text_size: f32,
}

impl Paint {
    /// Opaque-color paint.
    pub fn color(color: Color) -> Self {
        Self {
            color,
            text_size: 16.0,
        }
    }
}

/// Drawing context over a surface region.
pub struct Canvas<'a> {
    surf: &'a mut Surface,
    fonts: &'a FontSet,
    dx: i32,
    dy: i32,
    /// `save()` snapshots: (translate origin, clip depth).
    saved: Vec<(i32, i32, usize)>,
}

impl<'a> Canvas<'a> {
    /// Shift `r` by the current origin.
    fn tr(&self, r: Rect) -> Rect {
        Rect {
            x: r.x + self.dx,
            y: r.y + self.dy,
            x1: r.x1 + self.dx,
            y1: r.y1 + self.dy,
        }
    }

    /// Canvas covering the whole surface, untranslated.
    pub fn new(surf: &'a mut Surface, fonts: &'a FontSet) -> Self {
        Self {
            surf,
            fonts,
            dx: 0,
            dy: 0,
            saved: Vec::new(),
        }
    }

    /// `save()` — snapshot the translate origin and clip depth so a later
    /// [`Canvas::restore`] returns here, however many `clip_rect`/
    /// `translate` calls happened in between (Canvas.save/restoreToCount).
    pub fn save(&mut self) {
        self.saved.push((self.dx, self.dy, self.surf.clip_depth()));
    }

    /// Intersect the clip with `r` (translated). Pop with [`Surface::pop_clip`].
    pub fn clip_rect(&mut self, r: Rect) {
        self.surf.clip(self.tr(r));
    }

    /// Undo back to the last `save`: restores the translate origin and
    /// drops every clip pushed since. Without a matching `save`, pops one
    /// clip level (legacy `clip_rect` pairing).
    pub fn restore(&mut self) {
        match self.saved.pop() {
            Some((dx, dy, depth)) => {
                self.dx = dx;
                self.dy = dy;
                self.surf.truncate_clips(depth);
            }
            None => self.surf.pop_clip(),
        }
    }

    /// Shift the draw origin (`canvas.translate`).
    pub fn translate(&mut self, dx: i32, dy: i32) {
        self.dx += dx;
        self.dy += dy;
    }

    /// `drawColor` — fill the current clip.
    pub fn draw_color(&mut self, c: Color) {
        self.surf.clear(c);
    }

    /// Shaped advance width of `text` at `size_px` — for callers that
    /// center text (Paint.Align.CENTER) before `draw_text`.
    pub fn measure(&self, text: &str, size_px: f32) -> f32 {
        self.fonts.char_advances(text, size_px).iter().sum()
    }

    /// `drawRect` — fill a rect (translated).
    pub fn draw_rect(&mut self, r: Rect, p: Paint) {
        self.surf.fill_rect(self.tr(r), p.color);
    }

    /// `drawRoundRect` — fill a rounded rect (translated).
    pub fn draw_round_rect(&mut self, r: Rect, rad: i32, p: Paint) {
        self.surf.fill_rounded_rect(self.tr(r), rad, p.color);
    }

    /// `drawOval`/`drawCircle` with `Paint.Style.STROKE` — the ring
    /// between the outer ellipse and the ellipse shrunk by `w` on each
    /// side. Per-pixel: inside outer AND outside inner is stroke.
    pub fn stroke_oval(&mut self, r: Rect, w: i32, p: Paint) {
        let r = self.tr(r);
        let w = w as f32;
        let (rw, rh) = ((r.x1 - r.x) as f32, (r.y1 - r.y) as f32);
        if rw <= 0.0 || rh <= 0.0 {
            return;
        }
        let (cx, cy) = (r.x as f32 + rw / 2.0, r.y as f32 + rh / 2.0);
        let (rx, ry) = (rw / 2.0, rh / 2.0);
        // Inner ellipse shrunk by the stroke width on each axis; when a
        // side collapses the whole interior is ring.
        let (irx, iry) = ((rx - w).max(0.0), (ry - w).max(0.0));
        for y in r.y..r.y1 {
            for x in r.x..r.x1 {
                let dx = (x as f32 + 0.5 - cx) / rx;
                let dy = (y as f32 + 0.5 - cy) / ry;
                if dx * dx + dy * dy > 1.0 {
                    continue;
                }
                let in_inner = irx > 0.0 && iry > 0.0 && {
                    let ix = (x as f32 + 0.5 - cx) / irx;
                    let iy = (y as f32 + 0.5 - cy) / iry;
                    ix * ix + iy * iy <= 1.0
                };
                if !in_inner {
                    self.surf.blend_pixel(x, y, p.color, 255);
                }
            }
        }
    }

    /// `drawLine` — stroked segment (translated endpoints).
    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, w: i32, p: Paint) {
        self.surf.draw_line(
            x0 + self.dx,
            y0 + self.dy,
            x1 + self.dx,
            y1 + self.dy,
            w,
            p.color,
        );
    }

    /// Fill a flat-color polygon; `pts` is x,y pairs in canvas space
    /// (translated + clipped here).
    pub fn fill_poly(&mut self, pts: &[i32], p: Paint) {
        let pts: Vec<i32> = pts
            .chunks(2)
            .flat_map(|xy| [xy[0] + self.dx, xy[1] + self.dy])
            .collect();
        self.surf.fill_poly(&pts, p.color);
    }

    /// Fill multiple contours together, preserving holes.
    pub fn fill_polys(&mut self, contours: &[Vec<i32>], even_odd: bool, inverse: bool, p: Paint) {
        let contours: Vec<Vec<i32>> = contours
            .iter()
            .map(|pts| {
                pts.chunks(2)
                    .flat_map(|xy| [xy[0] + self.dx, xy[1] + self.dy])
                    .collect()
            })
            .collect();
        self.surf.fill_polys(&contours, even_odd, inverse, p.color);
    }

    /// `drawBitmap`-style blit: `src` (RGBA8, `src_w`×`src_h`) scaled
    /// nearest into the `w`×`h` rect at `(x, y)` — translated + clipped.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_image(
        &mut self,
        x: i32,
        y: i32,
        w: usize,
        h: usize,
        src_w: usize,
        src_h: usize,
        src: &[u8],
    ) {
        self.surf
            .blit_scaled(x + self.dx, y + self.dy, w, h, src_w, src_h, src);
    }

    /// `drawText` — run of text with the pen at `(x, baseline)`.
    ///
    /// When one font covers the whole text the shaper produces the run:
    /// BiDi visual order, ligatures and mark offsets, matching the
    /// shaped advances measure uses. Otherwise glyphs fall back per
    /// char; glyphs missing everywhere skip silently (`.notdef`
    /// rendering is a later layer).
    pub fn draw_text(&mut self, text: &str, x: i32, baseline: i32, p: Paint) {
        let mut pen = (x + self.dx) as f32;
        let base = baseline + self.dy;
        if let Some(run) = self.fonts.shaped_run(text, p.text_size) {
            for g in run {
                let gx = (pen + g.x_off).round() as i32 + g.bmp.xmin;
                let gy = base - g.y_off.round() as i32 - g.bmp.ymin - g.bmp.h as i32;
                self.blit_glyph(&g.bmp, gx, gy, p.color);
                pen += g.x_adv;
            }
            return;
        }
        for c in text.chars() {
            match self.fonts.raster(c, p.text_size) {
                Some(g) => {
                    let adv = self
                        .fonts
                        .font_for(c, p.text_size)
                        .map(|f| f.advance_for(c, p.text_size).unwrap_or(0.0))
                        .unwrap_or(g.w as f32);
                    let gx = pen.round() as i32 + g.xmin;
                    let gy = base - g.ymin - g.h as i32;
                    self.blit_glyph(&g, gx, gy, p.color);
                    pen += adv;
                }
                None => {
                    pen += self.fonts.advance(c, p.text_size);
                }
            }
        }
    }

    /// Coverage-blend one glyph bitmap with its top-left at `(gx, gy)`.
    fn blit_glyph(&mut self, g: &crate::font::GlyphBmp, gx: i32, gy: i32, c: Color) {
        for row in 0..g.h {
            for col in 0..g.w {
                let cov = g.cov[row * g.w + col];
                self.surf
                    .blend_pixel(gx + col as i32, gy + row as i32, c, cov);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_restore_round_trips_origin_and_clips() {
        let mut s = Surface::new(32, 32).unwrap();
        let fonts = FontSet::new();
        let mut c = Canvas::new(&mut s, &fonts);
        let red = Paint::color(Color::srgb(255, 0, 0));
        let blue = Paint::color(Color::srgb(0, 0, 255));

        c.save();
        c.translate(10, 10);
        c.clip_rect(Rect::new(0, 0, 4, 4));
        c.clip_rect(Rect::new(0, 0, 2, 2));
        c.draw_rect(Rect::new(0, 0, 4, 4), red);
        c.restore();

        // After restore: no residual translate, clip back to full surface.
        c.draw_rect(Rect::new(30, 30, 2, 2), blue);
        assert_eq!(c.surf.pixel(31, 31), Color::srgb(0, 0, 255));
        // Translated+clipped draw only covered (10..12, 10..12).
        assert_eq!(c.surf.pixel(10, 10), Color::srgb(255, 0, 0));
        assert_eq!(c.surf.pixel(13, 13), Color::TRANSPARENT);
        c.save();
        c.restore();
        c.draw_rect(Rect::new(0, 0, 1, 1), blue);
        assert_eq!(c.surf.pixel(0, 0), Color::srgb(0, 0, 255));
    }
}
