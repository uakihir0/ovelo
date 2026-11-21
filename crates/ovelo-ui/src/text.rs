//! Text layout, Minikin subset (design doc §06 "Text").
//!
//! Pipeline: BiDi paragraph levels (`unicode-bidi`) → UAX#14 line-break
//! opportunities (`unicode-linebreak`, which implements CJ/kinsoku classes)
//! → greedy wrap at `max_width` → end-ellipsizing. Measurement returns exact
//! integer px bounds (ceil'd), matching the doc's exact-match requirement.

use crate::font::FontSet;
use unicode_bidi::BidiInfo;
use unicode_linebreak::{linebreaks, BreakOpportunity};

/// Style for a layout run (single font size; spans are a later layer).
#[derive(Debug, Clone, Copy)]
pub struct TextStyle {
    /// Em size in device px.
    pub size_px: f32,
}

/// One laid-out line.
#[derive(Debug, Clone)]
pub struct Line {
    /// Byte range in the source text.
    pub range: (usize, usize),
    /// Line width, exact px (ceil).
    pub width: i32,
    /// Whether the paragraph direction is RTL for this text.
    pub rtl: bool,
    /// Whether the line ends in an ellipsis.
    pub ellipsized: bool,
}

/// Full layout result with exact integer metrics.
#[derive(Debug)]
pub struct TextLayout {
    /// Lines top to bottom.
    pub lines: Vec<Line>,
    /// Max line width (exact px).
    pub width: i32,
    /// Total height = lines × line_height (exact px).
    pub height: i32,
    /// Baseline ascent (px).
    pub ascent: i32,
    /// Baseline descent (px).
    pub descent: i32,
}

const ELLIPSIS: char = '\u{2026}'; // …

/// Greedy single-style layout. `max_width`: `None` = only hard breaks;
/// `Some(w)` wraps, and `ellipsize` truncates overflowing single lines.
pub fn layout(
    text: &str,
    style: TextStyle,
    fonts: &FontSet,
    max_width: Option<i32>,
    ellipsize: bool,
) -> TextLayout {
    // Base direction per paragraph (hard splits on \n are paragraphs too in
    // the UBA model when separated by B class; winit-independent).
    let paras: Vec<(usize, usize, bool)> = if text.is_empty() {
        vec![(0, 0, false)]
    } else {
        BidiInfo::new(text, None)
            .paragraphs
            .iter()
            .map(|p| (p.range.start, p.range.end, p.level.is_rtl()))
            .collect()
    };
    let para_rtl = |byte: usize| -> bool {
        paras
            .iter()
            .find(|(s, e, _)| byte >= *s && byte < *e)
            .map(|(_, _, r)| *r)
            .unwrap_or(false)
    };
    let size = style.size_px;
    let adv = |c: char| fonts.advance(c, size);

    // Break opportunities at char boundaries (UAX#14 handles kinsoku NS/CJ).
    let mut breakset: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (byte, op) in linebreaks(text) {
        if op == BreakOpportunity::Allowed || op == BreakOpportunity::Mandatory {
            breakset.insert(byte);
        }
    }
    breakset.insert(text.len());

    // Per-char advances by byte start index: shaped cluster advances from the
    // primary font where available (kerning/ligatures), fallback otherwise.
    let shaped = fonts.char_advances(text, size);
    let mut ch_adv: Vec<(usize, f32)> = text
        .char_indices()
        .zip(shaped)
        .map(|((i, _), a)| (i, a))
        .collect();

    let mut lines: Vec<Line> = Vec::new();
    let mut seg_start = 0usize; // index into ch_adv
    let mut i = 0usize;
    // Running max char index whose byte start is a breakable boundary —
    // keeps wrap scans O(n) total instead of O(n²) rescans.
    let mut last_break: usize = 0;
    // Running width since seg_start — avoids resumming per char.
    let mut run_w: f32 = 0.0;
    while i < ch_adv.len() {
        if breakset.contains(&ch_adv[i].0) && i > 0 {
            last_break = i;
        }
        // Hard break?
        if text[ch_adv[i].0..].starts_with('\n') {
            push_line(
                &mut lines,
                text,
                &ch_adv,
                seg_start,
                i,
                para_rtl(ch_adv[seg_start.min(ch_adv.len() - 1)].0),
                false,
            );
            seg_start = i + 1;
            i += 1;
            run_w = 0.0;
            continue;
        }
        run_w += ch_adv[i].1;
        if let Some(maxw) = max_width {
            if run_w > maxw as f32 && i > seg_start {
                let seg_byte = ch_adv[seg_start].0;
                // Break at the current char boundary counts too: it splits
                // the overflowing char onto the next line.
                let b = if last_break > seg_start && last_break <= i {
                    last_break
                } else {
                    usize::MAX
                };
                if b != usize::MAX {
                    push_line(
                        &mut lines,
                        text,
                        &ch_adv,
                        seg_start,
                        b,
                        para_rtl(seg_byte),
                        false,
                    );
                    seg_start = b;
                    i = b;
                    run_w = 0.0; // char b is re-examined below
                    continue;
                }
                // No break point in range: emit an ellipsized prefix that fits.
                if ellipsize {
                    let ell_w = adv(ELLIPSIS);
                    let mut acc = 0f32;
                    let mut j = seg_start;
                    while j < ch_adv.len()
                        && acc + ch_adv[j].1 + ell_w <= maxw as f32
                        && !text[ch_adv[j].0..].starts_with('\n')
                    {
                        acc += ch_adv[j].1;
                        j += 1;
                    }
                    push_ell(
                        &mut lines,
                        text,
                        &ch_adv,
                        seg_start,
                        j,
                        para_rtl(seg_byte),
                        ell_w,
                    );
                    while i < ch_adv.len() && !text[ch_adv[i].0..].starts_with('\n') {
                        i += 1;
                    }
                    seg_start = i;
                    run_w = 0.0;
                    continue;
                }
            }
        }
        i += 1;
    }
    if seg_start < ch_adv.len() {
        push_line(
            &mut lines,
            text,
            &ch_adv,
            seg_start,
            ch_adv.len(),
            para_rtl(ch_adv[seg_start].0),
            false,
        );
    }
    if text.is_empty() || text.ends_with('\n') {
        lines.push(Line {
            range: (text.len(), text.len()),
            width: 0,
            rtl: para_rtl(text.len().saturating_sub(1)),
            ellipsized: false,
        });
    }

    let (asc, desc) = fonts.metrics(size);
    let ascent = asc.ceil() as i32;
    let descent = desc.ceil() as i32;
    let line_h = ascent + descent;
    let width = lines.iter().map(|l| l.width).max().unwrap_or(0);
    let height = lines.len() as i32 * line_h;
    ch_adv.clear();
    TextLayout {
        lines,
        width,
        height,
        ascent,
        descent,
    }
}

fn push_line(
    lines: &mut Vec<Line>,
    text: &str,
    ch_adv: &[(usize, f32)],
    s: usize,
    e: usize,
    rtl: bool,
    ell: bool,
) {
    if e <= s {
        // Empty line (e.g. text starting with '\n').
        let bs = ch_adv.get(s).map(|(b, _)| *b).unwrap_or(0);
        lines.push(Line {
            range: (bs, bs),
            width: 0,
            rtl,
            ellipsized: ell,
        });
        return;
    }
    let (bs, be) = (ch_adv[s].0, ch_adv[e - 1].0);
    let end = be + text[be..].chars().next().unwrap().len_utf8();
    // Trim trailing spaces from the measured width (Minikin behavior).
    let mut w: f32 = ch_adv[s..e].iter().map(|(_, a)| *a).sum();
    let mut k = e;
    while k > s && text[ch_adv[k - 1].0..].starts_with(' ') {
        w -= ch_adv[k - 1].1;
        k -= 1;
    }
    lines.push(Line {
        range: (bs, end),
        width: w.ceil() as i32,
        rtl,
        ellipsized: ell,
    });
}

fn push_ell(
    lines: &mut Vec<Line>,
    text: &str,
    ch_adv: &[(usize, f32)],
    s: usize,
    e: usize,
    rtl: bool,
    ell_w: f32,
) {
    if e <= s {
        return;
    }
    let (bs, be) = (ch_adv[s].0, ch_adv[e - 1].0);
    let end = be + text[be..].chars().next().unwrap().len_utf8();
    let w: f32 = ch_adv[s..e].iter().map(|(_, a)| *a).sum::<f32>() + ell_w;
    lines.push(Line {
        range: (bs, end),
        width: w.ceil() as i32,
        rtl,
        ellipsized: true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::GlyphSource;

    /// Deterministic fake: ASCII 10px, CJK 20px advance, full coverage.
    struct FakeFont;
    impl GlyphSource for FakeFont {
        fn name(&self) -> &str {
            "fake"
        }
        fn advance_for(&self, c: char, size_px: f32) -> Option<f32> {
            Some(if (c as u32) > 0x2e7f {
                2.0 * size_px
            } else {
                size_px
            })
        }
        fn ascent(&self, size_px: f32) -> f32 {
            0.8 * size_px
        }
        fn descent(&self, size_px: f32) -> f32 {
            0.2 * size_px
        }
    }

    fn fs() -> FontSet {
        let mut f = FontSet::new();
        f.push(Box::new(FakeFont));
        f
    }
    fn st() -> TextStyle {
        TextStyle { size_px: 10.0 }
    }

    #[test]
    fn hard_break_and_metrics() {
        let l = layout("ab\ncd", st(), &fs(), None, false);
        assert_eq!(l.lines.len(), 2); // "ab", "cd"
        assert_eq!(l.width, 20);
        assert_eq!(l.ascent, 8);
        assert_eq!(l.descent, 2);
        assert_eq!(l.height, 20);
        let l2 = layout("ab\n", st(), &fs(), None, false);
        assert_eq!(l2.lines.len(), 2); // "ab", trailing empty
        let l3 = layout("\nab", st(), &fs(), None, false);
        assert_eq!(l3.lines.len(), 2); // leading empty line + "ab"
        assert_eq!(l3.lines[0].width, 0);
        // Wrap at the overflowing char's own boundary.
        let l4 = layout("ab c", st(), &fs(), Some(30), false);
        assert_eq!(l4.lines.len(), 2);
        assert_eq!(l4.lines[1].range.1 - l4.lines[1].range.0, 1); // "c"
                                                                  // No double-count after wrapping: "cdef" (40px) fits in 45px.
        let l5 = layout("ab cdef", st(), &fs(), Some(45), false);
        assert_eq!(l5.lines.len(), 2);
        assert!(!l5.lines[1].ellipsized);
    }

    #[test]
    fn word_wrap_no_mid_word_break() {
        // "ab cd": width allows ~3 chars; break only at the space.
        let l = layout("ab cd ef", st(), &fs(), Some(35), false);
        assert!(l.lines.len() >= 2);
        let first = &"ab cd ef"[l.lines[0].range.0..l.lines[0].range.1];
        assert!(first.ends_with(' ') || !first.contains("cd") || l.lines[0].range.1 <= 3);
        // no line may split "ab" or "cd" mid-word
        for ln in &l.lines {
            let s = &"ab cd ef"[ln.range.0..ln.range.1];
            assert_ne!(s, "a");
            assert_ne!(s, "b");
        }
    }

    #[test]
    fn kinsoku_no_line_start_punct() {
        // 、 must not start a line: with width 40 (=2 CJK chars per line),
        // breaks happen only where UAX#14 allows.
        let l = layout("あい、うえ", st(), &fs(), Some(40), false);
        for ln in &l.lines {
            let s = &"あい、うえ"[ln.range.0..ln.range.1];
            assert!(!s.starts_with('、'), "kinsoku violation: {s:?}");
        }
        assert!(l.lines.len() >= 2);
    }

    #[test]
    fn ellipsis_truncates() {
        let l = layout("abcdefghij", st(), &fs(), Some(45), true);
        assert_eq!(l.lines.len(), 1);
        assert!(l.lines[0].ellipsized);
        assert!(l.lines[0].width <= 45);
        assert!(l.lines[0].width > 30); // actually filled
    }

    #[test]
    fn bidi_direction() {
        let l = layout("שלום", st(), &fs(), None, false);
        assert!(l.lines[0].rtl);
        let l2 = layout("hello", st(), &fs(), None, false);
        assert!(!l2.lines[0].rtl);
    }
}
