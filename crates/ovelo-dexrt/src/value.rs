//! Runtime value model and object heap (design §04 "Heap and threads").

use std::collections::HashMap;

/// A register / field value. Wide values (`I64`, `F64`) are stored whole in
/// one slot; the DEX pair-register convention is handled by the VM mirroring
/// the value into the odd register.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum V {
    /// 32-bit int (also boolean/byte/char/short).
    I32(i32),
    /// 64-bit long.
    I64(i64),
    /// 32-bit float (raw bits preserved).
    F32(f32),
    /// 64-bit double.
    F64(f64),
    /// Heap handle; `0` is null.
    Ref(u32),
}

impl V {
    /// Default for a descriptor's slot.
    pub fn default_for(descriptor: &str) -> V {
        match descriptor {
            "J" => V::I64(0),
            "F" => V::F32(0.0),
            "D" => V::F64(0.0),
            "Z" | "B" | "S" | "C" | "I" => V::I32(0),
            _ => V::Ref(0), // object/array descriptors
        }
    }
}

/// A heap object. `Instance` fields are keyed by global `field_idx`
/// (`(dex_i << 32) | local_idx`), not by slot — field order differences
/// across classes can't alias, and multi-dex indexes can't collide.
#[derive(Debug)]
pub enum Obj {
    /// Class instance.
    Instance {
        /// Global `type_idx` of the class.
        class_idx: u64,
        /// Global `field_idx` → value.
        fields: HashMap<u64, V>,
    },
    /// Array (primitive or reference — elements keep their own type).
    Array {
        /// Element type descriptor (e.g. `I`, `Ljava/lang/String;`).
        elem: String,
        /// Element values.
        data: Vec<V>,
    },
    /// Interned `java.lang.String`.
    Str(String),
    /// `java.lang.Class` mirror for `const-class`.
    Class {
        /// Global `type_idx` the literal names.
        type_idx: u64,
    },
    /// Throwable object (thrown by VM builtins or guest `throw`).
    Throwable {
        /// Descriptor of the throwable class (`Ljava/lang/Throwable;`).
        descriptor: String,
        /// Diagnostic detail (message).
        detail: String,
    },
    /// Host-provided class (framework/JDK): the class isn't defined in
    /// the DEX, so all behavior comes from builtins. `fields` keeps
    /// guest-visible field values (e.g. `PackageInfo.versionCode` read
    /// through iget); `state` is the builtin payload the class's methods
    /// operate on (StringBuilder buffer, collection contents, …).
    Host {
        /// Global `type_idx` of the referenced class — `u64::MAX` when the
        /// descriptor isn't in any loaded DEX's type table (builtin-created
        /// objects); `desc` always names the class.
        class_idx: u64,
        /// Class descriptor (e.g. `Ljava/util/ArrayList;`).
        desc: String,
        /// Global `field_idx` → value, same keying as `Instance`.
        fields: HashMap<u64, V>,
        /// Builtin state chosen by the class descriptor.
        state: HostState,
    },
}

/// Streaming message-digest state.
#[derive(Debug)]
pub enum DigestState {
    /// SHA-1.
    Sha1(sha1::Sha1),
    /// SHA-256.
    Sha256(sha2::Sha256),
    /// SHA-384.
    Sha384(sha2::Sha384),
    /// SHA-512.
    Sha512(sha2::Sha512),
}

/// Builtin payload of a [`Obj::Host`]. Objects whose class needs no
/// internal state carry `Empty` — methods then key off the descriptor.
#[derive(Debug)]
pub enum HostState {
    /// No payload (Intent, managers, stub services).
    Empty,
    /// `StringBuilder`/`StringBuffer` buffer.
    StrBuf(String),
    /// Ordered elements: `ArrayList`/`LinkedList`/`*Deque`/`*Set`.
    List(Vec<V>),
    /// Insertion-ordered entries: `HashMap`/`LinkedHashMap`/`Hashtable`.
    Map(Vec<(V, V)>),
    /// Iterator over a snapshot taken at `iterator()` time.
    Iter(std::rc::Rc<Vec<V>>, usize),
    /// Epoch milliseconds (`Date`/`Calendar`/`Instant`-like).
    Millis(i64),
    /// One mutable slot (`Atomic*` holders, boxed primitives).
    Slot(V),
    /// A filesystem path (`java.io.File`).
    Path(String),
    /// A streaming `java.security.MessageDigest`.
    Digest(Box<DigestState>),
    /// A regex pattern string (`java.util.regex.Pattern`).
    Regex(String),
    /// A readable byte stream (`InputStream` over real bytes — raw
    /// resources, asset files): buffer + read cursor.
    Bytes(std::rc::Rc<Vec<u8>>, usize),
    /// A readable char stream (`Reader` over UTF-16 units —
    /// `StringReader`/`CharArrayReader`/`InputStreamReader`):
    /// buffer + read cursor + mark.
    Chars(std::rc::Rc<Vec<u16>>, usize, usize),
    /// A parsed ZIP/JAR archive.
    ZipArchive {
        /// Complete archive bytes.
        bytes: std::rc::Rc<Vec<u8>>,
        /// Central-directory entries.
        entries: std::rc::Rc<Vec<ovelo_apk::zip::CentralEntry>>,
        /// Verified first signer certificate DER, when v1 signing is present.
        cert_der: Option<std::rc::Rc<Vec<u8>>>,
    },
    /// One ZIP/JAR entry, carrying enough archive data to open it later.
    ZipEntry {
        /// Complete archive bytes.
        bytes: std::rc::Rc<Vec<u8>>,
        /// Central-directory record for this entry.
        entry: ovelo_apk::zip::CentralEntry,
        /// Verified signer certificate DER inherited from the archive.
        cert_der: Option<std::rc::Rc<Vec<u8>>>,
    },
    /// A real writable file (`FileOutputStream` — SAF
    /// `openOutputStream` resolves documents to host files).
    File(std::fs::File),
    /// A decoded image (`Bitmap`).
    Bitmap {
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Straight (non-premultiplied) RGBA8 pixel buffer.
        rgba: std::rc::Rc<Vec<u8>>,
    },
    /// An `XmlResourceParser`: binary-XML elements plus a precomputed
    /// pull-parser event stream and the cursor into it.
    Xml {
        /// Document elements in document order (start-elements only).
        elements: std::rc::Rc<Vec<ovelo_apk::manifest::Element>>,
        /// Pull-parser events; element indices refer into `elements`.
        events: Vec<XmlEvent>,
        /// Cursor: index of the current event in `events`.
        pos: usize,
    },
    /// An `android.graphics.Path`: absolute drawing commands in the
    /// path's own coordinate space.
    GraphicsPath(Vec<PathCmd>),
    /// A recording `android.graphics.Canvas`: draw calls from a guest
    /// `View.onDraw` append [`CanvasOp`]s the host view materializer
    /// replays into the real surface.
    CanvasOps(Vec<CanvasOp>),
}

/// One absolute `android.graphics.Path` command. Relative path methods
/// are resolved when the builtin records them.
#[derive(Debug, Clone, Copy)]
pub enum PathCmd {
    /// Start a new contour.
    MoveTo {
        /// X coordinate.
        x: f32,
        /// Y coordinate.
        y: f32,
    },
    /// Straight segment.
    LineTo {
        /// End x.
        x: f32,
        /// End y.
        y: f32,
    },
    /// Quadratic Bézier segment.
    QuadTo {
        /// Control x.
        cx: f32,
        /// Control y.
        cy: f32,
        /// End x.
        x: f32,
        /// End y.
        y: f32,
    },
    /// Cubic Bézier segment.
    CubicTo {
        /// First control x.
        cx1: f32,
        /// First control y.
        cy1: f32,
        /// Second control x.
        cx2: f32,
        /// Second control y.
        cy2: f32,
        /// End x.
        x: f32,
        /// End y.
        y: f32,
    },
    /// Close the current contour.
    Close,
}

/// One recorded `Canvas` call from a guest `View.onDraw`, in local
/// (view-relative) coordinates, for host-side replay.
#[derive(Debug, Clone)]
pub enum CanvasOp {
    /// `save()` — snapshot transform+clip.
    Save,
    /// `restore()` — pop the last save.
    Restore,
    /// `translate(dx, dy)`.
    Translate {
        /// Horizontal shift px.
        dx: f32,
        /// Vertical shift px.
        dy: f32,
    },
    /// `clipRect(l, t, r, b)` (intersect).
    ClipRect {
        /// Left edge.
        l: f32,
        /// Top edge.
        t: f32,
        /// Right edge.
        r: f32,
        /// Bottom edge.
        b: f32,
    },
    /// `drawColor`/`drawPaint` — fill the clip with the paint color.
    Color {
        /// Packed ARGB.
        argb: i32,
    },
    /// `drawRect` (all rect-typed overloads flatten to edges).
    Rect {
        /// Left edge.
        l: f32,
        /// Top edge.
        t: f32,
        /// Right edge.
        r: f32,
        /// Bottom edge.
        b: f32,
        /// Paint at call time.
        paint: PaintState,
    },
    /// `drawRoundRect`.
    RoundRect {
        /// Left edge.
        l: f32,
        /// Top edge.
        t: f32,
        /// Right edge.
        r: f32,
        /// Bottom edge.
        b: f32,
        /// Corner x radius.
        rx: f32,
        /// Corner y radius.
        ry: f32,
        /// Paint at call time.
        paint: PaintState,
    },
    /// `drawOval`.
    Oval {
        /// Left edge.
        l: f32,
        /// Top edge.
        t: f32,
        /// Right edge.
        r: f32,
        /// Bottom edge.
        b: f32,
        /// Paint at call time.
        paint: PaintState,
    },
    /// `drawCircle`.
    Circle {
        /// Center x.
        cx: f32,
        /// Center y.
        cy: f32,
        /// Radius.
        r: f32,
        /// Paint at call time.
        paint: PaintState,
    },
    /// `drawLine`.
    Line {
        /// Start x.
        x0: f32,
        /// Start y.
        y0: f32,
        /// End x.
        x1: f32,
        /// End y.
        y1: f32,
        /// Paint at call time.
        paint: PaintState,
    },
    /// `drawText` (pen at `(x, baseline)`).
    Text {
        /// The string content.
        text: String,
        /// Pen x.
        x: f32,
        /// Baseline y.
        y: f32,
        /// Paint at call time.
        paint: PaintState,
    },
    /// Flat-color convex polygon (GL triangle fan/strip flattened to
    /// a fill) — `pts` is x,y pairs in canvas space.
    FillPoly {
        /// x,y vertex pairs.
        pts: Vec<f32>,
        /// Packed ARGB.
        argb: i32,
    },
    /// `drawPath` — fill or stroke the recorded contours.
    Path {
        /// Absolute path commands.
        cmds: Vec<PathCmd>,
        /// Android Path.FillType ordinal (0 winding, 1 even-odd).
        fill_type: i32,
        /// Paint at call time.
        paint: PaintState,
    },
    /// `drawBitmap` — decoded pixels at `(x, y)`, scaled to `(w, h)`
    /// when both are nonzero (else natural size).
    Bitmap {
        /// Dest x.
        x: f32,
        /// Dest y.
        y: f32,
        /// Dest width (0 = natural).
        w: u32,
        /// Dest height (0 = natural).
        h: u32,
        /// Source bitmap width.
        src_w: u32,
        /// Source bitmap height.
        src_h: u32,
        /// Straight RGBA8 pixels (`src_w` × `src_h`).
        rgba: std::rc::Rc<Vec<u8>>,
        /// Paint at call time (color filters tint bitmap draws).
        paint: PaintState,
    },
    /// `drawRenderNode` — a *reference* to the node's recording, not a
    /// copy: a re-recorded child layer must show through without the
    /// parent re-recording (AOSP display lists store node references).
    /// Expanded against the live heap when ops leave the runtime —
    /// framework replay never sees this variant.
    Nested {
        /// `RenderNode` heap handle.
        node: u32,
    },
}

/// Snapshot of a `Paint` object's state at draw time.
#[derive(Debug, Clone, Copy)]
pub struct PaintState {
    /// `setColor` ARGB.
    pub argb: i32,
    /// SRC_IN-style color-filter tint, when one is installed.
    pub tint_argb: Option<i32>,
    /// `setTextSize` px (0 = unset).
    pub text_size: f32,
    /// `setTextAlign`: 0 LEFT, 1 CENTER, 2 RIGHT.
    pub align: u8,
    /// `setStyle`: 0 FILL, 1 STROKE, 2 FILL_AND_STROKE.
    pub style: u8,
    /// `setStrokeWidth` px.
    pub stroke_w: f32,
    /// `setFakeBoldText`.
    pub fake_bold: bool,
}

impl PaintState {
    /// Color used for solid fills/strokes after color-filter tinting.
    pub fn draw_argb(&self) -> i32 {
        match self.tint_argb {
            Some(tint) => {
                let paint_a = (self.argb as u32) >> 24;
                let tint_a = (tint as u32) >> 24;
                let a = tint_a * paint_a / 255;
                ((a << 24) | ((tint as u32) & 0x00ff_ffff)) as i32
            }
            None => self.argb,
        }
    }
}

/// One `XmlPullParser` event inside an [`HostState::Xml`] stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmlEvent {
    /// `XmlPullParser.START_DOCUMENT`.
    StartDocument,
    /// `XmlPullParser.START_TAG` for element at the given index.
    Start(usize),
    /// `XmlPullParser.END_TAG` for element at the given index.
    End(usize),
    /// `XmlPullParser.END_DOCUMENT`.
    EndDocument,
}

/// Build the pull-parser event stream for flat start-elements:
/// `START_DOCUMENT`, per-element `START_TAG`/`END_TAG` reconstructed
/// from recorded depths, then `END_DOCUMENT`.
pub fn xml_events(elements: &[ovelo_apk::manifest::Element]) -> Vec<XmlEvent> {
    let mut events = vec![XmlEvent::StartDocument];
    let mut stack: Vec<usize> = Vec::new();
    for (i, el) in elements.iter().enumerate() {
        while stack.len() > el.depth {
            if let Some(open) = stack.pop() {
                events.push(XmlEvent::End(open));
            }
        }
        events.push(XmlEvent::Start(i));
        stack.push(i);
    }
    while let Some(open) = stack.pop() {
        events.push(XmlEvent::End(open));
    }
    events.push(XmlEvent::EndDocument);
    events
}

/// Object heap with string interning.
#[derive(Default)]
pub struct Heap {
    objs: Vec<Option<Obj>>,
    interned: HashMap<String, u32>,
    /// Sweep-freed slots (1-based handles) recycled by `alloc`.
    free: Vec<u32>,
}

impl Heap {
    /// Allocate; returns the handle (1-based; 0 is reserved for null).
    pub fn alloc(&mut self, o: Obj) -> u32 {
        if let Some(h) = self.free.pop() {
            self.objs[h as usize - 1] = Some(o);
            return h;
        }
        self.objs.push(Some(o));
        self.objs.len() as u32
    }

    /// Number of slots ever allocated (live + free).
    pub fn len(&self) -> usize {
        self.objs.len()
    }

    /// True when no objects were ever allocated.
    pub fn is_empty(&self) -> bool {
        self.objs.is_empty()
    }

    /// Stop-the-world mark-sweep: `roots` plus every interned string
    /// are kept; unmarked slots are freed and their handles recycled.
    /// Returns the number of freed objects.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = u32>) -> usize {
        fn handle(v: &V) -> Option<u32> {
            match v {
                V::Ref(h) if *h != 0 => Some(*h),
                _ => None,
            }
        }
        let mut marked = vec![false; self.objs.len()];
        let mut wl: Vec<u32> = roots
            .into_iter()
            .chain(self.interned.values().copied())
            .filter(|&h| h != 0 && (h as usize) <= self.objs.len())
            .collect();
        while let Some(h) = wl.pop() {
            let i = h as usize - 1;
            if marked[i] {
                continue;
            }
            marked[i] = true;
            match &self.objs[i] {
                Some(Obj::Instance { fields, .. }) => {
                    wl.extend(fields.values().filter_map(handle));
                }
                Some(Obj::Array { data, .. }) => {
                    wl.extend(data.iter().filter_map(handle));
                }
                Some(Obj::Host { fields, state, .. }) => {
                    wl.extend(fields.values().filter_map(handle));
                    match state {
                        HostState::List(l) => {
                            wl.extend(l.iter().filter_map(handle));
                        }
                        HostState::Map(m) => {
                            wl.extend(m.iter().flat_map(|(k, v)| [k, v]).filter_map(handle));
                        }
                        HostState::Iter(rc, _) => {
                            wl.extend(rc.iter().filter_map(handle));
                        }
                        HostState::Slot(v) => {
                            wl.extend(handle(v));
                        }
                        // Display-list recordings reference RenderNode
                        // handles — a node reachable only through a
                        // parent's display list must survive or child
                        // layers vanish after a GC.
                        HostState::CanvasOps(ops) => {
                            wl.extend(ops.iter().filter_map(|op| match op {
                                CanvasOp::Nested { node }
                                    if *node != 0 && (*node as usize) <= self.objs.len() =>
                                {
                                    Some(*node)
                                }
                                _ => None,
                            }));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if crate::dbg_flag("OVELO_MEMDBG") {
            let mut kinds: HashMap<&'static str, usize> = HashMap::new();
            for (i, slot) in self.objs.iter().enumerate() {
                if marked[i] {
                    *kinds
                        .entry(match slot {
                            Some(Obj::Instance { .. }) => "instance",
                            Some(Obj::Array { .. }) => "array",
                            Some(Obj::Str(_)) => "str",
                            Some(Obj::Class { .. }) => "class",
                            Some(Obj::Throwable { .. }) => "throwable",
                            Some(Obj::Host { .. }) => "host",
                            None => "none",
                        })
                        .or_default() += 1;
                }
            }
            eprintln!("[gc-kinds] {kinds:?} interned={}", self.interned.len());
        }
        let mut freed = 0;
        for (i, slot) in self.objs.iter_mut().enumerate() {
            if slot.is_some() && !marked[i] {
                *slot = None;
                self.free.push(i as u32 + 1);
                freed += 1;
            }
        }
        // Trailing free slots keep len() (and the memory behind it)
        // pinned at the all-time high-water mark — drop them so the
        // vector tracks live objects instead.
        while self.objs.last().is_some_and(Option::is_none) {
            self.objs.pop();
        }
        self.free.retain(|&h| (h as usize) <= self.objs.len());
        if self.objs.capacity() > (1 << 16) && self.objs.len() < self.objs.capacity() / 4 {
            self.objs.shrink_to_fit();
        }
        freed
    }

    /// Borrow an object by handle.
    pub fn get(&self, h: u32) -> Option<&Obj> {
        if h == 0 {
            return None; // null is not a heap slot
        }
        self.objs.get(h as usize - 1).and_then(|o| o.as_ref())
    }

    /// Borrow mutably.
    pub fn get_mut(&mut self, h: u32) -> Option<&mut Obj> {
        if h == 0 {
            return None;
        }
        self.objs.get_mut(h as usize - 1).and_then(|o| o.as_mut())
    }

    /// Allocate a non-interned string value. Runtime-produced Java
    /// strings are ordinary objects; only literals and explicit
    /// `String.intern()` are identity-canonicalized.
    pub fn alloc_str(&mut self, s: &str) -> u32 {
        self.alloc(Obj::Str(s.to_string()))
    }

    /// Intern an existing string object, returning the pooled handle.
    /// Unlike `intern_str`, first-time interning keeps the receiver as
    /// the canonical object, preserving Java's `s.intern() == s` for a
    /// newly pooled runtime string.
    pub fn intern_handle(&mut self, s: &str, h: u32) -> u32 {
        if let Some(&existing) = self.interned.get(s) {
            return existing;
        }
        self.interned.insert(s.to_string(), h);
        h
    }

    /// Intern a string value; the same `V::Ref` comes back for equal text.
    pub fn intern_str(&mut self, s: &str) -> u32 {
        if let Some(&h) = self.interned.get(s) {
            return h;
        }
        let h = self.alloc(Obj::Str(s.to_string()));
        self.interned.insert(s.to_string(), h);
        h
    }

    /// The interned text behind a string handle.
    pub fn str_val(&self, h: u32) -> Option<&str> {
        match self.get(h) {
            Some(Obj::Str(s)) => Some(s),
            _ => None,
        }
    }

    /// Global class type index of a handle (`Instance`/`Host`/`Throwable`
    /// report their class descriptor index where applicable).
    pub fn class_of(&self, h: u32) -> Option<u64> {
        match self.get(h)? {
            Obj::Instance { class_idx, .. } | Obj::Host { class_idx, .. } => Some(*class_idx),
            _ => None,
        }
    }
}
