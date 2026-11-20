//! java.util collections, iterators, Optional, Random, UUID, atomics,
//! CountDownLatch/Semaphore, android.util.SparseArray/Pair/ArrayMap.

use super::*;
use crate::value::{HostState, Obj, V};
use std::rc::Rc;

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

fn is_list(class: &str) -> bool {
    LIST_CLASSES.iter().any(|c| desc_is_a(class, c))
}

fn is_map(class: &str) -> bool {
    MAP_CLASSES.iter().any(|c| desc_is_a(class, c))
}

fn is_iter(class: &str) -> bool {
    ITER_CLASSES.iter().any(|c| desc_is_a(class, c))
        || class.contains("$")
            && (class.starts_with("Ljava/util/")
                || class.starts_with("Landroid/util/"))
}

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(if class == "Ljava/util/Collections;" {
        collections_static(vm, name, args)
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
        bitset_call(vm, name, args)
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
        fieldupdater_call(vm, name, args)?
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
            "convert" | "toMillis" | "toNanos" | "toMicros" | "toSeconds"
            | "toMinutes" | "toHours" | "toDays" => {
                Some(Some(V::I64(i64v(*args.last().unwrap_or(&V::I64(0))))))
            }
            "sleep" | "timedWait" | "timedJoin" => Some(None),
            _ => None,
        }
    } else if class == "Ljava/util/concurrent/Executors;"
        || class == "Ljava/util/concurrent/ExecutorService;"
        || class.starts_with("Ljava/util/concurrent/")
    {
        match name {
            "execute" => {
                if let V::Ref(r) = args[1] {
                    let _ = vm.call_virtual(r, "run", &[]);
                }
                Some(None)
            }
            "submit" => {
                if let V::Ref(r) = args[1] {
                    let _ = vm.call_virtual(r, "run", &[]);
                }
                Some(Some(host(vm, "Ljava/util/concurrent/Future;", HostState::Empty)))
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
            "newSingleThreadExecutor" | "newCachedThreadPool"
            | "newFixedThreadPool" | "newScheduledThreadPool"
            | "unconfigurableExecutorService" | "defaultThreadFactory" => Some(Some(
                host(vm, "Ljava/util/concurrent/ExecutorService;", HostState::Empty),
            )),
            "get" | "call" => Some(Some(V::Ref(0))),
            "isDone" | "isCancelled" => Some(Some(V::I32(1))),
            "cancel" => Some(Some(V::I32(1))),
            "offer" | "add" | "put" | "take" | "poll" | "remove" => {
                if matches!(name, "add" | "offer") {
                    list_call(vm, name, sig, args)?
                } else {
                    Some(Some(V::Ref(0)))
                }
            }
            _ => None,
        }
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
// List

fn list_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "add" => {
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            if args.len() > 2 && params(sig).first() == Some(&"I") {
                l.insert((i32v(args[1]).max(0) as usize).min(l.len()), args[2]);
            } else {
                l.push(args[1]);
            }
            Some(if args.len() > 2 { None } else { Some(V::I32(1)) })
        }
        "addAll" => {
            let src: Vec<V> = match args.last().copied().unwrap_or(V::Ref(0)) {
                V::Ref(sh) => match vm.heap.get(sh) {
                    Some(Obj::Host {
                        state: HostState::List(l),
                        ..
                    }) => l.clone(),
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => vec![],
                },
                _ => vec![],
            };
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let at = if args.len() > 2 { i32v(args[1]).max(0) as usize } else { l.len() };
            for (i, v) in src.iter().enumerate() {
                l.insert((at + i).min(l.len()), *v);
            }
            Some(Some(V::I32((!src.is_empty()) as i32)))
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
            Some(if name.starts_with("offer") { Some(V::I32(1)) } else { None })
        }
        "removeFirst" | "removeLast" | "pop" | "poll" | "pollFirst" | "pollLast" => {
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let v = if matches!(name, "removeFirst" | "pop" | "poll" | "pollFirst") {
                if l.is_empty() { V::Ref(0) } else { l.remove(0) }
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
                Some(Some(if i < l.len() {
                    l.remove(i)
                } else {
                    V::Ref(0)
                }))
            } else {
                let pos = list_ref(vm, h).and_then(|l| {
                    l.iter().position(|e| value_eq(vm, *e, args[1]))
                });
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
                let cur: Vec<V> = list_ref(vm, h).cloned().unwrap_or_default();
                let keep: Vec<bool> = cur
                    .iter()
                    .map(|e| {
                        let in_src = src.iter().any(|s| value_eq(vm, *e, *s));
                        if name == "removeAll" { !in_src } else { in_src }
                    })
                    .collect();
                let before = cur.len();
                let mut it = keep.iter();
                if let Some(l) = list_mut(vm, h) {
                    l.retain(|_| *it.next().unwrap_or(&false));
                }
                Some(Some(V::I32((before != keep.iter().filter(|k| **k).count()) as i32)))
            }
        }
        "get" | "element" | "getFirst" | "getLast" | "peek" | "peekFirst"
        | "peekLast" => {
            let l = match list_ref(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let v = match name {
                "getFirst" | "element" | "peek" | "peekFirst" => {
                    l.first().copied().unwrap_or(V::Ref(0))
                }
                "getLast" | "peekLast" => {
                    l.last().copied().unwrap_or(V::Ref(0))
                }
                _ => l.get(i32v(args[1]) as usize).copied().unwrap_or(V::Ref(0)),
            };
            Some(Some(v))
        }
        "set" => {
            let l = match list_mut(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let i = i32v(args[1]) as usize;
            Some(Some(if i < l.len() {
                std::mem::replace(&mut l[i], args[2])
            } else {
                V::Ref(0)
            }))
        }
        "size" => {
            let n = list_ref(vm, h).map(|l| l.len()).unwrap_or(0);
            Some(Some(V::I32(n as i32)))
        }
        "isEmpty" => {
            let e = list_ref(vm, h).map(|l| l.is_empty()).unwrap_or(true);
            Some(Some(V::I32(e as i32)))
        }
        "clear" => {
            if let Some(l) = list_mut(vm, h) {
                l.clear();
            }
            Some(None)
        }
        "contains" => {
            let c = list_ref(vm, h)
                .is_some_and(|l| l.iter().any(|e| value_eq(vm, *e, args[1])));
            Some(Some(V::I32(c as i32)))
        }
        "containsAll" | "equals" => Some(Some(V::I32(1))),
        "indexOf" | "lastIndexOf" => {
            let l = match list_ref(vm, h) {
                Some(l) => l,
                None => return Ok(None),
            };
            let i = if name == "indexOf" {
                l.iter().position(|e| value_eq(vm, *e, args[1]))
            } else {
                l.iter().rposition(|e| value_eq(vm, *e, args[1]))
            };
            Some(Some(V::I32(i.map(|x| x as i32).unwrap_or(-1))))
        }
        "iterator" | "listIterator" | "descendingIterator" | "spliterator" => {
            let items: Vec<V> = match list_ref(vm, h) {
                Some(l) => l.clone(),
                None => vec![],
            };
            Some(Some(host(
                vm,
                "Ljava/util/Iterator;",
                HostState::Iter(Rc::new(items), 0),
            )))
        }
        "toArray" => {
            let items: Vec<V> = match list_ref(vm, h) {
                Some(l) => l.clone(),
                None => vec![],
            };
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
            let (a, b) = (i32v(args[1]) as usize, i32v(args[2]) as usize);
            let items: Vec<V> = match list_ref(vm, h) {
                Some(l) => l.iter().skip(a).take(b.saturating_sub(a)).copied().collect(),
                None => vec![],
            };
            Some(Some(host(vm, "Ljava/util/List;", HostState::List(items))))
        }
        "forEach" | "replaceAll" | "sort" | "stream" | "parallelStream" => {
            if name == "sort" {
                Some(None)
            } else if name.ends_with("stream") || name.ends_with("Stream") {
                Some(Some(host(vm, "Ljava/util/stream/Stream;", HostState::Empty)))
            } else {
                Some(None)
            }
        }
        "hashCode" => {
            let hc = list_ref(vm, h)
                .map(|l| l.len() as i32 * 31)
                .unwrap_or(1);
            Some(Some(V::I32(hc)))
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Map (covers Bundle/ContentValues keyed ops too)

fn map_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let class = class_of(vm, args[0]);
    Ok(match name {
        "put" => {
            let prev = map_put(vm, h, args[1], args[2]);
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
        "putInt" | "putLong" | "putFloat" | "putDouble" | "putBoolean"
        | "putString" | "putChar" | "putShort" | "putByte" | "putParcelable"
        | "putSerializable" | "putBundle" | "putCharSequence" | "putIntegerArrayList"
        | "putStringArrayList" | "putParcelableArrayList" | "putIntArray"
        | "putLongArray" | "putBooleanArray" | "putByteArray" | "putFloatArray"
        | "putDoubleArray" | "putCharArray" | "putShortArray" | "putStringArray"
        | "putParcelableArray" | "putCharSequenceArray" => {
            map_put(vm, h, args[1], args[2]);
            Some(None)
        }
        "get" | "getOrDefault" | "remove" | "removeObject" => {
            if name == "remove" || name == "removeObject" {
                Some(Some(map_remove(vm, h, args[1]).unwrap_or(V::Ref(0))))
            } else {
                let v = map_get(vm, h, args[1]);
                Some(Some(match v {
                    Some(v) => v,
                    None => args.get(2).copied().unwrap_or(V::Ref(0)),
                }))
            }
        }
        "getInt" | "getLong" | "getFloat" | "getDouble" | "getBoolean"
        | "getChar" | "getShort" | "getByte" => {
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
        "getString" | "getParcelable" | "getSerializable" | "getBundle"
        | "getCharSequence" | "getIntegerArrayList" | "getStringArrayList"
        | "getParcelableArrayList" | "getIntArray" | "getLongArray"
        | "getBooleanArray" | "getByteArray" | "getFloatArray" | "getDoubleArray"
        | "getCharArray" | "getShortArray" | "getStringArray"
        | "getParcelableArray" | "getCharSequenceArray" | "getClassLoader" => {
            Some(Some(
                map_get(vm, h, args[1])
                    .or_else(|| args.get(2).copied())
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
            let pairs: Vec<(V, V)> = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.clone(),
                _ => vec![],
            };
            let out: Vec<V> = match name {
                "entrySet" => pairs
                    .into_iter()
                    .map(|(k, v)| {
                        let eh = vm.alloc_host(
                            "Ljava/util/AbstractMap$SimpleEntry;",
                            HostState::Slot(k),
                        );
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
        "compute" | "computeIfAbsent" | "computeIfPresent" | "merge"
        | "putIfAbsent" => {
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
                                match vm.call_virtual(fh, "apply", &[key, old])
                                    .or_else(|_| {
                                        vm.call_virtual(fh, "apply", &[key])
                                    }) {
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
        _ => {
            let _ = sig;
            None
        }
    })
}

// ---------------------------------------------------------------------
// Iterator / Enumeration / Map.Entry

fn iter_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "hasNext" | "hasMoreElements" => {
            let n = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Iter(items, pos),
                    ..
                }) => (*pos < items.len()) as i32,
                Some(Obj::Host {
                    state: HostState::List(l),
                    ..
                }) => (!l.is_empty()) as i32,
                _ => 0,
            };
            Some(Some(V::I32(n)))
        }
        "next" | "nextElement" => {
            let (v, ok) = match vm.heap.get_mut(h) {
                Some(Obj::Host {
                    state: HostState::Iter(items, pos),
                    ..
                }) => {
                    if *pos < items.len() {
                        let v = items[*pos];
                        *pos += 1;
                        (v, true)
                    } else {
                        (V::Ref(0), false)
                    }
                }
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
        "hasPrevious" => Some(Some(V::I32(0))),
        "previous" | "nextIndex" | "previousIndex" => Some(Some(V::I32(0))),
        "remove" | "set" | "add" | "forEachRemaining" => Some(None),
        "getKey" => Some(Some(slot_of(vm, h).unwrap_or(V::Ref(0)))),
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

fn collections_static(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "emptyList" | "emptySet" | "emptyMap" | "emptyIterator"
        | "emptyEnumeration" | "emptyListIterator" | "emptyNavigableSet"
        | "emptySortedSet" | "emptyNavigableMap" | "emptySortedMap" => {
            let (d, st) = if name.contains("Map") {
                ("Ljava/util/Map;", HostState::Map(vec![]))
            } else if name.contains("Iterator") || name.contains("Enumeration") {
                ("Ljava/util/Iterator;", HostState::Iter(Rc::new(vec![]), 0))
            } else {
                ("Ljava/util/List;", HostState::List(vec![]))
            };
            Some(Some(host(vm, d, st)))
        }
        "singletonList" | "singleton" => {
            Some(Some(host(vm, "Ljava/util/List;", HostState::List(vec![args[0]]))))
        }
        "singletonMap" => Some(Some(host(
            vm,
            "Ljava/util/Map;",
            HostState::Map(vec![(args[0], args[1])]),
        ))),
        "unmodifiableList" | "unmodifiableSet" | "unmodifiableCollection"
        | "unmodifiableSortedSet" | "unmodifiableNavigableSet" | "synchronizedList"
        | "synchronizedSet" | "synchronizedCollection" | "checkedList"
        | "checkedSet" | "checkedCollection" | "unmodifiableSortedMap"
        | "unmodifiableNavigableMap" | "unmodifiableMap" | "synchronizedMap"
        | "checkedMap" | "checkedNavigableMap" | "checkedSortedMap" => {
            Some(Some(args[0]))
        }
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
                let mut items = list_ref(vm, h).cloned().unwrap_or_default();
                items.sort_by(|a, b| {
                    natural_cmp(vm, *a, *b).cmp(&0)
                });
                if let Some(l) = list_mut(vm, h) {
                    *l = items;
                }
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
                    .and_then(|l| {
                        l.iter().position(|e| value_eq(vm, *e, args[1]))
                    })
                    .map(|i| i as i32)
                    .unwrap_or(-1),
                _ => -1,
            };
            Some(Some(V::I32(idx)))
        }
        "indexOfSubList" | "lastIndexOfSubList" => Some(Some(V::I32(-1))),
        "min" | "max" => {
            let l = match args[0] {
                V::Ref(h) => list_ref(vm, h).cloned().unwrap_or_default(),
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
                V::Ref(h) => list_ref(vm, h)
                    .map(|l| l.iter().filter(|e| value_eq(vm, **e, args[1])).count())
                    .unwrap_or(0),
                _ => 0,
            };
            Some(Some(V::I32(n as i32)))
        }
        "disjoint" => Some(Some(V::I32(0))),
        "addAll" => {
            if let V::Ref(h) = args[0] {
                for v in &args[1..] {
                    if let Some(l) = list_mut(vm, h) {
                        l.push(*v);
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
            Some(Some(host(vm, "Ljava/util/ArrayList;", HostState::List(vec![]))))
        }
        "enumeration" => {
            let items = match args[0] {
                V::Ref(h) => list_ref(vm, h).cloned().unwrap_or_default(),
                _ => vec![],
            };
            Some(Some(host(
                vm,
                "Ljava/util/Enumeration;",
                HostState::Iter(Rc::new(items), 0),
            )))
        }
        "reverseOrder" | "comparing" | "comparingInt" | "comparingLong"
        | "comparingDouble" | "naturalOrder" => Some(Some(host(
            vm,
            "Ljava/util/Comparator;",
            HostState::Empty,
        ))),
        "newSetFromMap" => Some(Some(host(
            vm,
            "Ljava/util/Set;",
            HostState::List(vec![]),
        ))),
        "synchronizedCollection0" | "unmodifiableCollection0" => Some(Some(V::Ref(0))),
        _ => None,
    }
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
            Some(Some(host(vm, "Ljava/util/List;", HostState::List(items))))
        }
        "toString" | "deepToString" => {
            let t = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => {
                        let parts: Vec<String> =
                            data.iter().map(|v| fmt_v(*v)).collect();
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
            let eq = match (args[0], args[1]) {
                (V::Ref(a), V::Ref(b)) => match (vm.heap.get(a), vm.heap.get(b)) {
                    (Some(Obj::Array { data: x, .. }), Some(Obj::Array { data: y, .. })) => {
                        x == y
                    }
                    _ => a == b,
                },
                (a, b) => a == b,
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
            let out = if name == "copyOf" {
                let n = i32v(*args.get(1).unwrap_or(&V::I32(0))).max(0) as usize;
                let mut d = data;
                d.resize(n, V::I32(0));
                d
            } else {
                let (a, b) = (
                    i32v(args[1]).max(0) as usize,
                    i32v(args[2]).max(0) as usize,
                );
                let mut d: Vec<V> = data
                    .iter()
                    .skip(a)
                    .take(b.saturating_sub(a))
                    .copied()
                    .collect();
                d.resize(b - a, V::I32(0));
                d
            };
            Some(Some(arr(vm, &elem, out)))
        }
        "fill" | "setAll" => {
            if let V::Ref(h) = args[0] {
                if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(h) {
                    let end = args
                        .get(3)
                        .map(|v| i32v(*v) as usize)
                        .unwrap_or(data.len())
                        .min(data.len());
                    let a = args
                        .get(2)
                        .map(|v| i32v(*v).max(0) as usize)
                        .unwrap_or(0);
                    for e in data.iter_mut().take(end).skip(a) {
                        *e = args[1];
                    }
                }
            }
            Some(None)
        }
        "sort" | "parallelSort" => {
            if let V::Ref(h) = args[0] {
                if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(h) {
                    data.sort_by(|a, b| {
                        f64v(*a)
                            .partial_cmp(&f64v(*b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
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
        "stream" => Some(Some(host(vm, "Ljava/util/stream/Stream;", HostState::Empty))),
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
            let h = args.iter().fold(1i32, |acc, v| {
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
        "checkIndex" | "checkFromIndexSize" | "checkFromToIndex" => {
            Some(Some(V::I32(0)))
        }
        "compare" => Some(Some(V::I32(natural_cmp(vm, args[0], args[1])))),
        _ => None,
    })
}

fn hash_of(vm: &Vm, v: V) -> V {
    match v {
        V::Ref(0) => V::I32(0),
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Str(t)) => V::I32(
                t.chars().fold(0i32, |a, c| a.wrapping_mul(31).wrapping_add(c as i32)),
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
pub(crate) fn natural_cmp(vm: &Vm, a: V, b: V) -> i32 {
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
                    let (va, vb) = (
                        slot_of(vm, x).unwrap_or(V::I32(0)),
                        slot_of(vm, y).unwrap_or(V::I32(0)),
                    );
                    (f64v(va) - f64v(vb)).signum() as i32
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
        "ofInt" | "ofLong" | "ofDouble" => {
            Some(host(vm, class, HostState::Slot(args[0])))
        }
        "get" | "orElseThrow" | "orElseGet" => {
            let v = match args[0] {
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::Ref(0)),
                x => x,
            };
            if name != "get" && is_null(v) {
                let e =
                    vm.throw_builtin("Ljava/util/NoSuchElementException;", "Optional empty");
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
            Some(V::I32(if name == "isPresent" { !empty } else { empty } as i32))
        }
        "ifPresent" | "ifPresentOrElse" | "or" | "flatMap" | "map" | "filter" => {
            Some(args[0])
        }
        "stream" => Some(host(vm, "Ljava/util/stream/Stream;", HostState::Empty)),
        "equals" => Some(V::I32(1)),
        "getAsInt" | "getAsLong" | "getAsDouble" => {
            Some(match args[0] {
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::I32(0)),
                x => x,
            })
        }
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
            return Some(Some(host(vm, "Ljava/util/stream/Stream;", HostState::Empty)))
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

fn uuid_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "randomUUID" => Some(Some(host(
            vm,
            "Ljava/util/UUID;",
            HostState::Slot(V::I64(now_ms())),
        ))),
        "toString" => Some(Some(jstr(
            vm,
            &format!("{:08x}-0000-4000-8000-000000000000", now_ms() & 0xffffffff),
        ))),
        "nameUUIDFromBytes" => Some(Some(host(
            vm,
            "Ljava/util/UUID;",
            HostState::Slot(args[0]),
        ))),
        "fromString" => Some(Some(host(
            vm,
            "Ljava/util/UUID;",
            HostState::Slot(V::I64(0)),
        ))),
        "getMostSignificantBits" | "getLeastSignificantBits" => Some(Some(V::I64(0))),
        "version" | "variant" | "timestamp" | "clockSequence" | "node" => {
            Some(Some(V::I64(0)))
        }
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
                        let mut it = t2.splitn(2, |c: char| c.is_whitespace() || c == ',');
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

fn bitset_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return None;
    };
    match name {
        "set" | "clear" | "flip" => {
            let b = i32v(args[1]);
            let bit = 1i64 << (b % 64);
            if let Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) = vm.heap.get_mut(h)
            {
                let mut w = i64v(*v);
                match name {
                    "set" => w |= bit,
                    "clear" => w &= !bit,
                    _ => w ^= bit,
                }
                *v = V::I64(w);
            }
            None
        }
        "get" => {
            let b = i32v(args[1]);
            let v = slot_of(vm, h).map(i64v).unwrap_or(0);
            Some(V::I32(((v >> (b % 64)) & 1) as i32))
        }
        "cardinality" | "size" | "length" | "nextSetBit" | "nextClearBit"
        | "previousSetBit" | "previousClearBit" => Some(V::I32(0)),
        "isEmpty" => Some(V::I32(1)),
        "toByteArray" | "toLongArray" => Some(arr(vm, "B", vec![])),
        _ => return None,
    }
    .map(Some)
}

// ---------------------------------------------------------------------
// Atomics

fn atomic_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return None;
    };
    match name {
        "get" | "getOpaque" | "getAcquire" | "getPlain" => {
            Some(slot_of(vm, h).unwrap_or(V::I32(0)))
        }
        "set" | "setOpaque" | "setRelease" | "setPlain" | "lazySet" => {
            if let Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) = vm.heap.get_mut(h)
            {
                *v = args[1];
            }
            None
        }
        "getAndSet" => {
            let old = slot_of(vm, h).unwrap_or(V::I32(0));
            if let Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) = vm.heap.get_mut(h)
            {
                *v = args[1];
            }
            Some(old)
        }
        "compareAndSet" | "weakCompareAndSet" | "compareAndExchange"
        | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => {
            let cur = slot_of(vm, h).unwrap_or(V::I32(0));
            let ok = value_eq(vm, cur, args[1]);
            if ok {
                if let Some(Obj::Host {
                    state: HostState::Slot(v),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    *v = args[2];
                }
            }
            Some(if name.starts_with("compareAndExchange") {
                cur
            } else {
                V::I32(ok as i32)
            })
        }
        "incrementAndGet" | "getAndIncrement" | "decrementAndGet"
        | "getAndDecrement" | "addAndGet" | "getAndAdd" => {
            let cur = slot_of(vm, h).unwrap_or(V::I32(0));
            let delta = if matches!(name, "addAndGet" | "getAndAdd") {
                i64v(args[1])
            } else {
                1
            };
            let long = class.contains("Long") || matches!(cur, V::I64(_));
            let nv = i64v(cur) + delta;
            let next = if long { V::I64(nv) } else { V::I32(nv as i32) };
            if let Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) = vm.heap.get_mut(h)
            {
                *v = next;
            }
            Some(if name.starts_with("getAnd") { cur } else { next })
        }
        "length" => {
            Some(match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::List(l),
                    ..
                }) => V::I32(l.len() as i32),
                _ => V::I32(0),
            })
        }
        _ => return None,
    }
    .map(Some)
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

fn fieldupdater_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "newUpdater" | "newUpdaterWithImpl" => {
            let h = vm.alloc_host("Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;", HostState::Empty);
            // remember the field name for get/set
            if let Some(n) = args.get(2).and_then(|v| sv(vm, *v)) {
                let nv = jstr(vm, &n);
                pseudo_set(vm, h, P_ACTION, nv);
            }
            Some(Some(V::Ref(h)))
        }
        "get" | "set" | "lazySet" | "getAndSet" | "compareAndSet"
        | "weakCompareAndSet" | "getAndUpdate" | "updateAndGet"
        | "getAndAccumulate" | "accumulateAndGet" | "addAndGet"
        | "getAndAdd" | "incrementAndGet" | "getAndIncrement"
        | "decrementAndGet" | "getAndDecrement" => {
            let V::Ref(up) = args[0] else {
                return Ok(Some(Some(V::Ref(0))));
            };
            let V::Ref(obj) = args[1] else {
                return Ok(Some(Some(V::Ref(0))));
            };
            let fname = pseudo_get(vm, up, P_ACTION).and_then(|v| sv(vm, v));
            let owner = class_of(vm, args[1]);
            let fid = fname
                .as_deref()
                .and_then(|n| vm.dex.field_index_of(&owner, n))
                .unwrap_or(P_DATA);
            match name {
                "get" => {
                    let v = match vm.heap.get(obj) {
                        Some(Obj::Instance { fields, .. })
                        | Some(Obj::Host { fields, .. }) => {
                            fields.get(&fid).copied().unwrap_or(V::Ref(0))
                        }
                        _ => V::Ref(0),
                    };
                    Some(Some(v))
                }
                "compareAndSet" | "weakCompareAndSet" => {
                    let cur = match vm.heap.get(obj) {
                        Some(Obj::Instance { fields, .. })
                        | Some(Obj::Host { fields, .. }) => {
                            fields.get(&fid).copied().unwrap_or(V::Ref(0))
                        }
                        _ => V::Ref(0),
                    };
                    let ok = value_eq(vm, cur, args[2]);
                    if ok {
                        if let Some(f) = fields_mut(vm, obj) {
                            f.insert(fid, args[3]);
                        }
                    }
                    Some(Some(V::I32(ok as i32)))
                }
                "getAndSet" | "getAndUpdate" | "updateAndGet"
                | "getAndAccumulate" | "accumulateAndGet" | "addAndGet"
                | "getAndAdd" | "incrementAndGet" | "getAndIncrement"
                | "decrementAndGet" | "getAndDecrement" => {
                    let cur = match vm.heap.get(obj) {
                        Some(Obj::Instance { fields, .. })
                        | Some(Obj::Host { fields, .. }) => {
                            fields.get(&fid).copied().unwrap_or(V::Ref(0))
                        }
                        _ => V::Ref(0),
                    };
                    let nv = match name {
                        "getAndSet" => args[2],
                        "incrementAndGet" | "getAndIncrement" => {
                            V::I32(i32v(cur) + 1)
                        }
                        "decrementAndGet" | "getAndDecrement" => {
                            V::I32(i32v(cur) - 1)
                        }
                        "addAndGet" | "getAndAdd" => {
                            V::I64(i64v(cur) + i64v(args[2]))
                        }
                        _ => cur,
                    };
                    if let Some(f) = fields_mut(vm, obj) {
                        f.insert(fid, nv);
                    }
                    Some(Some(if name.starts_with("getAnd") { cur } else { nv }))
                }
                _ => {
                    if let Some(f) = fields_mut(vm, obj) {
                        f.insert(fid, args[2]);
                    }
                    Some(None)
                }
            }
        }
        _ => None,
    })
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

fn sparse_call(vm: &mut Vm, _class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "put" | "append" | "setValueAt" => {
            map_put(vm, h, V::I32(i32v(args[1])), *args.last().unwrap());
            Some(None)
        }
        "get" => Some(Some(
            map_get(vm, h, V::I32(i32v(args[1])))
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::Ref(0)),
        )),
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
            Some(Some(host(vm, "Landroid/util/SparseArray;", HostState::Map(m))))
        }
        _ => None,
    })
}
