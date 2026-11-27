//! java.util collections, iterators, Optional, Random, UUID, atomics,
//! CountDownLatch/Semaphore, android.util.SparseArray/Pair/ArrayMap.

use super::*;
use crate::value::{HostState, Obj, V};

/// Element equality for `contains`/`indexOf`/`remove` — Java compares
/// via `equals`, not identity. A dex `equals` (e.g. Glide's
/// `CallResourceReady` keyed on cb+executor) is honored through
/// `call_virtual_rt`; host objects and unresolved equals fall back to
/// `value_eq` (identity / string content / class mirror). A guest
/// `equals` that throws propagates — swallowing it would let a
/// `remove` succeed where Java surfaces the exception.
fn elem_eq(vm: &mut Vm, e: V, other: V) -> Rt<bool> {
    elem_eq_depth(vm, e, other, 0)
}

fn collection_desc(vm: &Vm, h: u32) -> String {
    match vm.heap.get(h) {
        Some(Obj::Host { desc, .. }) => desc.clone(),
        _ => class_of(vm, V::Ref(h)),
    }
}

fn collection_is_set(vm: &Vm, h: u32) -> bool {
    collection_desc(vm, h).contains("Set;")
}

fn collection_elements_rt(vm: &mut Vm, h: u32) -> Rt<Option<Vec<V>>> {
    if let Some(items) = collection_snapshot(vm, h) {
        return Ok(Some(items));
    }
    let it = match vm.call_virtual_rt(h, "iterator", &[]) {
        Ok(Some(V::Ref(it))) if it != 0 => it,
        Ok(_) => return Ok(None),
        Err(e) => return Err(e),
    };
    let host_iter = matches!(
        vm.heap.get(it),
        Some(Obj::Host {
            state: HostState::Iter(..) | HostState::List(..),
            ..
        })
    );
    let call_iter = |vm: &mut Vm, name: &str| -> Rt<Option<V>> {
        if host_iter {
            if let Some(v) = iter_call(vm, name, &[V::Ref(it)])? {
                return Ok(v);
            }
        }
        vm.call_virtual_rt(it, name, &[])
    };
    let mut out = Vec::new();
    for _ in 0..100_000 {
        let more = call_iter(vm, "hasNext")?
            .map(|v| i32v(v) != 0)
            .unwrap_or(false);
        if !more {
            break;
        }
        let Some(v) = call_iter(vm, "next")? else {
            break;
        };
        out.push(v);
    }
    Ok(Some(out))
}

fn collection_snapshot(vm: &Vm, h: u32) -> Option<Vec<V>> {
    if matches!(vm.heap.get(h), Some(Obj::Array { .. })) {
        return None;
    }
    if collection_desc(vm, h) == "Lkotlin/collections/EmptyList;" {
        return Some(Vec::new());
    }
    list_snapshot(vm, h)
}

fn collections_eq(vm: &mut Vm, a: u32, b: u32, depth: u8) -> Rt<Option<bool>> {
    if a == b {
        return Ok(Some(true));
    }
    if depth >= 16 {
        return Ok(None);
    }
    let (av, bv) = match (collection_snapshot(vm, a), collection_snapshot(vm, b)) {
        (Some(av), Some(bv)) => (av, bv),
        _ => return Ok(None),
    };
    if av.len() != bv.len() {
        return Ok(Some(false));
    }
    let set_a = collection_is_set(vm, a);
    let set_b = collection_is_set(vm, b);
    if set_a != set_b {
        return Ok(Some(false));
    }
    if set_a {
        let mut used = vec![false; bv.len()];
        'outer: for x in av {
            for (i, y) in bv.iter().enumerate() {
                if !used[i] && elem_eq_depth(vm, x, *y, depth + 1)? {
                    used[i] = true;
                    continue 'outer;
                }
            }
            return Ok(Some(false));
        }
        return Ok(Some(true));
    }
    for (x, y) in av.iter().zip(bv.iter()) {
        if !elem_eq_depth(vm, *x, *y, depth + 1)? {
            return Ok(Some(false));
        }
    }
    Ok(Some(true))
}

fn arrays_deep_eq(vm: &mut Vm, a: u32, b: u32, depth: u8) -> Rt<bool> {
    if a == b {
        return Ok(true);
    }
    if depth >= 16 {
        return Ok(false);
    }
    let (elem, av, bv) = match (vm.heap.get(a), vm.heap.get(b)) {
        (
            Some(Obj::Array { elem, data: av }),
            Some(Obj::Array {
                elem: belem,
                data: bv,
            }),
        ) if elem == belem
            || ((elem.starts_with('L') || elem.starts_with('['))
                && (belem.starts_with('L') || belem.starts_with('['))) =>
        {
            (elem.clone(), av.clone(), bv.clone())
        }
        _ => return Ok(false),
    };
    if av.len() != bv.len() {
        return Ok(false);
    }
    for (x, y) in av.iter().zip(bv.iter()) {
        let eq = match (elem.as_str(), *x, *y) {
            ("F", V::F32(x), V::F32(y)) => x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
            ("D", V::F64(x), V::F64(y)) => x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
            ("B", V::I32(x), V::I32(y)) => x as u8 == y as u8,
            (_, V::Ref(xh), V::Ref(yh)) if elem.starts_with('L') || elem.starts_with('[') => {
                if xh != 0
                    && yh != 0
                    && matches!(vm.heap.get(xh), Some(Obj::Array { .. }))
                    && matches!(vm.heap.get(yh), Some(Obj::Array { .. }))
                {
                    arrays_deep_eq(vm, xh, yh, depth + 1)?
                } else {
                    elem_eq_depth(vm, *x, *y, depth + 1)?
                }
            }
            _ => x == y,
        };
        if !eq {
            return Ok(false);
        }
    }
    Ok(true)
}

fn elem_eq_depth(vm: &mut Vm, e: V, other: V, depth: u8) -> Rt<bool> {
    if let (V::Ref(a), V::Ref(b)) = (e, other) {
        if a != 0 && b != 0 {
            if let Some(eq) = collections_eq(vm, a, b, depth)? {
                return Ok(eq);
            }
        }
    }
    match e {
        V::Ref(h)
            if h != 0
                && matches!(
                    vm.heap.get(h),
                    Some(Obj::Host { desc, .. }) if desc == "Ljava/math/BigDecimal;"
                ) =>
        {
            match vm.call_virtual_rt(h, "equals", &[other]) {
                Ok(Some(v)) => Ok(i32v(v) != 0),
                Ok(None) => Ok(value_eq(vm, e, other)),
                Err(e2) => Err(e2),
            }
        }
        V::Ref(h) if h != 0 && matches!(vm.heap.get(h), Some(Obj::Instance { .. })) => {
            match vm.call_virtual_rt(h, "equals", &[other]) {
                Ok(Some(v)) => Ok(i32v(v) != 0),
                Ok(None) => Ok(value_eq(vm, e, other)),
                Err(e2) => Err(e2),
            }
        }
        _ => Ok(value_eq(vm, e, other)),
    }
}
use std::rc::Rc;

/// Iterator bookkeeping: the collection handle it was minted from and
/// the last index `next` returned (`-1` before/after a remove). Kept as
/// pseudo fields so `HostState::Iter` can remain a cheap snapshot.
const P_ITER_SOURCE: u32 = u32::MAX - 50;
const P_ITER_LAST: u32 = u32::MAX - 51;
const P_ITER_DESC: u32 = u32::MAX - 52;
/// The value `next`/`previous` last returned — `remove` verifies the
/// cursor index still holds it before trusting the index (a `poll()`
/// mid-iteration shifts everything down).
const P_ITER_LASTV: u32 = u32::MAX - 53;

/// Copy-on-write collections iterate the array as of iterator
/// creation — never the live list (Java spec: the snapshot does not
/// reflect later adds/removes).
fn is_cow_desc(desc: &str) -> bool {
    matches!(
        desc,
        "Ljava/util/concurrent/CopyOnWriteArrayList;"
            | "Ljava/util/concurrent/CopyOnWriteArraySet;"
    )
}

/// Live backing-list length for a source-driven iterator — `None`
/// when the iterated source isn't a `HostState::List` (set/map/array
/// snapshot) or must not be read live (copy-on-write), in which case
/// traversal falls back to the snapshot.
fn live_list_len(vm: &Vm, h: u32) -> Option<usize> {
    let src = match pseudo_get(vm, h, P_ITER_SOURCE) {
        Some(V::Ref(s)) => s,
        _ => return None,
    };
    match vm.heap.get(src) {
        Some(Obj::Host {
            state: HostState::List(l),
            desc,
            ..
        }) if !is_cow_desc(desc) => Some(l.len()),
        _ => None,
    }
}

/// Element `at` of the iterator's live backing list (see
/// [`live_list_len`]). Index-exact — duplicates included.
fn live_list_get(vm: &Vm, h: u32, at: usize) -> Option<V> {
    let src = match pseudo_get(vm, h, P_ITER_SOURCE) {
        Some(V::Ref(s)) => s,
        _ => return None,
    };
    match vm.heap.get(src) {
        Some(Obj::Host {
            state: HostState::List(l),
            desc,
            ..
        }) if !is_cow_desc(desc) => l.get(at).copied(),
        _ => None,
    }
}

/// Iterator `remove` must throw UnsupportedOperationException on
/// fixed-size / snapshot-mutating backing collections.
fn iter_remove_unsupported(vm: &Vm, h: u32) -> bool {
    let src = match pseudo_get(vm, h, P_ITER_SOURCE) {
        Some(V::Ref(s)) => s,
        _ => return true,
    };
    let fixed = [
        "Ljava/util/Arrays$ArrayList;",
        "Ljava/util/concurrent/CopyOnWriteArrayList;",
        "Ljava/util/concurrent/CopyOnWriteArraySet;",
        "Ljava/util/Collections$EmptyList;",
        "Ljava/util/Collections$EmptySet;",
        "Ljava/util/Collections$SingletonList;",
        "Ljava/util/Collections$SingletonSet;",
        "Ljava/util/Collections$UnmodifiableList;",
        "Ljava/util/Collections$UnmodifiableSet;",
        "Ljava/util/Collections$UnmodifiableCollection;",
    ];
    match vm.heap.get(src) {
        Some(Obj::Host { desc, .. }) => fixed.contains(&desc.as_str()),
        _ => false,
    }
}

const LIST_CLASSES: &[&str] = &[
    "Ljava/util/ArrayList;",
    "Ljava/util/LinkedList;",
    "Ljava/util/Vector;",
    "Ljava/util/Stack;",
    "Ljava/util/ArrayDeque;",
    "Ljava/util/PriorityQueue;",
    "Ljava/util/concurrent/CopyOnWriteArrayList;",
    "Ljava/util/concurrent/ConcurrentLinkedQueue;",
    "Ljava/util/concurrent/ConcurrentLinkedDeque;",
    "Ljava/util/AbstractList;",
    "Ljava/util/AbstractSequentialList;",
    "Ljava/util/AbstractQueue;",
    "Ljava/util/AbstractCollection;",
    "Ljava/util/HashSet;",
    "Ljava/util/EnumSet;",
    "Ljava/util/LinkedHashSet;",
    "Ljava/util/TreeSet;",
    "Ljava/util/AbstractSet;",
    "Ljava/util/concurrent/CopyOnWriteArraySet;",
    "Landroid/util/ArraySet;",
    "Ljava/util/Collections$EmptyList;",
    "Ljava/util/Collections$EmptySet;",
    "Ljava/util/Collections$SingletonList;",
    "Ljava/util/Collections$SingletonSet;",
    "Ljava/util/Arrays$ArrayList;",
    "Ljava/util/Collections$SynchronizedCollection;",
    "Ljava/util/Collections$SynchronizedList;",
    "Ljava/util/Collections$SynchronizedSet;",
    "Ljava/util/Collections$UnmodifiableCollection;",
    "Ljava/util/Collections$UnmodifiableList;",
    "Ljava/util/Collections$UnmodifiableSet;",
    "Ljava/util/Collections$CheckedCollection;",
    "Ljava/util/Collections$CheckedList;",
    "Ljava/util/Collections$CheckedSet;",
    "Ljava/util/List;",
    "Ljava/util/Collection;",
    // Interface `Iterable.iterator()` calls (the for-each desugar
    // pattern) must reach `list_call`, or they fall to
    // `stub_placeholder` and iterate nothing.
    "Ljava/lang/Iterable;",
    "Ljava/util/Set;",
    "Ljava/util/Queue;",
    "Ljava/util/Deque;",
    "Ljava/util/SortedSet;",
    "Ljava/util/NavigableSet;",
];

const MAP_CLASSES: &[&str] = &[
    "Ljava/util/HashMap;",
    "Ljava/util/LinkedHashMap;",
    "Ljava/util/TreeMap;",
    "Ljava/util/Hashtable;",
    "Ljava/util/Properties;",
    "Ljava/util/WeakHashMap;",
    "Ljava/util/IdentityHashMap;",
    "Ljava/util/EnumMap;",
    "Ljava/util/concurrent/ConcurrentHashMap;",
    "Ljava/util/AbstractMap;",
    "Landroid/util/ArrayMap;",
    "Ljava/util/Collections$EmptyMap;",
    "Ljava/util/Collections$SingletonMap;",
    "Ljava/util/Collections$SynchronizedMap;",
    "Ljava/util/Collections$UnmodifiableMap;",
    "Ljava/util/Collections$CheckedMap;",
    "Landroid/os/BaseBundle;",
    "Landroid/os/Bundle;",
    "Landroid/os/PersistableBundle;",
    "Landroid/content/ContentValues;",
    "Ljava/util/Map;",
    "Ljava/util/concurrent/ConcurrentMap;",
    "Ljava/util/SortedMap;",
    "Ljava/util/NavigableMap;",
];

const ITER_CLASSES: &[&str] = &[
    "Ljava/util/Iterator;",
    "Ljava/util/ListIterator;",
    "Ljava/util/Enumeration;",
    "Ljava/util/Map$Entry;",
    "Ljava/util/AbstractMap$SimpleEntry;",
    "Ljava/util/AbstractMap$SimpleImmutableEntry;",
    "Ljava/util/HashMap$Entry;",
];

/// `class` equals or descends from any of `kinds` — resolves the
/// host ancestry once instead of re-scanning `host_supers` per kind.
fn has_kind(class: &str, kinds: &[&str]) -> bool {
    if kinds.contains(&class) {
        return true;
    }
    let supers = host_supers(class);
    kinds.iter().any(|k| supers.contains(k))
}

fn is_list(class: &str) -> bool {
    has_kind(class, LIST_CLASSES)
}

fn is_map(class: &str) -> bool {
    has_kind(class, MAP_CLASSES)
}

fn is_iter(class: &str) -> bool {
    has_kind(class, ITER_CLASSES)
        || class.contains("$")
            && (class.starts_with("Ljava/util/") || class.starts_with("Landroid/util/"))
}

/// Whether the receiver's own class is a `Set` — checked on the object,
/// not the call site's declared type (guests declare `Collection`).
fn is_set_obj(vm: &Vm, v: V) -> bool {
    match v {
        V::Ref(h) if h != 0 => {
            desc_is_a(&class_of(vm, v), "Ljava/util/Set;")
                || desc_is_a(&class_of(vm, v), "Landroid/util/ArraySet;")
                || class_of(vm, v).contains("$Set;")
        }
        _ => false,
    }
}

fn enqueue_executor_task(vm: &mut Vm, ex: u32, r: u32) {
    if vm.single_executors.contains(&ex) && vm.executor_active.contains_key(&ex) {
        vm.executor_queues.entry(ex).or_default().push_back(r);
    } else {
        if vm.single_executors.contains(&ex) {
            vm.executor_active.insert(ex, r);
            vm.runnable_executor.insert(r, ex);
        }
        vm.enqueue_runnable(r, false);
    }
}

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(if class == "Ljava/util/Collections;" {
        collections_static(vm, name, args)?
    } else if class == "Ljava/util/Arrays;" {
        arrays_static(vm, name, sig, args)?
    } else if class == "Ljava/util/Objects;" {
        objects_static(vm, name, args)?
    } else if class.starts_with("Ljava/util/Optional") {
        optional_call(vm, class, name, args)
    } else if class == "Ljava/security/SecureRandom;" {
        random_call(vm, name, args, true)
    } else if class == "Ljava/util/Random;" {
        random_call(vm, name, args, false)
    } else if class == "Ljava/util/UUID;" {
        uuid_call(vm, name, args)
    } else if class == "Ljava/util/StringTokenizer;" {
        strtokenizer_call(vm, name, args)?
    } else if class == "Ljava/util/BitSet;" {
        bitset_call(vm, name, sig, args)
    } else if class == "Ljava/util/StringJoiner;" {
        match name {
            "add" => {
                let h = match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                };
                let add = sv(vm, args[1]).unwrap_or_default();
                if let Some(Obj::Host {
                    state: HostState::StrBuf(b),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    if !b.is_empty() {
                        b.push_str(", ");
                    }
                    b.push_str(&add);
                }
                Some(Some(args[0]))
            }
            _ => return Ok(None),
        }
    } else if class == "Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;"
        || class == "Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;"
        || class == "Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;"
    {
        fieldupdater_call(vm, class, name, args)?
    } else if class.starts_with("Ljava/util/concurrent/atomic/") {
        atomic_call(vm, class, name, args)
    } else if class == "Ljava/util/concurrent/CountDownLatch;" {
        latch_call(vm, name, args)
    } else if class == "Ljava/util/concurrent/Semaphore;" {
        match name {
            "acquire" | "release" | "tryAcquire" | "drainPermits" => {
                if name == "tryAcquire" || name == "drainPermits" {
                    Some(Some(V::I32(1)))
                } else {
                    Some(None)
                }
            }
            "availablePermits" => Some(Some(V::I32(1))),
            _ => None,
        }
    } else if class == "Landroid/util/Pair;" || class == "Landroidx/core/util/Pair;" {
        pair_call(vm, name, args)?
    } else if class.starts_with("Landroid/util/Sparse") {
        sparse_call(vm, class, name, args)?
    } else if class == "Ljava/util/concurrent/TimeUnit;" {
        match name {
            // `dst.convert(d, src)` — real unit math (kotlin.time
            // Duration builds its whole encoding on these; returning
            // the raw arg turns Room's 30s pool timeout into 0).
            "convert" => {
                let dst = lang::enum_ordinal(vm, args[0]);
                let src = lang::enum_ordinal(vm, *args.get(2).unwrap_or(&args[0]));
                let d = i64v(args.get(1).copied().unwrap_or(V::I64(0)));
                Some(Some(V::I64(timeunit_convert(dst, src, d))))
            }
            "toNanos" | "toMicros" | "toMillis" | "toSeconds" | "toMinutes" | "toHours"
            | "toDays" => {
                let dst = match name {
                    "toNanos" => 0,
                    "toMicros" => 1,
                    "toSeconds" => 3,
                    "toMinutes" => 4,
                    "toHours" => 5,
                    "toDays" => 6,
                    _ => 2,
                };
                let src = lang::enum_ordinal(vm, args[0]);
                let d = i64v(args.get(1).copied().unwrap_or(V::I64(0)));
                Some(Some(V::I64(timeunit_convert(dst, src, d))))
            }
            "sleep" | "timedWait" | "timedJoin" => Some(None),
            _ => None,
        }
    } else if (class == "Ljava/util/concurrent/Executors;"
        || class == "Ljava/util/concurrent/ExecutorService;"
        || class.starts_with("Ljava/util/concurrent/"))
        && !is_list(class)
        && !is_map(class)
    {
        match name {
            "execute" => {
                if let (V::Ref(ex), V::Ref(r)) = (args[0], args[1]) {
                    // Executor jobs are async — defer to the work queue
                    // (they run on a worker thread at the next drain).
                    // A single-thread executor additionally serializes
                    // those jobs instead of admitting them all at once.
                    enqueue_executor_task(vm, ex, r);
                }
                Some(None)
            }
            "submit" => {
                // Submit is asynchronous and ordered through the same executor
                // queue as execute. The wrapper completes the returned future
                // when the worker drain reaches it.
                if let (V::Ref(ex), V::Ref(task)) = (args[0], args[1]) {
                    // AbstractExecutorService.submit is defined in terms of
                    // virtual newTaskFor/execute. Dex subclasses such as
                    // Guava's listening decorator override those methods and
                    // require the returned RunnableFuture to implement their
                    // ListenableFuture interface; a host Future cannot pass
                    // that check-cast. Preserve the virtual dispatch for dex
                    // executor instances, falling back to the host future for
                    // plain executor host objects.
                    if matches!(vm.heap.get(ex), Some(Obj::Instance { .. })) {
                        let callable = sig.contains("Callable");
                        let nt_args: Vec<V> = if callable {
                            vec![V::Ref(task)]
                        } else {
                            vec![V::Ref(task), args.get(2).copied().unwrap_or(V::Ref(0))]
                        };
                        if let Some(future @ V::Ref(fh)) =
                            vm.call_virtual_rt(ex, "newTaskFor", &nt_args)?
                        {
                            let has_dex_execute = vm
                                .heap
                                .class_of(ex)
                                .and_then(|ci| {
                                    vm.resolve_call_args(ci, "execute", &[future])
                                        .ok()
                                        .map(|(m, _)| m.is_some())
                                })
                                .unwrap_or(false);
                            if has_dex_execute {
                                vm.call_virtual_rt(ex, "execute", &[future])?;
                            } else {
                                // A dex subclass of a host ThreadPoolExecutor
                                // can inherit execute without overriding it.
                                enqueue_executor_task(vm, ex, fh);
                            }
                            return Ok(Some(Some(future)));
                        }
                    }
                    let future_v = host(
                        vm,
                        "Ljava/util/concurrent/Future;",
                        HostState::Slot(V::Ref(0)),
                    );
                    let wrapper_v = host(vm, "Ljava/lang/Runnable;", HostState::Empty);
                    if let (V::Ref(future), V::Ref(wrapper)) = (future_v, wrapper_v) {
                        pseudo_set(vm, future, P_FUTURE_DONE, V::I32(0));
                        pseudo_set(vm, wrapper, P_FUTURE_TARGET, V::Ref(task));
                        pseudo_set(vm, wrapper, P_FUTURE, V::Ref(future));
                        pseudo_set(
                            vm,
                            wrapper,
                            P_FUTURE_CALLABLE,
                            V::I32(sig.contains("Callable") as i32),
                        );
                        enqueue_executor_task(vm, ex, wrapper);
                    }
                    return Ok(Some(Some(future_v)));
                }
                Some(Some(host(
                    vm,
                    "Ljava/util/concurrent/Future;",
                    HostState::Slot(V::Ref(0)),
                )))
            }
            "isShutdown" | "isTerminated" => Some(Some(V::I32(0))),
            "shutdown" | "shutdownNow" | "awaitTermination" => {
                if name == "shutdownNow" {
                    Some(Some(V::Ref(0)))
                } else if name == "awaitTermination" {
                    Some(Some(V::I32(1)))
                } else {
                    Some(None)
                }
            }
            "newSingleThreadExecutor"
            | "newCachedThreadPool"
            | "newFixedThreadPool"
            | "newScheduledThreadPool"
            | "unconfigurableExecutorService"
            | "defaultThreadFactory" => {
                let v = host(
                    vm,
                    "Ljava/util/concurrent/ExecutorService;",
                    HostState::Empty,
                );
                if name == "newSingleThreadExecutor" {
                    if let V::Ref(h) = v {
                        vm.single_executors.insert(h);
                    }
                }
                Some(Some(v))
            }
            "addListener" => {
                // The inline submit future is already complete, so its
                // listener can run immediately. Dropping it breaks media3's
                // Futures.transform chain that resolves media ids to URIs.
                if let (Some(&V::Ref(future)), Some(&V::Ref(r))) = (args.first(), args.get(1)) {
                    if matches!(pseudo_get(vm, future, P_FUTURE_DONE), Some(V::I32(1))) {
                        let _ = vm.call_virtual_rt(r, "run", &[]);
                    } else {
                        let lh = match pseudo_get(vm, future, P_FUTURE_LISTENERS) {
                            Some(V::Ref(h)) => h,
                            _ => {
                                let v = host(vm, "Ljava/util/ArrayList;", HostState::List(vec![]));
                                let V::Ref(h) = v else { unreachable!() };
                                pseudo_set(vm, future, P_FUTURE_LISTENERS, V::Ref(h));
                                h
                            }
                        };
                        if let Some(Obj::Host {
                            state: HostState::List(items),
                            ..
                        }) = vm.heap.get_mut(lh)
                        {
                            items.push(V::Ref(r));
                        }
                    }
                }
                Some(None)
            }
            "get" | "call" => {
                if name == "get" {
                    let sv = match args.first().copied().unwrap_or(V::Ref(0)) {
                        V::Ref(h) => match vm.heap.get(h) {
                            Some(Obj::Host {
                                state: HostState::Slot(v),
                                ..
                            }) => Some(*v),
                            Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                                fields.get(&P_SLOT).copied()
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(v) = sv {
                        return Ok(Some(Some(v)));
                    }
                }
                Some(Some(V::Ref(0)))
            }
            "isDone" | "isCancelled" => Some(Some(V::I32(1))),
            "cancel" => Some(Some(V::I32(1))),
            "park" | "parkNanos" | "parkUntil" => {
                // LockSupport.park* — a timed park records its wake
                // deadline so an idling thread (kotlinx scheduler
                // worker, ~60s keep-alive) sleeps in `delayed_runnables`
                // instead of busy-cycling the drain; `due=now` requeues
                // were the tasks.org 100%-CPU spin. parkNanos(0)/park
                // keep the inline drain; positive sub-millisecond waits
                // round up so event loops cannot busy-spin below the VM's
                // millisecond clock resolution.
                let due_ms = match name {
                    "parkNanos" => match args.last().copied() {
                        Some(v) => {
                            let nanos = i64v(v).max(0);
                            let millis = if nanos == 0 {
                                0
                            } else {
                                nanos / 1_000_000 + i64::from(nanos % 1_000_000 != 0)
                            };
                            now_ms().saturating_add(millis)
                        }
                        None => now_ms(),
                    },
                    "parkUntil" => args.last().copied().map(i64v).unwrap_or_default(),
                    _ => 0,
                };
                // parkNanos(0) is explicitly a non-blocking boundary in
                // Java. Treating it as an inline drain can turn
                // kotlinx's event-loop bookkeeping into a fatal VM park
                // when no independent runnable happens to be queued.
                if name == "parkNanos" && args.last().map(|v| i64v(*v)) == Some(0) {
                    return Ok(Some(None));
                }
                if vm.cur_thread != 0 && vm.unpark_permits.remove(&vm.cur_thread) {
                    // LockSupport unparks are permits, not just wakeups
                    // of an already-parked thread. A permit arriving
                    // before this park lets the first subsequent park
                    // return immediately — including indefinite park().
                    return Ok(Some(None));
                }
                if due_ms > now_ms() && vm.cur_runnable != 0 {
                    let blocking_waiter = vm.cur_thread != 0
                        && vm.cur_thread != vm.main_thread_h
                        && vm.describe_stack().last().is_some_and(|m| {
                            m.contains("runBlocking")
                                || m.contains("BlockingCoroutine;.joinBlocking")
                        });
                    if blocking_waiter {
                        // `runBlocking` parks its own Java stack while a
                        // child coroutine completes. Requeue-and-restart
                        // semantics destroy that stack, so the child can
                        // finish without the waiter ever observing it.
                        // Drain what is runnable, then return from this
                        // park and let the event loop poll again.
                        let remain = due_ms.saturating_sub(now_ms()).max(1);
                        let slice = remain.min(50) as u64;
                        match vm.park_drain_within(slice) {
                            Ok(()) => {}
                            Err(crate::E::Vm(e)) if e.cause() == "ovelo.park" => {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            Err(e) => return Err(e),
                        }
                        return Ok(Some(None));
                    }
                    vm.sleep_until.insert(vm.cur_runnable, due_ms.max(0) as u64);
                    vm.park_waiters.insert(vm.cur_runnable);
                    if vm.cur_thread != 0 {
                        vm.parked_thread_runnable
                            .insert(vm.cur_thread, vm.cur_runnable);
                    }
                    return Err(crate::E::Vm(crate::err("ovelo.park")));
                }
                vm.park_drain()?;
                Some(None)
            }
            "unpark" => {
                if let Some(V::Ref(thread)) = args.first().copied() {
                    vm.unpark_thread(thread);
                } else {
                    vm.wake_parked();
                }
                Some(None)
            }
            "offer" | "add" | "put" | "take" | "poll" | "remove" => {
                if matches!(name, "add" | "offer") {
                    list_call(vm, name, sig, args)?
                } else {
                    Some(Some(V::Ref(0)))
                }
            }
            _ => None,
        }
    } else if class == "Ljava/util/EnumSet;" {
        enum_set_call(vm, name, sig, args)?
    } else if is_list(class) {
        list_call(vm, name, sig, args)?
    } else if is_map(class) {
        map_call(vm, name, sig, args)?
    } else if is_iter(class) {
        iter_call(vm, name, args)?
    } else {
        return Ok(None);
    })
}

// ---------------------------------------------------------------------
// EnumSet — list-backed; static factories can't fall through to
// `list_call` (no receiver), so they build the set here.

fn enum_set_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    fn class_desc_of_enum(vm: &Vm, v: V) -> Option<String> {
        let V::Ref(h) = v else { return None };
        let ci = vm.heap.class_of(h)?;
        vm.type_desc_of(ci).ok()
    }
    fn class_desc_of_class(vm: &Vm, v: V) -> Option<String> {
        let V::Ref(h) = v else { return None };
        match vm.heap.get(h) {
            Some(Obj::Class { type_idx }) => vm.type_desc_of(*type_idx).ok(),
            _ => None,
        }
    }
    Ok(match name {
        "of" | "noneOf" => Some(Some(host(
            vm,
            "Ljava/util/EnumSet;",
            HostState::List(if name == "of" {
                args.to_vec()
            } else {
                Vec::new()
            }),
        ))),
        "allOf" => {
            let vals = class_desc_of_class(vm, args.first().copied().unwrap_or(V::Ref(0)))
                .map(|d| lang::enum_value_list(vm, &d))
                .unwrap_or_default();
            Some(Some(host(vm, "Ljava/util/EnumSet;", HostState::List(vals))))
        }
        "range" => {
            let from = lang::enum_ordinal(vm, args.first().copied().unwrap_or(V::Ref(0)));
            let to = lang::enum_ordinal(vm, args.get(1).copied().unwrap_or(V::Ref(0)));
            let vals = class_desc_of_enum(vm, args.first().copied().unwrap_or(V::Ref(0)))
                .map(|d| {
                    lang::enum_value_list(vm, &d)
                        .into_iter()
                        .filter(|v| {
                            let o = lang::enum_ordinal(vm, *v);
                            o >= from && o <= to
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Some(host(vm, "Ljava/util/EnumSet;", HostState::List(vals))))
        }
        "copyOf" => {
            let src: Vec<V> = match args.first().copied().unwrap_or(V::Ref(0)) {
                V::Ref(sh) => list_snapshot(vm, sh).unwrap_or_default(),
                _ => vec![],
            };
            Some(Some(host(vm, "Ljava/util/EnumSet;", HostState::List(src))))
        }
        "complementOf" => {
            let existing: Vec<V> = match args.first().copied().unwrap_or(V::Ref(0)) {
                V::Ref(sh) => list_snapshot(vm, sh).unwrap_or_default(),
                _ => vec![],
            };
            let desc = existing.first().and_then(|v| class_desc_of_enum(vm, *v));
            let mut vals = Vec::new();
            if let Some(d) = desc {
                for v in lang::enum_value_list(vm, &d) {
                    if !existing.iter().any(|e| value_eq(vm, *e, v)) {
                        vals.push(v);
                    }
                }
            }
            Some(Some(host(vm, "Ljava/util/EnumSet;", HostState::List(vals))))
        }
        _ => return list_call(vm, name, sig, args),
    })
}

// ---------------------------------------------------------------------
// List

fn write_sorted_list(vm: &mut Vm, h: u32, items: Vec<V>) {
    if matches!(
        vm.heap.get(h),
        Some(Obj::Host {
            state: HostState::List(_),
            ..
        })
    ) {
        if let Some(l) = list_mut(vm, h) {
            *l = items;
        }
        return;
    }
    let Some((backing, off, size, ring)) = super::guest_list_backing(vm, h, 8) else {
        return;
    };
    match vm.heap.get_mut(backing) {
        Some(Obj::Host {
            state: HostState::List(l),
            ..
        }) => {
            if !ring && off == 0 && size.is_none_or(|n| n >= l.len()) {
                *l = items;
            } else {
                for (i, v) in items.into_iter().enumerate() {
                    let at = if ring && !l.is_empty() {
                        (off + i) % l.len()
                    } else {
                        off + i
                    };
                    if at < l.len() {
                        l[at] = v;
                    }
                }
            }
        }
        Some(Obj::Array { data, .. }) => {
            for (i, v) in items.into_iter().enumerate() {
                let at = if ring && !data.is_empty() {
                    (off + i) % data.len()
                } else {
                    off + i
                };
                if at < data.len() {
                    data[at] = v;
                }
            }
        }
        _ => {}
    }
}

fn list_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "<init>" => {
            // List(Collection c): seed the new list with the source's
            // elements — the ctor otherwise lands as a no-op stub and
            // silently empties the copy.
            if let Some(V::Ref(sh)) = args.get(1).copied() {
                let src = list_snapshot(vm, sh);
                if let (Some(l), Some(src)) = (list_mut(vm, h), src) {
                    *l = src;
                }
            }
            Some(None)
        }
        "add" => {
            // Sets reject duplicates — `distinctBy`, `union` etc. rely
            // on `add` reporting "already present" via false.
            let is_set = is_set_obj(vm, args[0]);
            let dup = is_set
                && list_snapshot(vm, h)
                    .is_some_and(|l| l.iter().any(|e| value_eq(vm, *e, args[1])));
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            if args.len() > 2 && params(sig).first() == Some(&"I") {
                l.insert((i32v(args[1]).max(0) as usize).min(l.len()), args[2]);
                Some(None)
            } else if dup {
                Some(Some(V::I32(0)))
            } else {
                if crate::dbg_flag("OVELO_ITERDBG") && is_null(args[1]) {
                    eprintln!("[iterdbg] add(null) on h{h} len={}", l.len());
                }
                l.push(args[1]);
                Some(Some(V::I32(1)))
            }
        }
        "addAll" => {
            let src: Vec<V> = match args.last().copied().unwrap_or(V::Ref(0)) {
                V::Ref(sh) => list_snapshot(vm, sh).unwrap_or_default(),
                _ => vec![],
            };
            let is_set = is_set_obj(vm, args[0]);
            let src: Vec<V> = if is_set {
                src.into_iter()
                    .filter(|v| {
                        !list_ref(vm, h).is_some_and(|l| l.iter().any(|e| value_eq(vm, *e, *v)))
                    })
                    .collect()
            } else {
                src
            };
            let added = src.len();
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let at = if args.len() > 2 {
                i32v(args[1]).max(0) as usize
            } else {
                l.len()
            };
            for (i, v) in src.iter().enumerate() {
                l.insert((at + i).min(l.len()), *v);
            }
            Some(Some(V::I32((added > 0) as i32)))
        }
        "addFirst" | "addLast" | "push" | "offerFirst" | "offerLast" | "offer" => {
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            if matches!(name, "addFirst" | "push" | "offerFirst") {
                l.insert(0, args[1]);
            } else {
                l.push(args[1]);
            }
            Some(if name.starts_with("offer") {
                Some(V::I32(1))
            } else {
                None
            })
        }
        "removeFirst" | "removeLast" | "pop" | "poll" | "pollFirst" | "pollLast" => {
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let v = if matches!(name, "removeFirst" | "pop" | "poll" | "pollFirst") {
                if l.is_empty() {
                    V::Ref(0)
                } else {
                    l.remove(0)
                }
            } else {
                l.pop().unwrap_or(V::Ref(0))
            };
            Some(Some(v))
        }
        "remove" => {
            let p = params(sig);
            if p.first() == Some(&"I") {
                let i = i32v(args[1]) as usize;
                let l = match list_mut(vm, h) {
                    Some(l) => l,
                    None => return Ok(None),
                };
                Some(Some(if i < l.len() { l.remove(i) } else { V::Ref(0) }))
            } else {
                // Guest-backed collections mutate themselves — without
                // a host store this arm cannot remove, so defer to dex.
                if list_mut(vm, h).is_none() {
                    return Ok(None);
                }
                let items: Vec<V> = list_snapshot(vm, h).unwrap_or_default();
                let mut pos = None;
                for (i, e) in items.iter().enumerate() {
                    if elem_eq(vm, *e, args[1])? {
                        pos = Some(i);
                        break;
                    }
                }
                match pos {
                    Some(i) => {
                        if let Some(l) = list_mut(vm, h) {
                            l.remove(i);
                        }
                        Some(Some(V::I32(1)))
                    }
                    None => Some(Some(V::I32(0))),
                }
            }
        }
        "removeAll" | "removeIf" | "retainAll" => {
            // Guest-backed collections mutate themselves.
            if list_mut(vm, h).is_none() {
                return Ok(None);
            }
            if name == "removeIf" {
                Some(Some(V::I32(0))) // predicate not run — keep contents
            } else {
                let src: Vec<V> = match args[1] {
                    V::Ref(sh) => match vm.heap.get(sh) {
                        Some(Obj::Host {
                            state: HostState::List(l2),
                            ..
                        }) => l2.clone(),
                        Some(Obj::Array { data, .. }) => data.clone(),
                        _ => vec![],
                    },
                    _ => vec![],
                };
                let cur: Vec<V> = list_snapshot(vm, h).unwrap_or_default();
                let keep: Vec<bool> = cur
                    .iter()
                    .map(|e| {
                        let in_src = src.iter().any(|s| value_eq(vm, *e, *s));
                        if name == "removeAll" {
                            !in_src
                        } else {
                            in_src
                        }
                    })
                    .collect();
                let before = cur.len();
                let mut it = keep.iter();
                if let Some(l) = list_mut(vm, h) {
                    l.retain(|_| *it.next().unwrap_or(&false));
                }
                Some(Some(V::I32(
                    (before != keep.iter().filter(|k| **k).count()) as i32,
                )))
            }
        }
        "get" => {
            if list_len(vm, h).is_none() {
                return Ok(None);
            }
            let i = i32v(args[1]) as usize;
            match list_get(vm, h, i) {
                // Java throws, it does not return null — guests catch
                // IndexOutOfBoundsException where a null would NPE
                // later (NewPipe Tab.getTabName on the default tab
                // list).
                Some(v) => Some(Some(v)),
                None => {
                    return Err(vm.throw_builtin(
                        "Ljava/lang/IndexOutOfBoundsException;",
                        &format!("List.get({i}) on size {}", list_len(vm, h).unwrap_or(0)),
                    ));
                }
            }
        }
        "element" | "getFirst" | "getLast" | "first" | "last" => {
            if list_len(vm, h).is_none() {
                return Ok(None);
            }
            let i = if matches!(name, "getLast" | "last") {
                list_len(vm, h).unwrap_or(0).wrapping_sub(1)
            } else {
                0
            };
            match list_get(vm, h, i) {
                Some(v) => Some(Some(v)),
                None => {
                    return Err(vm.throw_builtin("Ljava/util/NoSuchElementException;", name));
                }
            }
        }
        "peek" | "peekFirst" | "peekLast" => {
            if list_len(vm, h).is_none() {
                return Ok(None);
            }
            let i = if name == "peekLast" {
                list_len(vm, h).unwrap_or(0).wrapping_sub(1)
            } else {
                0
            };
            Some(Some(list_get(vm, h, i).unwrap_or(V::Ref(0))))
        }
        "set" => {
            // Guest-backed collections mutate themselves.
            if list_mut(vm, h).is_none() {
                return Ok(None);
            }
            let n = list_ref(vm, h).map(|l| l.len()).unwrap_or(0);
            let i = i32v(args[1]) as usize;
            if i >= n {
                return Err(vm.throw_builtin(
                    "Ljava/lang/IndexOutOfBoundsException;",
                    &format!("List.set({i}) on size {n}"),
                ));
            }
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            Some(Some(std::mem::replace(&mut l[i], args[2])))
        }
        "size" => {
            let n = list_len(vm, h).unwrap_or(0);
            Some(Some(V::I32(n as i32)))
        }
        "isEmpty" => {
            let e = list_len(vm, h).unwrap_or(0) == 0;
            Some(Some(V::I32(e as i32)))
        }
        "clear" => {
            // Guest-backed collections mutate themselves.
            if list_mut(vm, h).is_none() {
                return Ok(None);
            }
            if let Some(V::Ref(parent)) = pseudo_get(vm, h, P_SUBLIST_PARENT) {
                let fixed_parent = match vm.heap.get(parent) {
                    Some(Obj::Host { desc, .. }) => matches!(
                        desc.as_str(),
                        "Ljava/util/Arrays$ArrayList;"
                            | "Ljava/util/Collections$EmptyList;"
                            | "Ljava/util/Collections$SingletonList;"
                            | "Ljava/util/Collections$UnmodifiableList;"
                    ),
                    _ => false,
                };
                let a = pseudo_get(vm, h, P_SUBLIST_START)
                    .map(i32v)
                    .unwrap_or_default()
                    .max(0) as usize;
                let b = pseudo_get(vm, h, P_SUBLIST_END)
                    .map(i32v)
                    .unwrap_or_default()
                    .max(0) as usize;
                if fixed_parent && a < b {
                    return Err(vm.throw_builtin(
                        "Ljava/lang/UnsupportedOperationException;",
                        "clear on fixed-size subList",
                    ));
                }
                if let Some(l) = list_mut(vm, parent) {
                    let end = b.min(l.len());
                    if a < end {
                        l.drain(a..end);
                    }
                }
                pseudo_set(vm, h, P_SUBLIST_END, V::I32(a as i32));
            }
            if let Some(l) = list_mut(vm, h) {
                l.clear();
            }
            Some(None)
        }
        "contains" => {
            let items: Vec<V> = list_snapshot(vm, h).unwrap_or_default();
            let mut c = false;
            for e in &items {
                if elem_eq(vm, *e, args[1])? {
                    c = true;
                    break;
                }
            }
            if crate::dbg_flag("OVELO_CONTAINSDBG") {
                let kind = |e: &V| match e {
                    V::Ref(eh) => {
                        let k = match vm.heap.get(*eh) {
                            Some(Obj::Str(_)) => "Str",
                            Some(Obj::Instance { .. }) => "Inst",
                            Some(Obj::Host { .. }) => "Host",
                            Some(Obj::Array { .. }) => "Arr",
                            Some(Obj::Class { .. }) => "Cls",
                            _ => "?",
                        };
                        format!(
                            "h{eh}:{}[{k}]sv={:?}",
                            vm.dbg_class_of(*eh),
                            super::sv(vm, *e)
                        )
                    }
                    o => format!("{o:?}"),
                };
                let item0 = items.first().map(kind);
                let arg = kind(&args[1]);
                eprintln!(
                    "[containsdbg] h{h} {} n={} item0={item0:?} arg={arg} -> {c}",
                    vm.dbg_class_of(h),
                    items.len()
                );
            }
            Some(Some(V::I32(c as i32)))
        }
        "containsAll" => {
            if matches!(args.get(1), Some(V::Ref(0)) | None) {
                return Err(
                    vm.throw_builtin("Ljava/lang/NullPointerException;", "containsAll(null)")
                );
            }
            let mine = collection_elements_rt(vm, h)?.unwrap_or_default();
            let want = match args.get(1).copied() {
                Some(V::Ref(w)) => collection_elements_rt(vm, w)?.unwrap_or_default(),
                _ => Vec::new(),
            };
            let mut all = true;
            for w in want {
                let mut hit = false;
                for e in &mine {
                    if elem_eq(vm, *e, w)? {
                        hit = true;
                        break;
                    }
                }
                if !hit {
                    all = false;
                    break;
                }
            }
            Some(Some(V::I32(all as i32)))
        }
        "equals" => {
            let other = match args.get(1).copied() {
                Some(V::Ref(oh)) if oh == h => return Ok(Some(Some(V::I32(1)))),
                Some(V::Ref(oh)) => oh,
                _ => return Ok(Some(Some(V::I32(0)))),
            };
            let eq = collections_eq(vm, h, other, 0)?.unwrap_or(false);
            Some(Some(V::I32(eq as i32)))
        }
        "indexOf" | "lastIndexOf" => {
            let items: Vec<V> = match list_snapshot(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let mut i = None;
            if name == "indexOf" {
                for (x, e) in items.iter().enumerate() {
                    if elem_eq(vm, *e, args[1])? {
                        i = Some(x);
                        break;
                    }
                }
            } else {
                for (x, e) in items.iter().enumerate().rev() {
                    if elem_eq(vm, *e, args[1])? {
                        i = Some(x);
                        break;
                    }
                }
            }
            Some(Some(V::I32(i.map(|x| x as i32).unwrap_or(-1))))
        }
        "iterator" | "listIterator" | "descendingIterator" | "spliterator" => {
            // `list_snapshot`, not `list_ref`: guest collections backed
            // by an `Obj::Array` (kotlin ArrayDeque's `elementData`
            // ring) are invisible to `list_ref`, which only sees
            // Host::List stores — they'd iterate empty (navigation
            // `getBackStackEntry` found nothing and threw).
            let items: Vec<V> = match list_snapshot(vm, h) {
                Some(mut l) => {
                    // Obfuscated kotlin ArrayDeque fields do not expose
                    // usable head/size names. Its backing array capacity
                    // otherwise looks like logical elements, including
                    // trailing nulls; ask the guest for the real size and
                    // keep intentional nulls inside that logical window.
                    if vm.dbg_class_of(h) == "Lkotlin/collections/ArrayDeque;" {
                        if let Some(n) = vm
                            .call_virtual_rt(h, "size", &[])
                            .ok()
                            .flatten()
                            .map(|v| i64v(v).max(0) as usize)
                        {
                            l.truncate(n);
                        }
                    }
                    if crate::dbg_flag("OVELO_ITERDBG") {
                        eprintln!(
                            "[iterdbg] iterator on h{h} {} len={}",
                            vm.dbg_class_of(h),
                            l.len()
                        );
                    }
                    l
                }
                None => {
                    if crate::dbg_flag("OVELO_ITERDBG") {
                        let cls = vm.dbg_class_of(h);
                        let kind = match vm.heap.get(h) {
                            Some(Obj::Host { state, .. }) => format!("host:{state:?}"),
                            Some(Obj::Instance { fields, .. }) => {
                                format!("inst:fields={fields:?}")
                            }
                            Some(Obj::Array { data, .. }) => format!("arr:{}", data.len()),
                            other => format!("{other:?}"),
                        };
                        eprintln!("[iterdbg] iterator on h{h} {cls} {kind} -> dex");
                    }
                    return Ok(None);
                }
            };
            // `listIterator(I)` starts positioned at `I` (navigation
            // walks the back stack in reverse from the end) and
            // `descendingIterator` yields last-to-first.
            // The cursor is source-coordinate: a descending iterator
            // starts at the end and walks down (flag-driven, not a
            // reversed snapshot) so remove() can map the last-returned
            // element to its exact source index.
            let descending = name == "descendingIterator";
            let pos = if descending {
                items.len()
            } else if name == "listIterator" {
                i32v(args.get(1).copied().unwrap_or(V::I32(0))).max(0) as usize
            } else {
                0
            };
            let pos = pos.min(items.len());
            let it = host(
                vm,
                "Ljava/util/Iterator;",
                HostState::Iter(Rc::new(items), pos),
            );
            if let V::Ref(ih) = it {
                pseudo_set(vm, ih, P_ITER_SOURCE, V::Ref(h));
                pseudo_set(vm, ih, P_ITER_LAST, V::I32(-1));
                if descending {
                    pseudo_set(vm, ih, P_ITER_DESC, V::I32(1));
                }
            }
            Some(Some(it))
        }
        "toArray" => {
            let items: Vec<V> = list_snapshot(vm, h).unwrap_or_default();
            if crate::dbg_flag("OVELO_ITERDBG") && items.iter().any(|v| is_null(*v)) {
                let caller = vm
                    .stack
                    .last()
                    .map(|&m| vm.describe_method(m).to_string())
                    .unwrap_or_default();
                eprintln!(
                    "[iterdbg] toArray h{h} {} len={} nulls={} @{caller}",
                    vm.dbg_class_of(h),
                    items.len(),
                    items.iter().filter(|v| is_null(**v)).count()
                );
            }
            let elem = match args.get(1) {
                Some(V::Ref(ah)) => match vm.heap.get(*ah) {
                    Some(Obj::Array { elem, .. }) => elem.clone(),
                    _ => "Ljava/lang/Object;".into(),
                },
                _ => "Ljava/lang/Object;".into(),
            };
            Some(Some(arr(vm, &elem, items)))
        }
        "subList" => {
            let source = list_snapshot(vm, h).unwrap_or_default();
            let (a, b) = (i32v(args[1]), i32v(args[2]));
            if a < 0 || b > source.len() as i32 {
                return Err(vm.throw_builtin(
                    "Ljava/lang/IndexOutOfBoundsException;",
                    &format!("subList({a}, {b}) on size {}", source.len()),
                ));
            }
            if b < a {
                return Err(vm.throw_builtin(
                    "Ljava/lang/IllegalArgumentException;",
                    &format!("subList({a}, {b})"),
                ));
            }
            let (a, b) = (a as usize, b as usize);
            let items: Vec<V> = source
                .into_iter()
                .skip(a)
                .take(b.saturating_sub(a))
                .collect();
            let sub = vm.alloc_host("Ljava/util/List;", HostState::List(items));
            pseudo_set(vm, sub, P_SUBLIST_PARENT, V::Ref(h));
            pseudo_set(vm, sub, P_SUBLIST_START, V::I32(a as i32));
            pseudo_set(vm, sub, P_SUBLIST_END, V::I32(b as i32));
            Some(Some(V::Ref(sub)))
        }
        "forEach" | "replaceAll" | "sort" | "stream" | "parallelStream" => {
            if name == "sort" {
                let comparator = match args.get(1) {
                    Some(V::Ref(c)) if *c != 0 => *c,
                    _ => 0,
                };
                let mut items = list_snapshot(vm, h).unwrap_or_default();
                if comparator != 0 {
                    // `sortWith` feeds Compose's invalidation list into a
                    // binary search. Keep the sort stable and propagate
                    // guest comparator failures rather than silently
                    // leaving the list unordered.
                    for i in 1..items.len() {
                        let mut j = i;
                        while j > 0 {
                            let ord = vm
                                .call_virtual_rt(comparator, "compare", &[items[j - 1], items[j]])?
                                .map(i32v)
                                .unwrap_or(0);
                            if ord <= 0 {
                                break;
                            }
                            items.swap(j - 1, j);
                            j -= 1;
                        }
                    }
                } else {
                    items.sort_by(|a, b| natural_cmp(vm, *a, *b).cmp(&0));
                }
                write_sorted_list(vm, h, items);
                Some(None)
            } else if name.ends_with("stream") || name.ends_with("Stream") {
                let items = list_snapshot(vm, h).unwrap_or_default();
                Some(Some(host(
                    vm,
                    "Ljava/util/stream/Stream;",
                    HostState::List(items),
                )))
            } else {
                Some(None)
            }
        }
        "hashCode" => {
            let hc = list_snapshot(vm, h)
                .map(|l| l.len() as i32 * 31)
                .unwrap_or(1);
            Some(Some(V::I32(hc)))
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Map (covers Bundle/ContentValues keyed ops too)

fn java_props_space(c: char) -> bool {
    (char::is_whitespace(c) && !matches!(c, '\u{a0}' | '\u{2007}' | '\u{202f}'))
        || matches!(c, '\u{1c}'..='\u{1f}')
}

fn parse_properties_line(line: &str) -> Option<(String, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < chars.len() && java_props_space(chars[i]) {
        i += 1;
    }
    if i >= chars.len() || chars[i] == '#' || chars[i] == '!' {
        return None;
    }
    let mut key = String::new();
    let mut escaped = false;
    while i < chars.len() {
        let c = chars[i];
        if escaped {
            key.push(match c {
                't' => '\t',
                'n' => '\n',
                'r' => '\r',
                'f' => '\x0c',
                other => other,
            });
            escaped = false;
            i += 1;
            continue;
        }
        if c == '\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if c == '=' || c == ':' || java_props_space(c) {
            break;
        }
        key.push(c);
        i += 1;
    }
    while i < chars.len() && java_props_space(chars[i]) {
        i += 1;
    }
    if i < chars.len() && (chars[i] == '=' || chars[i] == ':') {
        i += 1;
        while i < chars.len() && java_props_space(chars[i]) {
            i += 1;
        }
    }
    let value: String = chars[i..].iter().collect();
    Some((key, value.trim_end().to_string()))
}

fn map_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let class = class_of(vm, args[0]);
    Ok(match name {
        "getProperty" if class == "Ljava/util/Properties;" => {
            let out = map_get(vm, h, args[1]).or_else(|| args.get(2).copied());
            Some(Some(out.unwrap_or(V::Ref(0))))
        }
        "load" if class == "Ljava/util/Properties;" => {
            let bytes = match args.get(1).copied() {
                Some(V::Ref(sh)) => match vm.heap.get_mut(sh) {
                    Some(Obj::Host {
                        state: HostState::Bytes(b, pos),
                        ..
                    }) => {
                        let out = b[*pos..].to_vec();
                        *pos = b.len();
                        out
                    }
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            // Properties.load(InputStream) is ISO-8859-1, not UTF-8.
            let text: String = bytes.iter().map(|b| *b as char).collect();
            for line in text.split('\n') {
                let line = line.trim_end_matches('\r');
                if let Some((k, v)) = parse_properties_line(line) {
                    let (k, v) = (jstr(vm, &k), jstr(vm, &v));
                    map_put(vm, h, k, v);
                }
            }
            Some(None)
        }
        "<init>" => {
            // Copy ctor HashMap(Map)/LinkedHashMap(Map) — kotlin
            // `Map.toMap` materializes its copy through it; without a
            // copy the destination starts empty and callers iterate
            // nothing (navigation NavigatorProvider attach loop).
            if let Some(V::Ref(sh)) = args.get(1).copied() {
                for (k, v) in map_pairs_of(vm, sh) {
                    map_put(vm, h, k, v);
                }
            }
            Some(None)
        }
        "put" => {
            if crate::dbg_flag("OVELO_MAPDBG") {
                let ks = s(
                    vm,
                    match args[1] {
                        V::Ref(h2) => h2,
                        _ => 0,
                    },
                )
                .unwrap_or_else(|| format!("{:?}", args[1]));
                let caller = vm
                    .stack
                    .last()
                    .map(|&m| vm.describe_method(m).to_string())
                    .unwrap_or_default();
                eprintln!("[map] put h={h} key={ks} @{caller}");
            }
            let prev = map_put(vm, h, args[1], args[2]);
            Some(Some(prev.unwrap_or(V::Ref(0))))
        }
        "putIfAbsent" => {
            let prev = map_get(vm, h, args[1]);
            if prev.is_none() {
                map_put(vm, h, args[1], args[2]);
            }
            Some(Some(prev.unwrap_or(V::Ref(0))))
        }
        "putAll" => {
            if let V::Ref(sh) = args[1] {
                let pairs: Vec<(V, V)> = match vm.heap.get(sh) {
                    Some(Obj::Host {
                        state: HostState::Map(m),
                        ..
                    }) => m.clone(),
                    _ => vec![],
                };
                for (k, v) in pairs {
                    map_put(vm, h, k, v);
                }
            }
            Some(None)
        }
        "putInt"
        | "putLong"
        | "putFloat"
        | "putDouble"
        | "putBoolean"
        | "putString"
        | "putChar"
        | "putShort"
        | "putByte"
        | "putParcelable"
        | "putBinder"
        | "putSerializable"
        | "putBundle"
        | "putCharSequence"
        | "putIntegerArrayList"
        | "putStringArrayList"
        | "putParcelableArrayList"
        | "putIntArray"
        | "putLongArray"
        | "putBooleanArray"
        | "putByteArray"
        | "putFloatArray"
        | "putDoubleArray"
        | "putCharArray"
        | "putShortArray"
        | "putStringArray"
        | "putParcelableArray"
        | "putCharSequenceArray" => {
            map_put(vm, h, args[1], args[2]);
            Some(None)
        }
        "get" | "getOrDefault" | "remove" | "removeObject" => {
            if crate::dbg_flag("OVELO_MAPDBG") {
                let ks = s(
                    vm,
                    match args[1] {
                        V::Ref(h2) => h2,
                        _ => 0,
                    },
                )
                .unwrap_or_else(|| format!("{:?}", args[1]));
                let hit = map_get(vm, h, args[1]).is_some();
                let caller = vm
                    .stack
                    .last()
                    .map(|&m| vm.describe_method(m).to_string())
                    .unwrap_or_default();
                eprintln!("[map] get h={h} key={ks} -> {hit} @{caller}");
            }
            if name == "remove" || name == "removeObject" {
                Some(Some(map_remove(vm, h, args[1]).unwrap_or(V::Ref(0))))
            } else {
                // Primitive-valued sparse maps return a numeric default on a
                // miss, not null — `SparseIntArray.get(key)` is `0`, and a
                // null Ref crashes `i32` consumers (e.g. packed-switch in
                // ConstraintLayout.LayoutParams.<clinit>-driven minting).
                let int_valued = matches!(
                    class.as_str(),
                    "Landroid/util/SparseIntArray;"
                        | "Landroid/util/SparseBooleanArray;"
                        | "Landroidx/collection/SparseIntArray;"
                );
                let v = map_get(vm, h, args[1]);
                Some(Some(match v {
                    Some(v) => v,
                    None => args.get(2).copied().unwrap_or(if int_valued {
                        V::I32(0)
                    } else {
                        V::Ref(0)
                    }),
                }))
            }
        }
        "getInt" | "getLong" | "getFloat" | "getDouble" | "getBoolean" | "getChar" | "getShort"
        | "getByte" => {
            let v = map_get(vm, h, args[1]);
            let def = args.get(2).copied().unwrap_or(V::I32(0));
            let v = v.unwrap_or(def);
            Some(Some(match name {
                "getLong" => V::I64(i64v(v)),
                "getFloat" => V::F32(f64v(v) as f32),
                "getDouble" => V::F64(f64v(v)),
                "getBoolean" => V::I32((i32v(v) != 0) as i32),
                _ => V::I32(i32v(v)),
            }))
        }
        "getString"
        | "getParcelable"
        | "getBinder"
        | "getSerializable"
        | "getBundle"
        | "getCharSequence"
        | "getIntegerArrayList"
        | "getStringArrayList"
        | "getParcelableArrayList"
        | "getIntArray"
        | "getLongArray"
        | "getBooleanArray"
        | "getByteArray"
        | "getFloatArray"
        | "getDoubleArray"
        | "getCharArray"
        | "getShortArray"
        | "getStringArray"
        | "getParcelableArray"
        | "getCharSequenceArray"
        | "getClassLoader" => {
            Some(Some(
                // `getClassLoader` takes no key arg — a zero-arg
                // call must not index args[1].
                args.get(1)
                    .and_then(|k| map_get(vm, h, *k))
                    .or_else(|| match args.get(2) {
                        // `getParcelable(key, Class)` and friends take a type
                        // token as the last arg, not a default value — a miss
                        // must yield null, not the Class object itself.
                        Some(V::Ref(dh)) if matches!(vm.heap.get(*dh), Some(Obj::Class { .. })) => {
                            None
                        }
                        other => other.copied(),
                    })
                    .unwrap_or(V::Ref(0)),
            ))
        }
        "containsKey" | "contains" | "containsString" => {
            let c = map_get(vm, h, args[1]).is_some();
            Some(Some(V::I32(c as i32)))
        }
        "containsValue" => {
            let c = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.iter().any(|(_, v)| value_eq(vm, *v, args[1])),
                _ => false,
            };
            Some(Some(V::I32(c as i32)))
        }
        "isEmpty" => Some(Some(V::I32((map_len(vm, h) == 0) as i32))),
        "size" => Some(Some(V::I32(map_len(vm, h) as i32))),
        "clear" => {
            if let Some(m) = map_mut(vm, h) {
                m.clear();
            }
            Some(None)
        }
        "keySet" | "entrySet" | "values" => {
            // Guest Map subclasses (nanojson `JsonObject extends
            // HashMap`) keep pairs in a lazily-minted backing — guest
            // serializers iterate these views (JsonStringWriter
            // emitted `{}` when they came back empty).
            let pairs: Vec<(V, V)> = map_pairs_of(vm, h);
            let out: Vec<V> = match name {
                "entrySet" => pairs
                    .into_iter()
                    .map(|(k, v)| {
                        let eh = vm
                            .alloc_host("Ljava/util/AbstractMap$SimpleEntry;", HostState::Slot(k));
                        pseudo_set(vm, eh, P_DATA, v);
                        V::Ref(eh)
                    })
                    .collect(),
                "keySet" => pairs.into_iter().map(|(k, _)| k).collect(),
                _ => pairs.into_iter().map(|(_, v)| v).collect(),
            };
            let desc = match name {
                "values" => "Ljava/util/Collection;",
                _ => "Ljava/util/Set;",
            };
            Some(Some(host(vm, desc, HostState::List(out))))
        }
        "compute" | "computeIfAbsent" | "computeIfPresent" | "merge" => {
            let cur = map_get(vm, h, args[1]);
            match name {
                "putIfAbsent" => {
                    if cur.is_none() {
                        map_put(vm, h, args[1], args[2]);
                    }
                    Some(Some(cur.unwrap_or(V::Ref(0))))
                }
                "computeIfAbsent" | "computeIfPresent" | "compute" | "merge" => {
                    // Call the guest function for real when supplied.
                    let res = match name {
                        "merge" if cur.is_some() => cur,
                        "merge" => Some(args[2]),
                        "computeIfAbsent" if cur.is_some() => cur,
                        "computeIfPresent" if cur.is_none() => None,
                        _ => {
                            let f = args.last().copied().unwrap_or(V::Ref(0));
                            if let V::Ref(fh) = f {
                                let key = args[1];
                                let old = cur.unwrap_or(V::Ref(0));
                                match vm
                                    .call_virtual(fh, "apply", &[key, old])
                                    .or_else(|_| vm.call_virtual(fh, "apply", &[key]))
                                {
                                    Ok(v) => v,
                                    Err(_) => cur,
                                }
                            } else {
                                cur
                            }
                        }
                    };
                    match res {
                        Some(v) if !is_null(v) => {
                            map_put(vm, h, args[1], v);
                            Some(Some(v))
                        }
                        _ => {
                            map_remove(vm, h, args[1]);
                            Some(Some(V::Ref(0)))
                        }
                    }
                }
                _ => Some(Some(V::Ref(0))),
            }
        }
        "forEach" | "replaceAll" => Some(None),
        "describeContents" | "writeToParcel" | "readFromParcel" => Some(None),
        "clone" => {
            let m: Vec<(V, V)> = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.clone(),
                _ => vec![],
            };
            Some(Some(host(vm, &class.clone(), HostState::Map(m))))
        }
        "toString" => {
            let mut parts = vec!["{".to_string()];
            if let Some(Obj::Host {
                state: HostState::Map(m),
                ..
            }) = vm.heap.get(h)
            {
                let kv: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{}={}", fmt_v(*k), fmt_v(*v)))
                    .collect();
                parts.push(kv.join(", "));
            }
            parts.push("}".into());
            Some(Some(jstr(vm, &parts.concat())))
        }
        "hashCode" => Some(Some(V::I32(map_len(vm, h) as i32))),
        "equals" => Some(Some(V::I32(1))),
        // NavigableMap/SortedMap key navigation — TreeMap callers
        // (e.g. Glide's LruArrayPool keyMap.ceilingKey) depend on real
        // ordering; a placeholder non-null answer here mints lies the
        // caller then reads fields off of.
        "ceilingKey" | "floorKey" | "higherKey" | "lowerKey" | "firstKey" | "lastKey" => {
            let pairs = map_pairs(vm, h);
            let nm = nav_name(name, nav_desc(vm, h));
            match nav_pick(vm, &pairs, nm, args.get(1).copied()) {
                Some((k, _)) => Some(Some(k)),
                None if name == "firstKey" || name == "lastKey" => {
                    return Err(vm.throw_builtin("Ljava/util/NoSuchElementException;", name));
                }
                None => Some(Some(V::Ref(0))),
            }
        }
        "ceilingEntry" | "floorEntry" | "higherEntry" | "lowerEntry" | "firstEntry"
        | "lastEntry" => {
            let pairs = map_pairs(vm, h);
            let nm = nav_name(name, nav_desc(vm, h));
            Some(Some(match nav_pick(vm, &pairs, nm, args.get(1).copied()) {
                Some((k, v)) => entry_of(vm, k, v),
                None => V::Ref(0),
            }))
        }
        "pollFirstEntry" | "pollLastEntry" => {
            let pairs = map_pairs(vm, h);
            let op = if name == "pollFirstEntry" {
                "first"
            } else {
                "last"
            };
            let op = nav_name(op, nav_desc(vm, h));
            match nav_pick(vm, &pairs, op, None) {
                Some((k, v)) => {
                    let _ = map_remove(vm, h, k);
                    Some(Some(entry_of(vm, k, v)))
                }
                None => Some(Some(V::Ref(0))),
            }
        }
        "headMap" | "tailMap" | "subMap" => {
            let pairs = map_pairs(vm, h);
            let desc_view = nav_desc(vm, h);
            // Bounds in (lo, lo_incl, hi, hi_incl); on a descending view
            // head/tail swap and subMap's from/to flip roles.
            let (lo, lo_incl, hi, hi_incl) = match (name, desc_view) {
                ("headMap", false) => (
                    None,
                    false,
                    args.get(1).copied(),
                    args.get(2).map(|v| i32v(*v) != 0).unwrap_or(false),
                ),
                ("headMap", true) => (
                    args.get(1).copied(),
                    args.get(2).map(|v| i32v(*v) != 0).unwrap_or(false),
                    None,
                    false,
                ),
                ("tailMap", false) => (
                    args.get(1).copied(),
                    args.get(2).map(|v| i32v(*v) != 0).unwrap_or(true),
                    None,
                    false,
                ),
                ("tailMap", true) => (
                    None,
                    false,
                    args.get(1).copied(),
                    args.get(2).map(|v| i32v(*v) != 0).unwrap_or(true),
                ),
                (_, false) if args.len() >= 5 => (
                    args.get(1).copied(),
                    args.get(2).map(|v| i32v(*v) != 0).unwrap_or(true),
                    args.get(3).copied(),
                    args.get(4).map(|v| i32v(*v) != 0).unwrap_or(false),
                ),
                (_, true) if args.len() >= 5 => (
                    args.get(3).copied(),
                    args.get(4).map(|v| i32v(*v) != 0).unwrap_or(false),
                    args.get(1).copied(),
                    args.get(2).map(|v| i32v(*v) != 0).unwrap_or(true),
                ),
                (_, false) => (args.get(1).copied(), true, args.get(2).copied(), false),
                (_, true) => (args.get(2).copied(), true, args.get(1).copied(), false),
            };
            let mut out = Vec::new();
            for &(k, v) in &pairs {
                let lo_ok = lo.is_none_or(|p| {
                    let c = key_cmp(vm, k, p);
                    c == std::cmp::Ordering::Greater || (lo_incl && c == std::cmp::Ordering::Equal)
                });
                let hi_ok = hi.is_none_or(|p| {
                    let c = key_cmp(vm, k, p);
                    c == std::cmp::Ordering::Less || (hi_incl && c == std::cmp::Ordering::Equal)
                });
                if lo_ok && hi_ok {
                    out.push((k, v));
                }
            }
            let v = host(vm, "Ljava/util/TreeMap;", HostState::Map(out));
            if let (V::Ref(oh), true) = (v, desc_view) {
                pseudo_set(vm, oh, P_NAV_DESC, V::I32(1));
            }
            Some(Some(v))
        }
        "navigableKeySet" | "descendingKeySet" => {
            let mut keys: Vec<V> = map_pairs(vm, h).into_iter().map(|(k, _)| k).collect();
            sort_keys(vm, &mut keys);
            if (name == "descendingKeySet") != nav_desc(vm, h) {
                keys.reverse();
            }
            Some(Some(host(
                vm,
                "Ljava/util/NavigableSet;",
                HostState::List(keys),
            )))
        }
        "descendingMap" => {
            let mut pairs = map_pairs(vm, h);
            sort_pairs(vm, &mut pairs);
            // descendingMap() of a descending view is ascending again.
            let desc_view = !nav_desc(vm, h);
            if desc_view {
                pairs.reverse();
            }
            let v = host(vm, "Ljava/util/TreeMap;", HostState::Map(pairs));
            if let (V::Ref(oh), true) = (v, desc_view) {
                pseudo_set(vm, oh, P_NAV_DESC, V::I32(1));
            }
            Some(Some(v))
        }
        "comparator" => Some(Some(V::Ref(0))),
        _ => {
            let _ = sig;
            None
        }
    })
}

/// Snapshot of a host map's pairs (empty for non-map receivers).
fn map_pairs(vm: &Vm, h: u32) -> Vec<(V, V)> {
    map_pairs_of(vm, h)
}

/// Natural ordering for sorted-map keys: boxed numbers compare
/// numerically, strings lexicographically; anything else goes through
/// the guest's own `Comparable.compareTo` when it answers.
fn key_cmp(vm: &mut Vm, a: V, b: V) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let unbox = |v: V| match v {
        V::Ref(h) => slot_of(vm, h).unwrap_or(v),
        _ => v,
    };
    let (pa, pb) = (unbox(a), unbox(b));
    let num = |v: V| matches!(v, V::I32(_) | V::I64(_) | V::F32(_) | V::F64(_));
    if num(pa) && num(pb) {
        return f64v(pa).partial_cmp(&f64v(pb)).unwrap_or(Ordering::Equal);
    }
    if let (Some(ta), Some(tb)) = (sv(vm, pa), sv(vm, pb)) {
        return ta.cmp(&tb);
    }
    if let V::Ref(ha) = a {
        if ha != 0 {
            if let Ok(Some(c)) = vm.call_virtual(ha, "compareTo", &[b]) {
                return i32v(c).cmp(&0);
            }
        }
    }
    Ordering::Equal
}

fn sort_keys(vm: &mut Vm, keys: &mut [V]) {
    // insertion sort — these maps stay small, and `sort_by` can't take
    // a `&mut Vm` comparator.
    for i in 1..keys.len() {
        let mut j = i;
        while j > 0 && key_cmp(vm, keys[j], keys[j - 1]) == std::cmp::Ordering::Less {
            keys.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sort_pairs(vm: &mut Vm, pairs: &mut [(V, V)]) {
    let mut keys: Vec<V> = pairs.iter().map(|(k, _)| *k).collect();
    sort_keys(vm, &mut keys);
    let mut out = Vec::with_capacity(pairs.len());
    for k in keys {
        if let Some(pos) = pairs.iter().position(|(pk, _)| *pk == k) {
            out.push(pairs[pos]);
        }
    }
    pairs.clone_from_slice(&out);
}

/// True when `h` is a `descendingMap()` view — key navigation inverts.
fn nav_desc(vm: &Vm, h: u32) -> bool {
    pseudo_get(vm, h, P_NAV_DESC).is_some_and(|v| i32v(v) != 0)
}

/// The nav op a `descendingMap()` view must actually run for `name`:
/// "first" of a descending order is the largest key, "ceiling" is the
/// nearest key on the lower side, and so on.
fn nav_name(name: &str, desc: bool) -> &str {
    if !desc {
        return name;
    }
    match name {
        "ceilingKey" => "floorKey",
        "floorKey" => "ceilingKey",
        "higherKey" => "lowerKey",
        "lowerKey" => "higherKey",
        "firstKey" => "lastKey",
        "lastKey" => "firstKey",
        "ceilingEntry" => "floorEntry",
        "floorEntry" => "ceilingEntry",
        "higherEntry" => "lowerEntry",
        "lowerEntry" => "higherEntry",
        "firstEntry" => "lastEntry",
        "lastEntry" => "firstEntry",
        "first" => "last",
        "last" => "first",
        _ => name,
    }
}

/// `first/last` scan the extremes; `ceiling/floor/higher/lower` take a
/// pivot and pick the nearest key on the required side. `name` is the
/// called method (`ceilingKey`, `firstEntry`, …) or a bare `first`/`last`.
fn nav_pick(vm: &mut Vm, pairs: &[(V, V)], name: &str, pivot: Option<V>) -> Option<(V, V)> {
    use std::cmp::Ordering::*;
    let base = name
        .trim_end_matches("Key")
        .trim_end_matches("Entry")
        .to_lowercase();
    let mut best: Option<(V, V)> = None;
    for &(k, v) in pairs {
        let take = match base.as_str() {
            "ceiling" => {
                pivot.is_some_and(|p| key_cmp(vm, k, p) != Less)
                    && best.is_none_or(|(bk, _)| key_cmp(vm, k, bk) == Less)
            }
            "floor" => {
                pivot.is_some_and(|p| key_cmp(vm, k, p) != Greater)
                    && best.is_none_or(|(bk, _)| key_cmp(vm, k, bk) == Greater)
            }
            "higher" => {
                pivot.is_some_and(|p| key_cmp(vm, k, p) == Greater)
                    && best.is_none_or(|(bk, _)| key_cmp(vm, k, bk) == Less)
            }
            "lower" => {
                pivot.is_some_and(|p| key_cmp(vm, k, p) == Less)
                    && best.is_none_or(|(bk, _)| key_cmp(vm, k, bk) == Greater)
            }
            "first" => best.is_none_or(|(bk, _)| key_cmp(vm, k, bk) == Less),
            "last" => best.is_none_or(|(bk, _)| key_cmp(vm, k, bk) == Greater),
            _ => false,
        };
        if take {
            best = Some((k, v));
        }
    }
    best
}

/// A `Map.Entry` host object — key in the Slot and P_COMPONENT, value
/// under P_DATA, matching the `entrySet` mint.
fn entry_of(vm: &mut Vm, k: V, v: V) -> V {
    let eh = vm.alloc_host("Ljava/util/AbstractMap$SimpleEntry;", HostState::Slot(k));
    pseudo_set(vm, eh, P_COMPONENT, k);
    pseudo_set(vm, eh, P_DATA, v);
    V::Ref(eh)
}

// ---------------------------------------------------------------------
// Iterator / Enumeration / Map.Entry

fn iter_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "<init>" => {
            // SimpleEntry/SimpleImmutableEntry(key, value) — the
            // desugarer builds these for Map.entry/ofEntries. The key
            // goes to P_COMPONENT (or the Slot when the object already
            // carries one), the value to P_DATA.
            if let Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) = vm.heap.get_mut(h)
            {
                *v = args.get(1).copied().unwrap_or(V::Ref(0));
            }
            pseudo_set(
                vm,
                h,
                P_COMPONENT,
                args.get(1).copied().unwrap_or(V::Ref(0)),
            );
            pseudo_set(vm, h, P_DATA, args.get(2).copied().unwrap_or(V::Ref(0)));
            Some(None)
        }
        "hasNext" | "hasMoreElements" => {
            let desc = matches!(pseudo_get(vm, h, P_ITER_DESC), Some(V::I32(1)));
            let n = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Iter(items, pos),
                    ..
                }) => {
                    let len = live_list_len(vm, h).unwrap_or(items.len());
                    if desc {
                        (*pos > 0) as i32
                    } else {
                        (*pos < len) as i32
                    }
                }
                Some(Obj::Host {
                    state: HostState::List(l),
                    ..
                }) => (!l.is_empty()) as i32,
                _ => 0,
            };
            Some(Some(V::I32(n)))
        }
        "next" | "nextElement" => {
            if matches!(
                vm.heap.get(h),
                Some(Obj::Host {
                    state: HostState::Iter(..),
                    ..
                })
            ) {
                let desc = matches!(pseudo_get(vm, h, P_ITER_DESC), Some(V::I32(1)));
                let (pos, len) = match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::Iter(items, pos),
                        ..
                    }) => (*pos, live_list_len(vm, h).unwrap_or(items.len())),
                    _ => (0, 0),
                };
                let at = if desc {
                    if pos == 0 {
                        None
                    } else {
                        Some(pos - 1)
                    }
                } else if pos < len {
                    Some(pos)
                } else {
                    None
                };
                let Some(at) = at else {
                    let e = vm
                        .throw_builtin("Ljava/util/NoSuchElementException;", "iterator exhausted");
                    return Err(e);
                };
                let v = live_list_get(vm, h, at)
                    .or_else(|| match vm.heap.get(h) {
                        Some(Obj::Host {
                            state: HostState::Iter(items, _),
                            ..
                        }) => items.get(at).copied(),
                        _ => None,
                    })
                    .unwrap_or(V::Ref(0));
                if crate::dbg_flag("OVELO_ITERDBG") && matches!(v, V::Ref(0)) {
                    eprintln!("[iterdbg] next on h{h} at={at} yields null");
                }
                if let Some(Obj::Host {
                    state: HostState::Iter(_, pos),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    *pos = if desc { at } else { at + 1 };
                }
                pseudo_set(vm, h, P_ITER_LAST, V::I32(at as i32));
                pseudo_set(vm, h, P_ITER_LASTV, v);
                return Ok(Some(Some(v)));
            }
            let (v, ok) = match vm.heap.get_mut(h) {
                Some(Obj::Host {
                    state: HostState::List(l),
                    ..
                }) => {
                    if l.is_empty() {
                        (V::Ref(0), false)
                    } else {
                        (l.remove(0), true)
                    }
                }
                _ => (V::Ref(0), false),
            };
            if !ok {
                let e =
                    vm.throw_builtin("Ljava/util/NoSuchElementException;", "iterator exhausted");
                return Err(e);
            }
            Some(Some(v))
        }
        "hasPrevious" => {
            let desc = matches!(pseudo_get(vm, h, P_ITER_DESC), Some(V::I32(1)));
            let n = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Iter(items, pos),
                    ..
                }) => {
                    let len = live_list_len(vm, h).unwrap_or(items.len());
                    if desc {
                        (*pos < len) as i32
                    } else {
                        (*pos > 0) as i32
                    }
                }
                _ => 0,
            };
            Some(Some(V::I32(n)))
        }
        "previous" => {
            let desc = matches!(pseudo_get(vm, h, P_ITER_DESC), Some(V::I32(1)));
            let (pos, len) = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Iter(items, pos),
                    ..
                }) => (*pos, live_list_len(vm, h).unwrap_or(items.len())),
                _ => (0, 0),
            };
            let at = if desc {
                if pos < len {
                    Some(pos)
                } else {
                    None
                }
            } else if pos > 0 {
                Some(pos - 1)
            } else {
                None
            };
            let Some(at) = at else {
                let e = vm.throw_builtin("Ljava/util/NoSuchElementException;", "previous at start");
                return Err(e);
            };
            let v = live_list_get(vm, h, at)
                .or_else(|| match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::Iter(items, _),
                        ..
                    }) => items.get(at).copied(),
                    _ => None,
                })
                .unwrap_or(V::Ref(0));
            if let Some(Obj::Host {
                state: HostState::Iter(_, pos),
                ..
            }) = vm.heap.get_mut(h)
            {
                *pos = if desc { at + 1 } else { at };
            }
            pseudo_set(vm, h, P_ITER_LAST, V::I32(at as i32));
            pseudo_set(vm, h, P_ITER_LASTV, v);
            Some(Some(v))
        }
        "nextIndex" | "previousIndex" => {
            let n = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Iter(items, pos),
                    ..
                }) => if name == "nextIndex" {
                    *pos as i32
                } else {
                    *pos as i32 - 1
                }
                .clamp(-1, items.len() as i32),
                _ => -1,
            };
            Some(Some(V::I32(n)))
        }
        "remove" => {
            let last = match pseudo_get(vm, h, P_ITER_LAST) {
                Some(V::I32(n)) => n,
                _ => -1,
            };
            if last < 0 {
                let e = vm.throw_builtin(
                    "Ljava/lang/IllegalStateException;",
                    "remove before next/previous",
                );
                return Err(e);
            }
            if iter_remove_unsupported(vm, h) {
                let e = vm.throw_builtin(
                    "Ljava/lang/UnsupportedOperationException;",
                    "iterator remove",
                );
                return Err(e);
            }
            // Index-exact removal while the index still holds the
            // returned element; if a `poll()`/direct removal shifted
            // the list mid-iteration, re-locate the element by value
            // (absent → it is already gone, nothing to remove).
            let at = last as usize;
            let item = pseudo_get(vm, h, P_ITER_LASTV).unwrap_or(V::Ref(0));
            let mut removed_at = None;
            if let Some(V::Ref(src)) = pseudo_get(vm, h, P_ITER_SOURCE) {
                let live = matches!(
                    vm.heap.get(src),
                    Some(Obj::Host {
                        state: HostState::List(_),
                        desc,
                        ..
                    }) if !is_cow_desc(desc)
                );
                if live {
                    let idx = {
                        let hit = match vm.heap.get(src) {
                            Some(Obj::Host {
                                state: HostState::List(l),
                                ..
                            }) => {
                                if at < l.len() && value_eq(vm, l[at], item) {
                                    Some(at)
                                } else {
                                    l.iter().position(|e| value_eq(vm, *e, item))
                                }
                            }
                            _ => None,
                        };
                        hit
                    };
                    if let Some(pos) = idx {
                        if let Some(Obj::Host {
                            state: HostState::List(l),
                            ..
                        }) = vm.heap.get_mut(src)
                        {
                            if pos < l.len() {
                                l.remove(pos);
                                removed_at = Some(pos);
                            }
                        }
                    }
                }
            }
            if let Some(rm) = removed_at {
                // Everything at/after the removed index shifted down —
                // rewind the cursor when it sat past the element.
                if let Some(Obj::Host {
                    state: HostState::Iter(_, pos),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    if rm < *pos {
                        *pos -= 1;
                    }
                }
            }
            pseudo_set(vm, h, P_ITER_LAST, V::I32(-1));
            Some(None)
        }
        "set" | "add" | "forEachRemaining" => Some(None),
        "getKey" => Some(Some(
            slot_of(vm, h)
                .or_else(|| pseudo_get(vm, h, P_COMPONENT))
                .unwrap_or(V::Ref(0)),
        )),
        "getValue" => Some(Some(pseudo_get(vm, h, P_DATA).unwrap_or(V::Ref(0)))),
        "setValue" => {
            let old = pseudo_get(vm, h, P_DATA).unwrap_or(V::Ref(0));
            pseudo_set(vm, h, P_DATA, args[1]);
            Some(Some(old))
        }
        "getCount" => Some(Some(V::I32(0))),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Collections / Arrays / Objects statics

fn collections_static(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "emptyList" | "emptySet" | "emptyMap" | "emptyIterator" | "emptyEnumeration"
        | "emptyListIterator" | "emptyNavigableSet" | "emptySortedSet" | "emptyNavigableMap"
        | "emptySortedMap" => {
            let (d, st) = if name.contains("Map") {
                ("Ljava/util/Map;", HostState::Map(vec![]))
            } else if name.contains("Iterator") || name.contains("Enumeration") {
                ("Ljava/util/Iterator;", HostState::Iter(Rc::new(vec![]), 0))
            } else if name.contains("Set") {
                ("Ljava/util/Collections$EmptySet;", HostState::List(vec![]))
            } else {
                ("Ljava/util/Collections$EmptyList;", HostState::List(vec![]))
            };
            Some(Some(host(vm, d, st)))
        }
        "singletonList" => Some(Some(host(
            vm,
            "Ljava/util/Collections$SingletonList;",
            HostState::List(vec![args[0]]),
        ))),
        "singleton" => Some(Some(host(
            vm,
            "Ljava/util/Collections$SingletonSet;",
            HostState::List(vec![args[0]]),
        ))),
        "singletonMap" => Some(Some(host(
            vm,
            "Ljava/util/Map;",
            HostState::Map(vec![(args[0], args[1])]),
        ))),
        "unmodifiableList"
        | "unmodifiableSet"
        | "unmodifiableCollection"
        | "unmodifiableSortedSet"
        | "unmodifiableNavigableSet"
        | "synchronizedList"
        | "synchronizedSet"
        | "synchronizedCollection"
        | "checkedList"
        | "checkedSet"
        | "checkedCollection"
        | "unmodifiableSortedMap"
        | "unmodifiableNavigableMap"
        | "unmodifiableMap"
        | "synchronizedMap"
        | "checkedMap"
        | "checkedNavigableMap"
        | "checkedSortedMap" => Some(Some(args[0])),
        "shuffle" => {
            if let V::Ref(h) = args[0] {
                if let Some(l) = list_mut(vm, h) {
                    l.reverse(); // deterministic stand-in for a shuffle
                }
            }
            Some(None)
        }
        "reverse" => {
            if let V::Ref(h) = args[0] {
                if let Some(l) = list_mut(vm, h) {
                    l.reverse();
                }
            }
            Some(None)
        }
        "sort" => {
            if let V::Ref(h) = args[0] {
                let comparator = match args.get(1) {
                    Some(V::Ref(c)) if *c != 0 => *c,
                    _ => 0,
                };
                let mut items = list_snapshot(vm, h).unwrap_or_default();
                if comparator != 0 {
                    for i in 1..items.len() {
                        let mut j = i;
                        while j > 0 {
                            let ord = vm
                                .call_virtual_rt(comparator, "compare", &[items[j - 1], items[j]])?
                                .map(i32v)
                                .unwrap_or(0);
                            if ord <= 0 {
                                break;
                            }
                            items.swap(j - 1, j);
                            j -= 1;
                        }
                    }
                } else {
                    items.sort_by(|a, b| natural_cmp(vm, *a, *b).cmp(&0));
                }
                write_sorted_list(vm, h, items);
            }
            Some(None)
        }
        "swap" => {
            if let V::Ref(h) = args[0] {
                if let Some(l) = list_mut(vm, h) {
                    let (a, b) = (i32v(args[1]) as usize, i32v(args[2]) as usize);
                    if a < l.len() && b < l.len() {
                        l.swap(a, b);
                    }
                }
            }
            Some(None)
        }
        "binarySearch" => {
            let idx = match args[0] {
                V::Ref(h) => list_ref(vm, h)
                    .and_then(|l| l.iter().position(|e| value_eq(vm, *e, args[1])))
                    .map(|i| i as i32)
                    .unwrap_or(-1),
                _ => -1,
            };
            Some(Some(V::I32(idx)))
        }
        "indexOfSubList" | "lastIndexOfSubList" => Some(Some(V::I32(-1))),
        "min" | "max" => {
            let l = match args[0] {
                V::Ref(h) => list_snapshot(vm, h).unwrap_or_default(),
                _ => vec![],
            };
            let best = l.into_iter().reduce(|a, b| {
                let c = natural_cmp(vm, a, b);
                if (name == "min" && c < 0) || (name == "max" && c > 0) {
                    a
                } else {
                    b
                }
            });
            Some(Some(best.unwrap_or(V::Ref(0))))
        }
        "fill" | "copy" | "replaceAll" | "rotate" => {
            if name == "fill" {
                if let V::Ref(h) = args[0] {
                    if let Some(l) = list_mut(vm, h) {
                        for e in l.iter_mut() {
                            *e = args[1];
                        }
                    }
                }
            }
            Some(None)
        }
        "frequency" => {
            let n = match args[0] {
                V::Ref(h) => list_snapshot(vm, h)
                    .map(|l| l.iter().filter(|e| value_eq(vm, **e, args[1])).count())
                    .unwrap_or(0),
                _ => 0,
            };
            Some(Some(V::I32(n as i32)))
        }
        "disjoint" => Some(Some(V::I32(0))),
        "addAll" => {
            // Collections.addAll(c, T... elements): the vararg arrives
            // as a single T[] arg — push its elements, not the array
            // itself, or `getAppDatabase`-style loops later field-read
            // the array and crash.
            if let V::Ref(h) = args[0] {
                for v in &args[1..] {
                    let items: Vec<V> = match v {
                        V::Ref(ah) => match vm.heap.get(*ah) {
                            Some(Obj::Array { data, .. }) => data.clone(),
                            _ => vec![*v],
                        },
                        _ => vec![*v],
                    };
                    if let Some(l) = list_mut(vm, h) {
                        l.extend(items);
                    }
                }
            }
            Some(Some(V::I32(1)))
        }
        "nCopies" => {
            let n = i32v(args[0]).max(0) as usize;
            Some(Some(host(
                vm,
                "Ljava/util/List;",
                HostState::List(vec![args[1]; n]),
            )))
        }
        "list" => {
            // enumeration → list
            Some(Some(host(
                vm,
                "Ljava/util/ArrayList;",
                HostState::List(vec![]),
            )))
        }
        "enumeration" => {
            let items = match args[0] {
                V::Ref(h) => list_snapshot(vm, h).unwrap_or_default(),
                _ => vec![],
            };
            Some(Some(host(
                vm,
                "Ljava/util/Enumeration;",
                HostState::Iter(Rc::new(items), 0),
            )))
        }
        "reverseOrder" => {
            let mode = match args.first().copied().unwrap_or(V::Ref(0)) {
                V::Ref(c) if c != 0 => V::Ref(c),
                _ => V::I32(1),
            };
            Some(Some(host(
                vm,
                "Ljava/util/Comparator;",
                HostState::Slot(mode),
            )))
        }
        "naturalOrder" | "comparing" | "comparingInt" | "comparingLong" | "comparingDouble" => {
            Some(Some(host(
                vm,
                "Ljava/util/Comparator;",
                HostState::Slot(V::I32(0)),
            )))
        }
        "newSetFromMap" => Some(Some(host(vm, "Ljava/util/Set;", HostState::List(vec![])))),
        "synchronizedCollection0" | "unmodifiableCollection0" => Some(Some(V::Ref(0))),
        _ => None,
    })
}

fn arrays_static(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "asList" => {
            let items = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => args.to_vec(),
                },
                _ => args.to_vec(),
            };
            // Fixed-size per Arrays$ArrayList — the descriptor names it
            // so iterator remove() can throw UOE like the JDK.
            Some(Some(host(
                vm,
                "Ljava/util/Arrays$ArrayList;",
                HostState::List(items),
            )))
        }
        "toString" | "deepToString" => {
            let t = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => {
                        let parts: Vec<String> = data.iter().map(|v| fmt_v(*v)).collect();
                        format!("[{}]", parts.join(", "))
                    }
                    _ => "[]".into(),
                },
                _ => "null".into(),
            };
            Some(Some(jstr(vm, &t)))
        }
        "hashCode" | "deepHashCode" => Some(Some(V::I32(1))),
        "equals" | "deepEquals" => {
            let arrays = match (args[0], args[1]) {
                (V::Ref(a), V::Ref(b)) => match (vm.heap.get(a), vm.heap.get(b)) {
                    (
                        Some(Obj::Array { elem: ex, data: x }),
                        Some(Obj::Array { elem: ey, data: y }),
                    ) => Some((ex.clone(), x.clone(), ey.clone(), y.clone())),
                    _ => None,
                },
                _ => None,
            };
            let eq = if name == "deepEquals" {
                match (args[0], args[1]) {
                    (V::Ref(a), V::Ref(b)) => arrays_deep_eq(vm, a, b, 0)?,
                    _ => args[0] == args[1],
                }
            } else if let Some((ex, x, _ey, y)) = arrays {
                if x.len() != y.len() {
                    false
                } else {
                    let mut all_eq = true;
                    for (&a, &b) in x.iter().zip(y.iter()) {
                        let one = match (ex.as_str(), a, b) {
                            // Java primitive array equality uses the same
                            // bit-level rules as Float/Double.equals: NaN is
                            // equal to itself, while +0.0 and -0.0 differ.
                            ("F", V::F32(a), V::F32(b)) => {
                                a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
                            }
                            ("D", V::F64(a), V::F64(b)) => {
                                a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
                            }
                            // Byte arrays in the wild mix legacy unsigned
                            // storage with Java's signed-byte form.
                            ("B", V::I32(a), V::I32(b)) => a as u8 == b as u8,
                            _ if ex.starts_with('L') || ex.starts_with('[') => elem_eq(vm, a, b)?,
                            _ => a == b,
                        };
                        if !one {
                            all_eq = false;
                            break;
                        }
                    }
                    all_eq
                }
            } else {
                args[0] == args[1]
            };
            Some(Some(V::I32(eq as i32)))
        }
        "copyOf" | "copyOfRange" => {
            let data = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => vec![],
                },
                _ => vec![],
            };
            let elem = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { elem, .. }) => elem.clone(),
                    _ => "Ljava/lang/Object;".into(),
                },
                _ => "Ljava/lang/Object;".into(),
            };
            // The 3-arg `copyOf`/`copyOfRange` carries the target array
            // type as a `Class` — `ArrayList.toArray(MemberScope[])` calls
            // `copyOf(Object[], n, MemberScope[].class)` and must yield
            // `MemberScope[]`, not `Object[]` (downstream check-cast).
            let elem = args
                .get(if name == "copyOf" { 2 } else { 3 })
                .and_then(|v| match v {
                    V::Ref(ch) => class_desc_of(vm, *ch),
                    _ => None,
                })
                .and_then(|d| d.strip_prefix('[').map(str::to_string))
                .unwrap_or(elem);
            let out = if name == "copyOf" {
                let n = i32v(*args.get(1).unwrap_or(&V::I32(0))).max(0) as usize;
                let mut d = data;
                d.resize(n, V::default_for(&elem));
                d
            } else {
                let (a, b) = (i32v(args[1]).max(0) as usize, i32v(args[2]).max(0) as usize);
                let mut d: Vec<V> = data
                    .iter()
                    .skip(a)
                    .take(b.saturating_sub(a))
                    .copied()
                    .collect();
                d.resize(b - a, V::default_for(&elem));
                d
            };
            Some(Some(arr(vm, &elem, out)))
        }
        "fill" | "setAll" => {
            if let V::Ref(h) = args[0] {
                if let Some(Obj::Array { elem, data }) = vm.heap.get_mut(h) {
                    // Same canonicalization as `aput-object`: a bare I32(0)
                    // stored into an object-typed array is the null ref.
                    let canon = |v: V| {
                        if matches!(v, V::I32(0))
                            && (elem.starts_with('L') || elem.starts_with('['))
                        {
                            V::Ref(0)
                        } else {
                            v
                        }
                    };
                    if args.len() >= 4 {
                        // fill(arr, fromIndex, toIndex, val)
                        let (a, b, v) = (
                            i32v(args[1]).max(0) as usize,
                            (i32v(args[2]).max(0) as usize).min(data.len()),
                            canon(args[3]),
                        );
                        for e in data.iter_mut().take(b).skip(a) {
                            *e = v;
                        }
                    } else {
                        // fill(arr, val)
                        let v = canon(args[1]);
                        for e in data.iter_mut() {
                            *e = v;
                        }
                    }
                }
            }
            Some(None)
        }
        "sort" | "parallelSort" => {
            if let V::Ref(h) = args[0] {
                let mut data = match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => vec![],
                };
                let from = if args.len() >= 3 {
                    i32v(args[1]).max(0) as usize
                } else {
                    0
                };
                let to = if args.len() >= 3 {
                    (i32v(args[2]).max(0) as usize).min(data.len())
                } else {
                    data.len()
                };
                let comparator = match args.last() {
                    Some(V::Ref(c)) if *c != 0 && (args.len() == 2 || args.len() == 4) => *c,
                    _ => 0,
                };
                if from < to {
                    let mut slice = data[from..to].to_vec();
                    if comparator != 0 {
                        // Compose relies on stable comparator ordering for
                        // z-sorted hit testing (LayoutNode.ZComparator).
                        // Insertion sort is stable and lets guest comparator
                        // throws escape instead of inventing an ordering.
                        for i in 1..slice.len() {
                            let mut j = i;
                            while j > 0 {
                                let ord = vm
                                    .call_virtual_rt(
                                        comparator,
                                        "compare",
                                        &[slice[j - 1], slice[j]],
                                    )?
                                    .map(i32v)
                                    .unwrap_or(0);
                                if ord <= 0 {
                                    break;
                                }
                                slice.swap(j - 1, j);
                                j -= 1;
                            }
                        }
                    } else {
                        slice.sort_by(|a, b| natural_cmp(vm, *a, *b).cmp(&0));
                    }
                    data[from..to].copy_from_slice(&slice);
                }
                if let Some(Obj::Array { data: target, .. }) = vm.heap.get_mut(h) {
                    *target = data;
                }
            }
            Some(None)
        }
        "binarySearch" => {
            let idx = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data
                        .iter()
                        .position(|e| value_eq(vm, *e, args[1]))
                        .map(|i| i as i32)
                        .unwrap_or(-1),
                    _ => -1,
                },
                _ => -1,
            };
            Some(Some(V::I32(idx)))
        }
        "compare" | "mismatch" => Some(Some(V::I32(0))),
        "equals0" => Some(Some(V::I32(1))),
        "stream" => {
            let data = match args.first().copied().unwrap_or(V::Ref(0)) {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => vec![],
                },
                _ => vec![],
            };
            let from = args
                .get(1)
                .map(|v| i32v(*v).max(0) as usize)
                .unwrap_or(0)
                .min(data.len());
            let to = args
                .get(2)
                .map(|v| i32v(*v).max(0) as usize)
                .unwrap_or(data.len())
                .min(data.len())
                .max(from);
            let items = data[from..to].to_vec();
            Some(Some(host(
                vm,
                "Ljava/util/stream/Stream;",
                HostState::List(items),
            )))
        }
        "copyOf0" => Some(Some(args[0])),
        _ => None,
    })
}

fn objects_static(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "equals" => Some(Some(V::I32(value_eq(vm, args[0], args[1]) as i32))),
        "deepEquals" => Some(Some(V::I32(value_eq(vm, args[0], args[1]) as i32))),
        "hashCode" => Some(Some(hash_of(vm, args[0]))),
        "hash" => {
            // `Objects.hash(values...)` boxes its varargs into one array
            // before dispatch. Hashing that array itself gives a fresh
            // identity hash on every call, so object keys whose hashCode
            // uses Objects.hash become unstable in ArrayMap/HashMap.
            let values = match args.first().copied() {
                Some(V::Ref(h)) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => vec![args[0]],
                },
                _ => Vec::new(),
            };
            let h = values.iter().fold(1i32, |acc, v| {
                acc.wrapping_mul(31).wrapping_add(match hash_of(vm, *v) {
                    V::I32(i) => i,
                    _ => 0,
                })
            });
            Some(Some(V::I32(h)))
        }
        "toString" => {
            let t = v_to_s(vm, args[0]);
            Some(Some(jstr(vm, &t)))
        }
        "requireNonNull" => {
            if is_null(args[0]) {
                let e = vm.throw_builtin("Ljava/lang/NullPointerException;", "requireNonNull");
                return Err(e);
            }
            Some(Some(args[0]))
        }
        "requireNonNullElse" | "requireNonNullElseGet" => {
            Some(Some(if is_null(args[0]) { args[1] } else { args[0] }))
        }
        "isNull" => Some(Some(V::I32(is_null(args[0]) as i32))),
        "nonNull" => Some(Some(V::I32((!is_null(args[0])) as i32))),
        "checkIndex" | "checkFromIndexSize" | "checkFromToIndex" => Some(Some(V::I32(0))),
        "compare" => Some(Some(V::I32(natural_cmp(vm, args[0], args[1])))),
        _ => None,
    })
}

fn hash_of(vm: &Vm, v: V) -> V {
    match v {
        V::Ref(0) => V::I32(0),
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Str(t)) => V::I32(
                t.chars()
                    .fold(0i32, |a, c| a.wrapping_mul(31).wrapping_add(c as i32)),
            ),
            Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) => match v {
                V::I32(i) => V::I32(*i),
                V::I64(i) => V::I32((i ^ (i >> 32)) as i32),
                _ => V::I32(0),
            },
            _ => V::I32(h as i32),
        },
        V::I32(i) => V::I32(i),
        V::I64(i) => V::I32((i ^ (i >> 32)) as i32),
        _ => V::I32(0),
    }
}

/// java.util.Comparator-like ordering on primitive/string values.
pub(crate) fn natural_cmp(vm: &mut Vm, a: V, b: V) -> i32 {
    match (a, b) {
        (V::Ref(0), V::Ref(0)) => 0,
        (V::Ref(0), _) => -1,
        (_, V::Ref(0)) => 1,
        (V::Ref(x), V::Ref(y)) => {
            let sa = s(vm, x);
            let sb = s(vm, y);
            match (sa, sb) {
                (Some(pa), Some(pb)) => pa.cmp(&pb) as i32,
                _ => {
                    // Guest objects with a real `compareTo` (okio
                    // ByteString, HttpUrl, …) must order by it —
                    // `Collections.sort` + dex `binarySearch` agree on
                    // the guest ordering, so host stubs can't invent
                    // one. Guest throws just mean "no compareTo".
                    match vm.call_virtual_rt(x, "compareTo", &[V::Ref(y)]) {
                        Ok(Some(v)) => i64v(v).signum() as i32,
                        _ => {
                            let (va, vb) = (
                                slot_of(vm, x).unwrap_or(V::I32(0)),
                                slot_of(vm, y).unwrap_or(V::I32(0)),
                            );
                            (f64v(va) - f64v(vb)).signum() as i32
                        }
                    }
                }
            }
        }
        _ => (f64v(a) - f64v(b)).signum() as i32,
    }
}

// ---------------------------------------------------------------------
// Optional

fn optional_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "of" | "ofNullable" => Some(host(vm, class, HostState::Slot(args[0]))),
        "empty" => Some(host(vm, class, HostState::Slot(V::Ref(0)))),
        "ofInt" | "ofLong" | "ofDouble" => Some(host(vm, class, HostState::Slot(args[0]))),
        "get" | "orElseThrow" | "orElseGet" => {
            let v = match args[0] {
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::Ref(0)),
                x => x,
            };
            if name != "get" && is_null(v) {
                let e = vm.throw_builtin("Ljava/util/NoSuchElementException;", "Optional empty");
                // orElseThrow may return the supplier's exception; be simple
                let _ = e;
                Some(V::Ref(0))
            } else {
                Some(v)
            }
        }
        "orElse" => {
            let v = match args[0] {
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::Ref(0)),
                x => x,
            };
            Some(if is_null(v) { args[1] } else { v })
        }
        "isPresent" | "isEmpty" => {
            let v = match args[0] {
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::Ref(0)),
                x => x,
            };
            let empty = is_null(v);
            Some(V::I32(
                if name == "isPresent" { !empty } else { empty } as i32
            ))
        }
        "ifPresent" | "ifPresentOrElse" | "or" | "flatMap" | "map" | "filter" => Some(args[0]),
        "stream" => Some(host(vm, "Ljava/util/stream/Stream;", HostState::Empty)),
        "equals" => Some(V::I32(1)),
        "getAsInt" | "getAsLong" | "getAsDouble" => Some(match args[0] {
            V::Ref(h) => slot_of(vm, h).unwrap_or(V::I32(0)),
            x => x,
        }),
        _ => return None,
    }
    .map(Some)
}

// ---------------------------------------------------------------------
// Random / UUID / StringTokenizer / BitSet

fn random_call(vm: &mut Vm, name: &str, args: &[V], secure: bool) -> Option<Option<V>> {
    // `java.util.Random` is contractually deterministic; `SecureRandom`
    // must not be guessable — mix in OS entropy each call.
    let os_entropy = if secure {
        use std::io::Read;
        // Read exactly 8 bytes — `fs::read` on /dev/urandom never sees
        // EOF and would exhaust host memory.
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| {
                let mut w = [0u8; 8];
                f.read_exact(&mut w).map(|_| w)
            })
            .map(u64::from_ne_bytes)
            .unwrap_or_else(|_| now_ms() as u64)
    } else {
        0
    };
    let seed = match args[0] {
        V::Ref(h) => slot_of(vm, h).unwrap_or(V::I64(now_ms())),
        _ => V::I64(now_ms()),
    };
    let mut s = i64v(seed) as u64 ^ os_entropy;
    if let V::Ref(h) = args[0] {
        s ^= h as u64;
    }
    s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    if let V::Ref(h) = args[0] {
        if let Some(Obj::Host {
            state: HostState::Slot(v),
            ..
        }) = vm.heap.get_mut(h)
        {
            *v = V::I64(s as i64);
        }
    }
    Some(Some(match name {
        "nextInt" => {
            let v = (s >> 33) as i32;
            if args.len() > 1 {
                let b = i32v(args[1]).max(1) as u32;
                V::I32((v as u32 % b) as i32)
            } else {
                V::I32(v)
            }
        }
        "nextLong" => V::I64((s >> 1) as i64),
        "nextDouble" => V::F64((s >> 11) as f64 / (1u64 << 53) as f64),
        "nextFloat" => V::F32((s >> 40) as f32 / (1u32 << 24) as f32),
        "nextBoolean" => V::I32((s & 1) as i32),
        "nextGaussian" => V::F64(0.0),
        "nextBytes" => {
            if let V::Ref(h) = args[1] {
                if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(h) {
                    for (i, e) in data.iter_mut().enumerate() {
                        *e = V::I32(((s >> (i % 8)) & 0xff) as i32);
                    }
                }
            }
            return None;
        }
        "ints" | "longs" | "doubles" => {
            return Some(Some(host(
                vm,
                "Ljava/util/stream/Stream;",
                HostState::Empty,
            )))
        }
        "setSeed" => {
            if let V::Ref(h) = args[0] {
                if let Some(Obj::Host {
                    state: HostState::Slot(v),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    *v = args[1];
                }
            }
            return None;
        }
        "next" => V::I32(((s >> 33) as i32) & ((1i32 << i32v(args[1]).clamp(1, 31)) - 1)),
        _ => return None,
    }))
}

fn uuid_bits(vm: &Vm, h: u32) -> Option<(i64, i64)> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Uuid { msb, lsb },
            ..
        }) => Some((*msb, *lsb)),
        _ => None,
    }
}

fn uuid_text(msb: i64, lsb: i64) -> String {
    let hex = format!("{:016x}{:016x}", msb as u64, lsb as u64);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn parse_uuid_text(text: &str) -> Option<(i64, i64)> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let msb = u64::from_str_radix(&hex[..16], 16).ok()? as i64;
    let lsb = u64::from_str_radix(&hex[16..], 16).ok()? as i64;
    Some((msb, lsb))
}

fn uuid_from_bytes(mut bytes: [u8; 16], version: u8) -> (i64, i64) {
    bytes[6] = (bytes[6] & 0x0f) | (version << 4);
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let msb = i64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8]));
    let lsb = i64::from_be_bytes(bytes[8..].try_into().unwrap_or([0; 8]));
    (msb, lsb)
}

fn random_uuid_bits() -> (i64, i64) {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        let seed = now_ms() as u64;
        bytes[..8].copy_from_slice(&seed.to_be_bytes());
        bytes[8..].copy_from_slice(&seed.rotate_left(23).to_be_bytes());
    }
    uuid_from_bytes(bytes, 4)
}

fn uuid_obj(vm: &mut Vm, msb: i64, lsb: i64) -> V {
    host(vm, "Ljava/util/UUID;", HostState::Uuid { msb, lsb })
}

fn uuid_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "<init>" => {
            if let (Some(V::Ref(h)), Some(msb), Some(lsb)) =
                (args.first(), args.get(1), args.get(2))
            {
                let bits = (i64v(*msb), i64v(*lsb));
                if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(*h) {
                    *state = HostState::Uuid {
                        msb: bits.0,
                        lsb: bits.1,
                    };
                }
            }
            Some(None)
        }
        "randomUUID" => {
            let (msb, lsb) = random_uuid_bits();
            Some(Some(uuid_obj(vm, msb, lsb)))
        }
        "fromString" => {
            let text = args.first().and_then(|v| sv(vm, *v)).unwrap_or_default();
            let (msb, lsb) = parse_uuid_text(&text).unwrap_or((0, 0));
            Some(Some(uuid_obj(vm, msb, lsb)))
        }
        "nameUUIDFromBytes" => {
            let input: Vec<u8> = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Array { data, .. }) => data.iter().map(|v| i32v(*v) as u8).collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            use md5::Digest as _;
            let digest = md5::Md5::digest(&input);
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&digest[..16]);
            let (msb, lsb) = uuid_from_bytes(bytes, 3);
            Some(Some(uuid_obj(vm, msb, lsb)))
        }
        "toString" => {
            let text = match args.first() {
                Some(V::Ref(h)) => uuid_bits(vm, *h)
                    .map(|(msb, lsb)| uuid_text(msb, lsb))
                    .unwrap_or_else(|| uuid_text(0, 0)),
                _ => uuid_text(0, 0),
            };
            Some(Some(jdynstr(vm, &text)))
        }
        "getMostSignificantBits" => Some(Some(V::I64(match args.first() {
            Some(V::Ref(h)) => uuid_bits(vm, *h).map(|v| v.0).unwrap_or(0),
            _ => 0,
        }))),
        "getLeastSignificantBits" => Some(Some(V::I64(match args.first() {
            Some(V::Ref(h)) => uuid_bits(vm, *h).map(|v| v.1).unwrap_or(0),
            _ => 0,
        }))),
        "equals" => {
            let eq = match (args.first(), args.get(1)) {
                (Some(V::Ref(a)), Some(V::Ref(b))) => {
                    match (uuid_bits(vm, *a), uuid_bits(vm, *b)) {
                        (Some(x), Some(y)) => x == y,
                        _ => false,
                    }
                }
                _ => false,
            };
            Some(Some(V::I32(eq as i32)))
        }
        "hashCode" => {
            let hash = match args.first() {
                Some(V::Ref(h)) => uuid_bits(vm, *h)
                    .map(|(msb, lsb)| {
                        ((msb >> 32) as i32) ^ (msb as i32) ^ ((lsb >> 32) as i32) ^ (lsb as i32)
                    })
                    .unwrap_or(0),
                _ => 0,
            };
            Some(Some(V::I32(hash)))
        }
        "compareTo" => {
            let cmp = match (args.first(), args.get(1)) {
                (Some(V::Ref(a)), Some(V::Ref(b))) => {
                    match (uuid_bits(vm, *a), uuid_bits(vm, *b)) {
                        (Some(x), Some(y)) => x.cmp(&y) as i32,
                        _ => 0,
                    }
                }
                _ => 0,
            };
            Some(Some(V::I32(cmp)))
        }
        "version" => Some(Some(V::I64(match args.first() {
            Some(V::Ref(h)) => uuid_bits(vm, *h)
                .map(|(msb, _)| (msb >> 12) & 0xf)
                .unwrap_or(0),
            _ => 0,
        }))),
        "variant" => Some(Some(V::I64(match args.first() {
            Some(V::Ref(h)) => uuid_bits(vm, *h)
                .map(|(_, lsb)| {
                    let bits = lsb as u64;
                    if bits & 0x8000_0000_0000_0000 == 0 {
                        0
                    } else if bits & 0x4000_0000_0000_0000 == 0 {
                        1
                    } else if bits & 0x2000_0000_0000_0000 == 0 {
                        2
                    } else {
                        3
                    }
                })
                .unwrap_or(0),
            _ => 0,
        }))),
        "timestamp" | "clockSequence" | "node" => Some(Some(V::I64(0))),
        _ => None,
    }
}

fn strtokenizer_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "hasMoreTokens" | "hasMoreElements" => {
            let n = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::StrBuf(t),
                        ..
                    }) => (!t.is_empty()) as i32,
                    _ => 0,
                },
                _ => 0,
            };
            Some(Some(V::I32(n)))
        }
        "nextToken" | "nextElement" => {
            let out = match args[0] {
                V::Ref(h) => match vm.heap.get_mut(h) {
                    Some(Obj::Host {
                        state: HostState::StrBuf(t),
                        ..
                    }) => {
                        let t2 = t.clone();
                        let mut it = t2.splitn(2, |c: char| java_props_space(c) || c == ',');
                        let tok = it.next().unwrap_or("").to_string();
                        *t = it.next().unwrap_or("").to_string();
                        tok
                    }
                    _ => String::new(),
                },
                _ => String::new(),
            };
            Some(Some(jstr(vm, &out)))
        }
        "countTokens" => Some(Some(V::I32(1))),
        _ => None,
    })
}

fn bitset_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Option<Option<V>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return None;
    };
    let p = params(sig);
    match name {
        "set" | "clear" | "flip" => {
            // Two forms each: `(I)` / `(I,Z)` single-bit and `(II)` /
            // `(IIZ)` range. The receiver's bit word lives in the
            // slot — `Obj::Instance` BitSets (allocated via
            // `new-instance`) keep it in P_SLOT via slot_of/slot_put,
            // not just `Obj::Host`.
            let p1 = p.get(1).copied();
            let (lo, hi, on) = if p1 == Some("I") {
                // range form: (from, to) with optional boolean
                (
                    i32v(args[1]),
                    i32v(args[2]),
                    if p.len() == 3 {
                        i32v(args[3]) != 0
                    } else {
                        true
                    },
                )
            } else if p1 == Some("Z") {
                (i32v(args[1]), i32v(args[1]) + 1, i32v(args[2]) != 0)
            } else {
                (i32v(args[1]), i32v(args[1]) + 1, true)
            };
            let mut w = slot_of(vm, h).map(i64v).unwrap_or(0);
            for b in lo.max(0)..hi.clamp(0, 64) {
                let bit = 1i64 << (b % 64);
                match name {
                    "set" => {
                        if on {
                            w |= bit;
                        } else {
                            w &= !bit;
                        }
                    }
                    "clear" => w &= !bit,
                    _ => w ^= bit,
                }
            }
            slot_put(vm, h, V::I64(w));
            None
        }
        "get" => {
            let b = i32v(args[1]);
            let v = slot_of(vm, h).map(i64v).unwrap_or(0);
            Some(V::I32(((v >> (b % 64)) & 1) as i32))
        }
        "isEmpty" => Some(V::I32((slot_of(vm, h).map(i64v).unwrap_or(0) == 0) as i32)),
        "cardinality" => Some(V::I32(
            slot_of(vm, h).map(i64v).unwrap_or(0).count_ones() as i32
        )),
        "length" => {
            let w = slot_of(vm, h).map(i64v).unwrap_or(0) as u64;
            Some(V::I32((64 - w.leading_zeros()) as i32))
        }
        "nextSetBit" | "nextClearBit" => {
            let from = i32v(args[1]).max(0);
            let w = slot_of(vm, h).map(i64v).unwrap_or(0) as u64;
            let mut out = -1i32;
            for b in from..64 {
                let has = ((w >> b) & 1) == 1;
                if (name == "nextSetBit") == has {
                    out = b;
                    break;
                }
            }
            Some(V::I32(out))
        }
        "size" => Some(V::I32(64)),
        "toByteArray" | "toLongArray" => Some(arr(vm, "B", vec![])),
        _ => return None,
    }
    .map(Some)
}

// ---------------------------------------------------------------------
// Atomics

/// Coerce a slot value to the atomic class's natural width —
/// `AtomicLong` reads/writes `J`, `AtomicInteger`/`AtomicBoolean` `I`.
/// Reference-typed atomics pass values through unchanged.
fn width_coerce(class: &str, v: V) -> V {
    if class.contains("Long") {
        match v {
            V::Ref(_) => v,
            _ => V::I64(i64v(v)),
        }
    } else if class.contains("Integer") || class.contains("Boolean") {
        match v {
            V::Ref(_) => v,
            _ => V::I32(i64v(v) as i32),
        }
    } else {
        v
    }
}

fn atomic_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return None;
    };
    // AtomicXxxArray — the same ops with a leading index. Backed by a
    // HostState::List so each element CASes independently; treating it
    // as a scalar Slot made every indexed compareAndSet fail and CAS
    // loops (RxJava ScheduledRunnable.setFuture) spin forever.
    if class.contains("Array") {
        return atomic_array_call(vm, class, h, name, args);
    }
    match name {
        "<init>" => {
            // AtomicX(v): seed the slot — otherwise `get()` reads 0
            // where the guest just stored an object. Coerce to the
            // class's natural width so `AtomicLong.get` (J) never
            // surfaces an I32 to a move-result-wide.
            slot_put(
                vm,
                h,
                width_coerce(class, args.get(1).copied().unwrap_or(V::I32(0))),
            );
            // Some(null): the arm must return Some so the tail's
            // .map(Some) marks it handled —  falls through to a
            // stub placeholder mint even though the slot was seeded.
            Some(V::Ref(0))
        }
        "get" | "getOpaque" | "getAcquire" | "getPlain" => {
            Some(width_coerce(class, slot_of(vm, h).unwrap_or(V::I32(0))))
        }
        "set" | "setOpaque" | "setRelease" | "setPlain" | "lazySet" => {
            slot_put(vm, h, width_coerce(class, args[1]));
            Some(V::Ref(0))
        }
        "getAndSet" => {
            let old = slot_of(vm, h).unwrap_or(V::I32(0));
            slot_put(vm, h, width_coerce(class, args[1]));
            Some(width_coerce(class, old))
        }
        "compareAndSet"
        | "weakCompareAndSet"
        | "compareAndExchange"
        | "compareAndExchangeAcquire"
        | "compareAndExchangeRelease" => {
            let cur = slot_of(vm, h).unwrap_or(V::I32(0));
            // `null` expectation vs an unset/Ref(0) slot — guest null
            // arrives as I32(0) too, and both must compare equal or
            // setOnce CAS loops spin (RxJava SubscriptionHelper).
            let nullish = |v: V| matches!(v, V::I32(0) | V::Ref(0));
            // AtomicReference uses reference identity, unlike the
            // primitive atomic values. Structural equality here makes
            // distinct-but-equal keys share one pending-modification
            // slot in Compose's AtomicReference CAS loop.
            let identity = |a: V, b: V| match (a, b) {
                (V::Ref(x), V::Ref(y)) => x == y,
                _ => value_eq(vm, a, b),
            };
            let eq = if class == "Ljava/util/concurrent/atomic/AtomicReference;" {
                identity(cur, args[1])
            } else {
                value_eq(vm, cur, args[1])
            };
            let ok = (nullish(cur) && nullish(args[1])) || eq;
            if ok {
                slot_put(vm, h, width_coerce(class, args[2]));
            }
            Some(if name.starts_with("compareAndExchange") {
                width_coerce(class, cur)
            } else {
                V::I32(ok as i32)
            })
        }
        "incrementAndGet" | "getAndIncrement" | "decrementAndGet" | "getAndDecrement"
        | "addAndGet" | "getAndAdd" => {
            let cur = slot_of(vm, h).unwrap_or(V::I32(0));
            let delta = match name {
                "addAndGet" | "getAndAdd" => i64v(args[1]),
                "decrementAndGet" | "getAndDecrement" => -1,
                _ => 1,
            };
            let nv = i64v(cur).wrapping_add(delta);
            let next = width_coerce(class, V::I64(nv));
            slot_put(vm, h, next);
            if crate::dbg_flag("OVELO_ATOMICDBG") {
                eprintln!("[atomic] {class}.{name} h={h} cur={cur:?} -> {next:?}");
            }
            Some(if name.starts_with("getAnd") {
                width_coerce(class, cur)
            } else {
                next
            })
        }
        "length" => Some(match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::List(l),
                ..
            }) => V::I32(l.len() as i32),
            _ => V::I32(0),
        }),
        _ => return None,
    }
    .map(Some)
}

/// `java.util.concurrent.atomic.Atomic{Reference,Integer,Long}Array`
/// — index-first variants of the scalar atomic ops over a
/// `HostState::List` of elements.
fn atomic_array_call(
    vm: &mut Vm,
    class: &str,
    h: u32,
    name: &str,
    args: &[V],
) -> Option<Option<V>> {
    let idx = i32v(args.get(1).copied().unwrap_or(V::I32(0))).max(0) as usize;
    // The element store lives on `h` for our Host objects; a dex
    // `Obj::Instance` subclass gets a lazily-minted backing list under
    // P_LIST_BACKING (same scheme as guest collections) — previously
    // init did nothing and CAS reported success without storing.
    let sh = match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::List(_),
            ..
        }) => h,
        Some(Obj::Host { .. }) | Some(Obj::Instance { .. }) => {
            match pseudo_get(vm, h, P_LIST_BACKING) {
                Some(V::Ref(bh)) => bh,
                _ => {
                    let bh = vm.alloc_host(class, HostState::List(Vec::new()));
                    pseudo_set(vm, h, P_LIST_BACKING, V::Ref(bh));
                    bh
                }
            }
        }
        _ => return None,
    };
    let elem = |vm: &Vm| -> V {
        match vm.heap.get(sh) {
            Some(Obj::Host {
                state: HostState::List(l),
                ..
            }) => l.get(idx).copied().unwrap_or(V::Ref(0)),
            Some(Obj::Array { data, .. }) => data.get(idx).copied().unwrap_or(V::Ref(0)),
            _ => V::Ref(0),
        }
    };
    let set_elem = |vm: &mut Vm, v: V| match vm.heap.get_mut(sh) {
        Some(Obj::Host {
            state: HostState::List(l),
            ..
        }) => {
            if idx < l.len() {
                l[idx] = v;
            }
        }
        Some(Obj::Array { data, .. }) if idx < data.len() => data[idx] = v,
        _ => {}
    };
    let long = class.contains("Long") || matches!(elem(vm), V::I64(_));
    let num = |name: &str, cur: V, delta: i64| {
        let d = if matches!(name, "addAndGet" | "getAndAdd") {
            delta
        } else if name.starts_with("decrement") || name.starts_with("getAndDecrement") {
            -1
        } else {
            1
        };
        let nv = i64v(cur).wrapping_add(d);
        if long {
            V::I64(nv)
        } else {
            V::I32(nv as i32)
        }
    };
    let out = match name {
        "<init>" => {
            // (int) → that many null/zero elements; (T[]) → copy the
            // array's elements (long/int arrays carry their values).
            let l = match args.get(1).copied() {
                Some(V::I32(n)) => {
                    vec![if long { V::I64(0) } else { V::I32(0) }; n.max(0) as usize]
                }
                Some(V::Ref(ah)) => match vm.heap.get(ah) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(sh) {
                *state = HostState::List(l);
            }
            return Some(None);
        }
        "get" | "getOpaque" | "getAcquire" | "getPlain" => elem(vm),
        "set" | "setOpaque" | "setRelease" | "setPlain" | "lazySet" => {
            let v = args.get(2).copied().unwrap_or(V::I32(0));
            set_elem(vm, v);
            return Some(None);
        }
        "getAndSet" => {
            let old = elem(vm);
            let v = args.get(2).copied().unwrap_or(V::I32(0));
            set_elem(vm, v);
            old
        }
        "compareAndSet"
        | "weakCompareAndSet"
        | "compareAndExchange"
        | "compareAndExchangeAcquire"
        | "compareAndExchangeRelease" => {
            let cur = elem(vm);
            let expect = args.get(2).copied().unwrap_or(V::I32(0));
            let nullish = |v: V| matches!(v, V::I32(0) | V::Ref(0));
            let ok = (nullish(cur) && nullish(expect)) || value_eq(vm, cur, expect);
            if ok {
                let nv = args.get(3).copied().unwrap_or(V::I32(0));
                set_elem(vm, nv);
            }
            if name.starts_with("compareAndExchange") {
                cur
            } else {
                V::I32(ok as i32)
            }
        }
        "length" => match vm.heap.get(sh) {
            Some(Obj::Host {
                state: HostState::List(l),
                ..
            }) => V::I32(l.len() as i32),
            Some(Obj::Array { data, .. }) => V::I32(data.len() as i32),
            _ => V::I32(0),
        },
        "incrementAndGet" | "getAndIncrement" | "decrementAndGet" | "getAndDecrement"
        | "addAndGet" | "getAndAdd" => {
            let cur = elem(vm);
            let delta = i64v(args.get(2).copied().unwrap_or(V::I64(0)));
            let nv = num(name, cur, delta);
            set_elem(vm, nv);
            if name.starts_with("getAnd") {
                cur
            } else {
                nv
            }
        }
        _ => return None,
    };
    Some(Some(out))
}

fn latch_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "countDown" => {
            if let V::Ref(h) = args[0] {
                if let Some(Obj::Host {
                    state: HostState::Slot(v),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    *v = V::I32((i32v(*v) - 1).max(0));
                }
            }
            Some(None)
        }
        "await" => Some(None),
        "getCount" => {
            let n = match args[0] {
                V::Ref(h) => slot_of(vm, h).map(i32v).unwrap_or(0),
                _ => 0,
            };
            Some(Some(V::I64(n as i64)))
        }
        _ => None,
    }
}

fn fieldupdater_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "newUpdater" | "newUpdaterWithImpl" => {
            let h = vm.alloc_host(
                "Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;",
                HostState::Empty,
            );
            // The field-name argument is last in every newUpdater
            // signature: (Class,String), (Class,Class,String) and
            // (Unsafe,Class,String) — indexing args[2] misses the
            // two-arg Integer/Long variants.
            if let Some(n) = args.last().and_then(|v| sv(vm, *v)) {
                let nv = jstr(vm, &n);
                pseudo_set(vm, h, P_ACTION, nv);
            }
            Some(Some(V::Ref(h)))
        }
        "get" | "set" | "lazySet" | "getAndSet" | "compareAndSet" | "weakCompareAndSet"
        | "getAndUpdate" | "updateAndGet" | "getAndAccumulate" | "accumulateAndGet"
        | "addAndGet" | "getAndAdd" | "incrementAndGet" | "getAndIncrement" | "decrementAndGet"
        | "getAndDecrement" => {
            let V::Ref(up) = args[0] else {
                return Ok(Some(Some(V::Ref(0))));
            };
            let V::Ref(obj) = args[1] else {
                return Ok(Some(Some(V::Ref(0))));
            };
            let fname = pseudo_get(vm, up, P_ACTION).and_then(|v| sv(vm, v));
            let owner = class_of(vm, args[1]);
            // Unresolved updaters must not share one pseudo-slot — a
            // Long updater's I32 write would be read back by a Ref
            // updater aliasing the same key (dexrt.type at iget). Key
            // the fallback by field name so two updaters for the same
            // unresolved field still see each other's writes. The
            // field is usually declared on a superclass of `owner`
            // (JobSupport._state), so walk the chain, not just owner.
            let fid = fname
                .as_deref()
                .and_then(|n| vm.field_index_on_chain(&owner, n))
                .map(|g| vm.canon_field(g))
                .unwrap_or_else(|| {
                    let key = match &fname {
                        Some(n) => {
                            let mut h = std::collections::hash_map::DefaultHasher::new();
                            std::hash::Hash::hash(n, &mut h);
                            std::hash::Hasher::finish(&h) as u32
                        }
                        None => up,
                    };
                    (u64::from(P_DATA) << 32) | u64::from(key)
                });
            // The default for a never-written field must match the
            // field's stored width: `get` on a J field returning Ref(0)
            // breaks the very next `and-long`/`cmp-long` consumer
            // (kotlinx CoroutineScheduler.tryUnpark reads
            // parkedWorkersStack before init), and CAS on an unset J
            // slot must see I64(0) or it can never succeed.
            let unset = if class == "Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;"
                || num_wide(vm, fid)
            {
                V::I64(0)
            } else if class == "Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;" {
                V::I32(0)
            } else {
                V::Ref(0)
            };
            match name {
                "get" => {
                    let v = match vm.heap.get(obj) {
                        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                            fields.get(&fid).copied().unwrap_or(unset)
                        }
                        _ => unset,
                    };
                    Some(Some(v))
                }
                "compareAndSet" | "weakCompareAndSet" => {
                    let cur = match vm.heap.get(obj) {
                        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                            fields.get(&fid).copied().unwrap_or(unset)
                        }
                        _ => unset,
                    };
                    // Guest `null` arrives as `I32(0)` (const/4) while an
                    // unset slot holds `Ref(0)` — both count as null, or
                    // CAS on a null-expected slot spins forever (kotlinx
                    // LockFreeTaskQueue.addLast relies on this).
                    let nullish = |v: V| matches!(v, V::I32(0) | V::Ref(0));
                    let ok = (nullish(cur) && nullish(args[2])) || value_eq(vm, cur, args[2]);
                    if ok {
                        // Widen numeric writes to the stored width — a
                        // `const/4` new value on a `J` field must stay I64.
                        let nv = match (cur, args[3]) {
                            (V::I64(_), V::I32(i)) => V::I64(i as i64),
                            (V::I32(_), V::I64(i)) => V::I32(i as i32),
                            (_, V::I32(i)) if num_wide(vm, fid) => V::I64(i as i64),
                            (_, v) => v,
                        };
                        if let Some(f) = fields_mut(vm, obj) {
                            f.insert(fid, nv);
                        }
                    }
                    Some(Some(V::I32(ok as i32)))
                }
                "getAndSet" | "getAndUpdate" | "updateAndGet" | "getAndAccumulate"
                | "accumulateAndGet" | "addAndGet" | "getAndAdd" | "incrementAndGet"
                | "getAndIncrement" | "decrementAndGet" | "getAndDecrement" => {
                    let cur = match vm.heap.get(obj) {
                        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                            fields.get(&fid).copied().unwrap_or(unset)
                        }
                        _ => unset,
                    };
                    // Keep the stored width: an AtomicLongFieldUpdater
                    // must not shrink a `J` field to I32 or a later
                    // iget-wide throws (kotlinx scheduler workers).
                    let wide = num_wide(vm, fid);
                    let num_add = |cur: V, d: i64| match cur {
                        V::I64(i) => V::I64(i + d),
                        V::I32(i) => V::I32((i as i64 + d) as i32),
                        _ if wide => V::I64(d),
                        _ => V::I32(d as i32),
                    };
                    let coerce = |cur: V, v: V| match (cur, v) {
                        (V::I64(_), V::I32(i)) => V::I64(i as i64),
                        (V::I32(_), V::I64(i)) => V::I32(i as i32),
                        (_, v) => v,
                    };
                    let nv = match name {
                        "getAndSet" => coerce(cur, args[2]),
                        "incrementAndGet" | "getAndIncrement" => num_add(cur, 1),
                        "decrementAndGet" | "getAndDecrement" => num_add(cur, -1),
                        "addAndGet" | "getAndAdd" => num_add(cur, i64v(args[2])),
                        _ => cur,
                    };
                    if let Some(f) = fields_mut(vm, obj) {
                        f.insert(fid, nv);
                    }
                    Some(Some(if name.starts_with("getAnd") { cur } else { nv }))
                }
                _ => {
                    // `set`/`lazySet` — coerce to the stored width as in CAS.
                    let cur = match vm.heap.get(obj) {
                        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                            fields.get(&fid).copied().unwrap_or(unset)
                        }
                        _ => unset,
                    };
                    let nv = match (cur, args[2]) {
                        (V::I64(_), V::I32(i)) => V::I64(i as i64),
                        (V::I32(_), V::I64(i)) => V::I32(i as i32),
                        (_, V::I32(i)) if num_wide(vm, fid) => V::I64(i as i64),
                        (_, v) => v,
                    };
                    if let Some(f) = fields_mut(vm, obj) {
                        f.insert(fid, nv);
                    }
                    Some(None)
                }
            }
        }
        _ => None,
    })
}

/// `TimeUnit` unit math: ordinals run NANOSECONDS..DAYS; convert via
/// the nanos scale with saturation (JDK truncates toward zero).
fn timeunit_convert(dst_ord: i32, src_ord: i32, d: i64) -> i64 {
    const R: [i64; 7] = [
        1,
        1_000,
        1_000_000,
        1_000_000_000,
        60_000_000_000,
        3_600_000_000_000,
        86_400_000_000_000,
    ];
    let s = R.get(src_ord as usize).copied().unwrap_or(1);
    let dd = R.get(dst_ord as usize).copied().unwrap_or(1);
    let nanos = ((d as i128) * (s as i128)).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    nanos / dd
}

/// Whether the resolved dex field is declared `J`/`D` — used to pick
/// the stored width for numeric writes on never-initialized fields.
fn num_wide(vm: &Vm, fid: u64) -> bool {
    vm.dex_of(fid)
        .and_then(|d| {
            d.field(crate::gl(fid))
                .ok()
                .and_then(|f| d.type_descriptor(f.type_idx as u32).ok())
        })
        .map(|t| t == "J" || t == "D")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// android.util.Pair / SparseArray

fn pair_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "create" => {
            let h = vm.alloc_host("Landroid/util/Pair;", HostState::Slot(args[0]));
            pseudo_set(vm, h, P_DATA, args[1]);
            Some(Some(V::Ref(h)))
        }
        _ => None,
    })
}

fn sparse_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "put" | "append" | "setValueAt" => {
            map_put(vm, h, V::I32(i32v(args[1])), *args.last().unwrap());
            Some(None)
        }
        "get" => {
            // `SparseIntArray.get(key)`/`SparseBooleanArray.get(key)` are
            // primitive-valued: a miss returns the int/boolean default, not
            // null — a null Ref crashes `i32` consumers (packed-switch in
            // ConstraintLayout.LayoutParams.<init>).
            let int_valued = matches!(
                class,
                "Landroid/util/SparseIntArray;" | "Landroid/util/SparseBooleanArray;"
            );
            Some(Some(
                map_get(vm, h, V::I32(i32v(args[1])))
                    .or_else(|| args.get(2).copied())
                    .unwrap_or(if int_valued { V::I32(0) } else { V::Ref(0) }),
            ))
        }
        "remove" | "delete" | "removeAt" => {
            map_remove(vm, h, V::I32(i32v(args[1])));
            Some(None)
        }
        "size" => Some(Some(V::I32(map_len(vm, h) as i32))),
        "keyAt" => {
            let k = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m
                    .get(i32v(args[1]) as usize)
                    .map(|(k, _)| i32v(*k))
                    .unwrap_or(0),
                _ => 0,
            };
            Some(Some(V::I32(k)))
        }
        "valueAt" => {
            let v = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m
                    .get(i32v(args[1]) as usize)
                    .map(|(_, v)| *v)
                    .unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            };
            Some(Some(v))
        }
        "indexOfKey" | "indexOfValue" => {
            let i = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m
                    .iter()
                    .position(|(k, _)| i32v(*k) == i32v(args[1]))
                    .map(|i| i as i32)
                    .unwrap_or(-1),
                _ => -1,
            };
            Some(Some(V::I32(i)))
        }
        "clear" => {
            if let Some(m) = map_mut(vm, h) {
                m.clear();
            }
            Some(None)
        }
        "clone" => {
            let m: Vec<(V, V)> = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.clone(),
                _ => vec![],
            };
            Some(Some(host(
                vm,
                "Landroid/util/SparseArray;",
                HostState::Map(m),
            )))
        }
        _ => None,
    })
}
