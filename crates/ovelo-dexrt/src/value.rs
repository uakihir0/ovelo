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
    /// A regex pattern string (`java.util.regex.Pattern`).
    Regex(String),
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
}

impl Heap {
    /// Allocate; returns the handle (1-based; 0 is reserved for null).
    pub fn alloc(&mut self, o: Obj) -> u32 {
        self.objs.push(Some(o));
        self.objs.len() as u32
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
