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

/// A heap object. `Instance` fields are keyed by `field_idx` (per the DEX
/// field table), not by slot — field order differences across classes can't
/// alias.
#[derive(Debug)]
pub enum Obj {
    /// Class instance.
    Instance {
        /// `type_idx` of the class.
        class_idx: u32,
        /// `field_idx` → value.
        fields: HashMap<u32, V>,
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
        /// `type_idx` the literal names.
        type_idx: u32,
    },
    /// Throwable object (thrown by VM builtins or guest `throw`).
    Throwable {
        /// Descriptor of the throwable class (`Ljava/lang/Throwable;`).
        descriptor: String,
        /// Diagnostic detail (message).
        detail: String,
    },
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
        self.objs
            .get_mut(h as usize - 1)
            .and_then(|o| o.as_mut())
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

    /// Class type index of a handle (`Instance`/`Throwable` report their
    /// class descriptor index where applicable).
    pub fn class_of(&self, h: u32) -> Option<u32> {
        match self.get(h)? {
            Obj::Instance { class_idx, .. } => Some(*class_idx),
            _ => None,
        }
    }
}
