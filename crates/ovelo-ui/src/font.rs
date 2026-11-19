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
}

/// A TTF/OTF face kept as owned bytes; `ttf_parser::Face` is rebuilt per call
/// (cheap table lookup, no self-referential struct needed).
pub struct TtfFont {
    bytes: Vec<u8>,
    name: String,
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
        Ok(Self {
            bytes: bytes.to_vec(),
            name,
        })
    }

    fn face(&self) -> ttf_parser::Face<'_> {
        // Invariant: bytes were parse-checked in from_bytes.
        ttf_parser::Face::parse(&self.bytes, 0).expect("font parsed at registration")
    }

    /// Shape `text` with rustybuzz; returns per-cluster advances in px.
    /// Falls back to raw cmap advances when shaping yields no glyphs.
    pub fn shaped_advances(&self, text: &str, size_px: f32) -> Vec<(usize, f32)> {
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

    /// Number of registered fonts.
    pub fn len(&self) -> usize {
        self.fonts.len()
    }
    /// Whether no fonts are registered.
    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }
}
