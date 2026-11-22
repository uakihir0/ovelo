//! Minimal `javax.microedition.khronos.opengles.GL10` for
//! `GLSurfaceView`-driven guest renderers: records `glDraw*` output
//! under the current matrices and flattens it to `CanvasOp`s so the
//! host 2D pipeline presents real GL content (line/fan meshes — the
//! Vector Pinball table — without a GPU or EGL context).

use std::collections::HashMap;
use std::sync::Mutex;

use super::{host, i32v, pseudo_get, pseudo_set};
use crate::value::{CanvasOp, HostState, Obj, PaintState, V};
use crate::{Rt, Vm};

/// pseudo key on the GLSurfaceView: its `GLSurfaceView.Renderer` handle.
pub(crate) const P_GLES_RENDERER: u64 = 0xFFFD_FE10;
/// pseudo key on the GLSurfaceView: the shared GL10 object handle.
const P_GLES_GL: u64 = 0xFFFD_FE11;

const GL_LINES: i32 = 1;
const GL_LINE_LOOP: i32 = 2;
const GL_LINE_STRIP: i32 = 3;
const GL_TRIANGLES: i32 = 4;
const GL_TRIANGLE_STRIP: i32 = 5;
const GL_TRIANGLE_FAN: i32 = 6;

const GL_MODELVIEW: i32 = 0x1700;
const GL_PROJECTION: i32 = 0x1701;

const GL_COLOR_ARRAY: i32 = 0x8076;

#[derive(Clone, Default)]
struct Arr {
    /// Host buffer object handle (FloatBuffer/ByteBuffer).
    buf: u32,
    /// Components per entry (2/3/4).
    size: i32,
    /// `GL_FLOAT`/`GL_UNSIGNED_BYTE`/…
    ty: i32,
    /// Bytes between entries (0 = packed).
    stride: i32,
}

struct Prim {
    /// `GL_*` primitive mode, or -1 = clear the viewport.
    mode: i32,
    /// Screen-space px vertices (already transformed).
    verts: Vec<(f32, f32)>,
    /// Per-vertex packed ARGB.
    colors: Vec<u32>,
}

struct GlState {
    mode: i32,
    proj: [f32; 16],
    model: [f32; 16],
    proj_stack: Vec<[f32; 16]>,
    model_stack: Vec<[f32; 16]>,
    vp: (i32, i32, i32, i32),
    clear: u32,
    color: u32,
    line_w: f32,
    vptr: Option<Arr>,
    cptr: Option<Arr>,
    created: bool,
    changed: bool,
    prims: Vec<Prim>,
}

impl GlState {
    fn new() -> Self {
        Self {
            mode: GL_MODELVIEW,
            proj: ident(),
            model: ident(),
            proj_stack: Vec::new(),
            model_stack: Vec::new(),
            vp: (0, 0, 0, 0),
            clear: 0xFF00_0000,
            color: 0xFFFF_FFFF,
            line_w: 1.0,
            vptr: None,
            cptr: None,
            created: false,
            changed: false,
            prims: Vec::new(),
        }
    }
}

static STATES: Mutex<Option<HashMap<u32, GlState>>> = Mutex::new(None);

fn ident() -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        0.0, 0.0, 0.0, 1.0,
    ]
}

/// Column-major GL matrix product `a * b`.
fn mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut o = [0.0f32; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[c * 4 + r] = (0..4).map(|k| a[k * 4 + r] * b[c * 4 + k]).sum();
        }
    }
    o
}

fn apply(m: &[f32; 16], v: [f32; 4]) -> [f32; 4] {
    let mut o = [0.0f32; 4];
    for r in 0..4 {
        o[r] = (0..4).map(|c| m[c * 4 + r] * v[c]).sum();
    }
    o
}

fn argb(f: &[f32; 4]) -> u32 {
    let c = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u32;
    (c(f[3]) << 24) | (c(f[0]) << 16) | (c(f[1]) << 8) | c(f[2])
}

fn state<R>(gl: u32, f: impl FnOnce(&mut GlState) -> R) -> R {
    let mut g = STATES.lock().unwrap();
    let st = g
        .get_or_insert_with(HashMap::new)
        .entry(gl)
        .or_insert_with(GlState::new);
    f(st)
}

fn has_state(gl: u32) -> bool {
    STATES
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|m| m.contains_key(&gl))
}

fn f32v(v: &V) -> f32 {
    match v {
        V::F32(f) => *f,
        V::F64(d) => *d as f32,
        V::I32(i) => *i as f32,
        V::I64(l) => *l as f32,
        _ => 0.0,
    }
}

/// Read a component from a vertex/color buffer in element space. `idx`
/// is the entry index (vertex number); returns the component values
/// packed into a `[f32; 4]` (unused lanes 0).
fn read_arr(vm: &Vm, a: &Arr, idx: i32) -> [f32; 4] {
    let mut o = [0.0f32; 4];
    let Some(Obj::Host {
        state: HostState::Bytes(data, _),
        ..
    }) = vm.heap.get(a.buf)
    else {
        return o;
    };
    let pos = i32v(pseudo_get(vm, a.buf, super::P_BBUF_POS).unwrap_or(V::I32(0))).max(0) as usize;
    let stride = if a.stride > 0 {
        a.stride as usize
    } else {
        a.size.max(1) as usize * 4
    };
    // Buffer positions are element-based for typed views (FloatBuffer),
    // byte-based for raw ByteBuffer — GL callers hand us typed buffers,
    // so scale pos by the element width (4 for float/int, else 1).
    let elem_w: usize = if a.ty == 0x1406 || a.ty == 0x1404 {
        4
    } else {
        1
    };
    let base = pos * elem_w + idx.max(0) as usize * stride;
    for (k, o) in o.iter_mut().enumerate().take(a.size.clamp(0, 4) as usize) {
        let at = base + k * elem_w;
        *o = match a.ty {
            0x1406 => {
                // GL_FLOAT
                data.get(at..at + 4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .unwrap_or(0.0)
            }
            0x140C => {
                // GL_FIXED 16.16
                data.get(at..at + 4)
                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 65536.0)
                    .unwrap_or(0.0)
            }
            0x1401 => data.get(at).map(|b| *b as f32 / 255.0).unwrap_or(0.0), // GL_UNSIGNED_BYTE
            0x1402 => {
                // GL_SHORT
                data.get(at..at + 2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32)
                    .unwrap_or(0.0)
            }
            _ => 0.0,
        };
    }
    o
}

/// Current projection*modelview transform → NDC → viewport px
/// (GL's bottom-left origin flips Y for our top-left surface).
fn xform(st: &GlState, v: [f32; 4]) -> (f32, f32) {
    let mvp = mul(&st.proj, &st.model);
    let o = apply(&mvp, v);
    let w = if o[3] != 0.0 { o[3] } else { 1.0 };
    let nx = o[0] / w;
    let ny = o[1] / w;
    let (vx, vy, vw, vh) = st.vp;
    (
        vx as f32 + (nx + 1.0) * vw as f32 / 2.0,
        vy as f32 + vh as f32 - (ny + 1.0) * vh as f32 / 2.0,
    )
}

fn record(vm: &Vm, gl: u32, mode: i32, first: i32, count: i32) {
    state(gl, |st| {
        let Some(a) = st.vptr.clone() else { return };
        let mut verts = Vec::with_capacity(count.max(0) as usize);
        let mut colors = Vec::with_capacity(count.max(0) as usize);
        for i in 0..count.max(0) {
            let c = read_arr(vm, &a, first + i);
            verts.push(xform(st, [c[0], c[1], c[2], 1.0]));
            colors.push(match &st.cptr {
                Some(ca) => {
                    let cc = read_arr(vm, ca, first + i);
                    argb(&cc)
                }
                None => st.color,
            });
        }
        st.prims.push(Prim { mode, verts, colors });
    });
}

/// `glDrawElements` — index buffer (`GL_UNSIGNED_BYTE`/`_SHORT`).
fn record_el(vm: &Vm, gl: u32, mode: i32, count: i32, ty: i32, buf: u32) {
    state(gl, |st| {
        let Some(a) = st.vptr.clone() else { return };
        let ia = Arr {
            buf,
            size: 1,
            ty,
            stride: 0,
        };
        let mut verts = Vec::with_capacity(count.max(0) as usize);
        let mut colors = Vec::with_capacity(count.max(0) as usize);
        for i in 0..count.max(0) {
            let ii = read_arr(vm, &ia, i)[0] as i32;
            let c = read_arr(vm, &a, ii);
            verts.push(xform(st, [c[0], c[1], c[2], 1.0]));
            colors.push(match &st.cptr {
                Some(ca) => argb(&read_arr(vm, ca, ii)),
                None => st.color,
            });
        }
        st.prims.push(Prim { mode, verts, colors });
    });
}

/// Flatten recorded prims into CanvasOps (post-`onDrawFrame`).
fn to_ops(st: &GlState) -> Vec<CanvasOp> {
    let mut ops = Vec::with_capacity(st.prims.len() * 2);
    for p in &st.prims {
        if p.mode == -1 {
            ops.push(CanvasOp::Color {
                argb: p.colors.first().copied().unwrap_or(0xFF00_0000) as i32,
            });
            continue;
        }
        let col = |i: usize| *p.colors.get(i).or(p.colors.last()).unwrap_or(&0) as i32;
        let paint = |i: usize| PaintState {
            argb: col(i),
            text_size: 0.0,
            align: 0,
            style: 0,
            stroke_w: st.line_w,
            fake_bold: false,
        };
        let line = |ops: &mut Vec<CanvasOp>, a: usize, b: usize| {
            ops.push(CanvasOp::Line {
                x0: p.verts[a].0,
                y0: p.verts[a].1,
                x1: p.verts[b].0,
                y1: p.verts[b].1,
                paint: paint(a),
            });
        };
        let fill = |ops: &mut Vec<CanvasOp>, idxs: &[usize]| {
            let mut pts = Vec::with_capacity(idxs.len() * 2);
            for &i in idxs {
                pts.push(p.verts[i].0);
                pts.push(p.verts[i].1);
            }
            ops.push(CanvasOp::FillPoly {
                pts,
                argb: col(idxs[0]),
            });
        };
        let n = p.verts.len();
        match p.mode {
            GL_LINES => {
                for i in (0..n.saturating_sub(1)).step_by(2) {
                    line(&mut ops, i, i + 1);
                }
            }
            GL_LINE_STRIP => {
                for i in 0..n.saturating_sub(1) {
                    line(&mut ops, i, i + 1);
                }
            }
            GL_LINE_LOOP => {
                for i in 0..n.saturating_sub(1) {
                    line(&mut ops, i, i + 1);
                }
                if n > 2 {
                    line(&mut ops, n - 1, 0);
                }
            }
            GL_TRIANGLES => {
                for i in (0..n.saturating_sub(2)).step_by(3) {
                    fill(&mut ops, &[i, i + 1, i + 2]);
                }
            }
            GL_TRIANGLE_STRIP => {
                for i in 0..n.saturating_sub(2) {
                    fill(&mut ops, &[i, i + 1, i + 2]);
                }
            }
            GL_TRIANGLE_FAN => {
                if n >= 3 {
                    // Convex fan → one polygon fill.
                    fill(&mut ops, &(0..n).collect::<Vec<_>>());
                }
            }
            0 => {
                // GL_POINTS — 3px dots.
                for (i, &(x, y)) in p.verts.iter().enumerate().take(n) {
                    ops.push(CanvasOp::Rect {
                        l: x - 1.5,
                        t: y - 1.5,
                        r: x + 1.5,
                        b: y + 1.5,
                        paint: paint(i),
                    });
                }
            }
            _ => {}
        }
    }
    ops
}

/// Create (once) the shared GL10 object for a GLSurfaceView and drive
/// one frame: `onSurfaceCreated`/`onSurfaceChanged` once each, then
/// `onDrawFrame`; returns the recorded drawing as CanvasOps.
pub(crate) fn gl_frame(vm: &mut Vm, view: u32, w: i32, h: i32) -> Vec<CanvasOp> {
    let Some(V::Ref(renderer)) = pseudo_get(vm, view, P_GLES_RENDERER) else {
        return Vec::new();
    };
    let gl = match pseudo_get(vm, view, P_GLES_GL) {
        Some(V::Ref(h)) => h,
        _ => {
            let V::Ref(h) = host(
                vm,
                "Ljavax/microedition/khronos/opengles/GL10;",
                HostState::Empty,
            ) else {
                return Vec::new();
            };
            state(h, |_| ());
            pseudo_set(vm, view, P_GLES_GL, V::Ref(h));
            h
        }
    };
    let (created, changed) = state(gl, |s| (s.created, s.changed));
    if !created {
        let cfg = host(
            vm,
            "Ljavax/microedition/khronos/egl/EGLConfig;",
            HostState::Empty,
        );
        let r = vm.call_virtual_sig(
            renderer,
            "onSurfaceCreated",
            &[
                "Ljavax/microedition/khronos/opengles/GL10;",
                "Ljavax/microedition/khronos/egl/EGLConfig;",
            ],
            &[V::Ref(gl), cfg],
        );
        if std::env::var_os("OVELO_DRAWDBG").is_some() {
            eprintln!("[gldbg] onSurfaceCreated err={:?}", r.is_err());
        }
        state(gl, |s| s.created = true);
    }
    if !changed {
        let r = vm.call_virtual_sig(
            renderer,
            "onSurfaceChanged",
            &["Ljavax/microedition/khronos/opengles/GL10;", "I", "I"],
            &[V::Ref(gl), V::I32(w), V::I32(h)],
        );
        if std::env::var_os("OVELO_DRAWDBG").is_some() {
            eprintln!("[gldbg] onSurfaceChanged {w}x{h} err={:?}", r.is_err());
        }
        state(gl, |s| {
            s.changed = true;
            if s.vp.2 == 0 {
                s.vp = (0, 0, w, h);
            }
        });
    }
    state(gl, |s| s.prims.clear());
    let r = vm.call_virtual_sig(
        renderer,
        "onDrawFrame",
        &["Ljavax/microedition/khronos/opengles/GL10;"],
        &[V::Ref(gl)],
    );
    let ops = state(gl, |s| to_ops(s));
    if std::env::var_os("OVELO_DRAWDBG").is_some() {
        eprintln!(
            "[gldbg] onDrawFrame {w}x{h} -> {} ops err={:?}",
            ops.len(),
            r.is_err()
        );
    }
    ops
}

/// GL10 builtins — dispatched by method name on handles we created.
pub(crate) fn call(
    vm: &mut Vm,
    _class: &str,
    name: &str,
    _sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    if !name.starts_with("gl") {
        return Ok(None);
    }
    let Some(V::Ref(h)) = args.first().copied() else {
        return Ok(None);
    };
    if h == 0 || !has_state(h) {
        return Ok(None);
    }
    if std::env::var_os("OVELO_DRAWDBG").is_some() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 60 {
            eprintln!("[gldbg] call {name}");
        }
    }
    let i = |n: usize| args.get(n).map(|v| i32v(*v)).unwrap_or(0);
    let f = |n: usize| args.get(n).map(f32v).unwrap_or(0.0);
    Ok(Some(match name {
        "glClearColor" => {
            let a = argb(&[f(1), f(2), f(3), f(4)]);
            state(h, |s| s.clear = a);
            None
        }
        "glClear" => {
            state(h, |s| {
                s.prims.push(Prim {
                    mode: -1,
                    verts: Vec::new(),
                    colors: vec![s.clear],
                });
            });
            None
        }
        "glViewport" => {
            state(h, |s| s.vp = (i(1), i(2), i(3), i(4)));
            None
        }
        "glMatrixMode" => {
            state(h, |s| s.mode = i(1));
            None
        }
        "glLoadIdentity" => {
            state(h, |s| match s.mode {
                GL_PROJECTION => s.proj = ident(),
                _ => s.model = ident(),
            });
            None
        }
        "glPushMatrix" => {
            state(h, |s| match s.mode {
                GL_PROJECTION => s.proj_stack.push(s.proj),
                _ => s.model_stack.push(s.model),
            });
            None
        }
        "glPopMatrix" => {
            state(h, |s| match s.mode {
                GL_PROJECTION => {
                    if let Some(m) = s.proj_stack.pop() {
                        s.proj = m;
                    }
                }
                _ => {
                    if let Some(m) = s.model_stack.pop() {
                        s.model = m;
                    }
                }
            });
            None
        }
        "glLoadMatrixf" | "glLoadMatrixx" => {
            let m = read_mat(vm, args.get(1), args.get(2));
            state(h, |s| match s.mode {
                GL_PROJECTION => s.proj = m,
                _ => s.model = m,
            });
            None
        }
        "glMultMatrixf" | "glMultMatrixx" => {
            let m = read_mat(vm, args.get(1), args.get(2));
            state(h, |s| match s.mode {
                GL_PROJECTION => s.proj = mul(&s.proj, &m),
                _ => s.model = mul(&s.model, &m),
            });
            None
        }
        "glOrthof" | "glOrthox" => {
            let (l, r, b, t, n, fa) = (f(1), f(2), f(3), f(4), f(5), f(6));
            let m = [
                2.0 / (r - l),
                0.0,
                0.0,
                0.0,
                0.0,
                2.0 / (t - b),
                0.0,
                0.0,
                0.0,
                0.0,
                -2.0 / (fa - n),
                0.0,
                -(r + l) / (r - l),
                -(t + b) / (t - b),
                -(fa + n) / (fa - n),
                1.0,
            ];
            state(h, |s| match s.mode {
                GL_PROJECTION => s.proj = mul(&s.proj, &m),
                _ => s.model = mul(&s.model, &m),
            });
            None
        }
        "glTranslatef" | "glTranslatex" => {
            let m = [
                1.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0, //
                f(1), f(2), f(3), 1.0,
            ];
            state(h, |s| s.model = mul(&s.model, &m));
            None
        }
        "glScalef" | "glScalex" => {
            let m = [
                f(1),
                0.0,
                0.0,
                0.0,
                0.0,
                f(2),
                0.0,
                0.0,
                0.0,
                0.0,
                f(3),
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ];
            state(h, |s| s.model = mul(&s.model, &m));
            None
        }
        "glRotatef" | "glRotatex" => {
            let (ang, x, y, z) = (f(1).to_radians(), f(2), f(3), f(4));
            let (s, c) = (ang.sin(), ang.cos());
            let m = if z != 0.0 {
                [
                    c + x * x * (1.0 - c),
                    x * y * (1.0 - c) + z * s,
                    0.0,
                    0.0,
                    x * y * (1.0 - c) - z * s,
                    c + y * y * (1.0 - c),
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                ]
            } else {
                ident()
            };
            state(h, |s| s.model = mul(&s.model, &m));
            None
        }
        "glColor4f" | "glColor4x" => {
            let a = argb(&[f(1), f(2), f(3), f(4)]);
            state(h, |s| s.color = a);
            None
        }
        "glVertexPointer" => {
            state(h, |s| {
                s.vptr = args.get(4).and_then(|v| match v {
                    V::Ref(b) if *b != 0 => Some(Arr {
                        buf: *b,
                        size: i(1),
                        ty: i(2),
                        stride: i(3),
                    }),
                    _ => None,
                });
            });
            None
        }
        "glColorPointer" => {
            state(h, |s| {
                s.cptr = args.get(4).and_then(|v| match v {
                    V::Ref(b) if *b != 0 => Some(Arr {
                        buf: *b,
                        size: i(1),
                        ty: i(2),
                        stride: i(3),
                    }),
                    _ => None,
                });
            });
            None
        }
        "glEnableClientState" | "glDisableClientState" => {
            state(h, |s| {
                let on = name.starts_with("glEnable");
                if i(1) == GL_COLOR_ARRAY && !on {
                    s.cptr = None;
                }
            });
            None
        }
        "glLineWidth" | "glLineWidthx" => {
            state(h, |s| s.line_w = f(1).max(0.5));
            None
        }
        "glDrawArrays" => {
            record(vm, h, i(1), i(2), i(3));
            None
        }
        "glDrawElements" => {
            if let Some(V::Ref(b)) = args.get(4).copied() {
                record_el(vm, h, i(1), i(2), i(3), b);
            }
            None
        }
        "glGetError" => Some(V::I32(0)),
        _ => None, // every other gl* call is a harmless no-op here
    }))
}

/// Read a 4x4 matrix arg — `float[16]` guest array or a FloatBuffer.
fn read_mat(vm: &Vm, v: Option<&V>, off: Option<&V>) -> [f32; 16] {
    let mut m = ident();
    if let Some(V::Ref(h)) = v {
        match vm.heap.get(*h) {
            Some(Obj::Array { data, .. }) => {
                let o = off.map(|v| i32v(*v)).unwrap_or(0).max(0) as usize;
                for (k, e) in m.iter_mut().enumerate() {
                    if let Some(v) = data.get(o + k) {
                        *e = f32v(v);
                    }
                }
            }
            Some(Obj::Host {
                state: HostState::Bytes(..),
                ..
            }) => {
                let a = Arr {
                    buf: *h,
                    size: 4,
                    ty: 0x1406,
                    stride: 0,
                };
                for k in 0..4 {
                    let c = read_arr(vm, &a, k as i32);
                    m[k * 4..k * 4 + 4].copy_from_slice(&c);
                }
            }
            _ => {}
        }
    }
    m
}
