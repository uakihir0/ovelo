//! `android.graphics` builtins — `Canvas` as a recording surface and
//! `Paint` as pseudo-field state. A guest `View.onDraw(canvas)` runs
//! against [`HostState::CanvasOps`]; the host materializer replays the
//! recorded [`CanvasOp`]s into the real surface, so custom views
//! (calendar month grids, charts, game HUDs) draw actual pixels.

use super::{android, f64v, fields_mut, host, i32v, jstr, pseudo_get, pseudo_set, s, sv, P_DATA};
use crate::value::{CanvasOp, HostState, Obj, PaintState, PathCmd, V};
use crate::{Rt, Vm};

// Pseudo keys (0xFFFD_FFxx block — the menu/ta keys stop at 0xFFFD_FFE0).
const PK_COLOR: u32 = 0xFFFD_FF00;
pub(crate) const PK_TSIZE: u32 = 0xFFFD_FF01;
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
/// RenderNode → the recording canvas produced by beginRecording.
const PK_RECCANVAS: u32 = 0xFFFD_FF14;
/// RenderNode placement — `setPosition(l, t, r, b)` records the node
/// origin; `drawRenderNode` must draw the node there (Compose 1.9
/// positions layers exclusively through these).
const PK_RN_LEFT: u32 = 0xFFFD_FF15;
const PK_RN_TOP: u32 = 0xFFFD_FF16;
/// Path fill-type enum object and ordinal.
const PK_FILLTYPE: u32 = 0xFFFD_FF17;
const PK_FILLORD: u32 = 0xFFFD_FF18;
/// Per-canvas matrix save stack (a host float array handle).
const PK_MATSTACK: u32 = 0xFFFD_FF19;
/// `android.graphics.Matrix` affine coefficients — Android's first
/// six of nine values: x' = m0·x + m1·y + m2, y' = m3·x + m4·y + m5.
/// Perspective row stays [0,0,1].
const PK_M0: u32 = 0xFFFD_FF20;

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
pub(crate) fn paint_state(vm: &Vm, v: V) -> PaintState {
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

fn canvas_mat_push(vm: &mut Vm, h: u32) {
    let mut vals = match pseudo_get(vm, h, PK_MATSTACK) {
        Some(V::Ref(ah)) => match vm.heap.get(ah) {
            Some(Obj::Array { data, .. }) => data.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    vals.extend(mat_read(vm, h).iter().map(|f| V::F32(*f)));
    let ah = vm.heap.alloc(Obj::Array {
        elem: "F".into(),
        data: vals,
    });
    if let Some(f) = fields_mut(vm, h) {
        f.insert(PK_MATSTACK as u64, V::Ref(ah));
    }
}

fn canvas_mat_pop(vm: &mut Vm, h: u32) {
    let mut vals = match pseudo_get(vm, h, PK_MATSTACK) {
        Some(V::Ref(ah)) => match vm.heap.get(ah) {
            Some(Obj::Array { data, .. }) => data.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    if vals.len() >= 6 {
        let tail = vals.split_off(vals.len() - 6);
        let mut m = [0.0; 6];
        for (k, v) in tail.iter().enumerate() {
            m[k] = f64v(*v) as f32;
        }
        mat_write(vm, h, m);
        let ah = vm.heap.alloc(Obj::Array {
            elem: "F".into(),
            data: vals,
        });
        if let Some(f) = fields_mut(vm, h) {
            f.insert(PK_MATSTACK as u64, V::Ref(ah));
        }
    }
}

fn mat_point(m: [f32; 6], x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
}

fn flatten_path_cmds(cmds: &[PathCmd], m: [f32; 6]) -> Vec<Vec<(f32, f32)>> {
    let mut out: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut cur = (0.0f32, 0.0f32);
    let mut start = cur;
    let mut pts: Vec<(f32, f32)> = Vec::new();
    let push = |pts: &mut Vec<(f32, f32)>, x: f32, y: f32| pts.push(mat_point(m, x, y));
    for c in cmds {
        match *c {
            PathCmd::MoveTo { x, y } => {
                if pts.len() > 1 {
                    out.push(std::mem::take(&mut pts));
                }
                cur = (x, y);
                start = cur;
                push(&mut pts, x, y);
            }
            PathCmd::LineTo { x, y } => {
                push(&mut pts, x, y);
                cur = (x, y);
            }
            PathCmd::QuadTo { cx, cy, x, y } => {
                for i in 1..=12 {
                    let t = i as f32 / 12.0;
                    let q = 1.0 - t;
                    let px = q * q * cur.0 + 2.0 * q * t * cx + t * t * x;
                    let py = q * q * cur.1 + 2.0 * q * t * cy + t * t * y;
                    push(&mut pts, px, py);
                }
                cur = (x, y);
            }
            PathCmd::CubicTo {
                cx1,
                cy1,
                cx2,
                cy2,
                x,
                y,
            } => {
                for i in 1..=16 {
                    let t = i as f32 / 16.0;
                    let q = 1.0 - t;
                    let px = q * q * q * cur.0
                        + 3.0 * q * q * t * cx1
                        + 3.0 * q * t * t * cx2
                        + t * t * t * x;
                    let py = q * q * q * cur.1
                        + 3.0 * q * q * t * cy1
                        + 3.0 * q * t * t * cy2
                        + t * t * t * y;
                    push(&mut pts, px, py);
                }
                cur = (x, y);
            }
            PathCmd::Close => {
                push(&mut pts, start.0, start.1);
                if pts.len() > 1 {
                    out.push(std::mem::take(&mut pts));
                }
                cur = start;
            }
        }
    }
    if pts.len() > 1 {
        out.push(pts);
    }
    out
}

fn blend_px(out: &mut [u8], i: usize, argb: i32) {
    let a = (argb as u32) >> 24;
    if a == 0 {
        return;
    }
    let src = [(argb as u32) >> 16, (argb as u32) >> 8, argb as u32];
    if a == 255 {
        out[i] = src[0] as u8;
        out[i + 1] = src[1] as u8;
        out[i + 2] = src[2] as u8;
        out[i + 3] = 255;
        return;
    }
    let ia = 255 - a;
    for c in 0..3 {
        out[i + c] = ((src[c] * a + out[i + c] as u32 * ia) / 255) as u8;
    }
    out[i + 3] = (a + (out[i + 3] as u32 * ia) / 255) as u8;
}

fn raster_path_to_bitmap(
    vm: &mut Vm,
    dst_h: u32,
    cmds: &[PathCmd],
    fill_type: i32,
    paint: PaintState,
    m: [f32; 6],
) {
    let (bw, bh, base) = match vm.heap.get(dst_h) {
        Some(Obj::Host {
            state: HostState::Bitmap { w, h, rgba },
            ..
        }) => (*w, *h, rgba.clone()),
        _ => return,
    };
    if bw == 0 || bh == 0 || paint.style == 1 {
        return;
    }
    let contours = flatten_path_cmds(cmds, m);
    if contours.is_empty() {
        return;
    }
    let even_odd = fill_type % 2 == 1;
    let inverse = fill_type >= 2;
    let mut out = base.as_ref().clone();
    // Scanline rasterization: evaluate each edge once per row instead
    // of running the full point-in-polygon walk for every pixel. These
    // bitmaps are small, but Compose regenerates many of them during
    // startup, so the quadratic version dominates boot time.
    for yy in 0..bh {
        let yc = yy as f32 + 0.5;
        let mut xs: Vec<(f32, i32)> = Vec::new();
        for pts in &contours {
            for i in 0..pts.len() {
                let (x0, y0) = pts[i];
                let (x1, y1) = pts[(i + 1) % pts.len()];
                if (y0 <= yc) != (y1 <= yc) {
                    let xi = x0 + (yc - y0) * (x1 - x0) / (y1 - y0);
                    xs.push((xi, if y1 > y0 { 1 } else { -1 }));
                }
            }
        }
        xs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut at = 0usize;
        let mut parity = false;
        let mut winding = 0i32;
        for xx in 0..bw {
            let xc = xx as f32 + 0.5;
            while at < xs.len() && xs[at].0 <= xc {
                if even_odd {
                    parity = !parity;
                } else {
                    winding += xs[at].1;
                }
                at += 1;
            }
            let inside = if even_odd { parity } else { winding != 0 };
            if inside != inverse {
                blend_px(&mut out, ((yy * bw + xx) * 4) as usize, paint.argb);
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

/// Append an op to a recording canvas handle.
pub(crate) fn rec(vm: &mut Vm, canvas: V, op: CanvasOp) {
    let V::Ref(h) = canvas else { return };
    // A canvas bound to a Bitmap (`new Canvas(bitmap)`) composites
    // pixel ops into the bitmap itself — recording only would leave
    // the target blank (Drawable.toBitmap, Glide transforms).
    if let CanvasOp::Bitmap {
        x,
        y,
        w,
        h: dh,
        src_w,
        src_h,
        rgba,
    } = &op
    {
        if let Some(V::Ref(th)) = pseudo_get(vm, h, PK_TARGET) {
            if th != 0 {
                bitmap_blit(
                    vm,
                    th,
                    (*x, *y, *w as f32, *dh as f32),
                    (*src_w, *src_h),
                    rgba,
                );
            }
        }
    }
    if let CanvasOp::Path {
        cmds,
        fill_type,
        paint,
    } = &op
    {
        if let Some(V::Ref(th)) = pseudo_get(vm, h, PK_TARGET) {
            if th != 0 {
                raster_path_to_bitmap(vm, th, cmds, *fill_type, *paint, mat_read(vm, h));
            }
        }
    }
    // A dex Canvas subclass (Compose's `CanvasForAndroidGetter`/`Lsa3`)
    // delegates to the real canvas through a field — forward ops into
    // the delegate's recorded list, otherwise text paints vanish.
    let h = match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::CanvasOps(_),
            ..
        }) => h,
        Some(Obj::Instance { fields, .. }) => {
            match fields.values().find_map(|v| match v {
                V::Ref(dh) if *dh != 0 => match vm.heap.get(*dh) {
                    Some(Obj::Host {
                        state: HostState::CanvasOps(_),
                        ..
                    }) => Some(*dh),
                    _ => None,
                },
                _ => None,
            }) {
                Some(dh) => dh,
                None => return,
            }
        }
        _ => return,
    };
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
        // RenderNode display lists are implemented below, so expose
        // the ordinary accelerated Compose playback path.
        "isHardwareAccelerated" => Some(Some(V::I32(1))),
        "save" | "saveLayer" | "saveLayerAlpha" => {
            rec(vm, this, CanvasOp::Save);
            if let V::Ref(h) = this {
                canvas_mat_push(vm, h);
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
                canvas_mat_pop(vm, h);
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
            let (dx, dy) = (flt(arg(args, 1)), flt(arg(args, 2)));
            if let V::Ref(h) = this {
                let cur = mat_read(vm, h);
                mat_write(vm, h, mat_mul(cur, mat_op("Translate", dx, dy, 0.0, 0.0)));
            }
            rec(vm, this, CanvasOp::Translate { dx, dy });
            Some(None)
        }
        "scale" => {
            let (sx, sy) = (flt(arg(args, 1)), flt(arg(args, 2)));
            if let V::Ref(h) = this {
                let cur = mat_read(vm, h);
                mat_write(vm, h, mat_mul(cur, mat_op("Scale", sx, sy, 0.0, 0.0)));
            }
            Some(None)
        }
        "rotate" => {
            let deg = flt(arg(args, 1));
            if let V::Ref(h) = this {
                let cur = mat_read(vm, h);
                mat_write(vm, h, mat_mul(cur, mat_op("Rotate", deg, 0.0, 0.0, 0.0)));
            }
            Some(None)
        }
        "skew" => {
            let (sx, sy) = (flt(arg(args, 1)), flt(arg(args, 2)));
            if let V::Ref(h) = this {
                let cur = mat_read(vm, h);
                mat_write(vm, h, mat_mul(cur, mat_op("Skew", sx, sy, 0.0, 0.0)));
            }
            Some(None)
        }
        "concat" => {
            if let (V::Ref(h), V::Ref(mh)) = (this, arg(args, 1)) {
                let cur = mat_read(vm, h);
                mat_write(vm, h, mat_mul(cur, mat_read(vm, mh)));
            }
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
        "getClipBounds" => {
            // AOSP fills the Rect and returns false only on an empty
            // clip. Compose gates `Layout.draw` on this result
            // (`TextLayout.paint → getClipBounds`), so a stub `0`
            // hides every label: report the whole canvas rect.
            if let V::Ref(rh) = arg(args, 1) {
                let (w, h) = if let V::Ref(ch) = this {
                    (
                        i32v(pseudo_get(vm, ch, PK_CW).unwrap_or(V::I32(0))),
                        i32v(pseudo_get(vm, ch, PK_CH).unwrap_or(V::I32(0))),
                    )
                } else {
                    (0, 0)
                };
                super::android::rect_write(vm, rh, 0, 0, w, h);
            }
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
        "drawRenderNode" => {
            // Record a *reference* to the node, not a copy of its
            // current ops: AOSP display lists reference child render
            // nodes, so a re-recorded child layer shows through even
            // when the parent never re-records. `expand_nested`
            // resolves the reference (applying the node's position)
            // when ops leave the runtime.
            if let (V::Ref(_), V::Ref(rh)) = (this, arg(args, 1)) {
                if crate::dbg_flag("OVELO_RNDBG") {
                    static N: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 40 {
                        eprintln!("[rndbg] drawRenderNode node h{rh} (ref)");
                    }
                }
                rec(vm, this, CanvasOp::Nested { node: rh });
            }
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
        "drawPath" => {
            if let V::Ref(ph) = arg(args, 1) {
                if let Some((cmds, fill_type)) = path_clone(vm, ph) {
                    rec(
                        vm,
                        this,
                        CanvasOp::Path {
                            cmds,
                            fill_type,
                            paint: ps(vm, args),
                        },
                    );
                }
            }
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
        // concat/clipPath/drawPicture/drawVertices/drawPatch.
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
            // API 34+/Compose pass a packed long Color. For ordinary
            // sRGB colors the high 32 bits are exactly ARGB and the low
            // word is zero (`Color(int).value << 32`); keeping the long
            // itself made paint_state read the zero low word, so every
            // Compose shape painted transparent black.
            let c = match arg(args, 1) {
                V::I64(x) if x as u64 & 0xffff_ffff == 0 => ((x as u64) >> 32) as u32 as i32,
                v => i32v(v),
            };
            pseudo_set(vm, h, PK_COLOR, V::I32(c));
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
        | "setTextLocales"
        | "setTextLocale" => Some(None),
        // Read-side defaults for unmodelled paint state.
        "getFlags" => Some(Some(V::I32(0))),
        "getTextScaleX" => Some(Some(V::F32(1.0))),
        "getTextSkewX" => Some(Some(V::F32(0.0))),
        "getLetterSpacing" => Some(Some(V::F32(0.0))),
        "isAntiAlias" | "isDither" | "isFilterBitmap" | "isLinearText" | "isSubpixelText" => {
            Some(Some(V::I32(1)))
        }
        "isUnderlineText" | "isStrikeThruText" | "isFakeBoldText" | "isElegantTextHeight" => {
            Some(Some(V::I32(0)))
        }
        "getTypeface" => Some(Some(V::Ref(0))),
        "getStrokeMiter" => Some(Some(V::F32(1.0))),
        "getShader" | "getColorFilter" | "getMaskFilter" | "getPathEffect" | "getXfermode" => {
            Some(Some(V::Ref(0)))
        }
        // Compose's word-boundary selection calls `getTextLocale()` —
        // a null locale throws inside `BreakIterator`/`SelectionSegment`.
        "getTextLocale" => Some(Some(host(
            vm,
            "Ljava/util/Locale;",
            HostState::StrBuf("en-US".into()),
        ))),
        "getTextLocales" => Some(Some(host(vm, "Landroid/os/LocaleList;", HostState::Empty))),
        _ => None,
    })
}

/// Width approximation for `measureText`/`getTextBounds`: Latin ~
/// 0.55em, CJK ~1em — average enough for layout math. The real font
/// advance table lives in ovelo-ui's FontSet; guest code mostly needs
/// a plausible number for centering, not pixel-exact shaping.
pub(crate) fn measure_text(text: &str, size: f32) -> f32 {
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

/// Stand-in enum constant (`Paint.getTextAlign()` result) — the
/// interned mint so `==` against the static field and ordinal lookups
/// (`when` tables) both agree with values()/sget.
fn host_enum_const(vm: &mut Vm, desc: &str, name: &str) -> V {
    super::lang::enum_const(vm, desc, name)
}

// ---------------------------------------------------------------------
// android.graphics.Matrix — 2x3 affine tracked in pseudo fields.

fn mat_read(vm: &Vm, h: u32) -> [f32; 6] {
    let mut m = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    for (k, e) in m.iter_mut().enumerate() {
        if let Some(v) = pseudo_get(vm, h, PK_M0 + k as u32) {
            *e = f64v(v) as f32;
        }
    }
    m
}

fn mat_write(vm: &mut Vm, h: u32, m: [f32; 6]) {
    // Matrices are drawing implementation state, not view hierarchy
    // state; dirtying view_gen for every scale/translate makes Compose
    // rematerialize the tree continuously.
    if let Some(f) = fields_mut(vm, h) {
        for (k, e) in m.iter().enumerate() {
            f.insert(PK_M0 as u64 + k as u64, V::F32(*e));
        }
    }
}

/// `l × r` — applying `r` first then `l` to a point.
fn mat_mul(l: [f32; 6], r: [f32; 6]) -> [f32; 6] {
    [
        l[0] * r[0] + l[1] * r[3],
        l[0] * r[1] + l[1] * r[4],
        l[0] * r[2] + l[1] * r[5] + l[2],
        l[3] * r[0] + l[4] * r[3],
        l[3] * r[1] + l[4] * r[4],
        l[3] * r[2] + l[4] * r[5] + l[5],
    ]
}

fn mat_inv(m: [f32; 6]) -> Option<[f32; 6]> {
    let det = m[0] * m[4] - m[1] * m[3];
    if det.abs() < 1e-12 {
        return None;
    }
    let id = 1.0 / det;
    let (a, b, c, d, e, f) = (m[0], m[1], m[2], m[3], m[4], m[5]);
    Some([
        e * id,
        -b * id,
        (b * f - e * c) * id,
        -d * id,
        a * id,
        (d * c - a * f) * id,
    ])
}

/// Elementary op matrix for set/pre/post variants — `pivot` applies
/// as T(px,py) × op × T(-px,-py).
fn mat_op(kind: &str, a1: f32, a2: f32, px: f32, py: f32) -> [f32; 6] {
    let op = match kind {
        "Scale" => [a1, 0.0, 0.0, 0.0, a2, 0.0],
        "Translate" => [1.0, 0.0, a1, 0.0, 1.0, a2],
        "Rotate" => {
            let (s, c) = a1.to_radians().sin_cos();
            [c, -s, 0.0, s, c, 0.0]
        }
        "Skew" => [1.0, a1.tan(), 0.0, a2.tan(), 1.0, 0.0],
        _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
    };
    if px != 0.0 || py != 0.0 {
        mat_mul(
            mat_mul([1.0, 0.0, px, 0.0, 1.0, py], op),
            [1.0, 0.0, -px, 0.0, 1.0, -py],
        )
    } else {
        op
    }
}

/// Read a `float[]` arg at `args[i]` (offset `args[i+1]` when the sig
/// asks for one) into `n` slots.
fn mat_farr(vm: &Vm, v: &V, off: usize, n: usize) -> Vec<f32> {
    match v {
        V::Ref(h) => match vm.heap.get(*h) {
            Some(Obj::Array { data, .. }) => (0..n)
                .map(|k| data.get(off + k).map(|v| f64v(*v) as f32).unwrap_or(0.0))
                .collect(),
            _ => vec![0.0; n],
        },
        _ => vec![0.0; n],
    }
}

fn mat_write_arr(vm: &mut Vm, v: &V, off: usize, vals: &[f32]) {
    if let V::Ref(h) = v {
        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(*h) {
            for (k, f) in vals.iter().enumerate() {
                if let Some(e) = data.get_mut(off + k) {
                    *e = V::F32(*f);
                }
            }
        }
    }
}

/// RectF edges — canon `left/top/right/bottom` fields first (floats),
/// pseudo/Rect ints as fallback.
fn mat_rectf_edges(vm: &Vm, h: u32) -> (f32, f32, f32, f32) {
    let read = |name: &str| -> Option<f32> {
        let f = vm
            .field_index_of("Landroid/graphics/RectF;", name)
            .map(|g| vm.canon_field(g))?;
        match vm.heap.get(h) {
            Some(Obj::Instance { fields, .. } | Obj::Host { fields, .. }) => {
                fields.get(&f).map(|v| f64v(*v) as f32)
            }
            _ => None,
        }
    };
    (
        read("left").unwrap_or_else(|| android::rect_edges(vm, h).0 as f32),
        read("top").unwrap_or_else(|| android::rect_edges(vm, h).1 as f32),
        read("right").unwrap_or_else(|| android::rect_edges(vm, h).2 as f32),
        read("bottom").unwrap_or_else(|| android::rect_edges(vm, h).3 as f32),
    )
}

fn mat_rectf_write(vm: &mut Vm, h: u32, l: f32, t: f32, r: f32, b: f32) {
    for (name, v) in [("left", l), ("top", t), ("right", r), ("bottom", b)] {
        if let Some(f) = vm
            .field_index_of("Landroid/graphics/RectF;", name)
            .map(|g| vm.canon_field(g))
        {
            if let Some(Obj::Instance { fields, .. } | Obj::Host { fields, .. }) =
                vm.heap.get_mut(h)
            {
                fields.insert(f, V::F32(v));
            }
        }
    }
}

fn matrix_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    Ok(match name {
        "<init>" => {
            // <init>() or <init>(Matrix).
            let m = match args.get(1) {
                Some(&V::Ref(src)) => mat_read(vm, src),
                _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            };
            mat_write(vm, this, m);
            Some(None)
        }
        "set" | "setConcat" => {
            let m = match args.get(1) {
                Some(&V::Ref(src)) => mat_read(vm, src),
                _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            };
            mat_write(vm, this, m);
            Some(None)
        }
        "reset" => {
            mat_write(vm, this, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
            Some(None)
        }
        n if n.starts_with("set") || n.starts_with("post") || n.starts_with("pre") => {
            // (set|pre|post)(Scale|Translate|Rotate|Skew|SinCos|Concat|...)
            let mode = if n.starts_with("set") {
                "set"
            } else if n.starts_with("pre") {
                "pre"
            } else {
                "post"
            };
            let kind = n.trim_start_matches(mode);
            if kind == "Concat" {
                // postConcat(a,b) = a ∘ b; preConcat(a,b) = b ∘ a.
                let a = args.get(1).map(|v| match v {
                    V::Ref(h) => mat_read(vm, *h),
                    _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
                });
                let b = args.get(2).map(|v| match v {
                    V::Ref(h) => mat_read(vm, *h),
                    _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
                });
                if let (Some(l), Some(r)) = (a, b) {
                    let m = if mode == "post" {
                        mat_mul(l, r)
                    } else {
                        mat_mul(r, l)
                    };
                    mat_write(vm, this, m);
                } else {
                    // (post|pre)Concat(Matrix) — one arg.
                    let o = args.get(1).map(|v| match v {
                        V::Ref(h) => mat_read(vm, *h),
                        _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
                    });
                    let cur = mat_read(vm, this);
                    let m = if mode == "post" {
                        mat_mul(cur, o.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]))
                    } else {
                        mat_mul(o.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]), cur)
                    };
                    mat_write(vm, this, m);
                }
            } else {
                // pivot variants carry two extra floats.
                let nfloats = args[1..]
                    .iter()
                    .take_while(|v| matches!(v, V::F32(_) | V::F64(_) | V::I32(_) | V::I64(_)))
                    .count();
                let (a1, a2, px, py) = (
                    flt(arg(args, 1)),
                    flt(arg(args, 2)),
                    if nfloats >= 4 { flt(arg(args, 3)) } else { 0.0 },
                    if nfloats >= 4 { flt(arg(args, 4)) } else { 0.0 },
                );
                let kind = if kind == "SinCos" { "Rotate" } else { kind };
                let op = if kind == "Rotate" && n == "setSinCos" || kind == "SinCos" {
                    // setSinCos(sinValue, cosValue[, px, py])
                    let (s, c) = (a1, a2);
                    let m = [c, -s, 0.0, s, c, 0.0];
                    if px != 0.0 || py != 0.0 {
                        mat_mul(
                            mat_mul([1.0, 0.0, px, 0.0, 1.0, py], m),
                            [1.0, 0.0, -px, 0.0, 1.0, -py],
                        )
                    } else {
                        m
                    }
                } else {
                    mat_op(kind, a1, a2, px, py)
                };
                let cur = mat_read(vm, this);
                let m = match mode {
                    "set" => op,
                    "pre" => mat_mul(op, cur),
                    _ => mat_mul(cur, op),
                };
                mat_write(vm, this, m);
            }
            Some(Some(V::I32(1)))
        }
        "invert" => {
            let cur = mat_read(vm, this);
            let inv = mat_inv(cur);
            match (inv, args.get(1)) {
                (Some(m), Some(&V::Ref(dst))) => {
                    mat_write(vm, dst, m);
                    Some(Some(V::I32(1)))
                }
                _ => Some(Some(V::I32(0))),
            }
        }
        "isIdentity" => {
            let m = mat_read(vm, this);
            let id = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
            Some(Some(V::I32(
                m.iter().zip(id).all(|(a, b)| (a - b).abs() < 1e-6) as i32,
            )))
        }
        "hasPerspective" => Some(Some(V::I32(0))),
        "rectStaysRect" => {
            let m = mat_read(vm, this);
            Some(Some(V::I32(
                (m[1].abs() < 1e-6 && m[3].abs() < 1e-6) as i32,
            )))
        }
        "getValues" => {
            let m = mat_read(vm, this);
            let vals = [m[0], m[1], m[2], m[3], m[4], m[5], 0.0, 0.0, 1.0];
            mat_write_arr(vm, &arg(args, 1), 0, &vals);
            Some(None)
        }
        "setValues" => {
            let vals = mat_farr(vm, &arg(args, 1), 0, 9);
            mat_write(
                vm,
                this,
                [vals[0], vals[1], vals[2], vals[3], vals[4], vals[5]],
            );
            Some(None)
        }
        n if n.starts_with("map") => {
            let m = mat_read(vm, this);
            let pmap = |x: f32, y: f32, v: bool| -> (f32, f32) {
                if v {
                    (m[0] * x + m[1] * y, m[3] * x + m[4] * y)
                } else {
                    (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
                }
            };
            let vectors = n == "mapVectors" || n == "mapRadius";
            match n {
                "mapPoints" | "mapVectors" => {
                    // mapPoints(dst[,dstOff],src[,srcOff],count).
                    let (dst, doff, src, soff, cnt) = if args.len() >= 5 {
                        (
                            arg(args, 1),
                            i32v(arg(args, 2)).max(0) as usize,
                            arg(args, 3),
                            i32v(arg(args, 4)).max(0) as usize,
                            i32v(arg(args, 5)).max(0) as usize,
                        )
                    } else {
                        (
                            arg(args, 1),
                            0,
                            arg(args, 2),
                            0,
                            i32v(arg(args, 3)).max(0) as usize,
                        )
                    };
                    let srcv = mat_farr(vm, &src, soff, cnt * 2);
                    let mut out = vec![0.0f32; cnt * 2];
                    for k in 0..cnt {
                        let (x, y) = pmap(srcv[2 * k], srcv[2 * k + 1], vectors);
                        out[2 * k] = x;
                        out[2 * k + 1] = y;
                    }
                    mat_write_arr(vm, &dst, doff, &out);
                }
                "mapRect" => {
                    // mapRect(dst,src) / mapRect(rect) / mapRect(dst,l,t,r,b).
                    let (l, t, r, b) = if args.len() >= 5 {
                        (
                            flt(arg(args, 2)),
                            flt(arg(args, 3)),
                            flt(arg(args, 4)),
                            flt(arg(args, 5)),
                        )
                    } else {
                        let srcv = args
                            .get(if args.len() >= 3 { 2 } else { 1 })
                            .map(|v| match v {
                                V::Ref(h) => mat_rectf_edges(vm, *h),
                                _ => (0.0, 0.0, 0.0, 0.0),
                            })
                            .unwrap_or((0.0, 0.0, 0.0, 0.0));
                        srcv
                    };
                    let mut out = [
                        pmap(l, t, false),
                        pmap(r, t, false),
                        pmap(r, b, false),
                        pmap(l, b, false),
                    ];
                    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                    let (xl, xr) = (out[0].0, out[3].0);
                    let mut ys: Vec<f32> = out.iter().map(|p| p.1).collect();
                    ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let (yt, yb) = (ys[0], ys[3]);
                    let dst = arg(args, 1);
                    if let V::Ref(h) = dst {
                        mat_rectf_write(vm, h, xl, yt, xr, yb);
                    }
                    let (nl, nt, nr, nb) = (xl, yt, xr, yb);
                    return Ok(Some(Some(V::I32((nl < nr && nt < nb) as i32))));
                }
                "mapRadius" => {
                    let r = flt(arg(args, 1));
                    // average of mapped unit vectors.
                    let (ux, uy) = pmap(1.0, 0.0, true);
                    let (vx, vy) = pmap(0.0, 1.0, true);
                    let len = ((ux * ux + uy * uy).sqrt() + (vx * vx + vy * vy).sqrt()) / 2.0;
                    return Ok(Some(Some(V::F32(r * len))));
                }
                _ => {}
            }
            Some(None)
        }
        "equals" | "hashCode" => {
            let eq = match args.get(1) {
                Some(&V::Ref(o)) => {
                    let (a, b) = (mat_read(vm, this), mat_read(vm, o));
                    a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-6)
                }
                _ => false,
            };
            if name == "hashCode" {
                let m = mat_read(vm, this);
                Some(Some(V::I32(m.iter().map(|f| f.to_bits() as i32).sum())))
            } else {
                Some(Some(V::I32(eq as i32)))
            }
        }
        "toString" | "toShortString" => {
            let m = mat_read(vm, this);
            Some(Some(jstr(
                vm,
                &format!(
                    "Matrix{{[{:.3},{:.3},{:.3}][{:.3},{:.3},{:.3}][0,0,1]}}",
                    m[0], m[1], m[2], m[3], m[4], m[5]
                ),
            )))
        }
        "setPolyToPoly" | "setRectToRect" | "setScaleToFit" => Some(Some(V::I32(1))),
        _ => return Ok(None),
    })
}

fn path_positions(cmds: &[PathCmd]) -> ((f32, f32), (f32, f32)) {
    let mut cur = (0.0, 0.0);
    let mut start = (0.0, 0.0);
    for c in cmds {
        match *c {
            PathCmd::MoveTo { x, y } => {
                cur = (x, y);
                start = cur;
            }
            PathCmd::LineTo { x, y } => cur = (x, y),
            PathCmd::QuadTo { x, y, .. } => cur = (x, y),
            PathCmd::CubicTo { x, y, .. } => cur = (x, y),
            PathCmd::Close => cur = start,
        }
    }
    (cur, start)
}

fn with_path<T>(vm: &mut Vm, h: u32, f: impl FnOnce(&mut Vec<PathCmd>) -> T) -> Option<T> {
    match vm.heap.get_mut(h) {
        Some(Obj::Host {
            state: HostState::GraphicsPath(cmds),
            ..
        }) => Some(f(cmds)),
        _ => None,
    }
}

fn path_clone(vm: &Vm, h: u32) -> Option<(Vec<PathCmd>, i32)> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::GraphicsPath(cmds),
            ..
        }) => Some((
            cmds.clone(),
            match pseudo_get(vm, h, PK_FILLORD) {
                Some(v) => i32v(v),
                _ => 0,
            },
        )),
        _ => None,
    }
}

/// `android.graphics.Path` command recording. Compose ImageVector
/// rendering uses only the move/line/curve/close subset; shapes are
/// flattened at replay time.
fn path_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = arg(args, 0) else {
        return Ok(Some(None));
    };
    let rel_point = |cmds: &[PathCmd], dx: f32, dy: f32| {
        let ((x, y), _) = path_positions(cmds);
        (x + dx, y + dy)
    };
    Ok(match name {
        "<init>" => {
            if let Some(V::Ref(src)) = args
                .get(1)
                .copied()
                .filter(|v| matches!(v, V::Ref(h) if *h != 0))
            {
                let copied = path_clone(vm, src).map(|x| x.0);
                if let Some(cmds) = copied {
                    with_path(vm, h, |dst| *dst = cmds);
                }
            }
            Some(None)
        }
        "moveTo" | "rMoveTo" => {
            with_path(vm, h, |cmds| {
                let (x, y) = if name.starts_with('r') {
                    rel_point(cmds, flt(arg(args, 1)), flt(arg(args, 2)))
                } else {
                    (flt(arg(args, 1)), flt(arg(args, 2)))
                };
                cmds.push(PathCmd::MoveTo { x, y });
            });
            Some(None)
        }
        "lineTo" | "rLineTo" => {
            with_path(vm, h, |cmds| {
                let (x, y) = if name.starts_with('r') {
                    rel_point(cmds, flt(arg(args, 1)), flt(arg(args, 2)))
                } else {
                    (flt(arg(args, 1)), flt(arg(args, 2)))
                };
                cmds.push(PathCmd::LineTo { x, y });
            });
            Some(None)
        }
        "quadTo" | "rQuadTo" => {
            with_path(vm, h, |cmds| {
                let ((px, py), _) = path_positions(cmds);
                let (mut cx, mut cy, mut x, mut y) = (
                    flt(arg(args, 1)),
                    flt(arg(args, 2)),
                    flt(arg(args, 3)),
                    flt(arg(args, 4)),
                );
                if name.starts_with('r') {
                    cx += px;
                    cy += py;
                    x += px;
                    y += py;
                }
                cmds.push(PathCmd::QuadTo { cx, cy, x, y });
            });
            Some(None)
        }
        "cubicTo" | "rCubicTo" => {
            with_path(vm, h, |cmds| {
                let ((px, py), _) = path_positions(cmds);
                let (mut cx1, mut cy1, mut cx2, mut cy2, mut x, mut y) = (
                    flt(arg(args, 1)),
                    flt(arg(args, 2)),
                    flt(arg(args, 3)),
                    flt(arg(args, 4)),
                    flt(arg(args, 5)),
                    flt(arg(args, 6)),
                );
                if name.starts_with('r') {
                    cx1 += px;
                    cy1 += py;
                    cx2 += px;
                    cy2 += py;
                    x += px;
                    y += py;
                }
                cmds.push(PathCmd::CubicTo {
                    cx1,
                    cy1,
                    cx2,
                    cy2,
                    x,
                    y,
                });
            });
            Some(None)
        }
        "close" => {
            with_path(vm, h, |cmds| cmds.push(PathCmd::Close));
            Some(None)
        }
        "reset" | "rewind" => {
            with_path(vm, h, |cmds| cmds.clear());
            Some(None)
        }
        "set" => {
            if let V::Ref(src) = arg(args, 1) {
                if let Some((cmds, _)) = path_clone(vm, src) {
                    with_path(vm, h, |dst| *dst = cmds);
                }
            }
            Some(None)
        }
        "isEmpty" => Some(Some(V::I32(
            path_clone(vm, h).is_none_or(|(cmds, _)| cmds.is_empty()) as i32,
        ))),
        "setFillType" => {
            let ft = arg(args, 1);
            let ord = match enum_name(vm, ft).as_deref() {
                Some("EVEN_ODD") => 1,
                Some("INVERSE_WINDING") => 2,
                Some("INVERSE_EVEN_ODD") => 3,
                _ => 0,
            };
            pseudo_set(vm, h, PK_FILLTYPE, ft);
            pseudo_set(vm, h, PK_FILLORD, V::I32(ord));
            Some(None)
        }
        "getFillType" => Some(Some(pseudo_get(vm, h, PK_FILLTYPE).unwrap_or(V::Ref(0)))),
        "addCircle" => {
            let (cx, cy, r) = (flt(arg(args, 1)), flt(arg(args, 2)), flt(arg(args, 3)));
            if r > 0.0 {
                let k = 0.552_284_8 * r;
                with_path(vm, h, |cmds| {
                    cmds.push(PathCmd::MoveTo { x: cx + r, y: cy });
                    cmds.push(PathCmd::CubicTo {
                        cx1: cx + r,
                        cy1: cy + k,
                        cx2: cx + k,
                        cy2: cy + r,
                        x: cx,
                        y: cy + r,
                    });
                    cmds.push(PathCmd::CubicTo {
                        cx1: cx - k,
                        cy1: cy + r,
                        cx2: cx - r,
                        cy2: cy + k,
                        x: cx - r,
                        y: cy,
                    });
                    cmds.push(PathCmd::CubicTo {
                        cx1: cx - r,
                        cy1: cy - k,
                        cx2: cx - k,
                        cy2: cy - r,
                        x: cx,
                        y: cy - r,
                    });
                    cmds.push(PathCmd::CubicTo {
                        cx1: cx + k,
                        cy1: cy - r,
                        cx2: cx + r,
                        cy2: cy - k,
                        x: cx + r,
                        y: cy,
                    });
                    cmds.push(PathCmd::Close);
                });
            }
            Some(None)
        }
        "addRect" => {
            let (l, t, r, b) = (
                flt(arg(args, 1)),
                flt(arg(args, 2)),
                flt(arg(args, 3)),
                flt(arg(args, 4)),
            );
            with_path(vm, h, |cmds| {
                cmds.push(PathCmd::MoveTo { x: l, y: t });
                cmds.push(PathCmd::LineTo { x: r, y: t });
                cmds.push(PathCmd::LineTo { x: r, y: b });
                cmds.push(PathCmd::LineTo { x: l, y: b });
                cmds.push(PathCmd::Close);
            });
            Some(None)
        }
        _ => return Ok(None),
    })
}

/// `android.graphics.RenderNode` display-list shell. Recording draws
/// into a host Canvas and drawRenderNode splices those recorded ops
/// into the destination canvas at the point of playback.
fn rendernode_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "<init>" => Some(None),
        // android.view.RenderNode factory (Compose's GraphicsLayerV23)
        "create" => Some(Some(host(
            vm,
            "Landroid/view/RenderNode;",
            HostState::Empty,
        ))),
        // android.graphics.RenderNode uses begin/endRecording; the
        // older android.view API uses start/end and names the recording
        // surface DisplayListCanvas. Both record the same CanvasOps.
        "beginRecording" | "start" => {
            let V::Ref(rh) = arg(args, 0) else {
                return Ok(Some(Some(V::Ref(0))));
            };
            let canvas_desc = if name == "start" {
                "Landroid/view/DisplayListCanvas;"
            } else {
                "Landroid/graphics/RecordingCanvas;"
            };
            let V::Ref(ch) = host(vm, canvas_desc, HostState::CanvasOps(Vec::new())) else {
                return Ok(Some(Some(V::Ref(0))));
            };
            pseudo_set(vm, ch, PK_CW, arg(args, 1));
            pseudo_set(vm, ch, PK_CH, arg(args, 2));
            pseudo_set(vm, rh, PK_RECCANVAS, V::Ref(ch));
            Some(Some(V::Ref(ch)))
        }
        "endRecording" | "endRecordingAndDisplayList" | "end" => Some(None),
        "hasDisplayList" | "isValid" => {
            // discardDisplayList drops the recording — answering
            // "valid" here would let Compose skip the re-record and
            // the layer would draw nothing.
            let valid = match arg(args, 0) {
                V::Ref(rh) => {
                    matches!(pseudo_get(vm, rh, PK_RECCANVAS), Some(V::Ref(ch)) if ch != 0)
                }
                _ => false,
            };
            Some(Some(V::I32(valid as i32)))
        }
        "discardDisplayList" | "clearDisplayList" => {
            if let V::Ref(rh) = arg(args, 0) {
                pseudo_set(vm, rh, PK_RECCANVAS, V::Ref(0));
            }
            Some(None)
        }
        // Placement: Compose positions layer render nodes through
        // these — the origin must survive to playback or every layer
        // draws at its parent's origin.
        "setPosition" | "setLeftTopRightBottom" => {
            if let V::Ref(rh) = arg(args, 0) {
                // (IIII)V carries the edges directly; the (Rect)V
                // overload reads them off the Rect object.
                let (l, t) = if args.len() >= 5 {
                    (arg(args, 1), arg(args, 2))
                } else if let Some(e) = rect_edges_of(vm, arg(args, 1)) {
                    (V::F32(e.0), V::F32(e.1))
                } else {
                    (V::I32(0), V::I32(0))
                };
                pseudo_set(vm, rh, PK_RN_LEFT, V::I32(i32v(l)));
                pseudo_set(vm, rh, PK_RN_TOP, V::I32(i32v(t)));
            }
            Some(Some(V::I32(1)))
        }
        "setLeft" => {
            if let V::Ref(rh) = arg(args, 0) {
                pseudo_set(vm, rh, PK_RN_LEFT, arg(args, 1));
            }
            Some(Some(V::I32(1)))
        }
        "setTop" => {
            if let V::Ref(rh) = arg(args, 0) {
                pseudo_set(vm, rh, PK_RN_TOP, arg(args, 1));
            }
            Some(Some(V::I32(1)))
        }
        "offsetLeftAndRight" => {
            if let V::Ref(rh) = arg(args, 0) {
                let cur = pseudo_get(vm, rh, PK_RN_LEFT).unwrap_or(V::I32(0));
                pseudo_set(vm, rh, PK_RN_LEFT, V::I32(i32v(cur) + i32v(arg(args, 1))));
            }
            Some(Some(V::I32(1)))
        }
        "offsetTopAndBottom" => {
            if let V::Ref(rh) = arg(args, 0) {
                let cur = pseudo_get(vm, rh, PK_RN_TOP).unwrap_or(V::I32(0));
                pseudo_set(vm, rh, PK_RN_TOP, V::I32(i32v(cur) + i32v(arg(args, 1))));
            }
            Some(Some(V::I32(1)))
        }
        // Remaining transform/outline/alpha mutations are visual
        // refinements we do not yet model.
        "getScaleX" | "getScaleY" | "getAlpha" => Some(Some(V::F32(1.0))),
        "getTranslationX" | "getTranslationY" | "getTranslationZ" | "getElevation"
        | "getRotation" | "getRotationX" | "getRotationY" | "getPivotX" | "getPivotY" => {
            Some(Some(V::F32(0.0)))
        }
        "getCameraDistance" => Some(Some(V::F32(1280.0))),
        "getClipToBounds" | "getClipToOutline" | "hasOverlappingRendering" => Some(Some(V::I32(0))),
        "setRight"
        | "setBottom"
        | "setAlpha"
        | "setTranslationX"
        | "setTranslationY"
        | "setTranslationZ"
        | "setElevation"
        | "setRotation"
        | "setRotationX"
        | "setRotationY"
        | "setScaleX"
        | "setScaleY"
        | "setPivotX"
        | "setPivotY"
        | "setCameraDistance"
        | "setClipToBounds"
        | "setClipToOutline"
        | "setOutline"
        | "setRenderEffect"
        | "setUseCompositingLayer"
        | "setLayerType"
        | "setHasOverlappingRendering" => Some(Some(V::I32(1))),
        _ => return Ok(None),
    })
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
        "Landroid/graphics/Canvas;"
        | "Landroid/graphics/RecordingCanvas;"
        | "Landroid/view/DisplayListCanvas;" => canvas_call(vm, name, sig, args),
        "Landroid/graphics/Path;" => path_call(vm, name, args),
        "Landroid/graphics/RenderNode;" | "Landroid/view/RenderNode;" => {
            rendernode_call(vm, name, args)
        }
        "Landroid/graphics/Paint;" | "Landroid/text/TextPaint;" => paint_call(vm, name, sig, args),
        "Landroid/graphics/Matrix;" => matrix_call(vm, name, sig, args),
        _ => Ok(None),
    }
}

/// Run a guest `View.onDraw(Canvas)` over a recording canvas sized to
/// the view's laid-out frame; returns the recorded ops (empty when the
/// class doesn't override `onDraw` or the call fails — a framework
/// `View.onDraw` is a no-op anyway).
/// OVELO_NODEDUMP: recursively walk the dex LayoutNode tree hanging
/// off an AndroidComposeView (`root` → `_unfoldedChildren` vectors)
/// so we can see what composition actually emitted.
fn node_tree_dump(vm: &mut Vm, node: u32, depth: usize) {
    if depth > 12 || node == 0 {
        return;
    }
    let cls = super::class_of(vm, V::Ref(node));
    // `_unfoldedChildren` is null while folded — `get_children$ui()`
    // is the accessor the layout code itself uses.
    let kids = vm
        .call_virtual_sig(node, "get_children$ui", &[], &[])
        .ok()
        .flatten();
    if depth == 0 {
        eprintln!("[nodedump] root kids={kids:?}");
    }
    let (n, arr) = match kids {
        Some(V::Ref(mv)) => {
            let sz = match vm.get_object_field(
                mv,
                "Landroidx/compose/runtime/collection/MutableVector;",
                "size",
            ) {
                Some(V::I32(s)) => s,
                _ => -1,
            };
            let a = match vm.get_object_field(
                mv,
                "Landroidx/compose/runtime/collection/MutableVector;",
                "content",
            ) {
                Some(V::Ref(a)) => a,
                _ => 0,
            };
            (sz, a)
        }
        _ => (-1, 0),
    };
    let (mut mw, mut mh) = (None, None);
    if let Some(V::Ref(coord)) = vm
        .call_virtual_sig(node, "getOuterCoordinator", &[], &[])
        .ok()
        .flatten()
    {
        mw = vm
            .call_virtual_sig(coord, "getMeasuredWidth", &[], &[])
            .ok()
            .flatten();
        mh = vm
            .call_virtual_sig(coord, "getMeasuredHeight", &[], &[])
            .ok()
            .flatten();
    }
    eprintln!(
        "[nodedump] {:indent$}h{node} {cls} children={n} measured={mw:?}x{mh:?}",
        "",
        indent = depth * 2
    );
    let kids_vec: Vec<u32> = match vm.heap.get(arr) {
        Some(Obj::Array { data, .. }) => data
            .iter()
            .take(n.max(0) as usize)
            .filter_map(|v| match v {
                V::Ref(k) => Some(*k),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for k in kids_vec {
        node_tree_dump(vm, k, depth + 1);
    }
}

/// OVELO_NODEDUMP entry point — call from the pump path even when the
/// view measured 0×0 (that is exactly the failure this debugs).
pub(crate) fn dump_node_tree(vm: &mut Vm, view: u32) {
    if crate::dbg_flag("OVELO_NODEDUMP") {
        if let Some(V::Ref(root)) = vm.get_object_field(
            view,
            "Landroidx/compose/ui/platform/AndroidComposeView;",
            "root",
        ) {
            // The depth-0 tail of call_virtual_sig drains queued guest
            // callbacks — firing them mid-borrow of the UI bridge
            // panics. Mark the VM as already draining so the walk is
            // purely diagnostic.
            let was = vm.draining;
            vm.draining = true;
            node_tree_dump(vm, root, 0);
            vm.draining = was;
        }
    }
}

pub(crate) fn guest_draw_ops(vm: &mut Vm, view: u32, w: i32, h: i32) -> Vec<CanvasOp> {
    if crate::dbg_flag("OVELO_NODEDUMP") {
        if let Some(V::Ref(root)) = vm.get_object_field(
            view,
            "Landroidx/compose/ui/platform/AndroidComposeView;",
            "root",
        ) {
            let was = vm.draining;
            vm.draining = true;
            node_tree_dump(vm, root, 0);
            vm.draining = was;
        }
    }
    // GLSurfaceView subclasses draw through a Renderer, not onDraw —
    // drive the GL10 path and flatten its recorded primitives.
    if pseudo_get(vm, view, super::gl::P_GLES_RENDERER).is_some() {
        let ops = super::gl::gl_frame(vm, view, w, h);
        if !ops.is_empty() {
            vm.first_guest_draw = true;
        }
        return ops;
    }
    // A native-renderer surface (Godot's GLThread) swaps into the
    // readback stash — present those pixels on the surface's view.
    if let Some((px, sw, sh)) = super::gl::swap_frame() {
        let n = super::class_of(vm, V::Ref(view));
        let glish =
            n.contains("GLSurfaceView") || n.contains("GLRenderView") || n.contains("SurfaceView");
        if glish {
            vm.first_guest_draw = true;
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
        if let Err(e) = &r {
            if crate::dbg_flag("OVELO_DRAWDBG") {
                eprintln!("[drawdbg] onDraw h{view} ERR {e}");
            }
        }
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
        if let Err(e) = &r2 {
            if crate::dbg_flag("OVELO_DRAWDBG") {
                eprintln!("[drawdbg] dispatchDraw h{view} ERR {e}");
            }
        }
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
            // Compose redraws the same view every frame — cap dumps or
            // the diagnostic itself starves the interpreter.
            static DUMPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if DUMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3 {
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
    }
    let out = match vm.heap.get(canvas) {
        Some(Obj::Host {
            state: HostState::CanvasOps(ops),
            ..
        }) => expand_nested(vm, &ops.clone(), &mut Vec::new()),
        _ => Vec::new(),
    };
    if !out.is_empty() {
        vm.first_guest_draw = true;
    }
    out
}

/// Flatten `CanvasOp::Nested` references against the live heap: the
/// referenced node's *current* recording is spliced in at its
/// `setPosition` origin, so re-recording a child layer shows through
/// without the parent re-recording (AOSP display-list semantics).
/// `path` carries the expansion chain — a node already on it means a
/// recording cycle, whose recursion is cut; legitimate deep nesting
/// expands fully.
fn expand_nested(vm: &Vm, ops: &[CanvasOp], path: &mut Vec<u32>) -> Vec<CanvasOp> {
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let CanvasOp::Nested { node } = op else {
            out.push(op.clone());
            continue;
        };
        if path.contains(node) {
            continue;
        }
        let inner = match pseudo_get(vm, *node, PK_RECCANVAS) {
            Some(V::Ref(ch)) if ch != 0 => match vm.heap.get(ch) {
                Some(Obj::Host {
                    state: HostState::CanvasOps(inner),
                    ..
                }) => inner.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        if inner.is_empty() {
            continue;
        }
        let dx = i32v(pseudo_get(vm, *node, PK_RN_LEFT).unwrap_or(V::I32(0))) as f32;
        let dy = i32v(pseudo_get(vm, *node, PK_RN_TOP).unwrap_or(V::I32(0))) as f32;
        out.push(CanvasOp::Save);
        if dx != 0.0 || dy != 0.0 {
            out.push(CanvasOp::Translate { dx, dy });
        }
        path.push(*node);
        out.extend(expand_nested(vm, &inner, path));
        path.pop();
        out.push(CanvasOp::Restore);
    }
    out
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
