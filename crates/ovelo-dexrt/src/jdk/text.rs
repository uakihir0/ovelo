//! java.text leftovers: `BreakIterator` — ICU boundary iteration.
//! Every code-unit index is a legal boundary, so returning all of them
//! is honest (more breaks than ICU, never fewer) and, critically, it
//! terminates: a stubbed `next()` returning 0 made `while(next()!=DONE)`
//! loops spin forever and starved the frame entirely.

use super::*;
use crate::value::{HostState, V};

/// Cursor position on a `BreakIterator` — the text itself is `P_DATA`.
pub(crate) const P_BIPOS: u64 = 0xFFFD_FEF4;
/// `android.text.Layout`: outer width, paint text size, maxLines and
/// the `TextPaint` the Builder saw (getPaint returns it).
const P_LWIDTH: u64 = 0xFFFD_FEF5;
const P_LSIZE: u64 = 0xFFFD_FEF6;
const P_LMAXL: u64 = 0xFFFD_FEF7;
const P_LPAINT: u64 = 0xFFFD_FEF8;

fn bi_len(vm: &Vm, this: u32) -> i64 {
    pseudo_get(vm, this, P_DATA)
        .and_then(|v| sv(vm, v))
        .map(|s| s.chars().count() as i64)
        .unwrap_or(0)
}

fn bi_pos(vm: &Vm, this: u32) -> i64 {
    pseudo_get(vm, this, P_BIPOS).map(i64v).unwrap_or(0)
}

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    match class {
        "Ljava/text/BreakIterator;" => Ok(breakiter_call(vm, name, args)),
        "Landroid/text/StaticLayout$Builder;" | "Landroid/text/Layout$Builder;" => {
            Ok(builder_call(vm, name, args))
        }
        "Landroid/text/Layout;" | "Landroid/text/StaticLayout;" | "Landroid/text/BoringLayout;" => {
            Ok(boring_call(vm, class, name, sig, args))
        }
        "Landroid/text/BoringLayout$Metrics;" => Ok(metrics_call(vm, name, args)),
        "Landroid/text/SpannableString;"
        | "Landroid/text/SpannableStringBuilder;"
        | "Landroid/text/Spannable;"
        | "Landroid/text/Spanned;" => Ok(spannable_call(vm, name, args)),
        "Landroid/text/Layout$Alignment;" => Ok(align_call(vm, name, args)),
        "Landroid/text/style/CharacterStyle;" => Ok(chstyle_call(name, args)),
        _ => Ok(None),
    }
}

/// CharSequence arg → text: `sv` first, then a dex `toString()` call
/// for Obj::Instance receivers (Compose's `AnnotatedString` carries its
/// text in dex fields `sv` can't reach — without this every paragraph
/// is laid out with an empty string and no text ever draws).
pub(crate) fn cs_arg(vm: &mut Vm, v: V) -> String {
    if let Some(t) = sv(vm, v) {
        return t;
    }
    if let V::Ref(h) = v {
        if matches!(vm.heap.get(h), Some(Obj::Instance { .. })) {
            if let Ok(Some(r)) = vm.call_virtual(h, "toString", &[]) {
                if let Some(t) = sv(vm, r) {
                    return t;
                }
            }
        }
    }
    String::new()
}

/// The CharSequence text behind a host object — `StrBuf` payload
/// first, `P_DATA` second.
fn cs_text(vm: &Vm, h: u32) -> String {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::StrBuf(t),
            ..
        }) => t.clone(),
        Some(Obj::Str(t)) => t.clone(),
        _ => pseudo_get(vm, h, P_DATA)
            .and_then(|v| sv(vm, v))
            .unwrap_or_default(),
    }
}

/// CharSequence/Spannable/Spanned methods on `this` — span queries
/// answer "no spans" (plain text is honest when spans are only
/// decorative).
fn spannable_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let this = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    match name {
        "<init>" => {
            let t = args
                .get(1)
                .and_then(|v| sv(vm, *v))
                .or_else(|| {
                    args.get(1).and_then(|v| match v {
                        V::Ref(h) => Some(cs_text(vm, *h)),
                        _ => None,
                    })
                })
                .unwrap_or_default();
            if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(this) {
                *state = HostState::StrBuf(t);
            }
            Some(None)
        }
        "length" => Some(Some(V::I32(cs_text(vm, this).chars().count() as i32))),
        "charAt" => {
            let t = cs_text(vm, this);
            let ix = args.get(1).map(|v| i64v(*v)).unwrap_or(0) as usize;
            Some(Some(V::I32(t.chars().nth(ix).unwrap_or('\0') as i32)))
        }
        "subSequence" => {
            let t: Vec<char> = cs_text(vm, this).chars().collect();
            let (a, b) = (
                args.get(1).map(|v| i64v(*v)).unwrap_or(0).max(0) as usize,
                args.get(2)
                    .map(|v| i64v(*v))
                    .unwrap_or(t.len() as i64)
                    .max(0) as usize,
            );
            Some(Some(jstr(
                vm,
                &t[a.min(t.len())..b.min(t.len())].iter().collect::<String>(),
            )))
        }
        "toString" => Some(Some(jstr(vm, &cs_text(vm, this)))),
        "setSpan" | "removeSpan" => Some(None),
        "getSpans" => {
            // `getSpans(int, int, Class<T>)` — the array must be typed
            // `T[]`: an `Object[]` fails checkcast to `CharacterStyle[]`
            // and aborts Compose's styled-text measurement.
            let desc = args
                .get(3)
                .and_then(|v| match v {
                    V::Ref(h) => class_desc_of(vm, *h),
                    _ => None,
                })
                .filter(|d| d.starts_with('L') || d.starts_with('['))
                .unwrap_or_else(|| "Ljava/lang/Object;".to_string());
            Some(Some(arr(vm, &desc, vec![])))
        }
        "getSpanStart" | "getSpanEnd" | "getSpanFlags" | "nextSpanTransition" => {
            Some(Some(V::I32(-1)))
        }
        "append" | "insert" | "delete" | "clear" | "clearSpans" => {
            Some(Some(args.first().copied().unwrap_or(V::Ref(0))))
        }
        _ => None,
    }
}

/// `BoringLayout$Metrics` — `width` canon field + pseudo mirror.
fn metrics_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let this = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    match name {
        "<init>" => Some(None),
        "toString" => {
            let w = pseudo_get(vm, this, P_LWIDTH).map(i64v).unwrap_or(0);
            Some(Some(jstr(vm, &format!("Metrics: width={w}"))))
        }
        _ => None,
    }
}

/// `Layout$Alignment` enum — values/valueOf/name/ordinal over the
/// three real constants.
fn align_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    const NAMES: [&str; 3] = ["ALIGN_NORMAL", "ALIGN_OPPOSITE", "ALIGN_CENTER"];
    let mk = |vm: &mut Vm, ord: usize| -> V {
        let h = host(vm, "Landroid/text/Layout$Alignment;", HostState::Empty);
        if let V::Ref(hh) = h {
            let n = jstr(vm, NAMES[ord]);
            pseudo_set(vm, hh, P_DATA, n);
            pseudo_set(vm, hh, P_FLAGS, V::I32(ord as i32));
        }
        h
    };
    match name {
        "values" => {
            let vals: Vec<V> = (0..3).map(|k| mk(vm, k)).collect();
            Some(Some(arr(vm, "Landroid/text/Layout$Alignment;", vals)))
        }
        "valueOf" => {
            let want = args.first().and_then(|v| sv(vm, *v)).unwrap_or_default();
            let ord = NAMES.iter().position(|n| *n == want).unwrap_or(0);
            Some(Some(mk(vm, ord)))
        }
        "name" | "toString" => {
            let n = match args.first() {
                Some(&V::Ref(h)) => pseudo_get(vm, h, P_DATA)
                    .and_then(|v| sv(vm, v))
                    .unwrap_or_else(|| NAMES[0].into()),
                _ => NAMES[0].into(),
            };
            Some(Some(jstr(vm, &n)))
        }
        "ordinal" => Some(Some(V::I32(match args.first() {
            Some(&V::Ref(h)) => i64v(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0))) as i32,
            _ => 0,
        }))),
        "compareTo" => Some(Some(V::I32(0))),
        _ => None,
    }
}

/// Text style marker classes — constructors and `updateDrawState`/`updateMeasureState`.
fn chstyle_call(name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "<init>" => Some(None),
        "updateDrawState" | "updateMeasureState" => Some(None),
        "clone" | "getUnderlying" => Some(args.first().copied()),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// android.text.Layout — line metrics the dex measures text with.
// Lines split on '\n' then greedy-wrap at the outer width using the
// same `measure_text` approximation Paint.measureText uses, so the dex
// sees a consistent model end to end.

/// (text, width) per line.
fn layout_lines(vm: &Vm, this: u32) -> Vec<(String, f32)> {
    let text = pseudo_get(vm, this, P_DATA)
        .and_then(|v| sv(vm, v))
        .unwrap_or_default();
    let size = pseudo_get(vm, this, P_LSIZE).map(f64v).unwrap_or(12.0) as f32;
    let maxw = pseudo_get(vm, this, P_LWIDTH).map(i64v).unwrap_or(0).max(1) as f32;
    let maxl = pseudo_get(vm, this, P_LMAXL).map(i64v).unwrap_or(0);
    let mut lines: Vec<(String, f32)> = Vec::new();
    for seg in text.split('\n') {
        let (mut cur, mut cur_w) = (String::new(), 0f32);
        for ch in seg.chars() {
            let cw = gfx::measure_text(&ch.to_string(), size);
            if !cur.is_empty() && cur_w + cw > maxw {
                lines.push((std::mem::take(&mut cur), cur_w));
                cur_w = 0.0;
            }
            cur.push(ch);
            cur_w += cw;
        }
        lines.push((cur, cur_w));
    }
    if maxl > 0 {
        lines.truncate(maxl as usize);
    }
    lines
}

/// Cumulative char start of each line.
fn line_starts(lines: &[(String, f32)]) -> Vec<i64> {
    let mut at = 0i64;
    lines
        .iter()
        .map(|(t, _)| {
            let s = at;
            at += t.chars().count() as i64;
            s
        })
        .collect()
}

fn boring_call(vm: &mut Vm, class: &str, name: &str, sig: &str, args: &[V]) -> Option<Option<V>> {
    // BoringLayout statics + constructor carry the real text/width —
    // everything else is the shared Layout model below.
    if class == "Landroid/text/BoringLayout;" {
        match name {
            "isBoring" | "measure" => {
                let t = cs_arg(vm, args.first().copied().unwrap_or(V::Ref(0)));
                if t.contains('\n') {
                    return Some(Some(V::Ref(0)));
                }
                let size = args
                    .get(1)
                    .and_then(|v| match v {
                        V::Ref(h) => pseudo_get(vm, *h, gfx::PK_TSIZE),
                        _ => None,
                    })
                    .map(f64v)
                    .unwrap_or(12.0) as f32;
                let w = gfx::measure_text(&t, size).ceil() as i64;
                if name == "measure" {
                    return Some(Some(V::I32(w as i32)));
                }
                let mh = host(vm, "Landroid/text/BoringLayout$Metrics;", HostState::Empty);
                if let V::Ref(h) = mh {
                    pseudo_set(vm, h, P_LWIDTH, V::I64(w));
                    if let Some(f) = vm
                        .field_index_of("Landroid/text/BoringLayout$Metrics;", "width")
                        .map(|g| vm.canon_field(g))
                    {
                        if let Some(Obj::Host { fields, .. } | Obj::Instance { fields, .. }) =
                            vm.heap.get_mut(h)
                        {
                            fields.insert(f, V::I32(w as i32));
                        }
                    }
                }
                return Some(Some(mh));
            }
            "<init>" => {
                if let Some(&V::Ref(this)) = args.first() {
                    let t = cs_arg(vm, args.get(1).copied().unwrap_or(V::Ref(0)));
                    let tv = jstr(vm, &t);
                    pseudo_set(vm, this, P_DATA, tv);
                    if let Some(&V::Ref(p)) = args.get(2) {
                        pseudo_set(vm, this, P_LPAINT, V::Ref(p));
                        if let Some(sz) = pseudo_get(vm, p, gfx::PK_TSIZE) {
                            pseudo_set(vm, this, P_LSIZE, V::F64(f64v(sz)));
                        }
                    }
                    pseudo_set(
                        vm,
                        this,
                        P_LWIDTH,
                        V::I64(args.get(3).copied().map(i64v).unwrap_or(0)),
                    );
                }
                return Some(None);
            }
            "isFallbackLineSpacingEnabled" => return Some(Some(V::I32(1))),
            _ => return layout_call(vm, class, name, sig, args),
        }
    }
    layout_call(vm, class, name, sig, args)
}

fn layout_call(vm: &mut Vm, _class: &str, name: &str, sig: &str, args: &[V]) -> Option<Option<V>> {
    let Some(&V::Ref(this)) = args.first() else {
        return Some(None);
    };
    let size = pseudo_get(vm, this, P_LSIZE).map(f64v).unwrap_or(12.0) as f32;
    let width = pseudo_get(vm, this, P_LWIDTH).map(i64v).unwrap_or(0);
    let line_h = (size * 1.25).ceil().max(1.0);
    let i = |k: usize| args.get(k).copied().map(i64v).unwrap_or(0);
    Some(Some(match name {
        "getLineCount" => V::I32(layout_lines(vm, this).len() as i32),
        "getText" => pseudo_get(vm, this, P_DATA).unwrap_or(V::Ref(0)),
        "getWidth" => V::I32(width as i32),
        "getHeight" => V::I32((layout_lines(vm, this).len() as f32 * line_h) as i32),
        "getLineTop" => V::I32((i(1) as f32 * line_h) as i32),
        "getLineBottom" => V::I32(((i(1) + 1) as f32 * line_h) as i32),
        "getLineDescent" => V::I32((line_h * 0.15) as i32),
        "getLineAscent" => V::I32(-(size * 0.85) as i32),
        "getLineBaseline" => V::I32((i(1) as f32 * line_h + size * 0.85) as i32),
        "getLineLeft" => V::F32(0.0),
        "getLineRight" => V::F32(width as f32),
        "getLineWidth" | "getLineMax" => {
            let lines = layout_lines(vm, this);
            let w = lines.get(i(1) as usize).map(|l| l.1).unwrap_or(0.0);
            V::F32(if name == "getLineMax" {
                lines.iter().map(|l| l.1).fold(0f32, f32::max)
            } else {
                w
            })
        }
        "getLineStart" | "getLineEnd" | "getLineVisibleEnd" => {
            let lines = layout_lines(vm, this);
            let starts = line_starts(&lines);
            let ix = (i(1) as usize).min(lines.len().saturating_sub(1));
            let mut v = starts.get(ix).copied().unwrap_or(0);
            if name != "getLineStart" {
                v += lines
                    .get(ix)
                    .map(|l| l.0.chars().count() as i64)
                    .unwrap_or(0);
            }
            V::I32(v as i32)
        }
        "getEllipsisCount" | "getEllipsisStart" | "isRtlCharAt" | "isLevelBoundary" => V::I32(0),
        "getLineForVertical" => {
            let lines = layout_lines(vm, this);
            let n = (i(1) as f32 / line_h) as i64;
            V::I32(n.clamp(0, (lines.len() as i64 - 1).max(0)) as i32)
        }
        "getLineForOffset" => {
            let lines = layout_lines(vm, this);
            let starts = line_starts(&lines);
            let off = i(1);
            let mut line = 0i64;
            for (ix, s) in starts.iter().enumerate() {
                if *s <= off {
                    line = ix as i64;
                }
            }
            V::I32(line as i32)
        }
        "getOffsetForHorizontal" => {
            let lines = layout_lines(vm, this);
            let starts = line_starts(&lines);
            let base = starts.get(i(1) as usize).copied().unwrap_or(0);
            V::I32((base + (i(2) as f32 / (size * 0.55)).round() as i64) as i32)
        }
        "getPrimaryHorizontal" | "getSecondaryHorizontal" => {
            let lines = layout_lines(vm, this);
            let starts = line_starts(&lines);
            let off = i(1);
            let mut rel = off;
            for (ix, s) in starts.iter().enumerate() {
                if *s <= off {
                    rel = off - s;
                    let _ = ix;
                }
            }
            V::F32(rel as f32 * size * 0.55)
        }
        "getParagraphDirection" => V::I32(1),
        "getPaint" => pseudo_get(vm, this, P_LPAINT).unwrap_or(V::Ref(0)),
        "getSpacingAdd" => V::F32(0.0),
        "getSpacingMultiplier" => V::F32(1.0),
        "getEllipsizedWidth" => V::I32(width as i32),
        "getDesiredWidth" => {
            if sig.starts_with("(Ljava/lang/CharSequence;") {
                // Static `Layout.getDesiredWidth` — args[0] is the text,
                // not a layout. Two forms: `(CharSequence, TextPaint)`
                // and the ranged `(CharSequence, int, int, TextPaint)`.
                let ranged = sig.contains(";IILandroid/text/TextPaint;");
                let mut t = cs_arg(vm, args.first().copied().unwrap_or(V::Ref(0)));
                let paint_arg = if ranged { args.get(3) } else { args.get(1) };
                if ranged {
                    let chars: Vec<char> = t.chars().collect();
                    let a = (args.get(1).map(|v| i64v(*v)).unwrap_or(0).max(0) as usize)
                        .min(chars.len());
                    let b = (args.get(2).map(|v| i64v(*v)).unwrap_or(0).max(0) as usize)
                        .min(chars.len())
                        .max(a);
                    t = chars[a..b].iter().collect();
                }
                let sz = paint_arg
                    .and_then(|v| match v {
                        V::Ref(h) => pseudo_get(vm, *h, gfx::PK_TSIZE),
                        _ => None,
                    })
                    .map(f64v)
                    .unwrap_or(12.0) as f32;
                // Desired width is the longest line — summing widths
                // across '\n' overstretches wrap-content text.
                V::F32(
                    t.split('\n')
                        .map(|l| gfx::measure_text(l, sz))
                        .fold(0f32, f32::max),
                )
            } else {
                let w = layout_lines(vm, this)
                    .iter()
                    .map(|l| l.1)
                    .fold(0f32, f32::max);
                V::F32(w)
            }
        }
        "getIncludePadding" => V::I32(1),
        "draw" => {
            // Layout.draw(canvas): one Text op per line — Compose's
            // TextDelegate paints paragraphs through this path, so
            // without it every label stays invisible.
            if let Some(canvas) = args.get(1).copied() {
                let mut paint = pseudo_get(vm, this, P_LPAINT)
                    .map(|p| gfx::paint_state(vm, p))
                    .unwrap_or_default();
                // A zero text size renders nothing — fall back to the
                // layout's own size, then a readable default.
                if paint.text_size <= 0.0 {
                    paint.text_size = if size > 0.0 { size } else { 14.0 };
                }
                let lh = (paint.text_size * 1.25).ceil().max(1.0);
                for (ix, (txt, _)) in layout_lines(vm, this).iter().enumerate() {
                    if txt.is_empty() {
                        continue;
                    }
                    gfx::rec(
                        vm,
                        canvas,
                        crate::value::CanvasOp::Text {
                            text: txt.clone(),
                            x: 0.0,
                            y: ix as f32 * lh + paint.text_size * 0.85,
                            paint,
                        },
                    );
                }
            }
            return Some(None);
        }
        "getSelectionPath" | "getCursorPath" | "increaseWidthTo" => return Some(None),
        _ => return None,
    }))
}

/// `StaticLayout$Builder` — stores obtain()/set*() params and mints a
/// Layout host on `build()`.
fn builder_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    Some(match name {
        "obtain" => {
            // (CharSequence, int start, int end, TextPaint, int width)
            let text = cs_arg(vm, args.first().copied().unwrap_or(V::Ref(0)));
            let (lo, hi) = (
                args.get(1).copied().map(i64v).unwrap_or(0),
                args.get(2).copied().map(i64v).unwrap_or(0),
            );
            let chars: Vec<char> = text.chars().collect();
            let slice: String = if hi > 0 && lo < hi && (hi as usize) <= chars.len() {
                chars[lo as usize..hi as usize].iter().collect()
            } else {
                text
            };
            let h = host(vm, "Landroid/text/StaticLayout$Builder;", HostState::Empty);
            if let V::Ref(b) = h {
                let sv = jstr(vm, &slice);
                pseudo_set(vm, b, P_DATA, sv);
                pseudo_set(vm, b, P_LPAINT, args.get(3).copied().unwrap_or(V::Ref(0)));
                pseudo_set(
                    vm,
                    b,
                    P_LWIDTH,
                    V::I64(args.get(4).copied().map(i64v).unwrap_or(0)),
                );
                // Text size at build time comes from the paint — read it
                // now so obtain-order doesn't matter.
                if let Some(V::Ref(p)) = args.get(3).copied() {
                    if let Some(sz) = pseudo_get(vm, p, gfx::PK_TSIZE) {
                        pseudo_set(vm, b, P_LSIZE, sz);
                    }
                }
            }
            Some(h)
        }
        "build" => {
            let V::Ref(b) = args.first().copied().unwrap_or(V::Ref(0)) else {
                return Some(Some(V::Ref(0)));
            };
            let h = host(vm, "Landroid/text/StaticLayout;", HostState::Empty);
            if let V::Ref(l) = h {
                for key in [u64::from(P_DATA), P_LPAINT, P_LWIDTH, P_LSIZE, P_LMAXL] {
                    if let Some(v) = pseudo_get(vm, b, key) {
                        pseudo_set(vm, l, key, v);
                    }
                }
            }
            Some(h)
        }
        "setText" => {
            if let V::Ref(b) = args.first().copied().unwrap_or(V::Ref(0)) {
                pseudo_set(vm, b, P_DATA, args.get(1).copied().unwrap_or(V::Ref(0)));
            }
            Some(args.first().copied().unwrap_or(V::Ref(0)))
        }
        "setMaxLines" => {
            if let V::Ref(b) = args.first().copied().unwrap_or(V::Ref(0)) {
                pseudo_set(
                    vm,
                    b,
                    P_LMAXL,
                    V::I64(args.get(1).copied().map(i64v).unwrap_or(0)),
                );
            }
            Some(args.first().copied().unwrap_or(V::Ref(0)))
        }
        "setWidth" | "setEllipsizedWidth" => {
            if let V::Ref(b) = args.first().copied().unwrap_or(V::Ref(0)) {
                pseudo_set(
                    vm,
                    b,
                    P_LWIDTH,
                    V::I64(args.get(1).copied().map(i64v).unwrap_or(0)),
                );
            }
            Some(args.first().copied().unwrap_or(V::Ref(0)))
        }
        // setAlignment/setLineSpacing/setIncludePad/setEllipsize/
        // setBreakStrategy/setHyphenationFrequency/setJustificationMode/
        // setTextDirection/setIndents/setLeftIndents/setRightIndents/
        // setShiftDrawingOffsetForStartOverhang/... — all fluent
        // setters returning the builder.
        n if n.starts_with("set") => Some(args.first().copied().unwrap_or(V::Ref(0))),
        "<init>" => None,
        _ => return None,
    })
}

// ---------------------------------------------------------------------

fn breakiter_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "getCharacterInstance"
        | "getLineInstance"
        | "getWordInstance"
        | "getSentenceInstance"
        | "getTitleInstance" => Some(Some(host(
            vm,
            "Ljava/text/BreakIterator;",
            HostState::Empty,
        ))),
        "setText" => {
            if let Some(&V::Ref(this)) = args.first() {
                pseudo_set(vm, this, P_DATA, args.get(1).copied().unwrap_or(V::Ref(0)));
                pseudo_set(vm, this, P_BIPOS, V::I64(0));
            }
            Some(None)
        }
        "getText" => Some(
            args.first()
                .and_then(|v| match v {
                    V::Ref(this) => pseudo_get(vm, *this, P_DATA),
                    _ => None,
                })
                .or(Some(V::Ref(0))),
        ),
        "clone" => Some(args.first().copied()),
        _ => {
            let Some(&V::Ref(this)) = args.first() else {
                return Some(None);
            };
            let (pos, len) = (bi_pos(vm, this), bi_len(vm, this));
            match name {
                "first" => {
                    pseudo_set(vm, this, P_BIPOS, V::I64(0));
                    Some(Some(V::I32(if len == 0 { -1 } else { 0 })))
                }
                "last" => {
                    pseudo_set(vm, this, P_BIPOS, V::I64(len));
                    Some(Some(V::I32(if len == 0 { -1 } else { len as i32 })))
                }
                "next" => {
                    let n = args.get(1).map(|v| i64v(*v)).unwrap_or(1);
                    let mut p = pos;
                    let mut out = -1i64;
                    for _ in 0..n.max(1) {
                        if p >= len {
                            out = -1;
                            break;
                        }
                        p += 1;
                        out = p;
                    }
                    pseudo_set(vm, this, P_BIPOS, V::I64(p));
                    Some(Some(V::I32(out as i32)))
                }
                "previous" => {
                    if pos <= 0 {
                        return Some(Some(V::I32(-1)));
                    }
                    let p = pos - 1;
                    pseudo_set(vm, this, P_BIPOS, V::I64(p));
                    Some(Some(V::I32(p as i32)))
                }
                "current" => Some(Some(V::I32(if pos > len { -1 } else { pos as i32 }))),
                "following" => {
                    let i = args.get(1).map(|v| i64v(*v)).unwrap_or(0);
                    Some(Some(V::I32(if i < len { (i + 1) as i32 } else { -1 })))
                }
                "preceding" => {
                    let i = args.get(1).map(|v| i64v(*v)).unwrap_or(0);
                    Some(Some(V::I32(if i > 0 {
                        (i - 1).min(len) as i32
                    } else {
                        -1
                    })))
                }
                "isBoundary" => {
                    let i = args.get(1).map(|v| i64v(*v)).unwrap_or(0);
                    Some(Some(V::I32((i >= 0 && i <= len) as i32)))
                }
                _ => None,
            }
        }
    }
}
