//! JNI local/global reference management.
//!
//! Handles index a shared handle table; the actual objects live in the DEX
//! heap, so a JNI handle is an indirection to a heap ref (`V::Ref`-style
//! u32), or the invalid handle 0 (`null`).

use std::collections::HashSet;

/// Handle 0 = JNI `NULL`.
pub const NULL: u32 = 0;

/// Local/global reference table for one native call frame stack.
pub struct Handles {
    /// Table slot → heap ref the handle points at (slot 0 unused = NULL).
    table: Vec<u32>,
    /// Free slot list.
    free: Vec<u32>,
    /// Global refs: slots kept alive across frames.
    globals: HashSet<u32>,
    /// Local frame stack: each entry is the list of slots allocated since
    /// the matching `PushLocalFrame`.
    frames: Vec<Vec<u32>>,
}

impl Handles {
    /// Empty table (slot 0 reserved as NULL).
    pub fn new() -> Self {
        Self {
            table: vec![0],
            free: Vec::new(),
            globals: HashSet::new(),
            frames: vec![Vec::new()], // outermost local frame
        }
    }

    fn alloc(&mut self, heap_ref: u32) -> u32 {
        let h = if let Some(h) = self.free.pop() {
            self.table[h as usize] = heap_ref;
            h
        } else {
            self.table.push(heap_ref);
            (self.table.len() - 1) as u32
        };
        self.frames.last_mut().unwrap().push(h);
        h
    }

    /// `NewLocalRef` — create a local handle to `heap_ref`.
    pub fn new_local(&mut self, heap_ref: u32) -> u32 {
        if heap_ref == 0 {
            return NULL;
        }
        self.alloc(heap_ref)
    }

    /// `NewGlobalRef` — promote/allocate a global handle.
    pub fn new_global(&mut self, h: u32) -> u32 {
        if h == NULL {
            return NULL;
        }
        let g = self.alloc(self.table[h as usize]);
        self.globals.insert(g);
        g
    }

    /// `DeleteGlobalRef`.
    pub fn delete_global(&mut self, h: u32) {
        if self.globals.remove(&h) {
            self.forget_frame(h);
            self.free(h);
        }
    }

    /// `DeleteLocalRef`.
    pub fn delete_local(&mut self, h: u32) {
        if h != NULL && !self.globals.contains(&h) {
            self.forget_frame(h);
            self.free(h);
        }
    }

    /// Drop `h` from every frame's record so `pop_frame` cannot free it
    /// a second time.
    fn forget_frame(&mut self, h: u32) {
        for f in &mut self.frames {
            f.retain(|&x| x != h);
        }
    }

    fn free(&mut self, h: u32) {
        self.table[h as usize] = 0;
        self.free.push(h);
    }

    /// `PushLocalFrame` — open a new local frame.
    pub fn push_frame(&mut self) {
        self.frames.push(Vec::new());
    }

    /// `PopLocalFrame` — free every local handle allocated in the frame;
    /// `result` is a handle to keep alive (re-created in the parent frame).
    pub fn pop_frame(&mut self, result: u32) -> u32 {
        let frame = self.frames.pop().unwrap_or_default();
        let keep = if result != NULL {
            Some(self.table[result as usize])
        } else {
            None
        };
        for h in frame {
            if !self.globals.contains(&h) {
                self.free(h);
            }
        }
        if self.frames.is_empty() {
            self.frames.push(Vec::new());
        }
        keep.map(|r| self.new_local(r)).unwrap_or(NULL)
    }

    /// Resolve a handle to its heap ref (`NULL` → 0).
    pub fn get(&self, h: u32) -> u32 {
        if h == NULL {
            return 0;
        }
        self.table.get(h as usize).copied().unwrap_or(0)
    }

    /// `IsSameObject`.
    pub fn same(&self, a: u32, b: u32) -> bool {
        self.get(a) == self.get(b)
    }

    /// Live handle count (diagnostics).
    pub fn live(&self) -> usize {
        self.table.len() - self.free.len() - 1
    }
}

impl Default for Handles {
    fn default() -> Self {
        Self::new()
    }
}
