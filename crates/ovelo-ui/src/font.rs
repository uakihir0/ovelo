//! Font registry and glyph metric sources.
//!
//! Real faces are parsed with `ttf-parser` and shaped with `rustybuzz`; the
//! layout engine works against the [`GlyphSource`] trait so tests can inject
//! deterministic fakes. Android font fallback ordering is modeled as an
//! ordered [`FontSet`]: the first font containing the glyph wins.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

fn err(cause: &str, detail: impl std::fmt::Display) -> OveloError {
    OveloError::new(ErrorCategory::ServiceMissing, Phase::Ui, cause)
        .with_detail(detail.to_string())
}

/// One font's metric/shaping interface, in output pixels.
pub trait GlyphSource {
    /// Font family name for diagnostics.
    fn name(&self) -> &str;
    /// Horizontal advance for `c` at `size_px`, or `None` when the font has
    /// no glyph for it (triggers fallback).
    fn advance_for(&self, c: char, size_px: f32) -> Option<f32>;
    /// Line ascent at `size_px`.
    fn ascent(&self, size_px: f32) -> f32;
    /// Line descent at `size_px` (positive downward).
    fn descent(&self, size_px: f32) -> f32;
    /// Shaped per-cluster advances (cluster byte offset, px advance) for
    /// `text`, when the font supports shaping. `None` = no shaping; layout
    /// then uses `advance_for` per char.
    fn shaped_advances(&self, _text: &str, _size_px: f32) -> Option<Vec<(usize, f32)>> {
        None
    }
    /// Raster coverage for `c`, or `None` when the font can't rasterize
    /// or doesn't cover the char (triggers fallback).
    /// `ymin`/`ymax` follow fontdue convention: distance from the baseline
    /// to the bitmap's bottom edge (positive = below baseline).
    fn raster_for(&self, _c: char, _size_px: f32) -> Option<GlyphBmp> {
        None
    }
    /// Shaped, positioned glyph run for `text` in direction `rtl`
    /// (ligatures, mark placement resolved by the shaper), or `None`
    /// when the font can't shape/rasterize.
    fn shaped_glyphs(
        &self,
        _text: &str,
        _rtl: bool,
        _size_px: f32,
    ) -> Option<Vec<ShapedGlyph>> {
        None
    }
    /// Whether [`GlyphSource::shaped_glyphs`] can produce a run — lets
    /// callers pick between the shaped path and per-char fallback
    /// without rasterizing to find out. The default probes
    /// `shaped_glyphs` once; implementations with a cheaper test should
    /// override.
    fn can_shape_glyphs(&self) -> bool {
        self.shaped_glyphs("", false, 1.0).is_some()
    }
}

/// One glyph of a shaped run: coverage bitmap plus pen position data in px.
pub struct ShapedGlyph {
    /// Rasterized coverage (rasterize_indexed). `w`/`h` may be 0 (space, marks).
    pub bmp: GlyphBmp,
    /// Pen advance after this glyph.
    pub x_adv: f32,
    /// Offset from the pen origin.
    pub x_off: f32,
    /// Baseline lift (positive = above baseline).
    pub y_off: f32,
}

/// A rasterized glyph: 8-bit coverage bitmap plus placement metrics.
pub struct GlyphBmp {
    /// Left edge offset from the pen origin (may be negative).
    pub xmin: i32,
    /// Bottom edge offset from the baseline (positive = below).
    pub ymin: i32,
    /// Bitmap width px.
    pub w: usize,
    /// Bitmap height px.
    pub h: usize,
    /// Row-major coverage, `w * h` bytes.
    pub cov: Vec<u8>,
}

/// A TTF/OTF face kept as owned bytes; `ttf_parser::Face` is rebuilt per call
/// (cheap table lookup, no self-referential struct needed).
pub struct TtfFont {
    bytes: Vec<u8>,
    name: String,
    /// Rasterizer face (fontdue), built once at load.
    fd: Option<fontdue::Font>,
}

impl TtfFont {
    /// Parse font bytes (`ttf`, `otf`; `.ttc` collections are unsupported and
    /// rejected — callers pick a face index when support lands).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let face =
            ttf_parser::Face::parse(bytes, 0).map_err(|e| err("ui.font.parse", e))?;
        if face.is_variable() {
            // Variable fonts need instance pinning for deterministic layout;
            // reject until pinning is implemented.
            return Err(err("ui.font.variable", face
                .names()
                .into_iter()
                .find(|n| n.name_id == ttf_parser::name_id::FAMILY)
                .and_then(|n| n.to_string())
                .unwrap_or_default()));
        }
        let name = face
            .names()
            .into_iter()
            .find(|n| n.name_id == ttf_parser::name_id::FAMILY)
            .and_then(|n| n.to_string())
            .unwrap_or_else(|| "unnamed".into());
        let fd = fontdue::Font::from_bytes(
            bytes,
            fontdue::FontSettings {
                scale: 100.0,
                ..fontdue::FontSettings::default()
            },
        )
        .ok();
        Ok(Self {
            bytes: bytes.to_vec(),
            name,
            fd,
        })
    }

    fn face(&self) -> ttf_parser::Face<'_> {
        // Invariant: bytes were parse-checked in from_bytes.
        ttf_parser::Face::parse(&self.bytes, 0).expect("font parsed at registration")
    }

    /// Shape `text` with rustybuzz; returns per-cluster advances in px.
    /// Falls back to raw cmap advances when shaping yields no glyphs.
    pub fn shape(&self, text: &str, size_px: f32) -> Vec<(usize, f32)> {
        let upm = self.face().units_per_em() as f32;
        let scale = size_px / upm;
        let rb_face = rustybuzz::Face::from_slice(&self.bytes, 0)
            .expect("font parsed at registration");
        let mut buf = rustybuzz::UnicodeBuffer::new();
        buf.push_str(text);
        let glyph_buf = rustybuzz::shape(&rb_face, &[], buf);
        let mut out: Vec<(usize, f32)> = Vec::new();
        for (info, pos) in glyph_buf
            .glyph_infos()
            .iter()
            .zip(glyph_buf.glyph_positions())
        {
            out.push((info.cluster as usize, pos.x_advance as f32 * scale));
        }
        out
    }
}

impl GlyphSource for TtfFont {
    fn name(&self) -> &str {
        &self.name
    }
    fn advance_for(&self, c: char, size_px: f32) -> Option<f32> {
        let face = self.face();
        let gid = face.glyph_index(c)?;
        let upm = face.units_per_em() as f32;
        let adv = face.glyph_hor_advance(gid)? as f32;
        Some(adv * size_px / upm)
    }
    fn ascent(&self, size_px: f32) -> f32 {
        let face = self.face();
        face.ascender() as f32 * size_px / face.units_per_em() as f32
    }
    fn descent(&self, size_px: f32) -> f32 {
        let face = self.face();
        -face.descender() as f32 * size_px / face.units_per_em() as f32
    }
    fn shaped_advances(&self, text: &str, size_px: f32) -> Option<Vec<(usize, f32)>> {
        Some(self.shape(text, size_px))
    }
    fn raster_for(&self, c: char, size_px: f32) -> Option<GlyphBmp> {
        // fontdue falls back to .notdef for missing chars — check coverage
        // first so FontSet::raster can continue down the fallback chain.
        self.face().glyph_index(c)?;
        let (m, cov) = self.fd.as_ref()?.rasterize(c, size_px);
        Some(GlyphBmp {
            xmin: m.xmin,
            ymin: m.ymin,
            w: m.width,
            h: m.height,
            cov,
        })
    }
    fn shaped_glyphs(&self, text: &str, rtl: bool, size_px: f32) -> Option<Vec<ShapedGlyph>> {
        let fd = self.fd.as_ref()?;
        let upm = self.face().units_per_em() as f32;
        let scale = size_px / upm;
        let rb_face =
            rustybuzz::Face::from_slice(&self.bytes, 0).expect("font parsed at registration");
        let mut buf = rustybuzz::UnicodeBuffer::new();
        buf.push_str(text);
        buf.set_direction(if rtl {
            rustybuzz::Direction::RightToLeft
        } else {
            rustybuzz::Direction::LeftToRight
        });
        let glyph_buf = rustybuzz::shape(&rb_face, &[], buf);
        let mut out = Vec::new();
        for (info, pos) in glyph_buf
            .glyph_infos()
            .iter()
            .zip(glyph_buf.glyph_positions())
        {
            let (m, cov) = fd.rasterize_indexed(info.glyph_id as u16, size_px);
            out.push(ShapedGlyph {
                bmp: GlyphBmp {
                    xmin: m.xmin,
                    ymin: m.ymin,
                    w: m.width,
                    h: m.height,
                    cov,
                },
                x_adv: pos.x_advance as f32 * scale,
                x_off: pos.x_offset as f32 * scale,
                y_off: pos.y_offset as f32 * scale,
            });
        }
        Some(out)
    }

    fn can_shape_glyphs(&self) -> bool {
        self.fd.is_some()
    }
}

/// Split `text` into visual-order bidi runs: `(byte range, is_rtl)`.
fn bidi_runs(text: &str) -> Vec<(std::ops::Range<usize>, bool)> {
    let info = unicode_bidi::BidiInfo::new(text, None);
    let mut out = Vec::new();
    for para in &info.paragraphs {
        let (levels, runs) = info.visual_runs(para, para.range.clone());
        for r in runs {
            out.push((r.clone(), levels[r.start].is_rtl()));
        }
    }
    out
}

/// Mark/ZWJ chars that must stay in their base char's font run (the
/// common combining blocks — enough for cluster-joined shaping).
fn is_combining(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF
        | 0x05C1..=0x05C2 | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670
        | 0x06D6..=0x06ED | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200D
        | 0x20D0..=0x20FF | 0x3099..=0x309A | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F)
}

/// Ordered fallback font set. Lookup walks in registration order.
#[derive(Default)]
pub struct FontSet {
    fonts: Vec<Box<dyn GlyphSource>>,
}

impl FontSet {
    /// Empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a font; earlier registrations win for shared glyphs.
    pub fn push(&mut self, f: Box<dyn GlyphSource>) {
        self.fonts.push(f);
    }

    /// Scan directories for `.ttf`/`.otf` files, registering parseable fonts.
    /// On macOS callers pass `/System/Library/Fonts`,
    /// `/System/Library/Fonts/Supplemental`, `/Library/Fonts`. Missing dirs
    /// are skipped; unparsable files are skipped, not fatal.
    pub fn load_dirs(&mut self, dirs: &[&str]) -> usize {
        let mut n = 0;
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let ok = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("otf"));
                if !ok {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&p) {
                    if let Ok(f) = TtfFont::from_bytes(&bytes) {
                        self.fonts.push(Box::new(f));
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// First font covering `c` (fallback order), else `None`.
    pub fn font_for(&self, c: char, size_px: f32) -> Option<&dyn GlyphSource> {
        self.fonts
            .iter()
            .map(|f| f.as_ref())
            .find(|f| f.advance_for(c, size_px).is_some())
    }

    /// Raster `c` via fallback order (first font with coverage wins).
    pub fn raster(&self, c: char, size_px: f32) -> Option<GlyphBmp> {
        self.fonts.iter().find_map(|f| f.raster_for(c, size_px))
    }

    /// Shaped, positioned glyph run for `text`, itemized like Minikin:
    /// BiDi `visual_runs` first, then each bidi run split into maximal
    /// consecutive same-font sub-runs and shaped by its own font, so
    /// digits keep their order inside RTL text and each char draws in
    /// the font [`FontSet::advance`] measures it with. Chars no font
    /// covers take a half-em advance and no bitmap (`.notdef` rendering
    /// is a later layer). `None` only when no fonts are registered.
    pub fn shaped_run(&self, text: &str, size_px: f32) -> Option<Vec<ShapedGlyph>> {
        if self.fonts.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for (range, fi, rtl) in self.itemize(text, size_px) {
            self.shape_subrun(&text[range], fi, rtl, size_px, &mut out);
        }
        Some(out)
    }

    /// Itemize `text` Minikin-style: bidi visual runs, each split into
    /// maximal consecutive same-font sub-runs emitted in visual order
    /// (reversed inside RTL runs). Combining marks stay with their base
    /// char's font run so they keep anchor context. Returns
    /// `(byte range in text, font index, rtl)` per sub-run.
    fn itemize(
        &self,
        text: &str,
        size_px: f32,
    ) -> Vec<(std::ops::Range<usize>, Option<usize>, bool)> {
        let mut out = Vec::new();
        for (run, rtl) in bidi_runs(text) {
            let base = run.start;
            let sub = &text[run];
            let mut segs: Vec<(std::ops::Range<usize>, Option<usize>)> = Vec::new();
            let mut start = 0usize;
            let mut cur: Option<usize> = None;
            let mut started = false;
            for (b, c) in sub.char_indices() {
                if is_combining(c) && started {
                    // A mark joins the run only when the current font
                    // covers it — or nothing does. A mark another font
                    // covers splits off: shaped in the base font it
                    // would rasterize as .notdef instead of its glyph.
                    let covered_here = cur.is_some_and(|i| {
                        self.fonts[i].advance_for(c, size_px).is_some()
                    });
                    if covered_here || self.font_index_for(c, size_px).is_none() {
                        continue;
                    }
                }
                let fi = self.font_index_for(c, size_px);
                if !started {
                    cur = fi;
                    started = true;
                } else if fi != cur {
                    segs.push((start..b, cur));
                    start = b;
                    cur = fi;
                }
            }
            if started {
                segs.push((start..sub.len(), cur));
            }
            if rtl {
                segs.reverse();
            }
            for (r, fi) in segs {
                out.push((base + r.start..base + r.end, fi, rtl));
            }
        }
        out
    }

    /// Index of the first font covering `c`, else `None`.
    fn font_index_for(&self, c: char, size_px: f32) -> Option<usize> {
        self.fonts
            .iter()
            .position(|f| f.advance_for(c, size_px).is_some())
    }

    /// Shape `text` with font `fi` (or draw nothing-covered when `None`),
    /// appending to `out`. `rtl` is the containing bidi run's direction.
    fn shape_subrun(
        &self,
        text: &str,
        fi: Option<usize>,
        rtl: bool,
        size_px: f32,
        out: &mut Vec<ShapedGlyph>,
    ) {
        if let Some(i) = fi {
            if self.fonts[i].can_shape_glyphs() {
                if let Some(gs) = self.fonts[i].shaped_glyphs(text, rtl, size_px) {
                    out.extend(gs);
                    return;
                }
            }
        }
        // Per-char fallback: draw each char's own bitmap at its own
        // advance. Shaped cluster advances are NOT usable here — the
        // per-char bitmaps are not the shaped glyphs, so a ligature's
        // member drawn at the leader's summed width would overlap the
        // next glyph. char_advances measures the same per-char widths.
        for c in text.chars() {
            // Walk the whole fallback chain: the run's font may cover the
            // char but be unable to rasterize (or shape) it.
            let bmp = self
                .raster(c, size_px)
                .unwrap_or(GlyphBmp {
                    xmin: 0,
                    ymin: 0,
                    w: 0,
                    h: 0,
                    cov: Vec::new(),
                });
            out.push(ShapedGlyph {
                bmp,
                x_adv: self.advance(c, size_px),
                x_off: 0.0,
                y_off: 0.0,
            });
        }
    }

    /// Advance of `c` via fallback ordering; missing glyphs take the last
    /// font's `.notdef`-style estimate (half em) so layout stays total.
    pub fn advance(&self, c: char, size_px: f32) -> f32 {
        self.font_for(c, size_px)
            .and_then(|f| f.advance_for(c, size_px))
            .unwrap_or(size_px * 0.5)
    }

    /// Max ascent/descent across the set (Minikin's conservative metrics).
    pub fn metrics(&self, size_px: f32) -> (f32, f32) {
        let mut a = 0f32;
        let mut d = 0f32;
        for f in &self.fonts {
            a = a.max(f.ascent(size_px));
            d = d.max(f.descent(size_px));
        }
        (a, d)
    }

    /// Per-char advances for `text`, itemized the same way
    /// [`FontSet::shaped_run`] draws it: each bidi/font sub-run is shaped
    /// by its own font, cluster advances land on the cluster's leading
    /// char, other members take 0, and a font that can't produce shaped
    /// glyphs falls back to per-char advances (what shape_subrun draws)
    /// — so measured width equals drawn width.
    pub fn char_advances(&self, text: &str, size_px: f32) -> Vec<f32> {
        let mut byte_to_ci = vec![usize::MAX; text.len() + 1];
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        for (ci, (b, _)) in chars.iter().enumerate() {
            byte_to_ci[*b] = ci;
        }
        let n = chars.len();
        let mut out: Vec<Option<f32>> = vec![None; n];
        for (range, fi, _rtl) in self.itemize(text, size_px) {
            let sub = &text[range.clone()];
            let Some(i) = fi else { continue };
            // Shape only when the font can also draw the shaped run;
            // shape_subrun's per-char fallback measures advance_for.
            match if self.fonts[i].can_shape_glyphs() {
                self.fonts[i].shaped_advances(sub, size_px)
            } else {
                None
            } {
                Some(glyphs) => {
                    let mut leaders: Vec<usize> = Vec::new();
                    let mut sums: std::collections::HashMap<usize, f32> =
                        std::collections::HashMap::new();
                    for (byte, adv) in glyphs {
                        let ci = byte_to_ci
                            .get(range.start + byte)
                            .copied()
                            .unwrap_or(usize::MAX);
                        if ci >= n {
                            continue;
                        }
                        if !sums.contains_key(&ci) {
                            leaders.push(ci);
                        }
                        *sums.entry(ci).or_insert(0.0) += adv;
                    }
                    leaders.sort_unstable();
                    for (k, &ci) in leaders.iter().enumerate() {
                        out[ci] = Some(sums[&ci]);
                        let end = leaders
                            .get(k + 1)
                            .copied()
                            .unwrap_or_else(|| byte_to_ci[range.end].min(n));
                        for m in ci + 1..end {
                            if self.fonts[i].advance_for(chars[m].1, size_px).is_some() {
                                out[m] = Some(0.0);
                            }
                        }
                    }
                }
                None => {
                    for (b, c) in sub.char_indices() {
                        let ci = byte_to_ci[range.start + b];
                        if ci < n {
                            out[ci] = self.fonts[i].advance_for(c, size_px);
                        }
                    }
                }
            }
        }
        chars
            .iter()
            .enumerate()
            .map(|(ci, &(_, c))| out[ci].unwrap_or_else(|| self.advance(c, size_px)))
            .collect()
    }

    /// Number of registered fonts.
    pub fn len(&self) -> usize {
        self.fonts.len()
    }
    /// Whether no fonts are registered.
    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_skips_uncovered_chars_for_fallback() {
        let mut fs = FontSet::new();
        fs.push(Box::new(
            TtfFont::from_bytes(include_bytes!("../tests/geneva.ttf"))
                .expect("font parses"),
        ));
        // A PUA char Geneva doesn't cover: raster must return None so a
        // later font in the chain can supply the glyph.
        assert!(fs.raster('\u{E000}', 16.0).is_none());
        assert!(fs.raster('A', 16.0).is_some());
        let run = fs.shaped_run("AB", 16.0).expect("shaped");
        assert_eq!(run.len(), 2);
        assert!(run.iter().all(|g| g.bmp.w > 0));
        // Uncovered char: advance-only empty glyph, not .notdef.
        let run = fs.shaped_run("\u{E000}", 16.0).expect("shaped");
        assert_eq!(run.len(), 1);
        assert_eq!(run[0].bmp.w, 0);
        assert!(run[0].x_adv > 0.0);
        // Width drawn == width measured: summed advances match char_advances.
        let drawn: f32 = run.iter().map(|g| g.x_adv).sum();
        let measured: f32 = fs.char_advances("\u{E000}", 16.0).iter().sum();
        assert!((drawn - measured).abs() < 0.01, "{drawn} vs {measured}");
    }

    /// A font that reports shaped (ligature) advances but cannot
    /// rasterize shaped glyphs must draw each char's own bitmap at its
    /// own advance — applying the ligature's summed width to the leader
    /// parks the member's bitmap on top of the next glyph.
    #[test]
    fn unrasterizable_shaper_falls_back_to_perchar_advances() {
        struct ShapeOnly;
        impl GlyphSource for ShapeOnly {
            fn name(&self) -> &str {
                "shape-only"
            }
            fn advance_for(&self, c: char, _size_px: f32) -> Option<f32> {
                matches!(c, 'f' | 'i').then_some(4.0)
            }
            fn ascent(&self, _s: f32) -> f32 {
                8.0
            }
            fn descent(&self, _s: f32) -> f32 {
                2.0
            }
            fn shaped_advances(&self, _t: &str, _s: f32) -> Option<Vec<(usize, f32)>> {
                // "fi" ligature: whole 7px on the cluster leader.
                Some(vec![(0, 7.0)])
            }
        }
        let mut fs = FontSet::new();
        fs.push(Box::new(ShapeOnly));
        fs.push(Box::new(
            TtfFont::from_bytes(include_bytes!("../tests/geneva.ttf"))
                .expect("font parses"),
        ));
        let run = fs.shaped_run("fi", 16.0).expect("run");
        assert_eq!(run.len(), 2);
        assert_eq!(run[0].x_adv, 4.0);
        assert_eq!(run[1].x_adv, 4.0);
        let drawn: f32 = run.iter().map(|g| g.x_adv).sum();
        let measured: f32 = fs.char_advances("fi", 16.0).iter().sum();
        assert!((drawn - measured).abs() < 0.01, "{drawn} vs {measured}");
    }
}
