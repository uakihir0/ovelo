//! Minimal `<vector>` drawable rasterizer.
//!
//! Real APKs ship most icons as `<vector>` drawables in binary AXML.
//! `BitmapFactory.decodeResource` and `Resources.getDrawable` need actual
//! pixels so `ImageView`/`BitmapDrawable`/`setImage*` paths see a bitmap:
//! we parse `viewportWidth`/`viewportHeight`, the `<path>` `pathData`
//! mini-language and fill/stroke colors, then rasterize into RGBA8 with
//! scanline coverage (nonzero winding) at 4x supersampling.

use std::rc::Rc;

use ovelo_apk::manifest::{Element, Value};

use crate::res::ApkResources;
use crate::value::{HostState, V};
use crate::Vm;

use super::{host, pseudo_set, P_BITMAP_SLOT, P_DRAW_COLOR};

/// Resources + theme the decode/color helpers consume — a `Vm` is not
/// needed, so foreign APK resources (installed-app icons) go through
/// the same path.
#[derive(Clone, Copy)]
pub(crate) struct RCtx<'a> {
    pub res: &'a ApkResources,
    pub theme: u32,
    /// Drawable-graph recursion depth — a drawable reference cycle
    /// (an adaptive-icon layer pointing back at itself) must not
    /// recurse unboundedly: the launcher decodes foreign APK drawables,
    /// so a malformed icon would otherwise stack-overflow the host.
    pub depth: u8,
}

/// `RCtx` over the Vm's own bound resources, if any.
pub(crate) fn rctx(vm: &Vm) -> Option<RCtx<'_>> {
    Some(RCtx {
        res: vm.apk_res.as_ref()?,
        theme: vm.theme_id,
        depth: 0,
    })
}

/// Cached rasterization result: (w, h, rgba) or None when not drawable.
type CachedDrawable = Option<(u32, u32, Rc<Vec<u8>>)>;

thread_local! {
    /// (res_id, theme_id, package) → rendered pixels (None = not an
    /// image we can make). Colors resolve through the current theme and
    /// res-ids repeat across packages, so both must be in the key.
    static DRAW_CACHE: std::cell::RefCell<
        hashbrown::HashMap<(u32, u32, String), CachedDrawable>,
    > = std::cell::RefCell::new(hashbrown::HashMap::new());
}

/// Pixels for a `res/drawable` id: `<vector>` rasterized,
/// `<adaptive-icon>` composited, or PNG/JPEG/WebP decoded. `None` for
/// non-image resources.
pub(crate) fn drawable_bitmap(rv: RCtx<'_>, res_id: u32) -> Option<(u32, u32, Rc<Vec<u8>>)> {
    if rv.depth >= 16 {
        return None;
    }
    let key = (
        res_id,
        rv.theme,
        rv.res.package_name().unwrap_or_default().to_owned(),
    );
    if let Some(hit) = DRAW_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let out = render_vector(rv, res_id)
        .or_else(|| render_adaptive(rv, res_id))
        .or_else(|| render_layer_list(rv, res_id))
        .or_else(|| {
            let bytes = rv.res.file_path(res_id).and_then(|p| rv.res.read(p))?;
            let img = image::load_from_memory(&bytes).ok()?;
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            Some((w, h, Rc::new(rgba.into_raw())))
        });
    DRAW_CACHE.with(|c| c.borrow_mut().insert(key, out.clone()));
    out
}

/// Flat composite of an `<adaptive-icon>` doc: background stretched
/// over the canvas, foreground centered at the spec's inner size.
/// Returns `None` when the doc isn't an adaptive icon.
fn render_adaptive(rv: RCtx<'_>, res_id: u32) -> Option<(u32, u32, Rc<Vec<u8>>)> {
    let els = rv.res.document(res_id)?;
    els.iter().find(|e| e.name == "adaptive-icon")?;
    const SZ: usize = 96;
    let mut canvas = vec![0u8; SZ * SZ * 4];
    for layer in ["background", "foreground"] {
        let el = els.iter().find(|e| e.name == layer)?;
        let Some(Value::Reference(r)) = el.android("drawable") else {
            continue;
        };
        let (w, h, px) = drawable_bitmap(
            RCtx {
                depth: rv.depth + 1,
                ..rv
            },
            *r,
        )
        .or_else(|| {
            // `<color>` resource → solid tile.
            resolve_ref_color(rv, *r).map(|argb| {
                let p = argb.to_be_bytes();
                let mut v = Vec::with_capacity(4);
                v.extend_from_slice(&p);
                (1u32, 1u32, Rc::new(v))
            })
        })?;
        let (tw, th, ox, oy) = if layer == "foreground" {
            // Adaptive spec: 108dp canvas, inner icon ≈72dp centered.
            let t = (SZ as f32 * 72.0 / 108.0).round() as usize;
            (t, t, (SZ - t) / 2, (SZ - t) / 2)
        } else {
            (SZ, SZ, 0, 0)
        };
        blit_scaled(&mut canvas, SZ, &px, w as usize, h as usize, ox, oy, tw, th);
    }
    Some((SZ as u32, SZ as u32, Rc::new(canvas)))
}

/// Nearest-neighbor blit of `src` into `canvas` at (ox,oy) scaled to tw×th.
#[allow(clippy::too_many_arguments)]
fn blit_scaled(
    canvas: &mut [u8],
    cw: usize,
    src: &[u8],
    sw: usize,
    sh: usize,
    ox: usize,
    oy: usize,
    tw: usize,
    th: usize,
) {
    if sw == 0 || sh == 0 || tw == 0 || th == 0 {
        return;
    }
    for dy in 0..th {
        for dx in 0..tw {
            let sx = dx * sw / tw;
            let sy = dy * sh / th;
            let si = (sy * sw + sx) * 4;
            let a = src[si + 3] as u32;
            if a == 0 {
                continue;
            }
            let dx_ = ox + dx;
            let dy_ = oy + dy;
            let di = (dy_ * cw + dx_) * 4;
            // src-over.
            let inv = 255 - a;
            for k in 0..4 {
                canvas[di + k] =
                    ((src[si + k] as u32 * a + canvas[di + k] as u32 * inv) / 255) as u8;
            }
        }
    }
}

/// Launcher icon of another installed package — `loadIcon` /
/// `getApplicationIcon` back it with this so home screens draw the
/// real icon instead of a placeholder.
pub(crate) fn package_icon_drawable(vm: &mut Vm, pkg: &str) -> Option<V> {
    let bytes = std::fs::read(vm.installed_apk_paths.get(pkg)?).ok()?;
    let decoded = crate::package_icon_rgba(&bytes);
    let (w, h, rgba) = decoded?;
    let d = host(
        vm,
        "Landroid/graphics/drawable/BitmapDrawable;",
        HostState::Empty,
    );
    if let V::Ref(dh) = d {
        let bmp = vm.alloc_host(
            "Landroid/graphics/Bitmap;",
            HostState::Bitmap { w, h, rgba },
        );
        pseudo_set(vm, dh, P_BITMAP_SLOT, V::Ref(bmp));
    }
    Some(d)
}

/// `Resources/appcompat getDrawable(resId)` backing: a `BitmapDrawable`
/// host carrying the rasterized bitmap when the resource is an image,
/// else a bare `Drawable` placeholder so callers keep flowing.
pub(crate) fn drawable_for_res(vm: &mut Vm, res_id: u32) -> V {
    // All `&vm.apk_res` reads finish before any `&mut vm` host alloc.
    let (bitmap, color, is_vector, is_layer) = match rctx(vm) {
        Some(rv) => (
            drawable_bitmap(rv, res_id),
            // Not gated on type "color": a `drawable-night` bucket may
            // swap the image for a `@color/x` reference — resolve_ref_color
            // walks the winning value's reference chain and only yields
            // a color when the chain actually ends at one.
            resolve_ref_color(rv, res_id),
            root_is(rv, res_id, "vector"),
            root_is(rv, res_id, "layer-list"),
        ),
        None => (None, None, false, false),
    };
    match bitmap {
        Some((w, h, rgba)) => {
            let d = if is_layer {
                host(
                    vm,
                    "Landroid/graphics/drawable/LayerDrawable;",
                    HostState::Empty,
                )
            } else if is_vector {
                [
                    "Landroidx/vectordrawable/graphics/drawable/VectorDrawableCompat;".to_string(),
                    "Landroid/support/graphics/drawable/VectorDrawableCompat;".to_string(),
                ]
                .into_iter()
                .chain(vm.vector_compat_class())
                .find_map(|c| vm.instantiate(&c).ok().map(V::Ref))
                .unwrap_or_else(|| {
                    host(
                        vm,
                        "Landroid/graphics/drawable/BitmapDrawable;",
                        HostState::Empty,
                    )
                })
            } else {
                host(
                    vm,
                    "Landroid/graphics/drawable/BitmapDrawable;",
                    HostState::Empty,
                )
            };
            if let V::Ref(dh) = d {
                let bmp = vm.alloc_host(
                    "Landroid/graphics/Bitmap;",
                    HostState::Bitmap { w, h, rgba },
                );
                pseudo_set(vm, dh, P_BITMAP_SLOT, V::Ref(bmp));
                // VectorDrawableCompat.newDrawable() shares the guest
                // ConstantState object rather than copying our host pseudo
                // field. Seed that nested state too or menu duplication
                // produces a blank icon.
                let nested: Vec<u32> = match vm.heap.get(dh) {
                    Some(crate::value::Obj::Instance { fields, .. })
                    | Some(crate::value::Obj::Host { fields, .. }) => fields
                        .values()
                        .filter_map(|v| match v {
                            V::Ref(h) if *h != 0 && *h != dh => Some(*h),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                for hnd in nested {
                    pseudo_set(vm, hnd, P_BITMAP_SLOT, V::Ref(bmp));
                }
            }
            d
        }
        None => {
            // A `res/color` id handed to getDrawable becomes a
            // ColorDrawable: carry the resolved ARGB so `draw` can
            // emit a fill for it.
            if is_layer {
                return host(
                    vm,
                    "Landroid/graphics/drawable/LayerDrawable;",
                    HostState::Empty,
                );
            }
            if let Some(argb) = color {
                let d = host(
                    vm,
                    "Landroid/graphics/drawable/ColorDrawable;",
                    HostState::Empty,
                );
                if let V::Ref(dh) = d {
                    pseudo_set(vm, dh, P_DRAW_COLOR, V::I32(argb as i32));
                }
                return d;
            }
            if crate::dbg_flag("OVELO_DRAWDBG") {
                let tn = vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.type_name(res_id))
                    .unwrap_or_default();
                eprintln!("[drawdbg] drawable_for_res #{res_id:08x} ({tn}) -> placeholder");
            }
            host(vm, "Landroid/graphics/drawable/Drawable;", HostState::Empty)
        }
    }
}

/// Output pixels per viewport unit — a 24-unit icon renders at 96px.
const OUT_SCALE: f32 = 4.0;
/// Supersample factor inside the output raster (AA).
const SS: u32 = 4;

fn theme_dimen(res: &ApkResources, theme: u32, attr: u32, depth: u8) -> Option<f32> {
    if depth >= 8 {
        return None;
    }
    let ev = res.style_item(theme, attr)?;
    match ev.data_type {
        // TYPE_ATTRIBUTE in a theme points at another theme attribute;
        // TYPE_REFERENCE may point either at a concrete dimen or at a
        // theme-provided indirection.
        0x02 => theme_dimen(res, theme, ev.data, depth + 1),
        0x01 => res
            .dimen(ev.data)
            .or_else(|| theme_dimen(res, theme, ev.data, depth + 1)),
        _ => Some(crate::res::typed_px(ev)),
    }
}

fn xml_dimen(rv: RCtx<'_>, v: &Value) -> Option<f32> {
    match v {
        Value::Reference(id) => rv
            .res
            .dimen(*id)
            .or_else(|| theme_dimen(rv.res, rv.theme, *id, 0)),
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f32),
        Value::Other { data_type, data } if *data_type == 0x02 => {
            theme_dimen(rv.res, rv.theme, *data, 0)
        }
        Value::Other { data_type, data } => {
            Some(crate::res::typed_px(&ovelo_apk::arsc::EntryValue {
                data_type: *data_type,
                data: *data,
                string: None,
            }))
        }
        _ => None,
    }
}

fn xml_dimen_attr(rv: RCtx<'_>, el: &Element, name: &str) -> Option<f32> {
    xml_dimen(rv, el.android(name)?)
}

fn blend_px(dst: &mut [u8], x: usize, y: usize, w: usize, argb: u32) {
    if x >= w {
        return;
    }
    let idx = (y * w + x) * 4;
    if idx + 3 >= dst.len() {
        return;
    }
    let [sa, sr, sg, sb] = argb.to_be_bytes();
    let (sa, sr, sg, sb) = (sa as u32, sr as u32, sg as u32, sb as u32);
    let da = dst[idx + 3] as u32;
    let inv = 255 - sa;
    let out_a = sa + da * inv / 255;
    if out_a == 0 {
        return;
    }
    // Non-premultiplied src-over. On a transparent destination this
    // preserves the source alpha instead of squaring it.
    for (k, sc) in [sr, sg, sb].into_iter().enumerate() {
        let dc = dst[idx + k] as u32;
        dst[idx + k] = ((sc * sa + dc * da * inv / 255) / out_a).min(255) as u8;
    }
    dst[idx + 3] = out_a as u8;
}

/// Render a `<layer-list>` into a small composite bitmap. The runtime
/// uses this both for visual fidelity and because guest code may cast
/// these resources to `android.graphics.drawable.LayerDrawable`.
fn render_layer_list(rv: RCtx<'_>, res_id: u32) -> Option<(u32, u32, Rc<Vec<u8>>)> {
    if rv.depth >= 16 {
        return None;
    }
    let els = rv.res.document(res_id)?;
    if !els.iter().any(|e| e.depth == 0 && e.name == "layer-list") {
        return None;
    }
    // A layer-list's intrinsic extent is the largest child extent plus
    // that item's insets. A fixed 96px canvas discarded layers whose
    // theme-resolved margins alone reached 96px even though the child
    // remained visible in a correctly sized drawable.
    let (mut cw, mut ch) = (96usize, 96usize);
    for item in els.iter().filter(|e| e.depth == 1 && e.name == "item") {
        let l = xml_dimen_attr(rv, item, "left").unwrap_or(0.0).max(0.0) as usize;
        let t = xml_dimen_attr(rv, item, "top").unwrap_or(0.0).max(0.0) as usize;
        let r = xml_dimen_attr(rv, item, "right").unwrap_or(0.0).max(0.0) as usize;
        let b = xml_dimen_attr(rv, item, "bottom").unwrap_or(0.0).max(0.0) as usize;
        if let Some(Value::Reference(child)) = item.android("drawable") {
            if let Some((w, h, _)) = drawable_bitmap(
                RCtx {
                    depth: rv.depth + 1,
                    ..rv
                },
                *child,
            ) {
                cw = cw.max(l + w as usize + r).min(1024);
                ch = ch.max(t + h as usize + b).min(1024);
            }
        }
    }
    let mut canvas = vec![0u8; cw * ch * 4];
    let mut any = false;
    for (i, item) in els.iter().enumerate() {
        if item.depth != 1 || item.name != "item" {
            continue;
        }
        let end = els[i + 1..]
            .iter()
            .position(|e| e.depth <= 1)
            .map(|p| i + 1 + p)
            .unwrap_or(els.len());
        let l = xml_dimen_attr(rv, item, "left")
            .unwrap_or(0.0)
            .round()
            .max(0.0) as usize;
        let t = xml_dimen_attr(rv, item, "top")
            .unwrap_or(0.0)
            .round()
            .max(0.0) as usize;
        let r = xml_dimen_attr(rv, item, "right")
            .unwrap_or(0.0)
            .round()
            .max(0.0) as usize;
        let b = xml_dimen_attr(rv, item, "bottom")
            .unwrap_or(0.0)
            .round()
            .max(0.0) as usize;
        if l >= cw || t >= ch || l + r >= cw || t + b >= ch {
            continue;
        }
        let (x0, y0, tw, th) = (l, t, cw - l - r, ch - t - b);
        if let Some(Value::Reference(child)) = item.android("drawable") {
            if let Some((w, h, px)) = drawable_bitmap(
                RCtx {
                    depth: rv.depth + 1,
                    ..rv
                },
                *child,
            ) {
                blit_scaled(&mut canvas, cw, &px, w as usize, h as usize, x0, y0, tw, th);
                any = true;
            } else if let Some(argb) = resolve_ref_color(rv, *child) {
                for y in y0..y0 + th {
                    for x in x0..x0 + tw {
                        blend_px(&mut canvas, x, y, cw, argb);
                    }
                }
                any = true;
            }
            continue;
        }
        let Some(shape) = els[i + 1..end].iter().find(|e| e.name == "shape") else {
            continue;
        };
        let oval = matches!(shape.android("shape"), Some(Value::Int(1)));
        let solid = els[i + 1..end]
            .iter()
            .find(|e| e.name == "solid")
            .and_then(|e| color_attr(rv, e, "color"));
        let Some(argb) = solid else { continue };
        for y in y0..y0 + th {
            for x in x0..x0 + tw {
                let inside = !oval || {
                    let nx = (x - x0) as f32 + 0.5 - tw as f32 / 2.0;
                    let ny = (y - y0) as f32 + 0.5 - th as f32 / 2.0;
                    let rx = tw as f32 / 2.0;
                    let ry = th as f32 / 2.0;
                    rx > 0.0 && ry > 0.0 && (nx * nx) / (rx * rx) + (ny * ny) / (ry * ry) <= 1.0
                };
                if inside {
                    blend_px(&mut canvas, x, y, cw, argb);
                }
            }
        }
        any = true;
    }
    any.then_some((cw as u32, ch as u32, Rc::new(canvas)))
}

fn root_is(rv: RCtx<'_>, res_id: u32, name: &str) -> bool {
    rv.res
        .document(res_id)
        .is_some_and(|els| els.iter().any(|e| e.depth == 0 && e.name == name))
}

/// Render `res_id` when its XML document is a `<vector>` drawable.
/// Returns `(w, h, rgba8)` or `None` when it is not a vector.
pub(crate) fn render_vector(rv: RCtx<'_>, res_id: u32) -> Option<(u32, u32, Rc<Vec<u8>>)> {
    let res = rv.res;
    let els = res.document(res_id)?;
    let root = els.iter().find(|e| e.depth == 0 && e.name == "vector")?;
    let vw = f32_attr(root, "viewportWidth").unwrap_or(24.0);
    let vh = f32_attr(root, "viewportHeight").unwrap_or(24.0);
    if vw <= 0.0 || vh <= 0.0 || !vw.is_finite() || !vh.is_finite() {
        return None;
    }
    // `android:width`/`height` are the drawable's intrinsic physical
    // size (dp converted through resources). Rasterizing every 24-unit
    // viewport at a fixed 96px made wrap-content ImageViews twice as
    // large on this runtime's 2x density; only use the fixed supersize
    // as a fallback when the XML omits dimensions.
    let out_w = xml_dimen_attr(rv, root, "width")
        .unwrap_or(vw * OUT_SCALE)
        .round()
        .clamp(1.0, 1024.0) as u32;
    let out_h = xml_dimen_attr(rv, root, "height")
        .unwrap_or(vh * OUT_SCALE)
        .round()
        .clamp(1.0, 1024.0) as u32;
    let rw = (out_w * SS) as usize;
    let rh = (out_h * SS) as usize;
    // viewport → raster, preserving a non-square aspect ratio.
    let kx = out_w as f32 / vw * SS as f32;
    let ky = out_h as f32 / vh * SS as f32;
    let mut any = false;

    // Composite buffer at raster res; each `<path>` paints stroke over fill
    // over the previous content (document order).
    let mut hi = vec![0u8; rw * rh * 4];
    // Affine stack, one entry per `<group>` depth.
    let mut xf: Vec<(usize, Mat)> = Vec::new();
    for el in els.iter().filter(|e| e.depth > 0) {
        while xf.last().is_some_and(|(d, _)| *d >= el.depth) {
            xf.pop();
        }
        match el.name.as_str() {
            "group" => {
                let m = group_mat(el);
                let parent = xf.iter().fold(Mat::IDENT, |acc, (_, m)| acc.then(m));
                xf.push((el.depth, parent.then(&m)));
            }
            "path" => {
                let m = xf.iter().fold(Mat::IDENT, |acc, (_, m)| acc.then(m));
                let Some(data) = path_attr(el) else { continue };
                let tr = Mat::scale(kx, ky).then(&m);
                let subs = parse_path(&data);
                let polys: Vec<Vec<(f32, f32)>> = subs
                    .iter()
                    .map(|s| s.pts.iter().map(|&(x, y)| tr.apply(x, y)).collect())
                    .filter(|p: &Vec<(f32, f32)>| p.len() >= 2)
                    .collect();
                if polys.is_empty() {
                    continue;
                }
                let fill = color_attr(rv, el, "fillColor");
                let stroke = color_attr(rv, el, "strokeColor");
                // No fill attr + stroke → transparent fill; no fill and no
                // stroke → the vector spec fills black.
                let fill_argb = match fill {
                    Some(c) => Some(mul_alpha(c, f32_attr(el, "fillAlpha").unwrap_or(1.0))),
                    None if stroke.is_none() => Some(0xff00_0000),
                    None => None,
                };
                if let Some(argb) = fill_argb {
                    let mut cov = vec![0u8; rw * rh];
                    accumulate(&mut cov, &polys, rw, rh);
                    paint(&mut hi, &cov, argb);
                    any = true;
                }
                if let Some(c) = stroke {
                    let argb = mul_alpha(c, f32_attr(el, "strokeAlpha").unwrap_or(1.0));
                    let sw = f32_attr(el, "strokeWidth").unwrap_or(1.0) * (kx + ky) * 0.5;
                    let polys: Vec<Vec<(f32, f32)>> = subs
                        .iter()
                        .flat_map(|s| {
                            let pts: Vec<(f32, f32)> =
                                s.pts.iter().map(|&(x, y)| tr.apply(x, y)).collect();
                            stroke_quads(&pts, s.closed, sw)
                        })
                        .collect();
                    let mut cov = vec![0u8; rw * rh];
                    accumulate(&mut cov, &polys, rw, rh);
                    paint(&mut hi, &cov, argb);
                    any = true;
                }
            }
            _ => {}
        }
    }
    if !any {
        return None;
    }
    let mut out = vec![0u8; (out_w * out_h * 4) as usize];
    let s = SS as usize;
    for y in 0..out_h as usize {
        for x in 0..out_w as usize {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..s {
                for dx in 0..s {
                    let p = ((y * s + dy) * rw + (x * s + dx)) * 4;
                    r += hi[p] as u32;
                    g += hi[p + 1] as u32;
                    b += hi[p + 2] as u32;
                    a += hi[p + 3] as u32;
                }
            }
            let n = (s * s) as u32;
            let d = (y * out_w as usize + x) * 4;
            out[d] = (r / n) as u8;
            out[d + 1] = (g / n) as u8;
            out[d + 2] = (b / n) as u8;
            out[d + 3] = (a / n) as u8;
        }
    }
    Some((out_w, out_h, Rc::new(out)))
}

/// 2D affine: x' = a*x + c*y + e, y' = b*x + d*y + f.
#[derive(Clone, Copy)]
struct Mat {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Mat {
    const IDENT: Mat = Mat {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };
    fn scale(sx: f32, sy: f32) -> Mat {
        Mat {
            a: sx,
            d: sy,
            ..Mat::IDENT
        }
    }
    fn translate(tx: f32, ty: f32) -> Mat {
        Mat {
            e: tx,
            f: ty,
            ..Mat::IDENT
        }
    }
    fn rotate(deg: f32) -> Mat {
        let (s, c) = deg.to_radians().sin_cos();
        Mat {
            a: c,
            b: s,
            c: -s,
            d: c,
            ..Mat::IDENT
        }
    }
    /// self ∘ other — apply `other` first, then `self`.
    fn then(&self, o: &Mat) -> Mat {
        Mat {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

/// `<group>` transform: T(pivot)·R(rot)·S(sx,sy)·T(translate)·T(-pivot).
fn group_mat(el: &Element) -> Mat {
    let px = f32_attr(el, "pivotX").unwrap_or(0.0);
    let py = f32_attr(el, "pivotY").unwrap_or(0.0);
    let rot = f32_attr(el, "rotation").unwrap_or(0.0);
    let sx = f32_attr(el, "scaleX").unwrap_or(1.0);
    let sy = f32_attr(el, "scaleY").unwrap_or(1.0);
    let tx = f32_attr(el, "translateX").unwrap_or(0.0);
    let ty = f32_attr(el, "translateY").unwrap_or(0.0);
    Mat::translate(px, py)
        .then(&Mat::rotate(rot))
        .then(&Mat::scale(sx, sy))
        .then(&Mat::translate(tx, ty))
        .then(&Mat::translate(-px, -py))
}

fn f32_attr(el: &Element, name: &str) -> Option<f32> {
    match el.android(name) {
        Some(Value::Float(f)) => Some(*f),
        Some(Value::Int(i)) => Some(*i as f32),
        Some(Value::Str(s)) => s.trim().parse().ok(),
        Some(Value::Other { data, .. }) => Some(f32::from_bits(*data)),
        _ => None,
    }
}

fn path_attr(el: &Element) -> Option<String> {
    match el.android("pathData") {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn mul_alpha(argb: u32, alpha: f32) -> u32 {
    let a = ((argb >> 24) as f32 * alpha.clamp(0.0, 1.0)) as u32;
    (a << 24) | (argb & 0x00ff_ffff)
}

fn color_attr(rv: RCtx<'_>, el: &Element, name: &str) -> Option<u32> {
    color_of(rv, el.android(name)?)
}

fn color_of(rv: RCtx<'_>, v: &Value) -> Option<u32> {
    match v {
        Value::Str(s) => parse_color(s),
        Value::Int(i) => Some(*i as u32),
        Value::Other { data_type, data } if (0x1c..=0x1f).contains(data_type) => Some(*data),
        Value::Reference(r) => resolve_ref_color(rv, *r),
        _ => None,
    }
}

/// `@color/x` → the entry; `?attr/x` → theme chain (with `?attr` chases).
pub(crate) fn resolve_ref_color(rv: RCtx<'_>, id: u32) -> Option<u32> {
    let res = rv.res;
    if let Some(ev) = res.entry_value(id) {
        if let Some(c) = entry_color(res, ev) {
            return Some(c);
        }
    }
    let mut cur = id;
    for _ in 0..8 {
        let ev = res.style_item(rv.theme, cur)?;
        match entry_color(res, ev) {
            Some(c) => return Some(c),
            None if ev.data_type == 0x02 => cur = ev.data,
            None => return None,
        }
    }
    None
}

fn entry_color(res: &crate::res::ApkResources, ev: &ovelo_apk::arsc::EntryValue) -> Option<u32> {
    // TYPE_REFERENCE chains are walked iteratively — a cyclic or
    // self-referencing color resource must not recurse without bound.
    let mut ev = ev;
    for _ in 0..8 {
        match ev.data_type {
            0x1c..=0x1f | 0x10 | 0x11 => return Some(ev.data),
            // `android.R.color` ids live outside the app table.
            0x01 => match res.entry_value(ev.data) {
                Some(next) => ev = next,
                None => return crate::res::framework_color(ev.data),
            },
            0x03 => return ev.string.as_deref().and_then(parse_color),
            _ => return None,
        }
    }
    None
}

/// A drawable resource's representative fill color — used when a view
/// `background`/`foreground` resolves to an XML drawable we can't fully
/// rasterize: `<shape><solid>`, `<color>`, `<ripple>`'s color, a
/// `<selector>`/`<layer-list>` default item, `<inset>`'s content. Stated
/// items that carry explicit state (`state_pressed`…) are skipped so
/// the resting-state color wins.
pub(crate) fn drawable_flat_color(rv: RCtx<'_>, res_id: u32) -> Option<u32> {
    if rv.depth >= 16 {
        return None;
    }
    let res = rv.res;
    let els = res.document(res_id)?;
    let mut fallback: Option<u32> = None;
    for el in &els {
        match el.name.as_str() {
            "shape" => {
                // handled by its <solid> child below
            }
            "solid" | "color" | "stroke" => {
                if let Some(c) = color_attr(rv, el, "color") {
                    return Some(c);
                }
            }
            "item" => {
                // Skip items bound to a state set; an unqualified item
                // is the drawable's default.
                let has_state = el.attrs.iter().any(|a| {
                    if !a.name.starts_with("state_") {
                        return false;
                    }
                    // state_enabled=true is the normal resting state and
                    // commonly accompanies otherwise default items;
                    // state_enabled=false is a real disabled-state rule.
                    if a.name == "state_enabled" {
                        matches!(&a.value, Value::Bool(false) | Value::Int(0))
                    } else {
                        true
                    }
                });
                if has_state {
                    continue;
                }
                if let Some(c) = color_attr(rv, el, "color") {
                    return Some(c);
                }
                if let Some(Value::Reference(r)) = el.android("drawable") {
                    if fallback.is_none() {
                        fallback = resolve_ref_color(rv, *r).or_else(|| {
                            drawable_flat_color(
                                RCtx {
                                    depth: rv.depth + 1,
                                    ..rv
                                },
                                *r,
                            )
                        });
                    }
                }
            }
            "ripple" => {
                if let Some(c) = color_attr(rv, el, "color") {
                    return Some(c);
                }
            }
            _ => {}
        }
    }
    fallback
}

pub(crate) fn parse_color(s: &str) -> Option<u32> {
    let s = s.trim().strip_prefix('#')?;
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(match s.len() {
        3 => {
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            0xff00_0000 | ((r * 0x11) << 16) | ((g * 0x11) << 8) | (b * 0x11)
        }
        4 => {
            let (a, r, g, b) = ((v >> 12) & 0xf, (v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            ((a * 0x11) << 24) | ((r * 0x11) << 16) | ((g * 0x11) << 8) | (b * 0x11)
        }
        6 => 0xff00_0000 | v,
        8 => v,
        _ => return None,
    })
}

/// src-over composite `argb` through coverage into the raster buffer.
fn paint(hi: &mut [u8], cov: &[u8], argb: u32) {
    let (sa, sr, sg, sb) = (
        argb >> 24,
        (argb >> 16) & 0xff,
        (argb >> 8) & 0xff,
        argb & 0xff,
    );
    for (i, &c) in cov.iter().enumerate() {
        let a = sa * c as u32 / 255;
        if a == 0 {
            continue;
        }
        let p = i * 4;
        let inv = 255 - a;
        hi[p] = ((sr * a + hi[p] as u32 * inv) / 255) as u8;
        hi[p + 1] = ((sg * a + hi[p + 1] as u32 * inv) / 255) as u8;
        hi[p + 2] = ((sb * a + hi[p + 2] as u32 * inv) / 255) as u8;
        hi[p + 3] = (a + hi[p + 3] as u32 * inv / 255).min(255) as u8;
    }
}

/// One subpath after flattening.
struct Sub {
    pts: Vec<(f32, f32)>,
    closed: bool,
}

/// Parse the `pathData` mini-language (M L H V C S Q T A Z, rel variants).
fn parse_path(d: &str) -> Vec<Sub> {
    fn skip(b: &[u8], i: &mut usize) {
        while *i < b.len() && (b[*i] == b',' || b[*i].is_ascii_whitespace()) {
            *i += 1;
        }
    }
    fn num(b: &[u8], i: &mut usize) -> Option<f32> {
        skip(b, i);
        let s = *i;
        if *i < b.len() && (b[*i] == b'+' || b[*i] == b'-') {
            *i += 1;
        }
        let mut any = false;
        while *i < b.len() {
            match b[*i] {
                c if c.is_ascii_digit() || c == b'.' => *i += 1,
                c @ (b'e' | b'E') if any => {
                    *i += 1;
                    let _ = c;
                    if *i < b.len() && (b[*i] == b'+' || b[*i] == b'-') {
                        *i += 1;
                    }
                }
                _ => break,
            }
            any = true;
        }
        if *i == s || !any {
            *i = s;
            return None;
        }
        std::str::from_utf8(&b[s..*i]).ok()?.parse().ok()
    }
    fn out_done(mut out: Vec<Sub>, cur: Vec<(f32, f32)>, closed: bool) -> Vec<Sub> {
        if !cur.is_empty() {
            out.push(Sub { pts: cur, closed });
        }
        out
    }
    let b = d.as_bytes();
    let mut i = 0usize;
    let mut cmd = b' ';
    let mut out: Vec<Sub> = Vec::new();
    let mut cur: Vec<(f32, f32)> = Vec::new();
    let (mut x, mut y) = (0f32, 0f32);
    let (mut sx, mut sy) = (0f32, 0f32);
    let (mut cx, mut cy) = (0f32, 0f32);
    let (mut qx, mut qy) = (0f32, 0f32);
    let (mut pc, mut pq) = (false, false);
    // Defined after the locals it touches (macro definition-site scoping).
    macro_rules! n {
        () => {
            match num(b, &mut i) {
                Some(v) => v,
                None => return out_done(out, cur, false),
            }
        };
    }
    loop {
        skip(b, &mut i);
        if i >= b.len() {
            return out_done(out, cur, false);
        }
        if b[i].is_ascii_alphabetic() {
            cmd = b[i];
            i += 1;
            if i >= b.len() && !matches!(cmd, b'Z' | b'z') {
                return out_done(out, cur, false);
            }
        }
        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (nx, ny) = (n!(), n!());
                if !cur.is_empty() {
                    out.push(Sub {
                        pts: std::mem::take(&mut cur),
                        closed: false,
                    });
                }
                (x, y) = if rel { (x + nx, y + ny) } else { (nx, ny) };
                (sx, sy) = (x, y);
                cur.push((x, y));
                cmd = if rel { b'l' } else { b'L' };
                pc = false;
                pq = false;
            }
            b'Z' => {
                if !cur.is_empty() {
                    if *cur.last().unwrap() != (sx, sy) {
                        cur.push((sx, sy));
                    }
                    out.push(Sub {
                        pts: std::mem::take(&mut cur),
                        closed: true,
                    });
                }
                (x, y) = (sx, sy);
                // A `Z` takes no operands — leaving `cmd` at Z makes the
                // implicit-repeat path re-execute it forever on stray
                // input like `Z0`. A number after Z is malformed; end.
                cmd = b' ';
                pc = false;
                pq = false;
            }
            b'L' => {
                let (nx, ny) = (n!(), n!());
                (x, y) = if rel { (x + nx, y + ny) } else { (nx, ny) };
                cur.push((x, y));
                pc = false;
                pq = false;
            }
            b'H' => {
                let nx = n!();
                x = if rel { x + nx } else { nx };
                cur.push((x, y));
                pc = false;
                pq = false;
            }
            b'V' => {
                let ny = n!();
                y = if rel { y + ny } else { ny };
                cur.push((x, y));
                pc = false;
                pq = false;
            }
            b'C' => {
                let (a, b1) = (n!(), n!());
                let (c, d1) = (n!(), n!());
                let (e, f) = (n!(), n!());
                let (p1, p2, p3) = if rel {
                    ((x + a, y + b1), (x + c, y + d1), (x + e, y + f))
                } else {
                    ((a, b1), (c, d1), (e, f))
                };
                flatten_cubic(&mut cur, (x, y), p1, p2, p3);
                (cx, cy) = p2;
                (x, y) = p3;
                pc = true;
                pq = false;
            }
            b'S' => {
                let (c, d1) = (n!(), n!());
                let (e, f) = (n!(), n!());
                let p1 = if pc {
                    (2.0 * x - cx, 2.0 * y - cy)
                } else {
                    (x, y)
                };
                let (p2, p3) = if rel {
                    ((x + c, y + d1), (x + e, y + f))
                } else {
                    ((c, d1), (e, f))
                };
                flatten_cubic(&mut cur, (x, y), p1, p2, p3);
                (cx, cy) = p2;
                (x, y) = p3;
                pc = true;
                pq = false;
            }
            b'Q' => {
                let (a, b1) = (n!(), n!());
                let (c, d1) = (n!(), n!());
                let (p1, p2) = if rel {
                    ((x + a, y + b1), (x + c, y + d1))
                } else {
                    ((a, b1), (c, d1))
                };
                flatten_quad(&mut cur, (x, y), p1, p2);
                (qx, qy) = p1;
                (x, y) = p2;
                pq = true;
                pc = false;
            }
            b'T' => {
                let (c, d1) = (n!(), n!());
                let p1 = if pq {
                    (2.0 * x - qx, 2.0 * y - qy)
                } else {
                    (x, y)
                };
                let p2 = if rel { (x + c, y + d1) } else { (c, d1) };
                flatten_quad(&mut cur, (x, y), p1, p2);
                (qx, qy) = p1;
                (x, y) = p2;
                pq = true;
                pc = false;
            }
            b'A' => {
                let (rx, ry) = (n!(), n!());
                let rot = n!();
                let (laf, sf) = (n!(), n!());
                let (nx, ny) = (n!(), n!());
                let p = if rel { (x + nx, y + ny) } else { (nx, ny) };
                flatten_arc(
                    &mut cur,
                    (x, y),
                    rx.abs(),
                    ry.abs(),
                    rot,
                    laf != 0.0,
                    sf != 0.0,
                    p,
                );
                (x, y) = p;
                pc = false;
                pq = false;
            }
            _ => return out_done(out, cur, false),
        }
    }
}

fn flatten_cubic(
    out: &mut Vec<(f32, f32)>,
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
) {
    const N: u32 = 16;
    for i in 1..=N {
        let t = i as f32 / N as f32;
        let u = 1.0 - t;
        let x =
            u * u * u * p0.0 + 3.0 * u * u * t * p1.0 + 3.0 * u * t * t * p2.0 + t * t * t * p3.0;
        let y =
            u * u * u * p0.1 + 3.0 * u * u * t * p1.1 + 3.0 * u * t * t * p2.1 + t * t * t * p3.1;
        out.push((x, y));
    }
}

fn flatten_quad(out: &mut Vec<(f32, f32)>, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)) {
    const N: u32 = 12;
    for i in 1..=N {
        let t = i as f32 / N as f32;
        let u = 1.0 - t;
        out.push((
            u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0,
            u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1,
        ));
    }
}

/// Endpoint-parametrized elliptical arc → polyline (SVG spec F.6.5).
#[allow(clippy::too_many_arguments)]
fn flatten_arc(
    out: &mut Vec<(f32, f32)>,
    p0: (f32, f32),
    rx: f32,
    ry: f32,
    rot_deg: f32,
    large_arc: bool,
    sweep: bool,
    p1: (f32, f32),
) {
    if rx <= 0.0 || ry <= 0.0 || p0 == p1 {
        out.push(p1);
        return;
    }
    let phi = rot_deg.to_radians();
    let (sp, cp) = phi.sin_cos();
    let dx = (p0.0 - p1.0) / 2.0;
    let dy = (p0.1 - p1.1) / 2.0;
    let x1p = cp * dx + sp * dy;
    let y1p = -sp * dx + cp * dy;
    let mut rx = rx;
    let mut ry = ry;
    let lam = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lam > 1.0 {
        let s = lam.sqrt();
        rx *= s;
        ry *= s;
    }
    let sign = if large_arc == sweep { -1.0f32 } else { 1.0 };
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let co = sign * (num.max(0.0) / den.max(1e-9)).sqrt();
    let cxp = co * rx * y1p / ry;
    let cyp = -co * ry * x1p / rx;
    let cx = cp * cxp - sp * cyp + (p0.0 + p1.0) / 2.0;
    let cy = sp * cxp + cp * cyp + (p0.1 + p1.1) / 2.0;
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let n = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
        let mut a = ((ux * vx + uy * vy) / n.max(1e-9)).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let th1 = angle(1.0, 0.0, ux, uy);
    let mut dth = angle(ux, uy, vx, vy);
    if !sweep && dth > 0.0 {
        dth -= std::f32::consts::TAU;
    }
    if sweep && dth < 0.0 {
        dth += std::f32::consts::TAU;
    }
    let n = ((dth.abs() / (std::f32::consts::FRAC_PI_8)).ceil() as u32).max(1);
    for i in 1..=n {
        let t = th1 + dth * (i as f32 / n as f32);
        let (st, ct) = t.sin_cos();
        out.push((
            cx + rx * ct * cp - ry * st * sp,
            cy + rx * ct * sp + ry * st * cp,
        ));
    }
}

/// Expand an open/closed polyline into quadrilaterals of `w` raster px.
fn stroke_quads(pts: &[(f32, f32)], closed: bool, w: f32) -> Vec<Vec<(f32, f32)>> {
    let hw = w / 2.0;
    let mut polys = Vec::new();
    let n = pts.len();
    let segs = if closed { n } else { n.saturating_sub(1) };
    for i in 0..segs {
        let p = pts[i];
        let q = pts[(i + 1) % n];
        let (dx, dy) = (q.0 - p.0, q.1 - p.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-6 {
            continue;
        }
        let (nx, ny) = (-dy / len * hw, dx / len * hw);
        polys.push(vec![
            (p.0 + nx, p.1 + ny),
            (q.0 + nx, q.1 + ny),
            (q.0 - nx, q.1 - ny),
            (p.0 - nx, p.1 - ny),
        ]);
        // Square joint at the shared vertex to cover gaps between segments.
        if i + 1 < segs || closed {
            let j = (i + 1) % n;
            polys.push(vec![
                (pts[j].0 - hw, pts[j].1 - hw),
                (pts[j].0 + hw, pts[j].1 - hw),
                (pts[j].0 + hw, pts[j].1 + hw),
                (pts[j].0 - hw, pts[j].1 + hw),
            ]);
        }
    }
    polys
}

/// Nonzero-winding scanline coverage into `cov` (0..255 per pixel).
fn accumulate(cov: &mut [u8], polys: &[Vec<(f32, f32)>], w: usize, h: usize) {
    for y in 0..h {
        let yc = y as f32 + 0.5;
        let mut xs: Vec<(f32, i32)> = Vec::new();
        for poly in polys {
            let n = poly.len();
            for i in 0..n {
                let (x0, y0) = poly[i];
                let (x1, y1) = poly[(i + 1) % n];
                if (y0 <= yc) == (y1 <= yc) {
                    continue;
                }
                let x = x0 + (yc - y0) * (x1 - x0) / (y1 - y0);
                xs.push((x, if y1 > y0 { 1 } else { -1 }));
            }
        }
        if xs.is_empty() {
            continue;
        }
        xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut wind = 0i32;
        let mut px = 0f32;
        for (x, dir) in xs {
            if wind != 0 {
                let (l, r) = (px.max(0.0), x.min(w as f32));
                if r > l {
                    let li = l.floor() as usize;
                    let ri = r.ceil() as usize;
                    for xi in li..ri.min(w) {
                        let xl = (xi as f32).max(l);
                        let xr = (xi as f32 + 1.0).min(r);
                        let c = ((xr - xl) * 255.0) as u32;
                        let idx = y * w + xi;
                        cov[idx] = cov[idx].saturating_add(c.min(255) as u8);
                    }
                }
            }
            wind += dir;
            px = x;
        }
    }
}
