//! `android.graphics` builtins — `Canvas` as a recording surface and
//! `Paint` as pseudo-field state. A guest `View.onDraw(canvas)` runs
//! against [`HostState::CanvasOps`]; the host materializer replays the
//! recorded [`CanvasOp`]s into the real surface, so custom views
//! (calendar month grids, charts, game HUDs) draw actual pixels.

use super::{android, f64v, host, i32v, jstr, pseudo_get, pseudo_set, s, sv, P_DATA};
use crate::value::{CanvasOp, HostState, Obj, PaintState, V};
use crate::{Rt, Vm};

// Pseudo keys (0xFFFD_FFxx block — the menu/ta keys stop at 0xFFFD_FFE0).
const PK_COLOR: u32 = 0xFFFD_FF00;
const PK_TSIZE: u32 = 0xFFFD_FF01;
const PK_ALIGN: u32 = 0xFFFD_FF02;
const PK_STYLE: u32 = 0xFFFD_FF03;
const PK_SW: u32 = 0xFFFD_FF04;
const PK_BOLD: u32 = 0xFFFD_FF05;
const PK_CW: u32 = 0xFFFD_FF10;
const PK_CH: u32 = 0xFFFD_FF11;
/// `new Canvas(bitmap)`: the backing bitmap handle — drawBitmap
/// composites pixels into it instead of only recording ops (Glide's
/// transform path draws into a pooled Bitmap this way).
const PK_TARGET: u32 = 0xFFFD_FF12;
const PK_SAVECNT: u32 = 0xFFFD_FF13;

fn flt(v: V) -> f32 {
    f64v(v) as f32
}

fn arg(args: &[V], i: usize) -> V {
    args.get(i).copied().unwrap_or(V::I32(0))
}

/// The name stored on a host enum constant (`Paint$Align.CENTER` →
/// `"CENTER"`); dex enums keep it in the `name` field.
fn enum_name(vm: &Vm, v: V) -> Option<String> {
    let V::Ref(h) = v else { return None };
    match pseudo_get(vm, h, P_DATA) {
        Some(V::Ref(nh)) => s(vm, nh).map(|x| x.to_string()),
        _ => match vm.heap.get(h) {
            Some(Obj::Instance { fields, .. }) => {
                let name_field = vm
                    .field_index_of("Ljava/lang/Enum;", "name")
                    .map(|g| vm.canon_field(g));
                name_field.and_then(|f| match fields.get(&f) {
                    Some(V::Ref(nh)) => s(vm, *nh).map(|x| x.to_string()),
                    _ => None,
                })
            }
            _ => None,
        },
    }
}

/// Snapshot a `Paint` handle's pseudo fields at draw time.
fn paint_state(vm: &Vm, v: V) -> PaintState {
    let V::Ref(h) = v else {
        return PaintState::default();
    };
    let gi = |k: u32, d: i32| match pseudo_get(vm, h, k) {
        Some(x) => i32v(x),
        None => d,
    };
    let gf = |k: u32, d: f32| match pseudo_get(vm, h, k) {
        Some(V::F32(f)) => f,
        Some(x) => f64v(x) as f32,
        None => d,
    };
    PaintState {
        argb: gi(PK_COLOR, 0xFF00_0000u32 as i32),
        text_size: gf(PK_TSIZE, 12.0),
        align: gi(PK_ALIGN, 0).clamp(0, 2) as u8,
        style: gi(PK_STYLE, 0).clamp(0, 2) as u8,
        stroke_w: gf(PK_SW, 1.0),
        fake_bold: gi(PK_BOLD, 0) != 0,
    }
}

/// Append an op to a recording canvas handle.
pub(crate) fn rec(vm: &mut Vm, canvas: V, op: CanvasOp) {
    let V::Ref(h) = canvas else { return };
    if let Some(Obj::Host {
        state: HostState::CanvasOps(ops),
        ..
    }) = vm.heap.get_mut(h)
    {
        ops.push(op);
    }
}

/// A rect-ish arg (`Rect`/`RectF` object) → edges.
fn rect_edges_of(vm: &Vm, v: V) -> Option<(f32, f32, f32, f32)> {
    let V::Ref(h) = v else { return None };
    let (l, t, r, b) = android::rect_edges(vm, h);
    Some((l as f32, t as f32, r as f32, b as f32))
}

fn canvas_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = arg(args, 0);
    let ps = |vm: &Vm, args: &[V]| paint_state(vm, arg(args, args.len() - 1));
    Ok(match name {
        "<init>" => {
            // Canvas() or Canvas(Bitmap): a Bitmap arg becomes the
            // draw target (pixel-backed) and its size the canvas size.
            if let V::Ref(h) = this {
                if let Some(V::Ref(b)) = args.get(1).copied() {
                    if android::bitmap_dims(vm, V::Ref(b)) != (0, 0) {
                        pseudo_set(vm, h, PK_TARGET, V::Ref(b));
                        let (w, hh) = android::bitmap_dims(vm, V::Ref(b));
                        pseudo_set(vm, h, PK_CW, V::I32(w as i32));
                        pseudo_set(vm, h, PK_CH, V::I32(hh as i32));
                    }
                }
            }
            Some(None)
        }
        "getWidth" => Some(Some(V::I32(match this {
            V::Ref(h) => i32v(pseudo_get(vm, h, PK_CW).unwrap_or(V::I32(0))),
            _ => 0,
        }))),
        "getHeight" => Some(Some(V::I32(match this {
            V::Ref(h) => i32v(pseudo_get(vm, h, PK_CH).unwrap_or(V::I32(0))),
            _ => 0,
        }))),
        "save" | "saveLayer" | "saveLayerAlpha" => {
            rec(vm, this, CanvasOp::Save);
            if let V::Ref(h) = this {
                let n = i32v(pseudo_get(vm, h, PK_SAVECNT).unwrap_or(V::I32(0))) + 1;
                pseudo_set(vm, h, PK_SAVECNT, V::I32(n));
                Some(Some(V::I32(n)))
            } else {
                Some(Some(V::I32(1)))
            }
        }
        "saveCount" => Some(Some(V::I32(match this {
            V::Ref(h) => i32v(pseudo_get(vm, h, PK_SAVECNT).unwrap_or(V::I32(0))),
            _ => 0,
        }))),
        "restore" => {
            rec(vm, this, CanvasOp::Restore);
            if let V::Ref(h) = this {
                let n = i32v(pseudo_get(vm, h, PK_SAVECNT).unwrap_or(V::I32(0)));
                pseudo_set(vm, h, PK_SAVECNT, V::I32((n - 1).max(0)));
            }
            Some(None)
        }
        // restoreToCount pops every save above `count` — recording a
        // single Restore would leave guest saves on the replay stack,
        // and the host's own bracket restore would then pop a guest
        // save instead, leaking the view's translate into its siblings.
        "restoreToCount" => {
            let want = i32v(args.first().copied().unwrap_or(V::I32(0))).max(0);
            if let V::Ref(h) = this {
                let have = i32v(pseudo_get(vm, h, PK_SAVECNT).unwrap_or(V::I32(0)));
                for _ in want..have {
                    rec(vm, this, CanvasOp::Restore);
                }
                pseudo_set(vm, h, PK_SAVECNT, V::I32(want.min(have)));
            } else {
                rec(vm, this, CanvasOp::Restore);
            }
            Some(None)
        }
        "translate" => {
            rec(
                vm,
                this,
                CanvasOp::Translate {
                    dx: flt(arg(args, 1)),
                    dy: flt(arg(args, 2)),
                },
            );
            Some(None)
        }
        "clipRect" | "clipOutRect" => {
            let (l, t, r, b) = if let Some(e) = rect_edges_of(vm, arg(args, 1)) {
                e
            } else {
                (
                    flt(arg(args, 1)),
                    flt(arg(args, 2)),
                    flt(arg(args, 3)),
                    flt(arg(args, 4)),
                )
            };
            rec(vm, this, CanvasOp::ClipRect { l, t, r, b });
            Some(Some(V::I32(1)))
        }
        "drawColor" | "drawRGB" | "drawARGB" => {
            rec(
                vm,
                this,
                CanvasOp::Color {
                    argb: i32v(arg(args, 1)),
                },
            );
            Some(None)
        }
        "drawPaint" => {
            let p = ps(vm, args);
            rec(vm, this, CanvasOp::Color { argb: p.argb });
            Some(None)
        }
        "drawRect" => {
            let p = ps(vm, args);
            let (l, t, r, b) = if let Some(e) = rect_edges_of(vm, arg(args, 1)) {
                e
            } else {
                (
                    flt(arg(args, 1)),
                    flt(arg(args, 2)),
                    flt(arg(args, 3)),
                    flt(arg(args, 4)),
                )
            };
            rec(
                vm,
                this,
                CanvasOp::Rect {
                    l,
                    t,
                    r,
                    b,
                    paint: p,
                },
            );
            Some(None)
        }
        "drawRoundRect" => {
            let p = ps(vm, args);
            let (l, t, r, b, rx, ry) = if let Some(e) = rect_edges_of(vm, arg(args, 1)) {
                (e.0, e.1, e.2, e.3, flt(arg(args, 2)), flt(arg(args, 3)))
            } else {
                (
                    flt(arg(args, 1)),
                    flt(arg(args, 2)),
                    flt(arg(args, 3)),
                    flt(arg(args, 4)),
                    flt(arg(args, 5)),
                    flt(arg(args, 6)),
                )
            };
            rec(
                vm,
                this,
                CanvasOp::RoundRect {
                    l,
                    t,
                    r,
                    b,
                    rx,
                    ry,
                    paint: p,
                },
            );
            Some(None)
        }
        "drawOval" | "drawArc" => {
            let p = ps(vm, args);
            let (l, t, r, b) = if let Some(e) = rect_edges_of(vm, arg(args, 1)) {
                e
            } else {
                (
                    flt(arg(args, 1)),
                    flt(arg(args, 2)),
                    flt(arg(args, 3)),
                    flt(arg(args, 4)),
                )
            };
            rec(
                vm,
                this,
                CanvasOp::Oval {
                    l,
                    t,
                    r,
                    b,
                    paint: p,
                },
            );
            Some(None)
        }
        "drawCircle" => {
            let p = ps(vm, args);
            rec(
                vm,
                this,
                CanvasOp::Circle {
                    cx: flt(arg(args, 1)),
                    cy: flt(arg(args, 2)),
                    r: flt(arg(args, 3)),
                    paint: p,
                },
            );
            Some(None)
        }
        "drawLine" => {
            let p = ps(vm, args);
            rec(
                vm,
                this,
                CanvasOp::Line {
                    x0: flt(arg(args, 1)),
                    y0: flt(arg(args, 2)),
                    x1: flt(arg(args, 3)),
                    y1: flt(arg(args, 4)),
                    paint: p,
                },
            );
            Some(None)
        }
        "drawLines" | "drawPoints" => {
            // (float[] pts [, offset, count], Paint): pairs — lines take
            // 4 floats per segment, points take 2 (dot = tiny circle).
            let p = ps(vm, args);
            let pts: Vec<f32> = match arg(args, 1) {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.iter().map(|v| flt(*v)).collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            let (off, cnt) = if args.len() >= 4 {
                (i32v(arg(args, 2)) as usize, i32v(arg(args, 3)) as usize)
            } else {
                (0, pts.len())
            };
            let pts = &pts[off.min(pts.len())..(off + cnt).min(pts.len())];
            if name == "drawLines" {
                for seg in pts.as_chunks::<4>().0 {
                    rec(
                        vm,
                        this,
                        CanvasOp::Line {
                            x0: seg[0],
                            y0: seg[1],
                            x1: seg[2],
                            y1: seg[3],
                            paint: p,
                        },
                    );
                }
            } else {
                for pt in pts.as_chunks::<2>().0 {
                    rec(
                        vm,
                        this,
                        CanvasOp::Circle {
                            cx: pt[0],
                            cy: pt[1],
                            r: (p.stroke_w / 2.0).max(0.5),
                            paint: p,
                        },
                    );
                }
            }
            Some(None)
        }
        "drawText" | "drawPosText" | "drawTextOnPath" | "drawTextRun" => {
            let p = ps(vm, args);
            let text_arg = arg(args, 1);
            let is_char_arr = sig.contains("[C");
            // Locate the (x, y) floats: after an optional (start, end)
            // int pair on bounded overloads.
            let bounded = sig.contains("IIFF") || is_char_arr;
            let (xi, yi) = if bounded { (4, 5) } else { (2, 3) };
            let full = sv(vm, text_arg).unwrap_or_else(|| match text_arg {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data
                        .iter()
                        .filter_map(|v| char::from_u32(i32v(*v) as u32))
                        .collect(),
                    _ => String::new(),
                },
                _ => String::new(),
            });
            let text = if bounded && !is_char_arr {
                let st = i32v(arg(args, 2)).max(0) as usize;
                let en = i32v(arg(args, 3)).max(0) as usize;
                full.chars().skip(st).take(en.saturating_sub(st)).collect()
            } else {
                full
            };
            rec(
                vm,
                this,
                CanvasOp::Text {
                    text,
                    x: flt(arg(args, xi)),
                    y: flt(arg(args, yi)),
                    paint: p,
                },
            );
            Some(None)
        }
        "drawBitmap" | "drawBitmapMesh" => {
            // (Bitmap, ...) — position from the first two float args or
            // a dest rect; pixels from the Bitmap host state.
            let (src_w, src_h, rgba) = match arg(args, 1) {
                V::Ref(bh) => match vm.heap.get(bh) {
                    Some(Obj::Host {
                        state: HostState::Bitmap { w, h, rgba },
                        ..
                    }) => (*w, *h, rgba.clone()),
                    _ => (0, 0, std::rc::Rc::new(Vec::new())),
                },
                _ => (0, 0, std::rc::Rc::new(Vec::new())),
            };
            // Dest rect: last rect-arg before the paint, else (x, y).
            let mut x = 0.0f32;
            let mut y = 0.0f32;
            let mut dw = 0.0f32;
            let mut dh = 0.0f32;
            let mut floats: Vec<f32> = Vec::new();
            for a in &args[1..] {
                if let Some(e) = rect_edges_of(vm, *a) {
                    x = e.0;
                    y = e.1;
                    dw = e.2 - e.0;
                    dh = e.3 - e.1;
                } else {
                    match a {
                        V::F32(f) => floats.push(*f),
                        V::I32(_) | V::F64(_) => floats.push(flt(*a)),
                        _ => {}
                    }
                }
            }
            if dw == 0.0 {
                x = floats.first().copied().unwrap_or(0.0);
                y = floats.get(1).copied().unwrap_or(0.0);
                dw = src_w as f32;
                dh = src_h as f32;
            }
            // A canvas bound to a Bitmap (`new Canvas(bitmap)`)
            // composites pixels into the bitmap itself — recording an
            // op would leave the target blank (Glide's transform).
            let target = match this {
                V::Ref(h) => pseudo_get(vm, h, PK_TARGET),
                _ => None,
            };
            match target {
                Some(V::Ref(th)) if th != 0 => {
                    bitmap_blit(vm, th, (x, y, dw, dh), (src_w, src_h), &rgba);
                }
                _ => rec(
                    vm,
                    this,
                    CanvasOp::Bitmap {
                        x,
                        y,
                        w: dw.max(0.0) as u32,
                        h: dh.max(0.0) as u32,
                        src_w,
                        src_h,
                        rgba,
                    },
                ),
            }
            Some(None)
        }
        // No-op or non-recordable transforms: scale/rotate/skew/
        // concat/clipPath/drawPicture/drawPath/drawVertices/drawPatch.
        _ => None,
    })
}

/// Composite `src` (RGBA8, `sw`x`sh`) scaled into the dest rect
/// `(x, y, dw, dh)` of bitmap `dst_h`, src-over. Nearest-neighbor.
fn bitmap_blit(
    vm: &mut Vm,
    dst_h: u32,
    dst_rect: (f32, f32, f32, f32),
    src_dim: (u32, u32),
    src: &[u8],
) {
    let (x, y, dw, dh) = dst_rect;
    let (sw, sh) = src_dim;
    let (bw, bh, base) = match vm.heap.get(dst_h) {
        Some(Obj::Host {
            state: HostState::Bitmap { w, h, rgba },
            ..
        }) => (*w, *h, rgba.clone()),
        _ => return,
    };
    if bw == 0 || bh == 0 || sw == 0 || sh == 0 || dw <= 0.0 || dh <= 0.0 {
        return;
    }
    let dx = x.max(0.0) as u32;
    let dy = y.max(0.0) as u32;
    let dw = (dw as u32).min(bw.saturating_sub(dx));
    let dh = (dh as u32).min(bh.saturating_sub(dy));
    if dw == 0 || dh == 0 {
        return;
    }
    let mut out = base.as_ref().clone();
    for yy in 0..dh {
        for xx in 0..dw {
            let sx = (xx * sw / dw).min(sw - 1);
            let sy = (yy * sh / dh).min(sh - 1);
            let si = ((sy * sw + sx) * 4) as usize;
            let di = (((dy + yy) * bw + dx + xx) * 4) as usize;
            let a = src[si + 3] as u32;
            if a == 0 {
                continue;
            }
            if a == 255 {
                out[di..di + 4].copy_from_slice(&src[si..si + 4]);
            } else {
                let ia = 255 - a;
                for c in 0..3 {
                    out[di + c] = ((src[si + c] as u32 * a + out[di + c] as u32 * ia) / 255) as u8;
                }
                out[di + 3] = (a + (out[di + 3] as u32 * ia) / 255) as u8;
            }
        }
    }
    if let Some(Obj::Host {
        state: HostState::Bitmap { rgba: dst, .. },
        ..
    }) = vm.heap.get_mut(dst_h)
    {
        *dst = std::rc::Rc::new(out);
    }
}

fn paint_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = arg(args, 0);
    let V::Ref(h) = this else { return Ok(None) };
    Ok(match name {
        "<init>" => {
            // Paint(Paint src) copies the source's state; plain init gets
            // defaults. AOSP copies every field — we carry the pseudo set.
            if let Some(V::Ref(src)) = args.get(1).copied() {
                for key in [PK_COLOR, PK_TSIZE, PK_ALIGN, PK_STYLE, PK_SW, PK_BOLD] {
                    if let Some(v) = pseudo_get(vm, src, key) {
                        pseudo_set(vm, h, key, v);
                    }
                }
            }
            if pseudo_get(vm, h, PK_TSIZE).is_none() {
                pseudo_set(vm, h, PK_COLOR, V::I32(0xFF00_0000u32 as i32));
                pseudo_set(vm, h, PK_TSIZE, V::F32(12.0));
            }
            Some(None)
        }
        "setColor" => {
            pseudo_set(vm, h, PK_COLOR, arg(args, 1));
            Some(None)
        }
        "setARGB" => {
            let (a, r, g, b) = (
                i32v(arg(args, 1)),
                i32v(arg(args, 2)),
                i32v(arg(args, 3)),
                i32v(arg(args, 4)),
            );
            pseudo_set(
                vm,
                h,
                PK_COLOR,
                V::I32((a << 24) | (r << 16) | (g << 8) | b),
            );
            Some(None)
        }
        "setAlpha" => {
            let a = i32v(arg(args, 1)).clamp(0, 255);
            let c = match pseudo_get(vm, h, PK_COLOR) {
                Some(v) => i32v(v) & 0x00FF_FFFF,
                None => 0,
            };
            pseudo_set(vm, h, PK_COLOR, V::I32((a << 24) | c));
            Some(None)
        }
        "setTextSize" => {
            let v = match arg(args, 1) {
                V::F32(f) => V::F32(f),
                x => V::F32(f64v(x) as f32),
            };
            pseudo_set(vm, h, PK_TSIZE, v);
            Some(None)
        }
        "setTextAlign" => {
            let al = match enum_name(vm, arg(args, 1)).as_deref() {
                Some("CENTER") => 1,
                Some("RIGHT") => 2,
                _ => 0,
            };
            pseudo_set(vm, h, PK_ALIGN, V::I32(al));
            Some(None)
        }
        "setStyle" => {
            let st = match enum_name(vm, arg(args, 1)).as_deref() {
                Some("STROKE") => 1,
                Some("FILL_AND_STROKE") => 2,
                _ => 0,
            };
            pseudo_set(vm, h, PK_STYLE, V::I32(st));
            Some(None)
        }
        "setStrokeWidth" => {
            let v = match arg(args, 1) {
                V::F32(f) => V::F32(f),
                x => V::F32(f64v(x) as f32),
            };
            pseudo_set(vm, h, PK_SW, v);
            Some(None)
        }
        "setFakeBoldText" => {
            pseudo_set(vm, h, PK_BOLD, V::I32(i32v(arg(args, 1))));
            Some(None)
        }
        "getColor" => Some(Some(
            pseudo_get(vm, h, PK_COLOR).unwrap_or(V::I32(0xFF00_0000u32 as i32)),
        )),
        "getAlpha" => Some(Some(V::I32(
            (i32v(pseudo_get(vm, h, PK_COLOR).unwrap_or(V::I32(0xFF00_0000u32 as i32))) >> 24)
                & 0xFF,
        ))),
        "getTextSize" => Some(Some(pseudo_get(vm, h, PK_TSIZE).unwrap_or(V::F32(12.0)))),
        "getStrokeWidth" => Some(Some(pseudo_get(vm, h, PK_SW).unwrap_or(V::F32(1.0)))),
        "getTextAlign" => {
            let n = match pseudo_get(vm, h, PK_ALIGN) {
                Some(V::I32(1)) => "CENTER",
                Some(V::I32(2)) => "RIGHT",
                _ => "LEFT",
            };
            Some(Some(host_enum_const(
                vm,
                "Landroid/graphics/Paint$Align;",
                n,
            )))
        }
        "getStyle" => {
            let n = match pseudo_get(vm, h, PK_STYLE) {
                Some(V::I32(1)) => "STROKE",
                Some(V::I32(2)) => "FILL_AND_STROKE",
                _ => "FILL",
            };
            Some(Some(host_enum_const(
                vm,
                "Landroid/graphics/Paint$Style;",
                n,
            )))
        }
        "measureText" => {
            let size = match pseudo_get(vm, h, PK_TSIZE) {
                Some(V::F32(f)) => f,
                Some(v) => f64v(v) as f32,
                None => 12.0,
            };
            let text = sv(vm, arg(args, 1)).unwrap_or_default();
            Some(Some(V::F32(measure_text(&text, size))))
        }
        // `breakText` returns the NUMBER OF CHARS that fit maxWidth —
        // int, not float; a float return trips the int-register check
        // and kills the guest call. Variants:
        //   (CharSequence, boolean, float maxWidth, float[] measured)
        //   (CharSequence, int start, int end, boolean, float, float[])
        //   (char[], int index, int count, float maxWidth, float[])
        "breakText" => {
            let size = match pseudo_get(vm, h, PK_TSIZE) {
                Some(V::F32(f)) => f,
                Some(v) => f64v(v) as f32,
                None => 12.0,
            };
            let text = sv(vm, arg(args, 1)).unwrap_or_default();
            let mut len = text.chars().count() as i32;
            // char[] overload: explicit count at args[3].
            if len == 0 {
                if let Some(V::I32(c)) = args.get(3) {
                    len = (*c).max(0);
                }
            }
            // maxWidth is the last F32 argument.
            let maxw = args
                .iter()
                .rev()
                .find_map(|a| match a {
                    V::F32(f) => Some(*f),
                    _ => None,
                })
                .unwrap_or(f32::MAX);
            let per = size * 0.55;
            let count = if per > 0.0 {
                ((maxw / per).floor() as i32).clamp(0, len)
            } else {
                len
            };
            // Optional trailing float[] receives the measured width.
            if let Some(V::Ref(mh)) = args.last() {
                if *mh != 0 {
                    let _ = vm.aput(*mh, 0, V::F32(count as f32 * per));
                }
            }
            Some(Some(V::I32(count)))
        }
        "getTextBounds" => {
            let text = sv(vm, arg(args, 1)).unwrap_or_default();
            let size = match pseudo_get(vm, h, PK_TSIZE) {
                Some(V::F32(f)) => f,
                Some(v) => f64v(v) as f32,
                None => 12.0,
            };
            let w = measure_text(&text, size) as i32;
            if let V::Ref(rh) = arg(args, args.len() - 1) {
                if rh != 0 {
                    android::rect_write(vm, rh, 0, (-0.8 * size) as i32, w, (0.25 * size) as i32);
                }
            }
            Some(None)
        }
        "descent" => Some(Some(V::F32(match pseudo_get(vm, h, PK_TSIZE) {
            Some(V::F32(f)) => f * 0.25,
            _ => 3.0,
        }))),
        "ascent" => Some(Some(V::F32(match pseudo_get(vm, h, PK_TSIZE) {
            Some(V::F32(f)) => f * -0.8,
            _ => -9.6,
        }))),
        "getFontMetrics" | "getFontMetricsInt" => {
            let size = match pseudo_get(vm, h, PK_TSIZE) {
                Some(V::F32(f)) => f,
                Some(v) => f64v(v) as f32,
                None => 12.0,
            };
            let (top, asc, des, bot) = (size * -0.95, size * -0.8, size * 0.25, size * 0.35);
            let desc = if name == "getFontMetrics" {
                "Landroid/graphics/Paint$FontMetrics;"
            } else {
                "Landroid/graphics/Paint$FontMetricsInt;"
            };
            // The signature — not the arg value — picks the overload: a
            // null metrics arg can arrive as V::I32(0), which looks
            // identical to "no argument". (FontMetrics)F fills the
            // object and returns -ascent; ()L...FontMetrics; creates
            // and returns a fresh filled object.
            let has_arg = _sig.contains("FontMetrics");
            let fh = match arg(args, 1) {
                V::Ref(fh) if fh != 0 && has_arg => Some(fh),
                _ => None,
            };
            let fh = match (fh, name, has_arg) {
                (Some(fh), _, _) => Some(fh),
                (None, "getFontMetrics", false) => match host(vm, desc, HostState::Empty) {
                    V::Ref(fh) => Some(fh),
                    _ => None,
                },
                _ => None,
            };
            if let Some(fh) = fh {
                for (n, v) in [
                    ("top", top),
                    ("ascent", asc),
                    ("descent", des),
                    ("bottom", bot),
                    ("leading", 0.0),
                ] {
                    if let Some(g) = vm.field_index_of(desc, n) {
                        let g = vm.canon_field(g);
                        if let Some(Obj::Instance { fields, .. } | Obj::Host { fields, .. }) =
                            vm.heap.get_mut(fh)
                        {
                            fields.insert(
                                g,
                                if name == "getFontMetrics" {
                                    V::F32(v)
                                } else {
                                    V::I32(v as i32)
                                },
                            );
                        }
                    }
                }
            }
            if has_arg {
                // metrics-arg overloads: recommended spacing above the
                // baseline (-ascent) in the declared return type.
                if name == "getFontMetrics" {
                    Some(Some(V::F32(-asc)))
                } else {
                    Some(Some(V::I32(-asc as i32)))
                }
            } else if name == "getFontMetrics" {
                // No-arg overload returns a fresh FontMetrics object.
                match fh {
                    Some(fh) => Some(Some(V::Ref(fh))),
                    None => Some(Some(V::F32(-asc))),
                }
            } else {
                Some(Some(V::I32(-asc as i32)))
            }
        }
        // Mutators we accept but don't model: typeface, AA, filters,
        // effects, flags, spacing, skew, cap/join, hinting, variants.
        "setAntiAlias"
        | "setSubpixelText"
        | "setLinearText"
        | "setDither"
        | "setFilterBitmap"
        | "setUnderlineText"
        | "setStrikeThruText"
        | "setTypeface"
        | "setTextSkewX"
        | "setTextScaleX"
        | "setLetterSpacing"
        | "setStrokeCap"
        | "setStrokeJoin"
        | "setStrokeMiter"
        | "setMaskFilter"
        | "setPathEffect"
        | "setShader"
        | "setColorFilter"
        | "setXfermode"
        | "setShadowLayer"
        | "setFlags"
        | "setHinting"
        | "setElegantTextHeight"
        | "setFontFeatureSettings"
        | "setFontVariationSettings"
        | "clearShadowLayer"
        | "set"
        | "reset"
        | "setLocales"
        | "setTextLocale" => Some(None),
        _ => None,
    })
}

/// Width approximation for `measureText`/`getTextBounds`: Latin ~
/// 0.55em, CJK ~1em — average enough for layout math. The real font
/// advance table lives in ovelo-ui's FontSet; guest code mostly needs
/// a plausible number for centering, not pixel-exact shaping.
fn measure_text(text: &str, size: f32) -> f32 {
    let w: f32 = text
        .chars()
        .map(|c| {
            if c.is_ascii() {
                0.55
            } else if (c as u32) > 0x2E7F {
                1.0
            } else {
                0.6
            }
        })
        .sum();
    w * size
}

/// Stand-in enum constant (`Paint.getTextAlign()` result): a host
/// object whose `P_DATA` carries the constant name.
fn host_enum_const(vm: &mut Vm, desc: &str, name: &str) -> V {
    let h = host(vm, desc, HostState::Empty);
    if let V::Ref(hh) = h {
        let n = jstr(vm, name);
        pseudo_set(vm, hh, P_DATA, n);
    }
    h
}

/// `Landroid/graphics/*` methods we model.
pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    match class {
        "Landroid/graphics/Canvas;" => canvas_call(vm, name, sig, args),
        "Landroid/graphics/Paint;" | "Landroid/text/TextPaint;" => paint_call(vm, name, sig, args),
        _ => Ok(None),
    }
}

/// Run a guest `View.onDraw(Canvas)` over a recording canvas sized to
/// the view's laid-out frame; returns the recorded ops (empty when the
/// class doesn't override `onDraw` or the call fails — a framework
/// `View.onDraw` is a no-op anyway).
pub(crate) fn guest_draw_ops(vm: &mut Vm, view: u32, w: i32, h: i32) -> Vec<CanvasOp> {
    // GLSurfaceView subclasses draw through a Renderer, not onDraw —
    // drive the GL10 path and flatten its recorded primitives.
    if pseudo_get(vm, view, super::gl::P_GLES_RENDERER).is_some() {
        return super::gl::gl_frame(vm, view, w, h);
    }
    // A native-renderer surface (Godot's GLThread) swaps into the
    // readback stash — present those pixels on the surface's view.
    if let Some((px, sw, sh)) = super::gl::swap_frame() {
        let n = super::class_of(vm, V::Ref(view));
        let glish =
            n.contains("GLSurfaceView") || n.contains("GLRenderView") || n.contains("SurfaceView");
        if glish {
            return vec![CanvasOp::Bitmap {
                x: 0.0,
                y: 0.0,
                w: sw,
                h: sh,
                src_w: sw,
                src_h: sh,
                rgba: std::rc::Rc::new(px),
            }];
        }
    }
    let Some(ci) = vm.heap.class_of(view) else {
        return Vec::new();
    };
    // Skip when no dex class in the chain overrides onDraw — otherwise
    // we'd pay an invoke per leaf view only to record nothing.
    let has = vm
        .find_method_on_chain_sig(ci, "onDraw", &["Landroid/graphics/Canvas;"])
        .ok()
        .flatten()
        .is_some();
    // `dispatchDraw` overrides matter too: `AndroidComposeView` keeps
    // `onDraw` empty and draws the whole LayoutNode tree from
    // `dispatchDraw` (its children are dex LayoutNodes, not host
    // views, so the host walk can't reach them).
    let has_dd = vm
        .find_method_on_chain_sig(ci, "dispatchDraw", &["Landroid/graphics/Canvas;"])
        .ok()
        .flatten()
        .is_some();
    if !has && !has_dd {
        return Vec::new();
    }
    let V::Ref(canvas) = host(
        vm,
        "Landroid/graphics/Canvas;",
        HostState::CanvasOps(Vec::new()),
    ) else {
        return Vec::new();
    };
    pseudo_set(vm, canvas, PK_CW, V::I32(w));
    pseudo_set(vm, canvas, PK_CH, V::I32(h));
    let mut err = false;
    if has {
        let r = vm.call_virtual_sig(
            view,
            "onDraw",
            &["Landroid/graphics/Canvas;"],
            &[V::Ref(canvas)],
        );
        err = r.is_err();
    }
    if has_dd {
        // dispatchDraw runs after onDraw, matching View.draw order.
        let r2 = vm.call_virtual_sig(
            view,
            "dispatchDraw",
            &["Landroid/graphics/Canvas;"],
            &[V::Ref(canvas)],
        );
        err = err || r2.is_err();
    }
    let r: Result<(), ()> = if err { Err(()) } else { Ok(()) };
    let n = match vm.heap.get(canvas) {
        Some(Obj::Host {
            state: HostState::CanvasOps(ops),
            ..
        }) => ops.len(),
        _ => 0,
    };
    if crate::dbg_flag("OVELO_DRAWDBG") {
        let d = crate::jdk::class_of(vm, V::Ref(view));
        eprintln!(
            "[drawdbg] onDraw h{view} {d} {w}x{h} -> {n} ops err={:?}",
            r.is_err()
        );
        if crate::dbg_flag("OVELO_DRAWDBG_DUMP") {
            if let Some(Obj::Host {
                state: HostState::CanvasOps(ops),
                ..
            }) = vm.heap.get(canvas)
            {
                for (i, op) in ops.iter().enumerate() {
                    eprintln!("[drawdbg]   op{i} {op:?}");
                }
            }
        }
    }
    match vm.heap.get(canvas) {
        Some(Obj::Host {
            state: HostState::CanvasOps(ops),
            ..
        }) => ops.clone(),
        _ => Vec::new(),
    }
}

impl Default for PaintState {
    fn default() -> Self {
        Self {
            argb: 0xFF00_0000u32 as i32,
            text_size: 12.0,
            align: 0,
            style: 0,
            stroke_w: 1.0,
            fake_bold: false,
        }
    }
}
