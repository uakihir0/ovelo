//! ovelo-dexrt — DEX interpreter (design §04, P1 scope).
//!
//! Executes verified DEX: typed heap handles, frame stack, int/long/float/
//! double arithmetic with Android's wrapping and trap semantics, arrays with
//! bounds checks, virtual/interface dispatch, `<clinit>` once-per-class, and
//! exception propagation through `try`/`catch`. Single UI-looper thread;
//! monitors are no-ops on one thread. Unimplemented opcodes stop with
//! `DexUnsupported` — never silently.

mod value;

pub use value::{Heap, Obj, V};

use ovelo_dex::{decode_all, ClassData, ClassDef, DexFile, EncodedMethod, Insn, TryItem};
use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use std::collections::HashMap;

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::DexUnsupported, Phase::DexExec, cause)
}
fn errd(cause: &str, detail: String) -> OveloError {
    OveloError::new(ErrorCategory::DexUnsupported, Phase::DexExec, cause)
        .with_detail(detail)
}

/// Internal control flow: a guest `throw` carries a heap handle, VM faults
/// carry an [`OveloError`].
enum E {
    Vm(OveloError),
    Throw(u32),
}
type Rt<T> = std::result::Result<T, E>;
fn vm<T>(c: &'static str) -> Rt<T> {
    Err(E::Vm(err(c)))
}

/// Runtime state of one loaded class.
#[derive(Default)]
struct ClassRt {
    statics: HashMap<u32, V>,
    /// 0 = pending, 1 = running (reentrancy guard), 2 = done.
    clinit: u8,
}

/// A resolved method: where it is declared plus its code.
#[derive(Clone)]
struct Loc {
    /// Owning class (`type_idx`) the method was found on.
    class_idx: u32,
    /// The method table entry.
    em: EncodedMethod,
}

/// A host-implemented framework method. Registered per `method_idx`;
/// `call_builtin` consults this table before the builtin stubs.
/// Single-threaded: the shared RefCell lets a callback path (e.g. a
/// deferred onClick) re-enter the same table entry safely.
pub type HostCall = std::rc::Rc<
    std::cell::RefCell<dyn FnMut(&mut Vm, &[V]) -> Result<Option<V>>>,
>;

/// DEX interpreter over a single [`DexFile`].
pub struct Vm {
    dex: DexFile,
    /// Object heap (public for tests / host plumbing).
    pub heap: Heap,
    classes: HashMap<u32, ClassRt>,
    /// method_idx → defining (class_idx, EncodedMethod).
    defined: HashMap<u32, (u32, EncodedMethod)>,
    /// class type_idx → class_def index.
    defs: HashMap<u32, usize>,
    /// method_idx → host implementation (framework bridge).
    hosts: HashMap<u32, HostCall>,
    /// (name, sig) → candidate host fns with the class idx they were
    /// registered on; used when the DEX references a framework method
    /// through a subclass descriptor. Only a reference whose class is
    /// the owner or a subclass of it may resolve — see `host_named`.
    hosts_named: HashMap<(String, String), Vec<(u32, HostCall)>>,
}

impl Vm {
    /// Wrap a verified DEX file.
    pub fn new(dex: DexFile) -> Result<Self> {
        let mut defined = HashMap::new();
        for (class_idx, em) in dex.methods()? {
            defined.insert(em.method_idx, (class_idx, em));
        }
        let defs = dex
            .class_defs()
            .iter()
            .enumerate()
            .map(|(i, d)| (d.class_idx, i))
            .collect();
        Ok(Self {
            dex,
            heap: Heap::default(),
            classes: HashMap::new(),
            defined,
            defs,
            hosts: HashMap::new(),
            hosts_named: HashMap::new(),
        })
    }

    /// Register a host implementation for the method reference
    /// `class_desc.name proto_desc` (e.g.
    /// `Landroid/app/Activity;` `setContentView`
    /// `(Landroid/view/View;)V`). Returns `false` when the DEX holds no
    /// such method reference — the app never calls it.
    pub fn register_host(
        &mut self,
        class_desc: &str,
        name: &str,
        proto_desc: &str,
        f: HostCall,
    ) -> Result<bool> {
        let mut found = false;
        for midx in 0..self.dex.method_ids_len() as u32 {
            let mid = self.dex.method(midx)?;
            let class = self.dex.type_descriptor(mid.class_idx as u32)?;
            if class != class_desc {
                continue;
            }
            if self.dex.string(mid.name_idx)? != name {
                continue;
            }
            let p = self.dex.proto(mid.proto_idx as u32)?;
            let mut sig = String::from("(");
            for t in self.dex.proto_params(p)? {
                sig.push_str(&self.dex.type_descriptor(t as u32)?);
            }
            sig.push(')');
            sig.push_str(&self.dex.type_descriptor(p.return_type_idx)?);
            if sig == proto_desc {
                self.hosts.insert(midx, f.clone());
                found = true;
            }
        }
        if found {
            // A DEX may reference the same method through a subclass
            // descriptor; fall back to (name, sig) in call_builtin.
            let owner = self.dex.type_index_of(class_desc)?.unwrap_or(u32::MAX);
            self.hosts_named
                .entry((name.to_string(), proto_desc.to_string()))
                .or_default()
                .push((owner, f));
        }
        Ok(found)
    }

    /// Heap borrow.
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// Entry point: invoke `<class>.<method>(args)` as a static call.
    /// Returns `Some(value)` or `None` for void. An uncaught guest exception
    /// surfaces as `dexrt.exception.uncaught`.
    pub fn run(&mut self, class_desc: &str, name: &str, args: &[V]) -> Result<Option<V>> {
        let midx = self
            .find_method_by_name(class_desc, name, args.len())?
            .ok_or_else(|| errd("dexrt.method.missing", format!("{class_desc}.{name}")))?;
        match self.invoke(midx, args.to_vec(), InvokeKind::Static) {
            Ok(v) => Ok(v),
            Err(E::Vm(e)) => Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                Err(errd("dexrt.exception.uncaught", d))
            }
        }
    }

    /// Instantiate `class_desc`: allocates the instance and invokes its
    /// `<init>` (a class without one gets `Object.<init>`'s no-op).
    /// Returns the heap handle.
    pub fn instantiate(&mut self, class_desc: &str) -> Result<u32> {
        let ty = self
            .dex
            .type_index_of(class_desc)?
            .ok_or_else(|| errd("dexrt.class.missing", class_desc.to_string()))?;
        if self.def_of(ty).is_none() {
            return Err(errd("dexrt.class.missing", class_desc.to_string()));
        }
        match self.ensure_init(ty) {
            Ok(()) => {}
            Err(E::Vm(e)) => return Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                return Err(errd("dexrt.exception.uncaught", d));
            }
        }
        let h = self.heap.alloc(Obj::Instance {
            class_idx: ty,
            fields: HashMap::new(),
        });
        if let Some(midx) = self.find_method_by_name(class_desc, "<init>", 0)? {
            match self.invoke(midx, vec![V::Ref(h)], InvokeKind::Direct) {
                Ok(_) => {}
                Err(E::Vm(e)) => return Err(e),
                Err(E::Throw(r)) => {
                    let d = self.describe_throw(r);
                    return Err(errd("dexrt.exception.uncaught", d));
                }
            }
        }
        Ok(h)
    }

    /// Invoke the virtual method `name` on `this`, with `args` after the
    /// receiver. `Ok(None)` when the object's class doesn't declare it —
    /// the platform base class (not in this DEX) provides the default.
    pub fn call_virtual(&mut self, this: u32, name: &str, args: &[V]) -> Result<Option<V>> {
        let ci = self
            .heap
            .class_of(this)
            .ok_or_else(|| errd("dexrt.bad_this", name.to_string()))?;
        let Some(midx) = self.find_method_on_chain(ci, name, args.len())? else {
            return Ok(None);
        };
        let mut a = Vec::with_capacity(args.len() + 1);
        a.push(V::Ref(this));
        a.extend_from_slice(args);
        match self.invoke(midx, a, InvokeKind::Virtual) {
            Ok(v) => Ok(v),
            Err(E::Vm(e)) => Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                Err(errd("dexrt.exception.uncaught", d))
            }
        }
    }

    fn describe_throw(&self, h: u32) -> String {
        match self.heap.get(h) {
            Some(Obj::Throwable { descriptor, detail }) => {
                format!("{descriptor}: {detail}")
            }
            _ => format!("object #{h}"),
        }
    }

    /// `find_method_by_name` over the class's superclass chain — an
    /// inherited callback (e.g. onCreate on a DEX parent) still runs.
    fn find_method_on_chain(
        &self,
        mut class_idx: u32,
        name: &str,
        nslots: usize,
    ) -> Result<Option<u32>> {
        for _ in 0..64 {
            let Some(def) = self.def_of(class_idx) else {
                return Ok(None);
            };
            let desc = self.dex.type_descriptor(def.class_idx)?;
            if let Some(m) = self.find_method_by_name(&desc, name, nslots)? {
                return Ok(Some(m));
            }
            if def.superclass_idx == u32::MAX {
                return Ok(None);
            }
            class_idx = def.superclass_idx;
        }
        Ok(None)
    }

    /// First method `name` on `class_desc` whose params occupy exactly
    /// `nslots` registers (J/D params take two) — args pack as a register
    /// image, so a long/double param counts double.
    fn find_method_by_name(
        &self,
        class_desc: &str,
        name: &str,
        nslots: usize,
    ) -> Result<Option<u32>> {
        for (i, def) in self.dex.class_defs().iter().enumerate() {
            let _ = i;
            if self.dex.type_descriptor(def.class_idx)? != class_desc {
                continue;
            }
            let cd = self.dex.class_data(def)?;
            for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
                let mid = self.dex.method(m.method_idx)?;
                if self.dex.string(mid.name_idx)? != name {
                    continue;
                }
                let p = self.dex.proto(mid.proto_idx as u32)?;
                let mut slots = 0usize;
                for t in self.dex.proto_params(p)? {
                    let d = self.dex.type_descriptor(t as u32)?;
                    slots += if d.starts_with('J') || d.starts_with('D') { 2 } else { 1 };
                }
                if slots == nslots {
                    return Ok(Some(m.method_idx));
                }
            }
        }
        Ok(None)
    }

    fn def_of(&self, class_idx: u32) -> Option<&ClassDef> {
        self.defs.get(&class_idx).map(|&i| &self.dex.class_defs()[i])
    }

    fn class_data_of(&self, class_idx: u32) -> Result<Option<ClassData>> {
        match self.def_of(class_idx) {
            Some(d) => Ok(Some(self.dex.class_data(d)?)),
            None => Ok(None),
        }
    }

    /// `sub` assignable to `sup` through the superclass chain (interfaces are
    /// matched at each level but not walked — P1 scope).
    fn assignable(&self, sub: u32, sup: u32) -> bool {
        if sub == sup {
            return true;
        }
        let mut cur = sub;
        for _ in 0..64 {
            match self.def_of(cur) {
                Some(def) => {
                    if def.interfaces_off != 0 {
                        if let Ok(list) = self.dex.type_list(def.interfaces_off) {
                            if list.contains(&(sup as u16)) {
                                return true;
                            }
                        }
                    }
                    cur = def.superclass_idx;
                    if cur == u32::MAX {
                        break;
                    }
                }
                None => {
                    // Host framework class — no class_def; walk the
                    // builtin hierarchy by descriptor. Intermediate
                    // classes may be absent from the DEX's type table
                    // (e.g. CustomButton extends Button but never names
                    // TextView), so keep going until `sup` or a dead end.
                    let Ok(mut d) = self.dex.type_descriptor(cur) else { break };
                    let sup_desc = self.dex.type_descriptor(sup).unwrap_or_default();
                    loop {
                        if d == sup_desc {
                            return true;
                        }
                        let Some(next) = Self::framework_super(&d) else {
                            return false;
                        };
                        match self.dex.type_index_of(next) {
                            Ok(Some(next)) => {
                                cur = next;
                                break;
                            }
                            _ => d = next.to_string(),
                        }
                    }
                }
            }
            if cur == sup {
                return true;
            }
        }
        false
    }

    /// Superclass descriptor for a host-provided framework class (the
    /// bridged widget tree; `None` at `Object`/unknown).
    fn framework_super(d: &str) -> Option<&'static str> {
        Some(match d {
            "Landroid/view/ViewGroup;" => "Landroid/view/View;",
            "Landroid/widget/LinearLayout;" => "Landroid/view/ViewGroup;",
            "Landroid/widget/TextView;" => "Landroid/view/View;",
            "Landroid/widget/Button;" => "Landroid/widget/TextView;",
            "Landroid/app/Activity;" => "Landroid/content/ContextWrapper;",
            "Landroid/content/ContextWrapper;" => "Landroid/content/Context;",
            "Landroid/view/View;" | "Landroid/content/Context;" => "Ljava/lang/Object;",
            _ => return None,
        })
    }

    /// Known supers of builtin throwable descriptors.
    fn builtin_supers(d: &str) -> &'static [&'static str] {
        match d {
            "Ljava/lang/NullPointerException;"
            | "Ljava/lang/ArrayIndexOutOfBoundsException;"
            | "Ljava/lang/IndexOutOfBoundsException;"
            | "Ljava/lang/ArithmeticException;"
            | "Ljava/lang/ClassCastException;"
            | "Ljava/lang/NegativeArraySizeException;" => &[
                "Ljava/lang/RuntimeException;",
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/RuntimeException;" => &[
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/NoClassDefFoundError;" => &[
                "Ljava/lang/LinkageError;",
                "Ljava/lang/Error;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/ExceptionInInitializerError;" => &[
                "Ljava/lang/LinkageError;",
                "Ljava/lang/Error;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/LinkageError;" => &[
                "Ljava/lang/Error;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/Exception;" | "Ljava/lang/Error;" => &[
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/Throwable;" => &["Ljava/lang/Object;"],
            _ => &[],
        }
    }

    /// Is `h` (non-null) an instance of `type_idx`'s descriptor?
    fn is_instance(&self, h: u32, type_idx: u32) -> bool {
        let Ok(sup) = self.dex.type_descriptor(type_idx) else {
            return false;
        };
        match self.heap.get(h) {
            Some(Obj::Instance { class_idx, .. }) => self.assignable(*class_idx, type_idx),
            Some(Obj::Throwable { descriptor, .. }) => {
                if descriptor == &sup {
                    return true;
                }
                Self::builtin_supers(descriptor).contains(&sup.as_str())
            }
            Some(Obj::Str(_)) => {
                sup == "Ljava/lang/String;" || sup == "Ljava/lang/Object;"
            }
            Some(Obj::Class { .. }) => {
                sup == "Ljava/lang/Class;" || sup == "Ljava/lang/Object;"
            }
            Some(Obj::Array { elem, .. }) => {
                sup == "Ljava/lang/Object;"
                    || sup == format!("[{elem}")
                    || sup.starts_with('[') && sup.ends_with(&elem[..])
            }
            None => false,
        }
    }

    /// Allocate a builtin throwable.
    fn throw_builtin(&mut self, descriptor: &str, detail: &str) -> E {
        let h = self.heap.alloc(Obj::Throwable {
            descriptor: descriptor.to_string(),
            detail: detail.to_string(),
        });
        E::Throw(h)
    }

    /// Run `<clinit>` once per class, superclass first (reentrancy-safe).
    /// `clinit` states: 0 = new, 1 = running, 2 = done, 3 = failed.
    fn ensure_init(&mut self, class_idx: u32) -> Rt<()> {
        let rt = self.classes.entry(class_idx).or_default();
        match rt.clinit {
            2 => return Ok(()),
            1 => return Ok(()), // recursive init: in progress
            3 => {
                return Err(self.throw_builtin(
                    "Ljava/lang/NoClassDefFoundError;",
                    "class initialization previously failed",
                ))
            }
            _ => {}
        }
        self.classes.get_mut(&class_idx).unwrap().clinit = 1;
        let fail = |this: &mut Self, e: E| -> Rt<()> {
            this.classes.get_mut(&class_idx).unwrap().clinit = 3;
            Err(e)
        };
        // Superclasses initialize before their subclasses.
        let sup = self
            .def_of(class_idx)
            .map(|d| d.superclass_idx)
            .unwrap_or(u32::MAX);
        if sup != u32::MAX && sup != class_idx {
            if let Err(e) = self.ensure_init(sup) {
                return fail(self, e);
            }
        }
        let cd = match self.class_data_of(class_idx) {
            Ok(cd) => cd,
            Err(_) => {
                self.classes.get_mut(&class_idx).unwrap().clinit = 2;
                return Ok(());
            }
        };
        if let Some(cd) = cd {
            for m in &cd.direct_methods {
                let mid = self.dex.method(m.method_idx).map_err(E::Vm)?;
                if self.dex.string(mid.name_idx).map_err(E::Vm)? == "<clinit>" {
                    if let Err(e) = self.exec(Loc {
                        class_idx,
                        em: *m,
                    }, Vec::new()) {
                        return fail(self, e);
                    }
                    break;
                }
            }
        }
        self.classes.get_mut(&class_idx).unwrap().clinit = 2;
        Ok(())
    }

    /// Method lookup by method table index (must be defined in this DEX).
    fn resolve_defined(&self, midx: u32) -> Rt<Loc> {
        self.defined
            .get(&midx)
            .map(|&(c, em)| Loc {
                class_idx: c,
                em,
            })
            .ok_or_else(|| E::Vm(err("dexrt.invoke.unresolved")))
    }

    /// Virtual/interface dispatch: find the override in `start_class`'s
    /// virtual table matching the target's (name, proto).
    fn resolve_virtual(&self, midx: u32, start_class: u32) -> Rt<Loc> {
        let mid = self.dex.method(midx).map_err(E::Vm)?;
        let mut cur = Some(start_class);
        for _ in 0..64 {
            let Some(ci) = cur else { break };
            if let Some(cd) = self.class_data_of(ci).map_err(E::Vm)? {
                for m in &cd.virtual_methods {
                    let om = self.dex.method(m.method_idx).map_err(E::Vm)?;
                    if om.name_idx == mid.name_idx && om.proto_idx == mid.proto_idx {
                        return Ok(Loc {
                            class_idx: ci,
                            em: *m,
                        });
                    }
                }
                cur = self
                    .def_of(ci)
                    .map(|d| d.superclass_idx)
                    .filter(|&s| s != u32::MAX);
            } else {
                break;
            }
        }
        // Fall back to the declared method itself (e.g. dispatch target is the
        // declared class — a class that doesn't override).
        self.resolve_defined(midx)
    }

    /// Builtin method table: (class descriptor, name) — never silent success,
    /// only listed signatures are served. Registered host methods win.
    /// Host stub lookup by `(name, proto)` — the DEX may reference a
    /// framework method through a subclass descriptor the exact-midx
    /// registration doesn't cover (e.g. `LSubAct;.setContentView`).
    fn host_named(&self, midx: u32) -> Rt<Option<HostCall>> {
        let mid = self.dex.method(midx).map_err(E::Vm)?;
        let name = self.dex.string(mid.name_idx).map_err(E::Vm)?;
        let p = self.dex.proto(mid.proto_idx as u32).map_err(E::Vm)?;
        let mut sig = String::from("(");
        for t in self.dex.proto_params(p).map_err(E::Vm)? {
            sig.push_str(&self.dex.type_descriptor(t as u32).map_err(E::Vm)?);
        }
        sig.push(')');
        sig.push_str(&self.dex.type_descriptor(p.return_type_idx).map_err(E::Vm)?);
        // The reference's declaring class must be the registered owner
        // or a subclass of it — otherwise an unrelated class naming the
        // same method (e.g. `Dialog.setContentView` when only
        // `Activity.setContentView` is bridged) would steal the call.
        let key = (name.to_string(), sig);
        for (owner, f) in self.hosts_named.get(&key).into_iter().flatten() {
            if *owner == mid.class_idx as u32
                || self.assignable(mid.class_idx as u32, *owner)
            {
                return Ok(Some(f.clone()));
            }
        }
        Ok(None)
    }

    fn call_builtin(&mut self, midx: u32, args: &[V]) -> Rt<Option<V>> {
        let host = self
            .hosts
            .get(&midx)
            .cloned()
            .or(self.host_named(midx)?);
        if let Some(f) = host {
            return f.borrow_mut()(self, args).map_err(E::Vm);
        }
        let mid = self.dex.method(midx).map_err(E::Vm)?;
        let class = self.dex.type_descriptor(mid.class_idx as u32).map_err(E::Vm)?;
        let name = self.dex.string(mid.name_idx).map_err(E::Vm)?;
        match (class.as_str(), name.as_str()) {
            ("Ljava/lang/Object;", "<init>") => Ok(None),
            ("Ljava/lang/Object;", "getClass") => {
                let this = args.first().copied().unwrap_or(V::Ref(0));
                let ci = match this {
                    V::Ref(h) if h != 0 => self.heap.class_of(h).unwrap_or(0),
                    _ => 0,
                };
                let h = self.heap.alloc(Obj::Class { type_idx: ci });
                Ok(Some(V::Ref(h)))
            }
            ("Ljava/lang/String;", "length") => {
                if let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) {
                    if let Some(s) = self.heap.str_val(h) {
                        return Ok(Some(V::I32(s.chars().count() as i32)));
                    }
                }
                Err(self.throw_builtin("Ljava/lang/NullPointerException;", "String.length"))
            }
            _ => {
                // A method whose class isn't in this DEX is a framework
                // stub: the host models the base behavior, so return the
                // proto's default (None for void) instead of failing —
                // e.g. Activity.<init> or super.onCreate().
                // Only constructors and void methods on non-DEX classes are
                // host no-ops — a value-returning stub (Math.abs) answering
                // with a default would let the app compute on a lie.
                if self.def_of(mid.class_idx as u32).is_none() {
                    let p = self.dex.proto(mid.proto_idx as u32).map_err(E::Vm)?;
                    let ret = self
                        .dex
                        .type_descriptor(p.return_type_idx)
                        .map_err(E::Vm)?;
                    if ret == "V" {
                        Ok(None)
                    } else {
                        vm("dexrt.invoke.unresolved")
                    }
                } else {
                    vm("dexrt.invoke.unresolved")
                }
            }
        }
    }

    /// Invoke `midx` per `kind`; `args` includes the receiver for non-static.
    fn invoke(&mut self, midx: u32, args: Vec<V>, kind: InvokeKind) -> Rt<Option<V>> {
        let loc = match kind {
            InvokeKind::Static | InvokeKind::Direct => {
                match self.resolve_defined(midx) {
                    Ok(l) => l,
                    Err(_) => return self.call_builtin(midx, &args),
                }
            }
            InvokeKind::Virtual | InvokeKind::Interface => {
                let recv = match args.first() {
                    Some(&V::Ref(h)) if h != 0 => h,
                    _ => {
                        return Err(self.throw_builtin(
                            "Ljava/lang/NullPointerException;",
                            "null receiver",
                        ))
                    }
                };
                let ci = self.heap.class_of(recv).unwrap_or(u32::MAX);
                match self.resolve_virtual(midx, ci) {
                    Ok(l) => l,
                    Err(_) => return self.call_builtin(midx, &args),
                }
            }
            InvokeKind::Super { caller_class } => {
                let sup = self
                    .def_of(caller_class)
                    .map(|d| d.superclass_idx)
                    .unwrap_or(u32::MAX);
                match self.resolve_virtual(midx, sup) {
                    Ok(l) => l,
                    Err(_) => return self.call_builtin(midx, &args),
                }
            }
        };
        if loc.em.code_off == 0 {
            return vm("dexrt.method.no_code");
        }
        if matches!(kind, InvokeKind::Static) {
            self.ensure_init(loc.class_idx)?;
        }
        self.exec(loc, args)
    }

    /// Execute one frame to return or throw.
    fn exec(&mut self, loc: Loc, args: Vec<V>) -> Rt<Option<V>> {
        let code = self
            .dex
            .code_item(loc.em.code_off)
            .map_err(E::Vm)?
            .ok_or_else(|| E::Vm(err("dexrt.method.no_code")))?;
        let nregs = code.registers_size as usize;
        let mut fr = Frame {
            regs: vec![V::I32(0); nregs],
            result: None,
            exc: None,
            table: decode_all(&code.insns).map_err(E::Vm)?,
            loc,
            code,
        };
        // Copy args into the last `ins_size` registers.
        let ins = fr.code.ins_size as usize;
        let start = nregs.saturating_sub(ins);
        for (i, a) in args.iter().enumerate() {
            if start + i < nregs {
                fr.regs[start + i] = *a;
            }
        }
        let mut pc: usize = 0;
        loop {
            let res = self.step(&mut fr, pc);
            match res {
                Ok(Out::Next(n)) => pc = n,
                Ok(Out::Return(v)) => return Ok(v),
                Err(E::Vm(e)) => return Err(E::Vm(e)),
                Err(E::Throw(r)) => match self.find_handler(&fr, pc, r) {
                    Some(addr) => {
                        pc = addr as usize;
                        fr.exc = Some(r);
                    }
                    None => return Err(E::Throw(r)),
                },
            }
        }
    }

    /// Locate a catch handler for `pc` throwing object `h`.
    fn find_handler(&self, fr: &Frame, pc: usize, h: u32) -> Option<u32> {
        for t in &fr.code.tries {
            let TryItem {
                start_addr,
                insn_count,
                handler_idx,
            } = *t;
            let end = start_addr + insn_count as u32;
            if (pc as u32) < start_addr || (pc as u32) >= end {
                continue;
            }
            let hnd = fr.code.handlers.get(handler_idx)?;
            for (ty, addr) in &hnd.handlers {
                if self.is_instance(h, *ty) {
                    return Some(*addr);
                }
            }
            if let Some(a) = hnd.catch_all_addr {
                return Some(a);
            }
        }
        None
    }
}

/// What the dispatcher decided for the current pc.
enum Out {
    /// Next pc in code units.
    Next(usize),
    /// Method returned.
    Return(Option<V>),
}

struct Frame {
    regs: Vec<V>,
    /// `move-result` source: set by the last invoke.
    result: Option<Option<V>>,
    /// Pending exception for `move-exception`.
    exc: Option<u32>,
    loc: Loc,
    code: ovelo_dex::CodeItem,
    /// Decoded instructions (payloads included), in offset order.
    table: Vec<Insn>,
}

#[derive(Clone, Copy)]
enum InvokeKind {
    Static,
    Direct,
    Virtual,
    Interface,
    /// Dispatch begins at the caller's superclass.
    Super { caller_class: u32 },
}

impl Vm {
    /// Set a possibly-wide value: mirrors into `regs[i+1]` for `I64`/`F64`.
    fn set(fr: &mut Frame, i: usize, v: V) {
        if let Some(r) = fr.regs.get_mut(i) {
            *r = v;
        }
        if matches!(v, V::I64(_) | V::F64(_)) {
            if let Some(r2) = fr.regs.get_mut(i + 1) {
                *r2 = v;
            }
        }
    }

    fn reg(fr: &Frame, i: usize) -> Rt<V> {
        fr.regs
            .get(i)
            .copied()
            .ok_or_else(|| E::Vm(err("dexrt.reg.bounds")))
    }

    fn i32v(v: V) -> Rt<i32> {
        match v {
            V::I32(i) => Ok(i),
            _ => vm("dexrt.type"),
        }
    }
    fn i64v(v: V) -> Rt<i64> {
        match v {
            V::I64(i) => Ok(i),
            _ => vm("dexrt.type"),
        }
    }
    fn f32v(v: V) -> Rt<f32> {
        match v {
            V::F32(f) => Ok(f),
            V::I32(i) => Ok(f32::from_bits(i as u32)),
            _ => vm("dexrt.type"),
        }
    }
    fn f64v(v: V) -> Rt<f64> {
        match v {
            V::F64(f) => Ok(f),
            V::I64(i) => Ok(f64::from_bits(i as u64)),
            _ => vm("dexrt.type"),
        }
    }
    fn refv(v: V) -> Rt<u32> {
        match v {
            V::Ref(h) => Ok(h),
            // `const/4 vx, 0` is how dex puts a null reference in a register.
            V::I32(0) => Ok(0),
            _ => vm("dexrt.type"),
        }
    }

    fn null_check(&mut self, h: u32, what: &str) -> Rt<()> {
        if h == 0 {
            Err(self.throw_builtin("Ljava/lang/NullPointerException;", what))
        } else {
            Ok(())
        }
    }

    /// One instruction at `pc` in `fr`.
    fn step(&mut self, fr: &mut Frame, pc: usize) -> Rt<Out> {
        let pos = fr
            .table
            .iter()
            .position(|i| i.offset as usize == pc)
            .ok_or_else(|| E::Vm(err("dexrt.pc.bounds")))?;
        let ins = fr.table[pos].clone();
        let next = fr
            .table
            .get(pos + 1)
            .map(|i| i.offset as usize)
            .unwrap_or(pc + 1);
        let op = ins.opcode;
        let regs = &ins.regs;
        let r = |i: usize| Self::reg(fr, regs.get(i).copied().unwrap_or(0) as usize);

        // helpers for typed results
        macro_rules! set {
            ($i:expr, $v:expr) => {
                Self::set(fr, $i as usize, $v)
            };
        }

        match op {
            0x00 => {} // nop
            // ---- moves ----
            0x01..=0x03 | 0x07..=0x09 => {
                let (a, b) = (regs[0] as usize, regs[1] as usize);
                let v = Self::reg(fr, b)?;
                Self::set(fr, a, v);
            }
            0x04..=0x06 => {
                let (a, b) = (regs[0] as usize, regs[1] as usize);
                let v = Self::reg(fr, b)?;
                Self::set(fr, a, v);
            }
            0x0a..=0x0c => {
                let v = fr
                    .result
                    .take()
                    .ok_or_else(|| E::Vm(err("dexrt.result.missing")))?
                    .ok_or_else(|| E::Vm(err("dexrt.result.void")))?;
                set!(regs[0], v);
            }
            0x0d => {
                let e = fr
                    .exc
                    .take()
                    .ok_or_else(|| E::Vm(err("dexrt.exc.missing")))?;
                set!(regs[0], V::Ref(e));
            }
            // ---- returns ----
            0x0e => return Ok(Out::Return(None)),
            0x0f..=0x11 => return Ok(Out::Return(Some(r(0)?))),
            // ---- consts ----
            0x12..=0x14 => set!(regs[0], V::I32(ins.lit.unwrap_or(0) as i32)),
            0x15 => set!(regs[0], V::I32((ins.lit.unwrap_or(0) << 16) as i32)),
            0x16..=0x18 => set!(regs[0], V::I64(ins.lit.unwrap_or(0))),
            0x19 => set!(regs[0], V::I64(ins.lit.unwrap_or(0) << 48)),
            0x1a | 0x1b => {
                let s = self.dex.string(ins.idx.unwrap_or(0)).map_err(E::Vm)?;
                let h = self.heap.intern_str(&s);
                set!(regs[0], V::Ref(h));
            }
            0x1c => {
                let h = self.heap.alloc(Obj::Class {
                    type_idx: ins.idx.unwrap_or(0),
                });
                set!(regs[0], V::Ref(h));
            }
            // ---- monitors: single-threaded no-ops (null still faults) ----
            0x1d | 0x1e => {
                let h = Self::refv(r(0)?)?;
                self.null_check(h, "monitor")?;
            }
            // ---- casts ----
            0x1f => {
                let h = Self::refv(r(0)?)?;
                if h != 0 && !self.is_instance(h, ins.idx.unwrap_or(0)) {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ClassCastException;",
                        "check-cast",
                    ));
                }
            }
            0x20 => {
                let h = Self::refv(r(1)?)?;
                let res = h != 0 && self.is_instance(h, ins.idx.unwrap_or(0));
                set!(regs[0], V::I32(res as i32));
            }
            0x21 => {
                let h = Self::refv(r(1)?)?;
                self.null_check(h, "array-length")?;
                let n = match self.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.len() as i32,
                    _ => return vm("dexrt.type"),
                };
                set!(regs[0], V::I32(n));
            }
            0x22 => {
                let ty = ins.idx.unwrap_or(0);
                self.ensure_init(ty)?;
                // A type with no class_def is a host-provided framework
                // class: allocate a field-less instance — its <init>
                // dispatches to a host stub via call_builtin.
                let h = self.heap.alloc(Obj::Instance {
                    class_idx: ty,
                    fields: HashMap::new(),
                });
                set!(regs[0], V::Ref(h));
            }
            0x23 => {
                let len = Self::i32v(r(1)?)?;
                if len < 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/NegativeArraySizeException;",
                        "new-array",
                    ));
                }
                let desc = self
                    .dex
                    .type_descriptor(ins.idx.unwrap_or(0))
                    .map_err(E::Vm)?;
                let elem = desc.trim_start_matches('[').to_string();
                let h = self.heap.alloc(Obj::Array {
                    data: vec![V::default_for(&elem); len as usize],
                    elem,
                });
                set!(regs[0], V::Ref(h));
            }
            0x24 | 0x25 => {
                // filled-new-array {args}, type@idx
                let desc = self
                    .dex
                    .type_descriptor(ins.idx.unwrap_or(0))
                    .map_err(E::Vm)?;
                let elem = desc.trim_start_matches('[').to_string();
                let data: Vec<V> = regs
                    .iter()
                    .map(|&i| Self::reg(fr, i as usize))
                    .collect::<Rt<_>>()?;
                let h = self.heap.alloc(Obj::Array { elem, data });
                fr.result = Some(Some(V::Ref(h)));
            }
            0x26 => {
                // fill-array-data vAA, +off — payload at `target`
                let h = Self::refv(r(0)?)?;
                self.null_check(h, "fill-array-data")?;
                let toff = ins.target.ok_or_else(|| E::Vm(err("dexrt.payload")))?;
                self.fill_array(h, &fr.table, toff as usize)?;
            }
            0x27 => {
                let h = Self::refv(r(0)?)?;
                if h == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/NullPointerException;",
                        "throw null",
                    ));
                }
                return Err(E::Throw(h));
            }
            // ---- branches ----
            0x28..=0x2a => {
                return Ok(Out::Next(
                    ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize,
                ))
            }
            0x2b | 0x2c => {
                let v = Self::i32v(r(0)?)?;
                let toff = ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize;
                let tgt = self.switch_target(&fr.table, toff, v, op == 0x2b)?;
                // Payload targets are relative to the switch insn's offset.
                return Ok(Out::Next(
                    tgt.map(|t| (pc as i64 + t as i64) as usize)
                        .unwrap_or(next),
                ));
            }
            0x2d..=0x30 => {
                let (a, b): (f64, f64) = if op >= 0x2f {
                    (Self::f64v(r(1)?)?, Self::f64v(r(2)?)?)
                } else {
                    (Self::f32v(r(1)?)? as f64, Self::f32v(r(2)?)? as f64)
                };
                let less_bias = op == 0x2d || op == 0x2f; // cmpl → NaN is -1
                let c = if a.is_nan() || b.is_nan() {
                    if less_bias { -1 } else { 1 }
                } else if a < b {
                    -1
                } else if a > b {
                    1
                } else {
                    0
                };
                set!(regs[0], V::I32(c));
            }
            0x31 => {
                let (a, b) = (Self::i64v(r(1)?)?, Self::i64v(r(2)?)?);
                set!(regs[0], V::I32(a.cmp(&b) as i32));
            }
            0x32..=0x37 => {
                let (va, vb) = (r(0)?, r(1)?);
                let t = match op {
                    // if-eq/if-ne also compare references by identity.
                    0x32 | 0x33 => {
                        let eq = match (va, vb) {
                            (V::Ref(a), V::Ref(b)) => a == b,
                            _ => Self::i32v(va)? == Self::i32v(vb)?,
                        };
                        if op == 0x32 {
                            eq
                        } else {
                            !eq
                        }
                    }
                    _ => {
                        let (a, b) = (Self::i32v(va)?, Self::i32v(vb)?);
                        match op {
                            0x34 => a < b,
                            0x35 => a >= b,
                            0x36 => a > b,
                            _ => a <= b,
                        }
                    }
                };
                if t {
                    return Ok(Out::Next(
                        ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize,
                    ));
                }
            }
            0x38..=0x3d => {
                let va = r(0)?;
                // if-eqz/if-nez also test references against null.
                let a = match va {
                    V::Ref(h) => h as i32,
                    _ => Self::i32v(va)?,
                };
                let t = match op {
                    0x38 => a == 0,
                    0x39 => a != 0,
                    0x3a => a < 0,
                    0x3b => a >= 0,
                    0x3c => a > 0,
                    _ => a <= 0,
                };
                if t {
                    return Ok(Out::Next(
                        ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize,
                    ));
                }
            }
            // ---- arrays ----
            0x44..=0x4a => {
                let (h, i) = (Self::refv(r(1)?)?, Self::i32v(r(2)?)?);
                let v = self.aget(h, i)?;
                set!(regs[0], v);
            }
            0x4b..=0x51 => {
                let (h, i, v) = (Self::refv(r(1)?)?, Self::i32v(r(2)?)?, r(0)?);
                self.aput(h, i, v)?;
            }
            // ---- instance fields ----
            0x52..=0x58 => {
                let h = Self::refv(r(1)?)?;
                self.null_check(h, "iget")?;
                let f = ins.idx.unwrap_or(0);
                let v = match self.heap.get(h) {
                    Some(Obj::Instance { fields, .. }) => {
                        fields.get(&f).copied().unwrap_or_else(|| {
                            self.field_default(f).unwrap_or(V::I32(0))
                        })
                    }
                    _ => return vm("dexrt.type"),
                };
                set!(regs[0], v);
            }
            0x59..=0x5f => {
                let h = Self::refv(r(1)?)?;
                self.null_check(h, "iput")?;
                let f = ins.idx.unwrap_or(0);
                match self.heap.get_mut(h) {
                    Some(Obj::Instance { fields, .. }) => {
                        fields.insert(f, r(0)?);
                    }
                    _ => return vm("dexrt.type"),
                }
            }
            // ---- static fields ----
            0x60..=0x66 => {
                let f = ins.idx.unwrap_or(0);
                let owner = self.dex.field(f).map_err(E::Vm)?.class_idx as u32;
                self.ensure_init(owner)?;
                let v = self
                    .classes
                    .entry(owner)
                    .or_default()
                    .statics
                    .get(&f)
                    .copied()
                    .unwrap_or_else(|| self.field_default(f).unwrap_or(V::I32(0)));
                set!(regs[0], v);
            }
            0x67..=0x6d => {
                let f = ins.idx.unwrap_or(0);
                let owner = self.dex.field(f).map_err(E::Vm)?.class_idx as u32;
                self.ensure_init(owner)?;
                self.classes
                    .entry(owner)
                    .or_default()
                    .statics
                    .insert(f, r(0)?);
            }
            // ---- invokes ----
            0x6e..=0x72 | 0x74..=0x78 => {
                let kind = match op {
                    0x6e | 0x74 => InvokeKind::Virtual,
                    0x6f | 0x75 => InvokeKind::Super {
                        caller_class: fr.loc.class_idx,
                    },
                    0x70 | 0x76 => InvokeKind::Direct,
                    0x71 | 0x77 => InvokeKind::Static,
                    _ => InvokeKind::Interface,
                };
                let midx = ins.idx.unwrap_or(0);
                let arg_regs: Vec<u16> = if ins.format == ovelo_dex::Format::F3rc {
                    // {vCCCC..+AA}: `regs`=[range start], `lit`=reg count
                    let (first, n) = (regs[0] as usize, ins.lit.unwrap_or(0) as usize);
                    (first..first + n).map(|i| i as u16).collect()
                } else {
                    regs.clone()
                };
                let args: Vec<V> = arg_regs
                    .iter()
                    .map(|&i| Self::reg(fr, i as usize))
                    .collect::<Rt<_>>()?;
                let res = self.invoke(midx, args, kind)?;
                fr.result = Some(res);
            }
            // ---- unary ops ----
            0x7b..=0x80 => {
                let v = r(1)?;
                let out = match op {
                    0x7b => V::I32(Self::i32v(v)?.wrapping_neg()),
                    0x7c => V::I32(!Self::i32v(v)?),
                    0x7d => V::I64(Self::i64v(v)?.wrapping_neg()),
                    0x7e => V::I64(!Self::i64v(v)?),
                    0x7f => V::F32(-Self::f32v(v)?),
                    _ => V::F64(-Self::f64v(v)?),
                };
                set!(regs[0], out);
            }
            0x81..=0x8f => {
                let v = r(1)?;
                let out = match op {
                    0x81 => V::I64(Self::i32v(v)? as i64),
                    0x82 => V::F32(Self::i32v(v)? as f32),
                    0x83 => V::F64(Self::i32v(v)? as f64),
                    0x84 => V::I32(Self::i64v(v)? as i32),
                    0x85 => V::F32(Self::i64v(v)? as f32),
                    0x86 => V::F64(Self::i64v(v)? as f64),
                    0x87 => V::I32(Self::f32v(v)? as i32),
                    0x88 => V::I64(Self::f32v(v)? as i64),
                    0x89 => V::F64(Self::f32v(v)? as f64),
                    0x8a => V::I32(Self::f64v(v)? as i32),
                    0x8b => V::I64(Self::f64v(v)? as i64),
                    0x8c => V::F32(Self::f64v(v)? as f32),
                    0x8d => V::I32(Self::i32v(v)? as i8 as i32),
                    0x8e => V::I32(Self::i32v(v)? as u16 as i32),
                    _ => V::I32(Self::i32v(v)? as i16 as i32),
                };
                set!(regs[0], out);
            }
            // ---- binary ops (3 args: a = b op c) ----
            0x90..=0xaf => {
                let (b, c) = (r(1)?, r(2)?);
                let out = self.binop(op, b, c)?;
                set!(regs[0], out);
            }
            // ---- binary ops (2addr: a op= b) ----
            0xb0..=0xcf => {
                let (a, b) = (r(0)?, r(1)?);
                let out = self.binop(op - 0x20, a, b)?; // 2addr shares 0x90 base
                set!(regs[0], out);
            }
            // ---- int literal ops ----
            0xd0..=0xd7 => {
                let (b, lit) = (r(1)?, ins.lit.unwrap_or(0) as i32);
                let out = self.binop_lit(op, Self::i32v(b)?, lit)?;
                set!(regs[0], V::I32(out));
            }
            0xd8..=0xe2 => {
                let (b, lit) = (r(1)?, ins.lit.unwrap_or(0) as i32);
                let out = self.binop_lit(op, Self::i32v(b)?, lit)?;
                set!(regs[0], V::I32(out));
            }
            _ => return vm("dexrt.op.unsupported"),
        }
        Ok(Out::Next(next))
    }

    fn field_default(&self, field_idx: u32) -> Option<V> {
        let f = self.dex.field(field_idx).ok()?;
        let desc = self.dex.type_descriptor(f.type_idx as u32).ok()?;
        Some(V::default_for(&desc))
    }

    fn aget(&mut self, h: u32, i: i32) -> Rt<V> {
        self.null_check(h, "aget")?;
        match self.heap.get(h) {
            Some(Obj::Array { data, .. }) => {
                let i = i as usize;
                if i >= data.len() {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArrayIndexOutOfBoundsException;",
                        "aget",
                    ));
                }
                Ok(data[i])
            }
            _ => vm("dexrt.type"),
        }
    }

    fn aput(&mut self, h: u32, i: i32, v: V) -> Rt<()> {
        self.null_check(h, "aput")?;
        match self.heap.get_mut(h) {
            Some(Obj::Array { data, .. }) => {
                if i < 0 || i as usize >= data.len() {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArrayIndexOutOfBoundsException;",
                        "aput",
                    ));
                }
                data[i as usize] = v;
                Ok(())
            }
            _ => vm("dexrt.type"),
        }
    }

    /// fill-array-data payload: `raw` layout is ident, elem_width, size(u32),
    /// data… — copy element-wise into the array (widths 1,2,4,8 bytes).
    fn fill_array(&mut self, h: u32, table: &[Insn], off: usize) -> Rt<()> {
        let p = table
            .iter()
            .find(|i| i.offset as usize == off)
            .ok_or_else(|| E::Vm(err("dexrt.payload")))?;
        let raw = &p.raw;
        if raw.len() < 4 || raw[0] != 0x0300 {
            return vm("dexrt.payload");
        }
        let width = raw[1] as usize;
        let count = (raw[3] as usize) << 16 | raw[2] as usize;
        let mut vals = Vec::with_capacity(count);
        for k in 0..count {
            let bo = 8 + k * width;
            // raw is u16 units; byte offset within it
            let u = bo / 2;
            let sh = (bo % 2) * 8;
            let mut val: u64 = 0;
            for b in 0..width {
                let idx = u + b / 2;
                let shift = ((b % 2) * 8 + sh) % 16;
                let byte = ((raw.get(idx).copied().unwrap_or(0) >> shift) & 0xff) as u64;
                val |= byte << (b * 8);
            }
            vals.push(match width {
                8 => V::I64(val as i64),
                _ => V::I32(val as i32),
            });
        }
        match self.heap.get_mut(h) {
            Some(Obj::Array { data, .. }) if data.len() == vals.len() => {
                *data = vals;
                Ok(())
            }
            Some(Obj::Array { .. }) => {
                Err(self.throw_builtin("Ljava/lang/ArrayIndexOutOfBoundsException;", "fill-array-data"))
            }
            _ => vm("dexrt.type"),
        }
    }

    /// Switch payload → case target (code units) or None for default.
    fn switch_target(&self, table: &[Insn], off: usize, v: i32, packed: bool) -> Rt<Option<u32>> {
        let p = table
            .iter()
            .find(|i| i.offset as usize == off)
            .ok_or_else(|| E::Vm(err("dexrt.payload")))?;
        let raw = &p.raw;
        if packed {
            if raw.len() < 4 || raw[0] != 0x0100 {
                return vm("dexrt.payload");
            }
            let size = raw[1] as usize;
            let first = ((raw[3] as i32) << 16) | raw[2] as i32;
            let k = v.wrapping_sub(first);
            if k < 0 || k as usize >= size {
                return Ok(None);
            }
            let i = 4 + k as usize * 2;
            return Ok(Some(
                (((raw[i + 1] as i32) << 16) | raw[i] as i32) as u32,
            ));
        }
        if raw.len() < 2 || raw[0] != 0x0200 {
            return vm("dexrt.payload");
        }
        let size = raw[1] as usize;
        for k in 0..size {
            let ki = 2 + k * 2;
            let key = ((raw[ki + 1] as i32) << 16) | raw[ki] as i32;
            if key == v {
                let ti = 2 + size * 2 + k * 2;
                return Ok(Some(
                    (((raw[ti + 1] as i32) << 16) | raw[ti] as i32) as u32,
                ));
            }
            if key > v {
                break;
            }
        }
        Ok(None)
    }

    /// int binary op with a literal operand (lit16/lit8 families).
    fn binop_lit(&mut self, op: u8, b: i32, lit: i32) -> Rt<i32> {
        Ok(match op {
            0xd0 | 0xd8 => b.wrapping_add(lit),
            0xd1 | 0xd9 => lit.wrapping_sub(b),
            0xd2 | 0xda => b.wrapping_mul(lit),
            0xd3 | 0xdb => {
                if lit == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "divide by zero",
                    ));
                }
                b.wrapping_div(lit)
            }
            0xd4 | 0xdc => {
                if lit == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "rem by zero",
                    ));
                }
                b.wrapping_rem(lit)
            }
            0xd5 | 0xdd => b & lit,
            0xd6 | 0xde => b | lit,
            0xd7 | 0xdf => b ^ lit,
            0xe0 => b.wrapping_shl(lit as u32),
            0xe1 => b.wrapping_shr(lit as u32),
            0xe2 => (b as u32).wrapping_shr(lit as u32) as i32,
            _ => return vm("dexrt.op.unsupported"),
        })
    }

    /// Binary ops 0x90..0xaf (b op c → V). Shift counts mask to 5/6 bits.
    fn binop(&mut self, op: u8, b: V, c: V) -> Rt<V> {
        Ok(match op {
            // int
            0x90 => V::I32(Self::i32v(b)?.wrapping_add(Self::i32v(c)?)),
            0x91 => V::I32(Self::i32v(b)?.wrapping_sub(Self::i32v(c)?)),
            0x92 => V::I32(Self::i32v(b)?.wrapping_mul(Self::i32v(c)?)),
            0x93 => {
                let (x, y) = (Self::i32v(b)?, Self::i32v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "divide by zero",
                    ));
                }
                V::I32(x.wrapping_div(y))
            }
            0x94 => {
                let (x, y) = (Self::i32v(b)?, Self::i32v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "rem by zero",
                    ));
                }
                V::I32(x.wrapping_rem(y))
            }
            0x95 => V::I32(Self::i32v(b)? & Self::i32v(c)?),
            0x96 => V::I32(Self::i32v(b)? | Self::i32v(c)?),
            0x97 => V::I32(Self::i32v(b)? ^ Self::i32v(c)?),
            0x98 => V::I32(Self::i32v(b)?.wrapping_shl(Self::i32v(c)? as u32 & 0x1f)),
            0x99 => V::I32(Self::i32v(b)?.wrapping_shr(Self::i32v(c)? as u32 & 0x1f)),
            0x9a => V::I32(
                (Self::i32v(b)? as u32).wrapping_shr(Self::i32v(c)? as u32 & 0x1f) as i32,
            ),
            // long
            0x9b => V::I64(Self::i64v(b)?.wrapping_add(Self::i64v(c)?)),
            0x9c => V::I64(Self::i64v(b)?.wrapping_sub(Self::i64v(c)?)),
            0x9d => V::I64(Self::i64v(b)?.wrapping_mul(Self::i64v(c)?)),
            0x9e => {
                let (x, y) = (Self::i64v(b)?, Self::i64v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "divide by zero",
                    ));
                }
                V::I64(x.wrapping_div(y))
            }
            0x9f => {
                let (x, y) = (Self::i64v(b)?, Self::i64v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "rem by zero",
                    ));
                }
                V::I64(x.wrapping_rem(y))
            }
            0xa0 => V::I64(Self::i64v(b)? & Self::i64v(c)?),
            0xa1 => V::I64(Self::i64v(b)? | Self::i64v(c)?),
            0xa2 => V::I64(Self::i64v(b)? ^ Self::i64v(c)?),
            0xa3 => V::I64(Self::i64v(b)?.wrapping_shl(Self::i32v(c)? as u32 & 0x3f)),
            0xa4 => V::I64(Self::i64v(b)?.wrapping_shr(Self::i32v(c)? as u32 & 0x3f)),
            0xa5 => V::I64(
                (Self::i64v(b)? as u64).wrapping_shr(Self::i32v(c)? as u32 & 0x3f) as i64,
            ),
            // float / double
            0xa6 => V::F32(Self::f32v(b)? + Self::f32v(c)?),
            0xa7 => V::F32(Self::f32v(b)? - Self::f32v(c)?),
            0xa8 => V::F32(Self::f32v(b)? * Self::f32v(c)?),
            0xa9 => V::F32(Self::f32v(b)? / Self::f32v(c)?),
            0xaa => V::F32(Self::f32v(b)? % Self::f32v(c)?),
            0xab => V::F64(Self::f64v(b)? + Self::f64v(c)?),
            0xac => V::F64(Self::f64v(b)? - Self::f64v(c)?),
            0xad => V::F64(Self::f64v(b)? * Self::f64v(c)?),
            0xae => V::F64(Self::f64v(b)? / Self::f64v(c)?),
            0xaf => V::F64(Self::f64v(b)? % Self::f64v(c)?),
            _ => return vm("dexrt.op.unsupported"),
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use ovelo_dex::testdex::*;


    fn vm(bytes: Vec<u8>) -> Vm {
        Vm::new(DexFile::open(bytes).unwrap()).unwrap()
    }

    fn virt(name: &'static str, insns: Vec<u16>) -> M {
        M {
            name,
            ret: "I",
            params: &[],
            is_virtual: true,
            regs: 4,
            ins: 0,
            outs: 2,
            insns,
            tries: vec![],
            lits: &[],
        }
    }

    fn dir(name: &'static str, ins: u16, insns: Vec<u16>) -> M {
        M {
            name,
            ret: "I",
            params: &[],
            is_virtual: false,
            regs: 6,
            ins,
            outs: 2,
            insns,
            tries: vec![],
            lits: &[],
        }
    }

    fn cls(name: &'static str, methods: Vec<M>) -> C {
        C {
            ext: false,
            class: name,
            sup: None,
            ifaces: &[],
            sfields: &[],
            ifields: &[],
            methods,
        }
    }

    // Instruction encodings (see DEX spec):
    //   const/4 vA,#lit4 : 0x12 | A<<8 | lit<<12
    //   return vA        : 0x0f | A<<8
    //   move-result vA   : 0x0a | A<<8
    //   move-exception vA: 0x0d | A<<8
    //   add-int vA,vB,vC : [0x90|A<<8, C|B<<8]
    //   div-int vA,vB,vC : [0x93|A<<8, C|B<<8]
    //   new-array vA,vB,T: [0x23|A<<8|B<<8, T]
    //   new-instance vA,T: [0x22|A<<8, T]
    //   aget vA,vB,vC    : [0x44|A<<8, C|B<<8]
    //   iget vA,vB,f     : [0x52|A<<8|B<<8, f]
    //   iput vA,vB,f     : [0x59|A<<8|B<<8, f]
    //   sget vA,f        : [0x60|A<<8, f]
    //   sput vA,f        : [0x69|A<<8, f]
    //   const-string vA,s: [0x1a|A<<8, s]
    //   invoke-virtual {regs},m : [0x6e|count<<12|first<<8, m, rest]

    #[test]
    fn add_two_args() {
        // regs=2, ins=2 -> args at v0,v1. add-int v0,v0,v1; return v0.
        let c = cls(
            "LCalc;",
            vec![M {
                name: "add",
                ret: "I",
                params: &["I", "I"],
                is_virtual: false,
                regs: 2,
                ins: 2,
                outs: 0,
                insns: vec![0x0090, 0x0100, 0x000f],
                tries: vec![],
                lits: &[],
            }],
        );
        let (b, _ix) = build(&[c]);
        let mut v = vm(b);
        assert_eq!(
            v.run("LCalc;", "add", &[V::I32(7), V::I32(5)]).unwrap(),
            Some(V::I32(12))
        );
    }

    #[test]
    fn div_by_zero_caught_by_handler() {
        // try units 1..3: div-int v0,v0,v0 -> handler @4 returns -1.
        let mut m = dir("f", 0, vec![
            0x0012, // const/4 v0,0
            0x0093, 0x0000, // div-int v0,v0,v0
            0x000f, // return v0
            0x020d, // move-exception v2
            0xf112, // const/4 v1,-1
            0x010f, // return v1
        ]);
        m.tries = vec![(1, 2, vec![], Some(4))];
        let (b, _ix) = build(&[cls("LT;", vec![m])]);
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "f", &[]).unwrap(), Some(V::I32(-1)));
    }

    #[test]
    fn aget_out_of_bounds_uncaught() {
        let c = C {
            class: "LT;",
            // forces "I" into the type table
            sfields: &[("I", "unused")],
            methods: vec![dir("f", 0, vec![
                0x1112,             // const/4 v1,1
                0x1023, 0x0000,     // new-array v0,v1,type@I (patched)
                0x2212,             // const/4 v2,2
                0x0044, 0x0200,     // aget v0,v0,v2
                0x000f,
            ])],
            ..cls("LT;", vec![])
        };
        let (mut b, ix) = build(&[c]);
        let ti = ix.t("I");
        for i in (0..b.len() - 4).step_by(2) {
            if u16::from_le_bytes([b[i], b[i + 1]]) == 0x1023 {
                b[i + 2..i + 4].copy_from_slice(&(ti as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        let e = v.run("LT;", "f", &[]).unwrap_err();
        assert_eq!(e.cause(), "dexrt.exception.uncaught");
    }

    #[test]
    fn virtual_dispatch_picks_override() {
        let base = C {
            class: "LB;",
            methods: vec![virt("who", vec![0x1112, 0x010f])],
            ..cls("LB;", vec![])
        };
        let child = C {
            class: "LC;",
            sup: Some("LB;"),
            methods: vec![virt("who", vec![0x2112, 0x010f])],
            ..cls("LC;", vec![])
        };
        let entry = cls(
            "LT;",
            vec![dir("main", 0, vec![
                0x0022, 0x0000,           // new-instance v0,LC (patched)
                0x106e, 0x0000, 0x0000,   // invoke-virtual {v0},LB.who (patched)
                0x010a,                   // move-result v1
                0x010f,                   // return v1
            ])],
        );
        let (mut b, ix) = build(&[base, child, entry]);
        let tc = ix.t("LC;");
        let mw = ix.m("LB;", "who");
        for i in (0..b.len() - 4).step_by(2) {
            let w = u16::from_le_bytes([b[i], b[i + 1]]);
            if w == 0x0022 {
                b[i + 2..i + 4].copy_from_slice(&(tc as u16).to_le_bytes());
            }
            if w == 0x106e {
                b[i + 2..i + 4].copy_from_slice(&(mw as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "main", &[]).unwrap(), Some(V::I32(2)));
    }

    #[test]
    fn clinit_runs_once() {
        // <clinit>: LT.x = 1 ; main: LT.x + LT.x -> 2 (clinit ran once)
        let c = C {
            class: "LT;",
            sfields: &[("I", "x")],
            methods: vec![
                M {
                    name: "<clinit>",
                    ret: "V",
                    params: &[],
                    is_virtual: false,
                    regs: 2,
                    ins: 0,
                    outs: 0,
                    insns: vec![
                        0x1112,         // const/4 v1,1
                        0x0169, 0x0000, // sput v1,LT.x (patched)
                        0x000e,         // return-void
                    ],
                    tries: vec![],
                    lits: &[],
                },
                dir("main", 0, vec![
                    0x0060, 0x0000, // sget v0,LT.x
                    0x0160, 0x0000, // sget v1,LT.x
                    0x0090, 0x0100, // add-int v0,v0,v1
                    0x000f,
                ]),
            ],
            ..cls("LT;", vec![])
        };
        let (mut b, ix) = build(&[c]);
        let fx = ix.f("LT;", "x");
        for i in (0..b.len() - 4).step_by(2) {
            let w = u16::from_le_bytes([b[i], b[i + 1]]);
            if w == 0x0169 || w == 0x0060 || w == 0x0160 {
                b[i + 2..i + 4].copy_from_slice(&(fx as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "main", &[]).unwrap(), Some(V::I32(2)));
    }



    #[test]
    fn const_string_and_builtin_length() {
        // Ljava/lang/String;.length declared so the invoke idx exists; the
        // builtin interpreter path handles it without running its insns.
        let strcls = C {
            ext: true,
            class: "Ljava/lang/String;",
            methods: vec![virt("length", vec![0x000e])],
            ..cls("Ljava/lang/String;", vec![])
        };
        let entry = cls(
            "LT;",
            vec![M {
                name: "len",
                ret: "I",
                params: &[],
                is_virtual: false,
                regs: 3,
                ins: 0,
                outs: 2,
                insns: vec![
                    0x001a, 0x0000, // const-string v0,"hello" (patched)
                    0x106e, 0x0000, 0x0000, // invoke-virtual {v0},String.length
                    0x010a, // move-result v1
                    0x010f, // return v1
                ],
                tries: vec![],
                lits: &["hello"],
            }],
        );
        let (mut b, ix) = build(&[strcls, entry]);
        let si = ix.s_idx("hello");
        let ml = ix.m("Ljava/lang/String;", "length");
        for i in (0..b.len() - 4).step_by(2) {
            let w = u16::from_le_bytes([b[i], b[i + 1]]);
            if w == 0x001a {
                b[i + 2..i + 4].copy_from_slice(&(si as u16).to_le_bytes());
            }
            if w == 0x106e {
                b[i + 2..i + 4].copy_from_slice(&(ml as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "len", &[]).unwrap(), Some(V::I32(5)));
    }

    #[test]
    fn assignable_walks_missing_intermediate() {
        // LCustomBtn subclasses Button, but the DEX never names the
        // intermediate TextView — the framework chain must still reach
        // View, so Button→View stays assignable.
        let ext = |class: &'static str| C {
            ext: true,
            class,
            sup: None,
            ifaces: &[],
            sfields: &[],
            ifields: &[],
            methods: vec![],
        };
        let (b, ix) = build(&[
            ext("Landroid/view/View;"),
            ext("Landroid/widget/Button;"),
            C {
                sup: Some("Landroid/widget/Button;"),
                ..cls("LCustomBtn;", vec![])
            },
        ]);
        let v = vm(b);
        assert!(v.assignable(
            ix.t("LCustomBtn;"),
            ix.t("Landroid/view/View;")
        ));
        // …and an unrelated class is still rejected.
        assert!(!v.assignable(
            ix.t("Landroid/widget/Button;"),
            ix.t("LCustomBtn;")
        ));
    }

    #[test]
    fn packed_switch_jumps_to_case() {
        // v0=1; packed-switch v0 -> case key 1 returns 20.
        // Layout: [0] const/4 v0,1 ; [1..3] packed-switch +8 ;
        // [4] case0: const/4 v1,10 ; [5] return v1 ;
        // [6..8] case1: const/16 v1,20 ; return v1 ;
        // [9..] payload: ident, size=2, first_key=0, targets rel. to switch
        //       (case0 abs 4 -> +3, case1 abs 6 -> +5)
        let c = cls(
            "LT;",
            vec![dir("f", 0, vec![
                0x1012,                 // const/4 v0,1
                0x002b, 0x0008, 0x0000, // packed-switch v0,+8 -> @9
                0xa112,                 // const/4 v1,10
                0x010f,                 // return v1
                0x0113, 0x0014,         // const/16 v1,20
                0x010f,                 // return v1
                0x0100, 0x0002,         // payload: packed, size 2
                0x0000, 0x0000,         // first_key = 0
                0x0003, 0x0000,         // target[0] = +3 -> @4
                0x0005, 0x0000,         // target[1] = +5 -> @6
            ])],
        );
        let (b, _ix) = build(&[c]);
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "f", &[]).unwrap(), Some(V::I32(20)));
    }
}
