//! java.lang builtins: boxed numbers, Math, System, Class, Thread,
//! Throwable, String extras, StringBuilder.

use super::*;
use crate::value::{HostState, Obj, V};

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(if class == STRING || class == "Ljava/lang/CharSequence;" {
        // `CharSequence` is the interface `String` implements — its
        // methods answer on the receiver's string value the same way.
        string_call(vm, name, sig, args)
    } else if desc_is_a(class, "Ljava/lang/AbstractStringBuilder;") {
        strbuf(vm, name, sig, args)?
    } else if boxed(class) {
        boxed_call(vm, class, name, sig, args)?
    } else if desc_is_a(class, "Ljava/lang/Number;") {
        number_call(vm, name, args)
    } else if class == "Ljava/lang/Math;" || class == "Ljava/lang/StrictMath;" {
        math_call(name, args)
    } else if class == "Ljava/lang/System;" {
        system_call(vm, name, sig, args)?
    } else if class == "Ljava/lang/Runtime;" {
        runtime_call(vm, name, args)
    } else if desc_is_a(class, "Ljava/lang/Class;") {
        class_call(vm, name, sig, args)?
    } else if desc_is_a(class, "Ljava/lang/ClassLoader;") || class.ends_with("ClassLoader;") {
        classloader_call(vm, name, args)
    } else if class == "Ljava/lang/Package;" {
        package_call(vm, name, sig, args)
    } else if desc_is_a(class, "Ljava/lang/Thread;")
        || vm.assignable_desc(class, "Ljava/lang/Thread;")
    {
        thread_call(vm, name, sig, args)?
    } else if class.ends_with("ThreadLocal;") {
        threadlocal_call(vm, name, args)
    } else if desc_is_a(class, THROWABLE) {
        throwable_call(vm, name, sig, args)?
    } else if class == "Ljava/lang/Enum;"
        || platform_enum_consts(class).is_some()
        || (!class.starts_with("Ljava/time/") && vm.assignable_desc(class, "Ljava/lang/Enum;"))
    {
        // java.time's enums carry the Enum super but have dedicated
        // (interned, correctly-ordinal'd) handling in jtime_call —
        // routing them through enum_call would return empty values()
        // and panic on 1-arg valueOf.
        enum_call(vm, class, name, args)?
    } else if class == "Lsun/misc/Unsafe;" {
        unsafe_call(vm, name, args)?
    } else if class == "Ljava/lang/StackTraceElement;" {
        stacktrace_call(vm, name, args)
    } else if class.starts_with("Ljava/lang/reflect/") {
        reflect_call(vm, class, name, sig, args)?
    } else if class == "Ljava/lang/ref/ReferenceQueue;" {
        match name {
            // Nothing ever enqueues references (the host GC feeds no
            // queue), so `remove()` is an unbounded block we can't
            // truly suspend. Unwinding the caller with ovelo.park
            // burned the repark budget of loaders that legitimately
            // loop on take-style calls, so return one cached empty
            // Reference per queue instead — the watcher spins on a
            // single recycled object instead of minting millions.
            "poll" => Some(Some(V::Ref(0))),
            "remove" => {
                let this = args.first().copied().unwrap_or(V::Ref(0));
                let cached = if let V::Ref(q) = this {
                    match vm.heap.get(q) {
                        Some(Obj::Host {
                            state: HostState::Slot(v),
                            ..
                        }) if !matches!(v, V::Ref(0) | V::I32(0)) => Some(*v),
                        Some(Obj::Host { .. }) => {
                            let r = vm.alloc_host(
                                "Ljava/lang/ref/Reference;",
                                HostState::Slot(V::Ref(0)),
                            );
                            if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(q) {
                                *state = HostState::Slot(V::Ref(r));
                            }
                            Some(V::Ref(r))
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                match cached {
                    Some(v) => Some(Some(v)),
                    _ => {
                        let r =
                            vm.alloc_host("Ljava/lang/ref/Reference;", HostState::Slot(V::Ref(0)));
                        Some(Some(V::Ref(r)))
                    }
                }
            }
            _ => None,
        }
    } else if class == "Ljava/lang/ref/WeakReference;"
        || class == "Ljava/lang/ref/SoftReference;"
        || class == "Ljava/lang/ref/Reference;"
        || class == "Ljava/lang/ref/PhantomReference;"
    {
        match name {
            "<init>" => {
                // Reference(referent[, queue]): keep the referent —
                // androidx stores the LifecycleOwner in a WeakReference
                // and reads it back to gate performRestore.
                let this = args.first().copied().unwrap_or(V::Ref(0));
                if let (V::Ref(h), Some(&v)) = (this, args.get(1)) {
                    if let Some(Obj::Host {
                        state: HostState::Slot(slot),
                        ..
                    }) = vm.heap.get_mut(h)
                    {
                        *slot = v;
                    }
                }
                Some(None)
            }
            "get" => Some(Some(match args[0] {
                // PhantomReference.get() is null by contract.
                _ if class == "Ljava/lang/ref/PhantomReference;" => V::Ref(0),
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            })),
            "clear" => {
                if let V::Ref(h) = args[0] {
                    if let Some(Obj::Host {
                        state: HostState::Slot(slot),
                        ..
                    }) = vm.heap.get_mut(h)
                    {
                        *slot = V::Ref(0);
                    }
                }
                Some(None)
            }
            "enqueue" | "isEnqueued" => Some(Some(V::I32(0))),
            _ => None,
        }
    } else {
        return Ok(None);
    })
}

pub(crate) fn primitive_type(o: &str) -> bool {
    matches!(
        o,
        "Ljava/lang/Integer;"
            | "Ljava/lang/Long;"
            | "Ljava/lang/Short;"
            | "Ljava/lang/Byte;"
            | "Ljava/lang/Character;"
            | "Ljava/lang/Boolean;"
            | "Ljava/lang/Double;"
            | "Ljava/lang/Float;"
    )
}

pub(crate) fn hashcode(vm: &mut Vm, args: &[V]) -> V {
    match args[0] {
        V::Ref(0) => V::I32(0),
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Str(t)) => V::I32(java_str_hash(t)),
            Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) => v_hash(*v),
            Some(Obj::Host {
                state: HostState::StrBuf(t),
                ..
            }) => V::I32(java_str_hash(t)),
            _ => V::I32(h as i32),
        },
        v => v_hash(v),
    }
}

fn v_hash(v: V) -> V {
    match v {
        V::I32(i) => V::I32(i),
        V::I64(i) => V::I32((i ^ (i >> 32)) as i32),
        V::F32(f) => V::I32(f.to_bits() as i32),
        V::F64(f) => V::I32((f.to_bits() ^ (f.to_bits() >> 32)) as i32),
        _ => V::I32(0),
    }
}

fn java_str_hash(t: &str) -> i32 {
    t.chars()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

// ---------------------------------------------------------------------
// String

fn string_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Option<Option<V>> {
    let t = sv(vm, args[0]).unwrap_or_default();
    let p = params(sig);
    let r: Option<V> = match name {
        "length" => Some(V::I32(t.chars().count() as i32)),
        "isEmpty" => Some(V::I32(t.is_empty() as i32)),
        "charAt" => Some(V::I32(
            t.chars()
                .nth(i32v(args[1]) as usize)
                .map(|c| c as i32)
                .unwrap_or(0),
        )),
        "codePointAt" => Some(V::I32(
            t.chars()
                .nth(i32v(args[1]) as usize)
                .map(|c| c as u32 as i32)
                .unwrap_or(0),
        )),
        "codePointBefore" => {
            let i = i32v(args[1]) as usize;
            Some(V::I32(
                t.chars()
                    .nth(i.saturating_sub(1))
                    .map(|c| c as u32 as i32)
                    .unwrap_or(0),
            ))
        }
        "indexOf" => {
            // `from` is a char index: convert to a byte offset — slicing
            // at a non-boundary would panic on non-ASCII text.
            let from = args.get(2).map(|v| i32v(*v) as usize).unwrap_or(0);
            let start = t
                .char_indices()
                .nth(from)
                .map(|(i, _)| i)
                .unwrap_or(t.len());
            let idx = match args[1] {
                V::I32(ch) => t[start..].find(char::from_u32(ch as u32).unwrap_or('\0')),
                _ => {
                    let n = sv(vm, args[1]).unwrap_or_default();
                    t[start..].find(&n)
                }
            };
            Some(V::I32(
                idx.map(|i| (from + t[start..start + i].chars().count()) as i32)
                    .unwrap_or(-1),
            ))
        }
        "lastIndexOf" => {
            let n = match args[1] {
                V::I32(ch) => char::from_u32(ch as u32).unwrap_or('\0').to_string(),
                _ => sv(vm, args[1]).unwrap_or_default(),
            };
            Some(V::I32(t.rfind(&n).map(|i| i as i32).unwrap_or(-1)))
        }
        "contains" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(V::I32(t.contains(&n) as i32))
        }
        "startsWith" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            let off = args.get(2).map(|v| i32v(*v) as usize).unwrap_or(0);
            Some(V::I32(
                t.get(off..).is_some_and(|s| s.starts_with(&n)) as i32
            ))
        }
        "endsWith" => Some(V::I32(
            t.ends_with(&sv(vm, args[1]).unwrap_or_default()) as i32
        )),
        "equals" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(V::I32((t == n) as i32))
        }
        "equalsIgnoreCase" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(V::I32(t.eq_ignore_ascii_case(&n) as i32))
        }
        "compareTo" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(V::I32(t.cmp(&n) as i32))
        }
        "compareToIgnoreCase" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(V::I32(t.to_lowercase().cmp(&n.to_lowercase()) as i32))
        }
        "contentEquals" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(V::I32((t == n) as i32))
        }
        "regionMatches" => {
            // Two overloads: regionMatches(to, other, oo, len) and
            // regionMatches(ignoreCase, to, other, oo, len). Kotlin's
            // `startsWith`/`contains` with ignoreCase use the latter.
            let (ic, to, oo, len, other_idx) = if args.len() >= 6 {
                (
                    i32v(args[1]) != 0,
                    i32v(args[2]) as usize,
                    i32v(args[4]) as usize,
                    i32v(args[5]) as usize,
                    3,
                )
            } else {
                (
                    false,
                    i32v(args[1]) as usize,
                    i32v(args[3]) as usize,
                    i32v(args[4]) as usize,
                    2,
                )
            };
            let n = sv(vm, args[other_idx]).unwrap_or_default();
            let a: Vec<char> = t.chars().skip(to).take(len).collect();
            let b: Vec<char> = n.chars().skip(oo).take(len).collect();
            let eq = if ic {
                a.iter()
                    .flat_map(|c| c.to_lowercase())
                    .eq(b.iter().flat_map(|c| c.to_lowercase()))
            } else {
                a == b
            };
            Some(V::I32(eq as i32))
        }
        "substring" | "subSequence" => {
            let b = i32v(args[1]) as usize;
            let e = if args.len() > 2 {
                i32v(args[2]) as usize
            } else {
                t.chars().count()
            };
            let sub: String = t.chars().skip(b).take(e.saturating_sub(b)).collect();
            Some(jstr(vm, &sub))
        }
        "concat" => {
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(jstr(vm, &format!("{t}{n}")))
        }
        "replace" => {
            if p.first() == Some(&"Ljava/lang/CharSequence;") {
                let a = sv(vm, args[1]).unwrap_or_default();
                let b = sv(vm, args[2]).unwrap_or_default();
                Some(jstr(vm, &t.replace(&a, &b)))
            } else {
                let a = char::from_u32(i32v(args[1]) as u32).unwrap_or('\0');
                let b = char::from_u32(i32v(args[2]) as u32).unwrap_or('\0');
                Some(jstr(vm, &t.replace(a, &b.to_string())))
            }
        }
        "replaceAll" | "replaceFirst" => {
            let pat = sv(vm, args[1]).unwrap_or_default();
            let rep = sv(vm, args[2]).unwrap_or_default();
            // `String.replaceAll` takes a regex, not a literal —
            // `replaceAll("\\D+", "")` is how callers scrub digits out
            // of strings (NewPipe view-count parsing depends on this).
            let out = if let Ok(rx) = super::rx::Rx::compile(&pat) {
                let chars: Vec<char> = t.chars().collect();
                super::io::rx_replace(&rx, &chars, &rep, name == "replaceFirst")
            } else if name == "replaceFirst" {
                t.replacen(&pat, &rep, 1)
            } else {
                t.replace(&pat, &rep)
            };
            Some(jstr(vm, &out))
        }
        "toUpperCase" | "toLowerCase" => {
            let out = if name == "toUpperCase" {
                t.to_uppercase()
            } else {
                t.to_lowercase()
            };
            Some(jstr(vm, &out))
        }
        "trim" | "strip" => Some(jstr(vm, t.trim())),
        "isBlank" => Some(V::I32(t.trim().is_empty() as i32)),
        "repeat" => Some(jstr(vm, &t.repeat(i32v(args[1]).max(0) as usize))),
        "getBytes" => {
            let bs: Vec<V> = t.bytes().map(|b| V::I32(b as i8 as i32)).collect();
            Some(arr(vm, "B", bs))
        }
        "toCharArray" => {
            let cs: Vec<V> = t.chars().map(|c| V::I32(c as i32)).collect();
            Some(arr(vm, "C", cs))
        }
        "matches" => {
            let pat = sv(vm, args[1]).unwrap_or_default();
            Some(V::I32(
                (super::rx::full_match(&pat, &t) || io::literal_match(&pat, &t)) as i32,
            ))
        }
        "split" => {
            // Java takes a regex here (`"a.b".split("\\.")`), not a
            // literal separator — NewPipe's Parser.getValue dot-paths
            // came back as one un-split key when we split literally.
            let pat = sv(vm, args[1]).unwrap_or_default();
            let lim = args.get(2).map(|v| i32v(*v)).unwrap_or(0);
            let parts: Vec<V> = super::rx::regex_split(&pat, &t, lim)
                .into_iter()
                .map(|x| jstr(vm, &x))
                .collect();
            Some(arr(vm, STRING, parts))
        }
        "intern" => Some(args[0]),
        "toString" => Some(args[0]),
        "valueOf" => {
            // static: valueOf(x) → string of arg. `valueOf(char)` must
            // render the character — a bare int falls through to
            // v_to_s which prints the digits ("|" → "124"), breaking
            // every caller that relies on the char overload (kotlin's
            // StringsKt.split delimiter conversion did).
            let s = match args[0] {
                V::I32(c) if p.first() == Some(&"C") => char::from_u32(c as u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
                V::I32(b) if p.first() == Some(&"Z") => (b != 0).to_string(),
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => {
                        let a = args.get(1).map(|v| i32v(*v) as usize).unwrap_or(0);
                        let n = args.get(2).map(|v| i32v(*v) as usize).unwrap_or(usize::MAX);
                        data.iter()
                            .skip(a)
                            .take(n)
                            .filter_map(|v| char::from_u32(i32v(*v) as u32))
                            .collect()
                    }
                    _ => v_to_s(vm, args[0]),
                },
                _ => v_to_s(vm, args[0]),
            };
            Some(jstr(vm, &s))
        }
        "format" => {
            // format(fmt, args...) or format(locale, fmt, args...)
            let (fmt_i, rest) = if p.len() > 1 && p[0] == "Ljava/util/Locale;" {
                (1, &args[2..])
            } else {
                (0, &args[1..])
            };
            let fmt = sv(vm, args[fmt_i]).unwrap_or_default();
            let flat: Vec<V> = match rest.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => rest.to_vec(),
                },
                _ => rest.to_vec(),
            };
            let out = io::simple_format(vm, &fmt, &flat);
            Some(jstr(vm, &out))
        }
        "join" => {
            let d = sv(vm, args[0]).unwrap_or_default();
            let parts: Vec<String> = match args[1] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::List(l),
                        ..
                    }) => l.iter().map(|v| sv(vm, *v).unwrap_or_default()).collect(),
                    Some(Obj::Array { data, .. }) => data
                        .iter()
                        .map(|v| sv(vm, *v).unwrap_or_default())
                        .collect(),
                    _ => args[1..]
                        .iter()
                        .map(|v| sv(vm, *v).unwrap_or_default())
                        .collect(),
                },
                _ => vec![],
            };
            Some(jstr(vm, &parts.join(&d)))
        }
        "copyValueOf" | "valueOf0" => {
            let s = v_to_s(vm, args[0]);
            Some(jstr(vm, &s))
        }
        "chars" | "codePoints" => {
            let cs: Vec<V> = t.chars().map(|c| V::I32(c as u32 as i32)).collect();
            Some(arr(vm, "I", cs))
        }
        "hashCode" => Some(V::I32(java_str_hash(&t))),
        _ => None,
    };
    r.map(Some)
}

// ---------------------------------------------------------------------
// StringBuilder / StringBuffer

fn strbuf(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    // `StringWriter.getBuffer()` hands out a StringBuffer view that
    // delegates through P_DATA to the writer's shared buffer — follow
    // the chain so view operations hit the real storage.
    let h = {
        let mut cur = h;
        for _ in 0..4 {
            if matches!(
                vm.heap.get(cur),
                Some(Obj::Host {
                    state: HostState::StrBuf(_),
                    ..
                })
            ) {
                break;
            }
            match pseudo_get(vm, cur, P_DATA) {
                Some(V::Ref(n)) if n != cur => cur = n,
                _ => break,
            }
        }
        cur
    };
    Ok(match name {
        "<init>" => {
            // StringBuilder(String|CharSequence|int capacity).
            let seed = args
                .get(1)
                .filter(|v| matches!(v, V::Ref(_)))
                .and_then(|v| sv(vm, *v))
                .unwrap_or_default();
            if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(h) {
                *state = HostState::StrBuf(seed);
            }
            Some(None)
        }
        "append" => {
            let p = params(sig);
            let add = match p.first().copied().unwrap_or("") {
                "Z" => (i32v(args[1]) != 0).to_string(),
                "C" => char::from_u32(i32v(args[1]) as u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
                "I" | "B" | "S" => i32v(args[1]).to_string(),
                "J" => i64v(args[1]).to_string(),
                "F" | "D" => fmt_f64(f64v(args[1])),
                "[C" => match args[1] {
                    V::Ref(ah) => match vm.heap.get(ah) {
                        Some(Obj::Array { data, .. }) => {
                            // `append(char[], off, len)` takes a slice —
                            // tokener/scanner code passes a shared
                            // buffer and bounds the view (nanojson
                            // `JsonTokener.consumeTokenSemiString`).
                            let slice: &[V] = if p.len() == 3 {
                                let off = i32v(args[2]).max(0) as usize;
                                let n = i32v(args[3]).max(0) as usize;
                                &data[off.min(data.len())..(off + n).min(data.len())]
                            } else {
                                data
                            };
                            // char[] is UTF-16 — decode code units so
                            // surrogate pairs survive.
                            let units: Vec<u16> = slice.iter().map(|v| i32v(*v) as u16).collect();
                            String::from_utf16_lossy(&units)
                        }
                        _ => String::new(),
                    },
                    _ => String::new(),
                },
                "Ljava/lang/CharSequence;" if p.len() == 3 => {
                    // `append(CharSequence, start, end)` appends the
                    // char-range view — kotlin's replace/split builders
                    // rely on it (appending the whole input instead
                    // duplicated the source into the output).
                    let t = sv(vm, args[1]).unwrap_or_default();
                    let lo = i32v(args[2]).max(0) as usize;
                    let hi = i32v(args[3]).max(0) as usize;
                    t.chars().skip(lo).take(hi.saturating_sub(lo)).collect()
                }
                _ => v_to_s(vm, args[1]),
            };
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                b.push_str(&add);
            }
            Some(Some(args[0]))
        }
        "insert" => {
            let off = i32v(args[1]) as usize;
            let add = v_to_s(vm, *args.get(2).unwrap_or(&V::Ref(0)));
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                let at = b.char_indices().nth(off).map(|(i, _)| i).unwrap_or(b.len());
                b.insert_str(at, &add);
            }
            Some(Some(args[0]))
        }
        "delete" => {
            let (a, z) = (i32v(args[1]) as usize, i32v(args[2]) as usize);
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                let lo = b.char_indices().nth(a).map(|(i, _)| i).unwrap_or(b.len());
                let hi = b.char_indices().nth(z).map(|(i, _)| i).unwrap_or(b.len());
                b.replace_range(lo..hi, "");
            }
            Some(Some(args[0]))
        }
        "deleteCharAt" => {
            let a = i32v(args[1]) as usize;
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                let lo = b.char_indices().nth(a).map(|(i, _)| i).unwrap_or(b.len());
                let hi = b
                    .char_indices()
                    .nth(a + 1)
                    .map(|(i, _)| i)
                    .unwrap_or(b.len());
                if lo < b.len() {
                    b.replace_range(lo..hi, "");
                }
            }
            Some(Some(args[0]))
        }
        "setCharAt" => {
            let a = i32v(args[1]) as usize;
            let c = char::from_u32(i32v(args[2]) as u32).unwrap_or('\0');
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                let lo = b.char_indices().nth(a).map(|(i, _)| i).unwrap_or(b.len());
                let hi = b
                    .char_indices()
                    .nth(a + 1)
                    .map(|(i, _)| i)
                    .unwrap_or(b.len());
                if lo < b.len() {
                    b.replace_range(lo..hi, &c.to_string());
                }
            }
            Some(None)
        }
        "setLength" => {
            let n = i32v(args[1]).max(0) as usize;
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                let cur = b.chars().count();
                if n < cur {
                    let at = b.char_indices().nth(n).map(|(i, _)| i).unwrap_or(b.len());
                    b.truncate(at);
                } else {
                    b.extend(std::iter::repeat_n('\0', n - cur));
                }
            }
            Some(None)
        }
        "toString" => {
            let t = s(vm, h).unwrap_or_default();
            Some(Some(jstr(vm, &t)))
        }
        "length" => {
            let n = s(vm, h).map(|t| t.chars().count() as i32).unwrap_or(0);
            Some(Some(V::I32(n)))
        }
        "capacity" => {
            let n = s(vm, h)
                .map(|t| t.chars().count().max(16) as i32)
                .unwrap_or(16);
            Some(Some(V::I32(n)))
        }
        "charAt" => {
            let t = s(vm, h).unwrap_or_default();
            let c = t
                .chars()
                .nth(i32v(args[1]) as usize)
                .map(|c| c as i32)
                .unwrap_or(0);
            Some(Some(V::I32(c)))
        }
        "substring" | "subSequence" => {
            let t = s(vm, h).unwrap_or_default();
            let b = i32v(args[1]) as usize;
            let e = if args.len() > 2 {
                i32v(args[2]) as usize
            } else {
                t.chars().count()
            };
            let sub: String = t.chars().skip(b).take(e.saturating_sub(b)).collect();
            Some(Some(jstr(vm, &sub)))
        }
        "indexOf" | "lastIndexOf" => {
            let t = s(vm, h).unwrap_or_default();
            let n = sv(vm, args[1]).unwrap_or_default();
            let i = if name == "indexOf" {
                t.find(&n)
            } else {
                t.rfind(&n)
            };
            Some(Some(V::I32(i.map(|x| x as i32).unwrap_or(-1))))
        }
        "ensureCapacity" => Some(None),
        "reverse" => {
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                *b = b.chars().rev().collect();
            }
            Some(Some(args[0]))
        }
        "replace" => {
            let (a, z) = (i32v(args[1]) as usize, i32v(args[2]) as usize);
            let rep = sv(vm, args[3]).unwrap_or_default();
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                let lo = b.char_indices().nth(a).map(|(i, _)| i).unwrap_or(b.len());
                let hi = b.char_indices().nth(z).map(|(i, _)| i).unwrap_or(b.len());
                b.replace_range(lo..hi, &rep);
            }
            Some(Some(args[0]))
        }
        "appendCodePoint" => {
            if let Some(Obj::Host {
                state: HostState::StrBuf(b),
                ..
            }) = vm.heap.get_mut(h)
            {
                if let Some(c) = char::from_u32(i32v(args[1]) as u32) {
                    b.push(c);
                }
            }
            Some(Some(args[0]))
        }
        "getChars" => Some(None),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// boxed primitives

fn boxed(class: &str) -> bool {
    matches!(
        class,
        "Ljava/lang/Integer;"
            | "Ljava/lang/Long;"
            | "Ljava/lang/Short;"
            | "Ljava/lang/Byte;"
            | "Ljava/lang/Character;"
            | "Ljava/lang/Boolean;"
            | "Ljava/lang/Double;"
            | "Ljava/lang/Float;"
    )
}

fn prim(vm: &Vm, v: V) -> V {
    if let V::Ref(h) = v {
        slot_of(vm, h).unwrap_or(v)
    } else {
        v
    }
}

fn boxed_call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "<init>" => {
            // `new Integer(1)` allocates a dex `Obj::Instance` — without
            // this the boxed value never lands and intValue() reads 0.
            let arg = args.get(1).copied().unwrap_or(V::I32(0));
            let v = match arg {
                V::Ref(h) => match sv(vm, arg) {
                    Some(t) if class != "Ljava/lang/Character;" => {
                        let t = t.trim();
                        match class {
                            "Ljava/lang/Long;" => t.parse::<i64>().map(V::I64).unwrap_or(V::I64(0)),
                            "Ljava/lang/Double;" => {
                                t.parse::<f64>().map(V::F64).unwrap_or(V::F64(0.0))
                            }
                            "Ljava/lang/Float;" => {
                                t.parse::<f32>().map(V::F32).unwrap_or(V::F32(0.0))
                            }
                            "Ljava/lang/Boolean;" => V::I32((t == "true") as i32),
                            _ => t.parse::<i32>().map(V::I32).unwrap_or(V::I32(0)),
                        }
                    }
                    _ => V::Ref(h),
                },
                other => other,
            };
            if let V::Ref(this) = args[0] {
                slot_put(vm, this, v);
            }
            Some(None)
        }
        "intValue" | "longValue" | "shortValue" | "byteValue" | "charValue" | "booleanValue"
        | "doubleValue" | "floatValue" => {
            let v = prim(vm, args[0]);
            Some(Some(match name {
                "longValue" => V::I64(i64v(v)),
                "shortValue" => V::I32(i32v(v) as i16 as i32),
                "byteValue" => V::I32(i32v(v) as i8 as i32),
                "booleanValue" => V::I32((i32v(v) != 0) as i32),
                "doubleValue" => V::F64(f64v(v)),
                "floatValue" => V::F32(f64v(v) as f32),
                _ => V::I32(i32v(v)),
            }))
        }
        "valueOf" => {
            let p = params(sig);
            if p.first() == Some(&"Ljava/lang/String;") {
                let t = sv(vm, args[0]).unwrap_or_default();
                let parsed: Result<V, ()> = match class {
                    "Ljava/lang/Integer;" => t.trim().parse::<i32>().map(V::I32).map_err(|_| ()),
                    "Ljava/lang/Long;" => t.trim().parse::<i64>().map(V::I64).map_err(|_| ()),
                    "Ljava/lang/Short;" => t
                        .trim()
                        .parse::<i16>()
                        .map(|i| V::I32(i as i32))
                        .map_err(|_| ()),
                    "Ljava/lang/Byte;" => t
                        .trim()
                        .parse::<i8>()
                        .map(|i| V::I32(i as i32))
                        .map_err(|_| ()),
                    "Ljava/lang/Double;" => t.trim().parse::<f64>().map(V::F64).map_err(|_| ()),
                    "Ljava/lang/Float;" => t.trim().parse::<f32>().map(V::F32).map_err(|_| ()),
                    "Ljava/lang/Boolean;" => Ok(V::I32((t == "true") as i32)),
                    _ => Err(()),
                };
                match parsed {
                    Ok(v) => Some(Some(host(vm, class, HostState::Slot(v)))),
                    Err(()) => {
                        let e = vm.throw_builtin(
                            "Ljava/lang/NumberFormatException;",
                            &format!("For input string: \"{t}\""),
                        );
                        return Err(e);
                    }
                }
            } else {
                Some(Some(host(vm, class, HostState::Slot(args[0]))))
            }
        }
        "parseInt" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let radix = args.get(1).map(|v| i32v(*v) as u32).unwrap_or(10);
            match i32::from_str_radix(t.trim(), radix) {
                Ok(i) => Some(Some(V::I32(i))),
                Err(_) => {
                    let e = vm.throw_builtin(
                        "Ljava/lang/NumberFormatException;",
                        &format!("For input string: \"{t}\""),
                    );
                    return Err(e);
                }
            }
        }
        "parseLong" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let radix = args.get(1).map(|v| i32v(*v) as u32).unwrap_or(10);
            match i64::from_str_radix(t.trim(), radix) {
                Ok(i) => Some(Some(V::I64(i))),
                Err(_) => {
                    let e = vm.throw_builtin(
                        "Ljava/lang/NumberFormatException;",
                        &format!("For input string: \"{t}\""),
                    );
                    return Err(e);
                }
            }
        }
        "parseShort" => Some(Some(V::I32(
            sv(vm, args[0])
                .unwrap_or_default()
                .trim()
                .parse::<i16>()
                .unwrap_or(0) as i32,
        ))),
        "parseByte" => Some(Some(V::I32(
            sv(vm, args[0])
                .unwrap_or_default()
                .trim()
                .parse::<i8>()
                .unwrap_or(0) as i32,
        ))),
        "parseBoolean" => Some(Some(V::I32(
            (sv(vm, args[0]).unwrap_or_default() == "true") as i32,
        ))),
        "parseDouble" => Some(Some(V::F64(
            sv(vm, args[0])
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or(f64::NAN),
        ))),
        "parseFloat" => Some(Some(V::F32(
            sv(vm, args[0])
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or(f32::NAN),
        ))),
        "parseUnsignedInt" | "parseUnsignedLong" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            Some(Some(if name == "parseUnsignedLong" {
                V::I64(t.trim().parse::<u64>().unwrap_or(0) as i64)
            } else {
                V::I32(t.trim().parse::<u32>().unwrap_or(0) as i32)
            }))
        }
        "numberOfLeadingZeros" => Some(Some(V::I32(if class == "Ljava/lang/Long;" {
            i64v(args[0]).leading_zeros() as i32
        } else {
            i32v(args[0]).leading_zeros() as i32
        }))),
        "numberOfTrailingZeros" => Some(Some(V::I32(if class == "Ljava/lang/Long;" {
            i64v(args[0]).trailing_zeros() as i32
        } else {
            i32v(args[0]).trailing_zeros() as i32
        }))),
        "bitCount" => Some(Some(V::I32(if class == "Ljava/lang/Long;" {
            i64v(args[0]).count_ones() as i32
        } else {
            i32v(args[0]).count_ones() as i32
        }))),
        "highestOneBit" => Some(Some(if class == "Ljava/lang/Long;" {
            V::I64(if i64v(args[0]) == 0 {
                0
            } else {
                1i64 << (63 - i64v(args[0]).leading_zeros())
            })
        } else {
            V::I32(if i32v(args[0]) == 0 {
                0
            } else {
                1i32 << (31 - i32v(args[0]).leading_zeros())
            })
        })),
        "lowestOneBit" => Some(Some(if class == "Ljava/lang/Long;" {
            V::I64(i64v(args[0]) & i64v(args[0]).wrapping_neg())
        } else {
            V::I32(i32v(args[0]) & i32v(args[0]).wrapping_neg())
        })),
        "reverse" => Some(Some(if class == "Ljava/lang/Long;" {
            V::I64(i64v(args[0]).reverse_bits())
        } else {
            V::I32(i32v(args[0]).reverse_bits())
        })),
        "reverseBytes" => Some(Some(match class {
            "Ljava/lang/Long;" => V::I64(i64v(args[0]).swap_bytes()),
            "Ljava/lang/Short;" => V::I32((i32v(args[0]) as i16).swap_bytes() as i32),
            _ => V::I32(i32v(args[0]).swap_bytes()),
        })),
        "rotateLeft" | "rotateRight" => {
            let d = match args[1] {
                V::I64(l) => l as u32,
                V::I32(i) => i as u32,
                _ => 0,
            };
            Some(Some(if class == "Ljava/lang/Long;" {
                let x = i64v(args[0]);
                V::I64(if name == "rotateLeft" {
                    x.rotate_left(d)
                } else {
                    x.rotate_right(d)
                })
            } else {
                let x = i32v(args[0]);
                V::I32(if name == "rotateLeft" {
                    x.rotate_left(d)
                } else {
                    x.rotate_right(d)
                })
            }))
        }
        "signum" => Some(Some(V::I32(i32v(args[0]).signum()))),
        "compare" | "compareUnsigned" => {
            let c = if name == "compareUnsigned" {
                (i32v(args[0]) as u32).cmp(&(i32v(args[1]) as u32))
            } else if matches!(args[0], V::I64(_)) {
                i64v(args[0]).cmp(&i64v(args[1]))
            } else if matches!(args[0], V::F64(_) | V::F32(_)) {
                f64v(args[0])
                    .partial_cmp(&f64v(args[1]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                i32v(args[0]).cmp(&i32v(args[1]))
            };
            Some(Some(V::I32(c as i32)))
        }
        "sum" | "max" | "min" => {
            let (a, b) = (i64v(args[0]), i64v(args[1]));
            let v = match name {
                "sum" => a.wrapping_add(b),
                "max" => a.max(b),
                _ => a.min(b),
            };
            Some(Some(if class == "Ljava/lang/Long;" {
                V::I64(v)
            } else {
                V::I32(v as i32)
            }))
        }
        "divideUnsigned" | "remainderUnsigned" => {
            let div = name.starts_with("divide");
            Some(Some(if class == "Ljava/lang/Long;" {
                let (a, b) = (i64v(args[0]) as u64, (i64v(args[1]) as u64).max(1));
                V::I64(if div { (a / b) as i64 } else { (a % b) as i64 })
            } else {
                let (a, b) = (i32v(args[0]) as u32, (i32v(args[1]) as u32).max(1));
                V::I32(if div { (a / b) as i32 } else { (a % b) as i32 })
            }))
        }
        "toHexString" => Some(Some(jstr(vm, &format!("{:x}", i64v(args[0]))))),
        "toOctalString" => Some(Some(jstr(vm, &format!("{:o}", i64v(args[0]))))),
        "toBinaryString" => Some(Some(jstr(vm, &format!("{:b}", i64v(args[0]))))),
        "toUnsignedString" => Some(Some(jstr(vm, &format!("{}", i32v(args[0]) as u32)))),
        "toString" if args.len() == 1 => {
            let p = params(sig);
            if p.first() == Some(&"Ljava/lang/String;") {
                // instance toString handled by universal — skip
                return Ok(None);
            }
            let v = prim(vm, args[0]);
            let t = match v {
                V::I32(i) => i.to_string(),
                V::I64(i) => i.to_string(),
                V::F32(f) => fmt_f64(f as f64),
                V::F64(f) => fmt_f64(f),
                V::Ref(0) => "false".into(),
                _ => fmt_v(v),
            };
            Some(Some(jstr(vm, &t)))
        }
        "toString" => {
            // toString(int, radix)
            let (v, radix) = (i64v(args[0]), i32v(args[1]) as u32);
            let radix = radix.clamp(2, 36);
            let mut n = v.unsigned_abs();
            let mut t = String::new();
            loop {
                t.push(std::char::from_digit((n % radix as u64) as u32, radix).unwrap_or('0'));
                n /= radix as u64;
                if n == 0 {
                    break;
                }
            }
            if v < 0 {
                t.push('-');
            }
            Some(Some(jstr(vm, &t.chars().rev().collect::<String>())))
        }
        "decode" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let t = t.trim();
            let (neg, s) = match t.strip_prefix('-') {
                Some(x) => (true, x),
                None => (false, t),
            };
            let (radix, s) = if let Some(x) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
            {
                (16, x)
            } else if let Some(x) = s.strip_prefix('#') {
                (16, x)
            } else if s.len() > 1 && s.starts_with('0') {
                (8, &s[1..])
            } else {
                (10, s)
            };
            let v = i64::from_str_radix(s, radix).unwrap_or(0) * if neg { -1 } else { 1 };
            Some(Some(host(
                vm,
                class,
                HostState::Slot(if class == "Ljava/lang/Long;" {
                    V::I64(v)
                } else {
                    V::I32(v as i32)
                }),
            )))
        }
        "getInteger" => {
            // Integer.getInteger(nm[, val]) — the named system
            // properties callers ask for (rx3.*-priority etc.) are
            // never set in this runtime, so the answer is the
            // caller's default: boxed when int, passed through when
            // an Integer (or null).
            match args.get(1) {
                Some(&V::I32(v)) => Some(Some(host(vm, class, HostState::Slot(V::I32(v))))),
                Some(v) => Some(Some(*v)),
                None => Some(Some(V::Ref(0))),
            }
        }
        "isNaN" | "isInfinite" | "isFinite" => {
            let v = f64v(args[0]);
            let r = match name {
                "isNaN" => v.is_nan(),
                "isInfinite" => v.is_infinite(),
                _ => v.is_finite(),
            };
            Some(Some(V::I32(r as i32)))
        }
        "doubleToLongBits" | "doubleToRawLongBits" => {
            Some(Some(V::I64(f64v(args[0]).to_bits() as i64)))
        }
        "longBitsToDouble" => Some(Some(V::F64(f64::from_bits(i64v(args[0]) as u64)))),
        "floatToIntBits" | "floatToRawIntBits" => {
            Some(Some(V::I32((f64v(args[0]) as f32).to_bits() as i32)))
        }
        "intBitsToFloat" => Some(Some(V::F32(f32::from_bits(i32v(args[0]) as u32)))),
        "forDigit" => {
            let c =
                std::char::from_digit(i32v(args[0]) as u32, i32v(args[1]) as u32).unwrap_or('\0');
            Some(Some(V::I32(c as i32)))
        }
        "digit" => {
            let c = match args[0] {
                V::I32(i) => char::from_u32(i as u32).unwrap_or('\0'),
                _ => sv(vm, args[0])
                    .and_then(|t| t.chars().next())
                    .unwrap_or('\0'),
            };
            Some(Some(V::I32(
                c.to_digit(i32v(args[1]) as u32)
                    .map(|d| d as i32)
                    .unwrap_or(-1),
            )))
        }
        "isDigit"
        | "isLetter"
        | "isLetterOrDigit"
        | "isWhitespace"
        | "isSpaceChar"
        | "isUpperCase"
        | "isLowerCase"
        | "isHighSurrogate"
        | "isLowSurrogate"
        | "isSurrogate"
        | "isAlphabetic"
        | "isDefined"
        | "isIdeographic"
        | "isJavaIdentifierStart"
        | "isJavaIdentifierPart"
        | "isTitleCase"
        | "isUnicodeIdentifierStart"
        | "isUnicodeIdentifierPart"
        | "isISOControl"
        | "isMirrored"
        | "isIdentifierIgnorable" => {
            let cp = i32v(args[0]) as u32;
            let c = char::from_u32(cp).unwrap_or('\0');
            let r = match name {
                "isDigit" => c.is_ascii_digit(),
                "isLetter" | "isAlphabetic" => c.is_alphabetic(),
                "isLetterOrDigit" => c.is_alphanumeric(),
                "isWhitespace" | "isSpaceChar" => c.is_whitespace(),
                "isUpperCase" => c.is_uppercase(),
                "isLowerCase" => c.is_lowercase(),
                "isTitleCase" => c.is_uppercase(),
                "isHighSurrogate" => (0xD800..0xDC00).contains(&cp),
                "isLowSurrogate" => (0xDC00..0xE000).contains(&cp),
                "isSurrogate" => (0xD800..0xE000).contains(&cp),
                "isDefined" => cp < 0x110000,
                "isJavaIdentifierStart" | "isUnicodeIdentifierStart" => {
                    c.is_alphabetic() || c == '_' || c == '$'
                }
                "isJavaIdentifierPart" | "isUnicodeIdentifierPart" => {
                    c.is_alphanumeric() || c == '_' || c == '$'
                }
                "isISOControl" => c.is_control(),
                "isMirrored" | "isIdentifierIgnorable" | "isIdeographic" => false,
                _ => false,
            };
            Some(Some(V::I32(r as i32)))
        }
        "toUpperCase" | "toLowerCase" | "toTitleCase" => {
            let c = char::from_u32(i32v(args[0]) as u32).unwrap_or('\0');
            let out = match name {
                "toLowerCase" => c.to_lowercase().next().unwrap_or(c),
                _ => c.to_uppercase().next().unwrap_or(c),
            };
            Some(Some(V::I32(out as i32)))
        }
        "toChars" => {
            let c = char::from_u32(i32v(args[0]) as u32).unwrap_or('\0');
            if args.len() > 1 {
                // toChars(codePoint, dst, dstIndex)
                if let V::Ref(ah) = args[1] {
                    let di = i32v(args[2]) as usize;
                    if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(ah) {
                        if di < data.len() {
                            data[di] = V::I32(c as i32);
                        }
                    }
                }
                Some(Some(V::I32(1)))
            } else {
                Some(Some(arr(vm, "C", vec![V::I32(c as i32)])))
            }
        }
        "toCodePoint" => {
            let (hi, lo) = (i32v(args[0]) as u32, i32v(args[1]) as u32);
            Some(Some(V::I32(
                (0x10000 + ((hi & 0x3ff) << 10) + (lo & 0x3ff)) as i32,
            )))
        }
        "charCount" => Some(Some(V::I32(if i32v(args[0]) >= 0x10000 { 2 } else { 1 }))),
        "highSurrogate" | "lowSurrogate" => {
            let cp = i32v(args[0]) as u32;
            let v = if name == "highSurrogate" {
                0xD800 + ((cp - 0x10000) >> 10)
            } else {
                0xDC00 + ((cp - 0x10000) & 0x3ff)
            };
            Some(Some(V::I32(v as i32)))
        }
        "hashCode" if args.len() == 1 && !matches!(args[0], V::Ref(_)) => {
            Some(Some(v_hash(args[0])))
        }
        _ => None,
    })
}

fn number_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let v = match args[0] {
        V::Ref(h) => slot_of(vm, h).unwrap_or(V::I32(0)),
        x => x,
    };
    Some(Some(match name {
        "intValue" => V::I32(i32v(v)),
        "longValue" => V::I64(i64v(v)),
        "shortValue" => V::I32(i32v(v) as i16 as i32),
        "byteValue" => V::I32(i32v(v) as i8 as i32),
        "doubleValue" => V::F64(f64v(v)),
        "floatValue" => V::F32(f64v(v) as f32),
        _ => return None,
    }))
}

// ---------------------------------------------------------------------
// Math

fn math_call(name: &str, args: &[V]) -> Option<Option<V>> {
    let r: V = match name {
        "abs" => match args[0] {
            V::I64(i) => V::I64(i.wrapping_abs()),
            V::F32(f) => V::F32(f.abs()),
            V::F64(f) => V::F64(f.abs()),
            _ => V::I32(i32v(args[0]).wrapping_abs()),
        },
        "min" | "max" => {
            let mx = name == "max";
            match (args[0], args[1]) {
                (V::F64(_), _) | (_, V::F64(_)) => {
                    let (a, b) = (f64v(args[0]), f64v(args[1]));
                    V::F64(if mx { a.max(b) } else { a.min(b) })
                }
                (V::F32(a), V::F32(b)) => V::F32(if mx { a.max(b) } else { a.min(b) }),
                (V::I64(a), V::I64(b)) => V::I64(if mx { a.max(b) } else { a.min(b) }),
                _ => {
                    let (a, b) = (i32v(args[0]), i32v(args[1]));
                    V::I32(if mx { a.max(b) } else { a.min(b) })
                }
            }
        }
        "floor" => V::F64(f64v(args[0]).floor()),
        "ceil" => V::F64(f64v(args[0]).ceil()),
        "round" => match args[0] {
            V::F32(f) => V::I32(f.round() as i32),
            _ => V::I64(f64v(args[0]).round() as i64),
        },
        "rint" => V::F64(f64v(args[0]).round()),
        "pow" => V::F64(f64v(args[0]).powf(f64v(args[1]))),
        "sqrt" => V::F64(f64v(args[0]).sqrt()),
        "cbrt" => V::F64(f64v(args[0]).cbrt()),
        "exp" => V::F64(f64v(args[0]).exp()),
        "expm1" => V::F64(f64v(args[0]).exp_m1()),
        "log" => V::F64(f64v(args[0]).ln()),
        "log10" => V::F64(f64v(args[0]).log10()),
        "log1p" => V::F64(f64v(args[0]).ln_1p()),
        "sin" => V::F64(f64v(args[0]).sin()),
        "cos" => V::F64(f64v(args[0]).cos()),
        "tan" => V::F64(f64v(args[0]).tan()),
        "asin" => V::F64(f64v(args[0]).asin()),
        "acos" => V::F64(f64v(args[0]).acos()),
        "atan" => V::F64(f64v(args[0]).atan()),
        "sinh" => V::F64(f64v(args[0]).sinh()),
        "cosh" => V::F64(f64v(args[0]).cosh()),
        "tanh" => V::F64(f64v(args[0]).tanh()),
        "atan2" => V::F64(f64v(args[0]).atan2(f64v(args[1]))),
        "hypot" => V::F64(f64v(args[0]).hypot(f64v(args[1]))),
        "toRadians" => V::F64(f64v(args[0]).to_radians()),
        "toDegrees" => V::F64(f64v(args[0]).to_degrees()),
        "signum" => match args[0] {
            // signum(float) must return a float — cmpl-float on the
            // caller side rejects an F64.
            V::F32(f) => V::F32(f.signum()),
            _ => V::F64(f64v(args[0]).signum()),
        },
        "floorDiv" | "floorMod" => match args[0] {
            V::I64(a) => {
                let b = i64v(args[1]).max(1);
                if name == "floorDiv" {
                    V::I64(a.div_euclid(b))
                } else {
                    V::I64(a.rem_euclid(b))
                }
            }
            _ => {
                let (a, b) = (i32v(args[0]), i32v(args[1]).max(1));
                if name == "floorDiv" {
                    V::I32(a.div_euclid(b))
                } else {
                    V::I32(a.rem_euclid(b))
                }
            }
        },
        "addExact" | "subtractExact" | "multiplyExact" | "negateExact" | "incrementExact"
        | "decrementExact" | "toIntExact" => {
            let wide = matches!(args[0], V::I64(_));
            let (a, b) = (
                i64v(args[0]),
                i64v(args.get(1).copied().unwrap_or(V::I64(0))),
            );
            let v = match name {
                "addExact" => a + b,
                "subtractExact" => a - b,
                "multiplyExact" => a * b,
                "negateExact" => -a,
                "incrementExact" => a + 1,
                "decrementExact" => a - 1,
                _ => a,
            };
            if wide {
                V::I64(v)
            } else {
                V::I32(v as i32)
            }
        }
        "random" => V::F64(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(1)
                .wrapping_mul(6364136223846793005)
                >> 11) as f64
                / (1u64 << 53) as f64,
        ),
        "IEEEremainder" => V::F64(f64v(args[0]) % f64v(args[1])),
        "copySign" => V::F64(f64v(args[0]).copysign(f64v(args[1]))),
        "scalb" => V::F64(f64v(args[0]) * 2f64.powi(i32v(args[1]))),
        "ulp" | "nextAfter" | "nextUp" | "nextDown" => V::F64(f64v(args[0])),
        "getExponent" => V::I32(0),
        _ => return None,
    };
    Some(Some(r))
}

// ---------------------------------------------------------------------
// System / Runtime

fn system_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "currentTimeMillis" => Some(Some(V::I64(now_ms()))),
        "nanoTime" => Some(Some(V::I64(now_ms() * 1_000_000))),
        "arraycopy" => {
            let (V::Ref(src), V::Ref(dst)) = (args[0], args[2]) else {
                return Ok(Some(None));
            };
            if !(matches!(vm.heap.get(src), Some(Obj::Array { .. }))
                && matches!(vm.heap.get(dst), Some(Obj::Array { .. })))
            {
                return Ok(Some(None));
            }
            let (sp, dp, len) = (
                i32v(args[1]) as usize,
                i32v(args[3]) as usize,
                i32v(args[4]) as usize,
            );
            let vals: Vec<V> = match vm.heap.get(src) {
                Some(Obj::Array { data, .. }) => data.iter().skip(sp).take(len).copied().collect(),
                _ => vec![],
            };
            if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(dst) {
                for (i, v) in vals.into_iter().enumerate() {
                    if dp + i < data.len() {
                        data[dp + i] = v;
                    }
                }
            }
            Some(None)
        }
        "identityHashCode" => Some(Some(match args[0] {
            V::Ref(h) => V::I32(h as i32),
            _ => V::I32(0),
        })),
        "getProperty" => {
            let k = sv(vm, args[0]).unwrap_or_default();
            let v = match k.as_str() {
                "file.separator" => "/",
                "path.separator" => ":",
                "line.separator" => "\n",
                "os.name" => "Linux",
                "os.arch" => "aarch64",
                "os.version" => "ovelo",
                "user.home" | "user.dir" => "/data/local/ovelo",
                "user.name" | "http.agent" => "ovelo",
                "java.vendor" => "The Android Project",
                "java.version" => "17",
                // Android identity — gdx's SharedLibraryLoader picks the
                // System.loadLibrary path only when the runtime reports
                // Android; desktop identity sends it down the file
                // extraction path we don't implement.
                "java.runtime.name" => "Android Runtime",
                "java.vm.name" => "Dalvik",
                "java.vm.version" => "2.1.0",
                "java.vm.vendor" | "java.specification.vendor" | "java.vm.specification.vendor" => {
                    "The Android Project"
                }
                "java.io.tmpdir" => "/data/local/ovelo/tmp",
                "file.encoding"
                | "sun.jnu.encoding"
                | "sun.stdout.encoding"
                | "stderr.encoding" => "UTF-8",
                _ => {
                    if args.len() > 1 {
                        return Ok(Some(Some(args[1])));
                    }
                    return Ok(Some(Some(V::Ref(0))));
                }
            };
            Some(Some(jstr(vm, v)))
        }
        "setProperty" | "clearProperty" => Some(Some(V::Ref(0))),
        "lineSeparator" => Some(Some(jstr(vm, "\n"))),
        "loadLibrary" | "load" => {
            let arg = sv(vm, args[0]).unwrap_or_default();
            crate::jni::load_native_lib(vm, &arg, name == "load");
            Some(None)
        }
        "gc" | "runFinalization" | "exit" | "halt" | "setIn" | "setOut" | "setErr"
        | "setProperties" | "setSecurityManager" => Some(None),
        "mapLibraryName" => {
            let n = sv(vm, args[0]).unwrap_or_default();
            Some(Some(jstr(vm, &format!("lib{n}.so"))))
        }
        "getSecurityManager" | "console" | "inheritedChannel" | "getLogger" => {
            Some(Some(V::Ref(0)))
        }
        _ => None,
    })
}

fn runtime_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "getRuntime" => Some(Some(host(vm, "Ljava/lang/Runtime;", HostState::Empty))),
        "availableProcessors" => Some(Some(V::I32(4))),
        "freeMemory" => Some(Some(V::I64(64 << 20))),
        "totalMemory" => Some(Some(V::I64(256 << 20))),
        "maxMemory" => Some(Some(V::I64(512 << 20))),
        "exec" => Some(Some(V::Ref(0))),
        "loadLibrary" | "load" => {
            let arg = sv(vm, args[0]).unwrap_or_default();
            crate::jni::load_native_lib(vm, &arg, name == "load");
            Some(None)
        }
        "exit" | "halt" | "gc" | "runFinalization" | "addShutdownHook" | "removeShutdownHook" => {
            let _ = args;
            Some(None)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Class / reflection

fn class_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "forName" => {
            let mut d = sv(vm, args[0]).unwrap_or_default();
            if !d.starts_with('L') && !d.starts_with('[') {
                d = format!("L{};", d.replace('.', "/"));
            }
            let in_dex = vm.type_index_of(&d).is_some();
            // Classes the runtime stubs without a dex def (JDK + Android
            // framework + kotlin stdlib shims) still resolve; a name with
            // no dex def and no host shim is genuinely absent, and a real
            // ClassNotFoundException is what guests expect when probing
            // optional dependencies (GMS, generated lifecycle adapters).
            let host_known = d.starts_with('[')
                || [
                    "Ljava/",
                    "Ljavax/",
                    "Lsun/",
                    "Lcom/sun/",
                    "Landroid/",
                    "Lkotlin/",
                    "Lkotlinx/",
                    "Lorg/json/",
                    "Lorg/xmlpull/",
                    "Lorg/apache/",
                ]
                .iter()
                .any(|p| d.starts_with(p));
            if !in_dex && !host_known {
                return Err(vm.throw_builtin("Ljava/lang/ClassNotFoundException;", &d));
            }
            Some(Some(class_obj(vm, d)))
        }
        "getName" | "getCanonicalName" | "getTypeName" => {
            let d = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_else(|| class_of(vm, args[0]));
            Some(Some(jstr(vm, &dotted(&d))))
        }
        "getSimpleName" => {
            let d = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_else(|| class_of(vm, args[0]));
            let base = dotted(&d);
            // Member name per InnerClasses: the part after the last
            // `$` — but when that last `$` is itself preceded by `$`
            // (`Foo$$serializer`), the member's declared name keeps
            // the leading `$`. kotlinx.serialization's fallback scans
            // getDeclaredClasses for simpleName == "$serializer".
            let after_pkg = base.rsplit('.').next().unwrap_or(&base);
            let short = match after_pkg.rfind('$') {
                Some(i) if i > 0 && after_pkg.as_bytes()[i - 1] == b'$' => &after_pkg[i - 1..],
                Some(i) => &after_pkg[i + 1..],
                None => after_pkg,
            };
            Some(Some(jstr(vm, short)))
        }
        "getPackageName" | "getPackage" => {
            let d = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let base = dotted(&d);
            let pkg = base.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
            if name == "getPackageName" {
                Some(Some(jstr(vm, pkg)))
            } else {
                Some(Some(host(
                    vm,
                    "Ljava/lang/Package;",
                    HostState::StrBuf(pkg.into()),
                )))
            }
        }
        "isInterface" | "isPrimitive" | "isEnum" | "isAnnotation" | "isSynthetic"
        | "isAnonymousClass" | "isLocalClass" | "isMemberClass" | "isSealed" | "isRecord"
        | "isHidden" => Some(Some(V::I32(0))),
        "isArray" => {
            let d = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            Some(Some(V::I32(d.starts_with('[') as i32)))
        }
        "isInstance" => {
            let want = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let got = class_of(vm, args[1]);
            let ok = desc_is_a(&got, &want) || vm.assignable_desc(&got, &want);
            Some(Some(V::I32(ok as i32)))
        }
        "isAssignableFrom" => {
            let a = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let b = class_desc_of(
                vm,
                match args[1] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            // desc_is_a only knows host classes — dex-to-dex
            // subclassing (e.g. a generated protobuf class extending
            // GeneratedMessageLite) needs assignable_desc's dex walk.
            let ok = desc_is_a(&b, &a) || vm.assignable_desc(&b, &a);
            Some(Some(V::I32(ok as i32)))
        }
        "getSuperclass" => {
            // `s()` returns None for `Obj::Class` mirrors — resolve the
            // descriptor via `class_desc_of` so `SomeType.class` works,
            // then walk the real dex superclass (`super_desc`) before
            // host_supers/Object. Previously an `Obj::Class` argument
            // yielded `d=""`, never matched `None=>Ref(0)` and returned
            // `Object.class` for every class — guest loops like
            // `while (c != null) c = c.getSuperclass()` never exited.
            let d = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let sup = host_supers(&d)
                .first()
                .map(|x| (*x).to_string())
                .or_else(|| vm.super_desc(&d));
            match sup {
                Some(sup) => Some(Some(class_obj(vm, sup))),
                None if !d.is_empty() && d != OBJ => Some(Some(class_obj(vm, OBJ.into()))),
                None => Some(Some(V::Ref(0))),
            }
        }
        "getGenericSuperclass" => {
            // A ParameterizedType over the real superclass: Gson's
            // TypeToken checks `instanceof ParameterizedType` and reads
            // actualTypeArguments[0]. The dex keeps no usable Signature
            // annotation, so the type argument degrades to Object — the
            // result still walks Gson's Object adapter instead of
            // throwing "Must only create direct subclasses of TypeToken".
            let d = match args[0] {
                V::Ref(h) => class_desc_of(vm, h).unwrap_or_default(),
                _ => String::new(),
            };
            let sup = host_supers(&d)
                .first()
                .map(|x| (*x).to_string())
                .or_else(|| vm.super_desc(&d));
            match sup {
                Some(sup) => {
                    // Interned `Obj::Class` mirrors so `getRawType() ==
                    // TypeToken.class` (handle equality, Gson's check).
                    let raw = V::Ref(vm.class_handle(&sup));
                    let obj = V::Ref(vm.class_handle(OBJ));
                    let args_arr = arr(vm, OBJ, vec![obj]);
                    let h =
                        vm.alloc_host("Ljava/lang/reflect/ParameterizedType;", HostState::Empty);
                    pseudo_set(vm, h, P_TARGET, raw);
                    pseudo_set(vm, h, P_DATA, args_arr);
                    Some(Some(V::Ref(h)))
                }
                None => Some(Some(V::Ref(0))),
            }
        }
        "getComponentType" => {
            let d = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            Some(Some(if let Some(inner) = d.strip_prefix('[') {
                class_obj(vm, inner.to_string())
            } else {
                V::Ref(0)
            }))
        }
        "getClassLoader" => Some(Some(host(vm, "Ljava/lang/ClassLoader;", HostState::Empty))),
        "getMethods" | "getDeclaredMethods" => {
            let owner = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let items = vm.declared_method_mirrors(&owner);
            Some(Some(arr(vm, "Ljava/lang/reflect/Method;", items)))
        }
        "getFields" | "getDeclaredFields" => {
            let owner = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let items = vm.declared_field_mirrors(&owner);
            Some(Some(arr(vm, "Ljava/lang/reflect/Field;", items)))
        }
        "getDeclaredClasses" | "getClasses" => {
            let owner = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let items = vm.nested_class_mirrors(&owner);
            Some(Some(arr(vm, "Ljava/lang/Class;", items)))
        }
        "getConstructors"
        | "getDeclaredConstructors"
        | "getInterfaces"
        | "getEnumConstants"
        | "getPermittedSubclasses"
        | "getSigners"
        | "getTypeParameters"
        | "getRecordComponents"
        | "getNestMembers"
        | "getGenericInterfaces" => Some(Some(arr(vm, OBJ, vec![]))),
        "getField" | "getDeclaredField" => {
            let owner = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let fname = sv(vm, args[1]).unwrap_or_default();
            // Missing field → NoSuchFieldException: reflective probes
            // (kotlinx.serialization's Companion/INSTANCE lookup) branch
            // on the throw, not on null. Host classes (`sun.misc.Unsafe`)
            // have no dex field table — always mirror them.
            if !fname.is_empty()
                && vm.is_dex_class(&owner)
                && vm.field_index_of(&owner, &fname).is_none()
            {
                if crate::dbg_flag("OVELO_REFDBG") {
                    eprintln!("[refdbg] NoSuchField {owner}.{fname}");
                }
                let t = vm.throw_builtin(
                    "Ljava/lang/NoSuchFieldException;",
                    &format!("{owner}.{fname}"),
                );
                return Err(t);
            }
            let cv = jstr(vm, &owner);
            let hv = host(
                vm,
                "Ljava/lang/reflect/Field;",
                HostState::StrBuf(fname.clone()),
            );
            if let V::Ref(h) = hv {
                pseudo_set(vm, h, P_COMPONENT, cv);
            }
            Some(Some(hv))
        }
        "getMethod" | "getDeclaredMethod" => {
            let owner = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let mname = sv(vm, args[1]).unwrap_or_default();
            // Missing method → NoSuchMethodException: reflective
            // serializer lookup branches on the throw.
            if !mname.is_empty()
                && !mname.starts_with('<')
                && vm.is_dex_class(&owner)
                && !vm.has_method_named(&owner, &mname)
            {
                if crate::dbg_flag("OVELO_REFDBG") {
                    eprintln!("[refdbg] NoSuchMethod {owner}.{mname}");
                }
                let t = vm.throw_builtin(
                    "Ljava/lang/NoSuchMethodException;",
                    &format!("{owner}.{mname}"),
                );
                return Err(t);
            }
            let cv = jstr(vm, &owner);
            let hv = host(
                vm,
                "Ljava/lang/reflect/Method;",
                HostState::StrBuf(mname.clone()),
            );
            if let V::Ref(h) = hv {
                pseudo_set(vm, h, P_COMPONENT, cv);
            }
            Some(Some(hv))
        }
        "getConstructor" | "getDeclaredConstructor" => {
            let owner = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            // The `Class[]` signature argument → param descriptors, so
            // `newInstance` runs the ctor the caller actually resolved.
            let params: Vec<String> = match args.get(1) {
                Some(V::Ref(a)) => match vm.heap.get(*a) {
                    Some(Obj::Array { data, .. }) => data
                        .iter()
                        .map(|e| match e {
                            V::Ref(h) => class_desc_of(vm, *h).unwrap_or_default(),
                            _ => String::new(),
                        })
                        .collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            let midx = vm.find_ctor_sig(&owner, &params).unwrap_or_default();
            if params.iter().any(|p| p.is_empty()) && !params.is_empty() {
                // Unreadable param class — cannot match a real ctor.
                let t = vm.throw_builtin(
                    "Ljava/lang/NoSuchMethodException;",
                    &format!("{owner}.<init>"),
                );
                return Err(t);
            }
            if midx.is_none() && vm.is_dex_class(&owner) {
                // Dex-defined class: signature lookup is authoritative —
                // NoSuchMethodException is the real Java answer. Host
                // classes (String, View, …) have no dex defs to match
                // against, so fall through to a mirror without P_CTOR:
                // newInstance then uses instantiate_with.
                let t = vm.throw_builtin(
                    "Ljava/lang/NoSuchMethodException;",
                    &format!("{owner}.<init>({})", params.join(",")),
                );
                return Err(t);
            }
            let cv = jstr(vm, &owner);
            let hv = host(
                vm,
                "Ljava/lang/reflect/Constructor;",
                HostState::StrBuf(String::new()),
            );
            if let V::Ref(h) = hv {
                pseudo_set(vm, h, P_COMPONENT, cv);
                if let Some(m) = midx {
                    pseudo_set(vm, h, P_CTOR, V::I64(m as i64));
                }
            }
            Some(Some(hv))
        }
        "newInstance" => {
            let d = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            match vm.instantiate(&d) {
                Ok(h) => Some(Some(V::Ref(h))),
                Err(e) => {
                    // A class with no dex def and no host impl is
                    // absent — `ClassNotFoundException` is what callers
                    // catching optional-dependency probes expect (the
                    // kotlin-reflect `ReflectionFactoryImpl` fallback
                    // catches CNFE, not InstantiationException).
                    let absent =
                        e.cause() == "dexrt.class.missing" || vm.type_index_of(&d).is_none();
                    let t = vm.throw_builtin(
                        if absent {
                            "Ljava/lang/ClassNotFoundException;"
                        } else {
                            "Ljava/lang/InstantiationException;"
                        },
                        &d,
                    );
                    return Err(t);
                }
            }
        }
        "getResource"
        | "getResourceAsStream"
        | "getProtectionDomain"
        | "getEnclosingMethod"
        | "getEnclosingConstructor"
        | "getEnclosingClass"
        | "getDeclaringClass"
        | "getModule"
        | "getNestHost"
        | "describeConstable"
        | "getModifiers0" => Some(Some(V::Ref(0))),
        "getModifiers" => Some(Some(V::I32(1))),
        // Class-level dex annotations — Navigator.@Name, DI scopes,
        // retrofit annotations all ride getAnnotation; a null stub
        // throws IllegalArgumentException in getNameForNavigator.
        "getAnnotation" | "getDeclaredAnnotation" => {
            let cdesc = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let adesc = class_desc_of(
                vm,
                match args[1] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let v = vm
                .class_gidx_of(&cdesc)
                .map(|cg| vm.class_annotation(cg, &adesc))
                .unwrap_or(V::Ref(0));
            Some(Some(v))
        }
        "getAnnotations" | "getDeclaredAnnotations" => {
            let cdesc = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let items = vm
                .class_gidx_of(&cdesc)
                .map(|cg| vm.class_annotations(cg))
                .unwrap_or_default();
            Some(Some(arr(vm, "Ljava/lang/annotation/Annotation;", items)))
        }
        "isAnnotationPresent" => {
            let cdesc = class_desc_of(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let adesc = class_desc_of(
                vm,
                match args[1] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let v = vm
                .class_gidx_of(&cdesc)
                .map(|cg| vm.class_annotation(cg, &adesc))
                .unwrap_or(V::Ref(0));
            Some(Some(V::I32((v != V::Ref(0)) as i32)))
        }
        // `cast(obj)` yields the object arg; `asSubclass(Class)` yields
        // `this` Class (the arg is the bound to check, not the result).
        "cast" => Some(Some(args.get(1).copied().unwrap_or(args[0]))),
        "asSubclass" => Some(Some(args[0])),
        "desiredAssertionStatus" => Some(Some(V::I32(0))),
        "arrayType" | "componentType" => Some(Some(V::Ref(0))),
        "toString" => {
            let d = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            Some(Some(jstr(vm, &format!("class {}", dotted(&d)))))
        }
        _ => None,
    })
}

fn classloader_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "getSystemClassLoader" | "getPlatformClassLoader" | "getParent" | "getClassLoader" => {
            Some(Some(host(vm, "Ljava/lang/ClassLoader;", HostState::Empty)))
        }
        "loadClass" | "findClass" => {
            let mut d = sv(vm, args[0]).unwrap_or_default();
            if !d.starts_with('L') {
                d = format!("L{};", d.replace('.', "/"));
            }
            Some(Some(class_obj(vm, d)))
        }
        "getResource"
        | "getResourceAsStream"
        | "getDefinedPackage"
        | "getName"
        | "getUnnamedModule" => Some(Some(V::Ref(0))),
        "getResources" => Some(Some(host(
            vm,
            "Ljava/util/Enumeration;",
            HostState::Iter(std::rc::Rc::new(vec![]), 0),
        ))),
        "isRegisteredAsParallelCapable"
        | "clearAssertionStatus"
        | "setClassAssertionStatus"
        | "setPackageAssertionStatus"
        | "setDefaultAssertionStatus" => {
            if name.starts_with("is") {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Thread

fn thread_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "currentThread" => Some(Some(if vm.cur_thread != 0 {
            V::Ref(vm.cur_thread)
        } else {
            main_thread(vm)
        })),
        "getId" => Some(Some(V::I64(1))),
        "getName" => {
            let n = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_else(|| "main".into());
            Some(Some(jstr(vm, &n)))
        }
        "setName" => {
            if let V::Ref(h) = args[0] {
                let n = sv(vm, args[1]).unwrap_or_default();
                if let Some(Obj::Host {
                    state: HostState::StrBuf(t),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    *t = n;
                }
            }
            Some(None)
        }
        "getPriority" => Some(Some(V::I32(5))),
        "isAlive" | "isDaemon" | "isVirtual" | "isInterrupted" | "interrupted"
        | "isCCLOverriden" => Some(Some(V::I32(0))),
        "holdsLock" => Some(Some(V::I32(1))),
        "activeCount" => Some(Some(V::I32(1))),
        "getState" => Some(Some(host(
            vm,
            "Ljava/lang/Thread$State;",
            HostState::StrBuf("RUNNABLE".into()),
        ))),
        "getStackTrace" => Some(Some(stack_frames(vm))),
        "getAllStackTraces" => Some(Some(arr(vm, "Ljava/lang/StackTraceElement;", vec![]))),
        "getContextClassLoader" => {
            Some(Some(host(vm, "Ljava/lang/ClassLoader;", HostState::Empty)))
        }
        "getUncaughtExceptionHandler" => {
            // A null handler turns a worker's death report into an NPE
            // that masks the real exception — hand back a placeholder;
            // its uncaughtException dispatch is a builtin no-op.
            Some(Some(host(
                vm,
                "Ljava/lang/Thread$UncaughtExceptionHandler;",
                HostState::Empty,
            )))
        }
        // AOSP contract: null until a default handler is installed —
        // callers branch on the null to pick their own reporting path.
        "getDefaultUncaughtExceptionHandler" => Some(Some(V::Ref(0))),
        "<init>" => {
            // Thread(Runnable[, name]) / Thread(group, Runnable, …):
            // remember the first Runnable-ish arg so `start` can run it.
            if let V::Ref(h) = args[0] {
                if let Some(&t) = args[1..].iter().find(|a| {
                    matches!(a, V::Ref(rh) if *rh != 0
                        && s(vm, *rh).is_none()
                        && !matches!(vm.heap.get(*rh), Some(Obj::Host { desc, .. })
                            if desc.contains("ThreadGroup")))
                }) {
                    pseudo_set(vm, h, P_TARGET, t);
                }
            }
            Some(None)
        }
        "start" => {
            // A started Thread is async: enqueue the Runnable for the
            // next drain point rather than running it inline — inline
            // execution deadlocks guests whose tasks reschedule work
            // (coroutine dispatchers, self-perpetuating handlers).
            if let V::Ref(h) = args[0] {
                let t = pseudo_get(vm, h, P_TARGET).unwrap_or(V::Ref(h));
                if let V::Ref(r) = t {
                    // Thread-spawned runnables get an unbounded park
                    // budget — a `while (running)` game loop parks on
                    // every Thread.sleep and must never hit the worker
                    // repark cap.
                    let rcls = super::class_of(vm, V::Ref(r));
                    eprintln!("[thread] start {rcls} runnable=0x{r:x}");
                    // GLSurfaceView$GLThread waits on surface state inside
                    // its own manager — set it directly since the real
                    // SurfaceHolder.Callback chain never fires here.
                    if rcls.contains("GLSurfaceView$GLThread") {
                        for (m, a) in [
                            ("surfaceCreated", vec![]),
                            ("onWindowResize", vec![V::I32(720), V::I32(1280)]),
                            ("setRenderMode", vec![V::I32(1)]),
                            ("requestRender", vec![]),
                        ] {
                            let r_ = vm.call_virtual(r, m, &a);
                            eprintln!("[thread] {m} -> {r_:?}");
                        }
                    }
                    vm.thread_runnables.insert(r);
                    vm.enqueue_runnable(r, false);
                }
            }
            Some(None)
        }
        "sleep" => {
            // A guest thread can't really suspend: on a worker context
            // record the requested wake time and unwind with ovelo.park
            // — the drain requeues the runnable into delayed_runnables
            // at that deadline. On the main thread a real sleep would
            // freeze the UI, so stay a no-op.
            if vm.cur_thread != 0 && vm.cur_thread != vm.main_thread_h {
                // `sleep` is static: args[0] is the millisecond count
                // (args[1] is the nanos of the (JI)V overload).
                let ms = match args.first().copied().unwrap_or(V::I64(0)) {
                    V::I64(m) => m,
                    V::I32(m) => m as i64,
                    V::F64(m) => m as i64,
                    _ => 0,
                }
                .max(0);
                // A spawned Thread's run() IS the loop — restart it at
                // the top and re-enter: parking yields the drain slice
                // so frame-scheduler loops (`while { work; sleep(1) }`)
                // don't captive the worker inside one drain call.
                // Executor workers instead keep a short real sleep —
                // parking there re-runs the whole queued task.
                if ms <= 500 && !vm.thread_runnables.contains(&vm.cur_runnable) {
                    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                    return Ok(Some(None));
                }
                let due = (now_ms().saturating_add(ms)).max(0) as u64;
                if vm.cur_runnable != 0 {
                    if vm.slept_once.remove(&vm.cur_runnable) {
                        // Re-run past the waited-out deadline — the
                        // first sleep after resume returns at once or
                        // the task re-parks at this call forever.
                        return Ok(Some(None));
                    }
                    vm.slept_once.insert(vm.cur_runnable);
                    vm.sleep_until.insert(vm.cur_runnable, due);
                }
                return Err(crate::E::Vm(crate::err("ovelo.park")));
            }
            Some(None)
        }
        "setPriority"
        | "setDaemon"
        | "interrupt"
        | "join"
        | "yield"
        | "setUncaughtExceptionHandler"
        | "setDefaultUncaughtExceptionHandler"
        | "dumpStack"
        | "setContextClassLoader"
        | "onSpinWait"
        | "stop"
        | "suspend"
        | "resume"
        | "checkAccess"
        | "run"
        | "start0"
        | "setName0"
        | "enumerate"
        | "registerNatives" => Some(None),
        "currentCarrierThread" | "threadId" => Some(Some(args[0])),
        _ => None,
    })
}

fn package_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "getName"
        | "getImplementationTitle"
        | "getSpecificationTitle"
        | "getImplementationVendor"
        | "getSpecificationVendor"
        | "getImplementationVersion"
        | "getSpecificationVersion" => {
            let t = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            Some(Some(jstr(vm, &t)))
        }
        _ => Some(Some(stub_ret(vm, sig))),
    }
}

fn threadlocal_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "get" => {
            let V::Ref(h) = args[0] else {
                return Some(Some(V::Ref(0)));
            };
            let cur = slot_of(vm, h)
                .or_else(|| pseudo_get(vm, h, P_TARGET))
                .unwrap_or(V::Ref(0));
            if !matches!(cur, V::Ref(0) | V::I32(0)) {
                return Some(Some(cur));
            }
            // First access: `initialValue` is virtual so a dex-defined
            // subclass (desugared ThreadLocalRandom's `u`) supplies it;
            // `withInitial` locals call their stored supplier instead.
            let init = match pseudo_get(vm, h, P_DATA) {
                Some(V::Ref(sup)) if sup != 0 => vm
                    .call_virtual(sup, "get", &[])
                    .ok()
                    .flatten()
                    .unwrap_or(V::Ref(0)),
                _ => vm
                    .call_virtual(h, "initialValue", &[])
                    .ok()
                    .flatten()
                    .unwrap_or(V::Ref(0)),
            };
            if let Some(Obj::Host {
                state: HostState::Slot(s),
                ..
            }) = vm.heap.get_mut(h)
            {
                *s = init;
            } else {
                // Dex-defined ThreadLocal subclass: cache on a pseudo field
                // so repeated get() returns the same value.
                pseudo_set(vm, h, P_TARGET, init);
            }
            Some(Some(init))
        }
        "set" | "remove" => {
            if let V::Ref(h) = args[0] {
                let v = if name == "set" { args[1] } else { V::Ref(0) };
                if let Some(Obj::Host {
                    state: HostState::Slot(s),
                    ..
                }) = vm.heap.get_mut(h)
                {
                    *s = v;
                } else {
                    pseudo_set(vm, h, P_TARGET, v);
                }
            }
            Some(None)
        }
        "withInitial" => {
            let hv = host(vm, "Ljava/lang/ThreadLocal;", HostState::Slot(V::Ref(0)));
            if let V::Ref(h) = hv {
                // Static method: the Supplier is args[0], not args[1].
                pseudo_set(vm, h, P_DATA, args.first().copied().unwrap_or(V::Ref(0)));
            }
            Some(Some(hv))
        }
        "initialValue" => Some(Some(V::Ref(0))),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Throwable / Enum / StackTraceElement

/// The current guest call stack as a `StackTraceElement[]` —
/// declaring class in the element's StrBuf, method name in
/// P_COMPONENT (the slots `<init>`/`getStackTrace` fill).
fn stack_frames(vm: &mut Vm) -> V {
    let frames: Vec<V> = vm
        .describe_stack()
        .into_iter()
        .rev()
        .map(|d| {
            // "Lpkg/Cls;.name(sig)ret" -> (pkg.Cls, name)
            let (cn, mn) = d
                .strip_prefix('L')
                .and_then(|r| r.split_once(";."))
                .map(|(c, r)| {
                    (
                        c.replace('/', "."),
                        r.split('(').next().unwrap_or("").to_string(),
                    )
                })
                .unwrap_or((d.clone(), String::new()));
            let v = host(vm, "Ljava/lang/StackTraceElement;", HostState::StrBuf(cn));
            if let V::Ref(h) = v {
                let m = jstr(vm, &mn);
                pseudo_set(vm, h, P_COMPONENT, m);
            }
            v
        })
        .collect();
    arr(vm, "Ljava/lang/StackTraceElement;", frames)
}

fn throwable_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "<init>" => {
            // Throwable(msg) / Throwable(cause) / Throwable(msg, cause):
            // host classes have no dex <init> — store the args on pseudo
            // fields so getMessage/getCause and crash reporting see them.
            for &a in args.iter().skip(1) {
                match a {
                    V::Ref(fh) if fh != 0 => match vm.heap.get(fh) {
                        Some(Obj::Str(_)) => {
                            pseudo_set(vm, h, P_DATA, a);
                            field_set(vm, h, THROWABLE, "detailMessage", a);
                        }
                        _ => {
                            field_set(vm, h, THROWABLE, "cause", a);
                            pseudo_set(vm, h, P_TARGET, a);
                        }
                    },
                    _ => {}
                }
            }
            Some(None)
        }
        "getMessage" | "getLocalizedMessage" => Some(Some(
            slot_of(vm, h)
                .or_else(|| pseudo_get(vm, h, P_DATA))
                .or_else(|| field_get(vm, h, THROWABLE, "detailMessage"))
                .unwrap_or(V::Ref(0)),
        )),
        "getCause" => Some(Some(
            pseudo_get(vm, h, P_TARGET)
                .or_else(|| field_get(vm, h, THROWABLE, "cause"))
                .or_else(|| field_get(vm, h, "Ljava/lang/Exception;", "cause"))
                .unwrap_or(V::Ref(0)),
        )),
        "initCause" => {
            field_set(vm, h, THROWABLE, "cause", args[1]);
            pseudo_set(vm, h, P_TARGET, args[1]);
            Some(Some(args[0]))
        }
        "fillInStackTrace" => Some(Some(args[0])),
        // Real frames — Timber's getTag and kotlin's
        // checkNotNullParameter both index this array; an empty
        // array turns those into first{} crashes / aget AIOOBEs.
        "getStackTrace" | "getOurStackTrace" => Some(Some(stack_frames(vm))),
        "setStackTrace" | "addSuppressed" => Some(None),
        "getSuppressed" => Some(Some(arr(vm, THROWABLE, vec![]))),
        "printStackTrace" => {
            let d = class_of(vm, args[0]);
            let mut msg = slot_of(vm, h).and_then(|v| sv(vm, v)).unwrap_or_default();
            // Runtime-generated throwables keep their message and throw
            // site on the Obj::Throwable itself rather than in a detail
            // field — surface both so printStackTrace shows something
            // useful for VM-thrown exceptions.
            if let Some(Obj::Throwable { detail, .. }) = vm.heap.get(h) {
                if msg.is_empty() {
                    msg = detail.clone();
                }
                if let Some(site) = vm.throw_sites.get(&h) {
                    eprintln!("[guest] {}: {msg} at {site}", dotted(&d));
                } else {
                    eprintln!("[guest] {}: {}", dotted(&d), msg);
                }
            } else {
                eprintln!("[guest] {}: {}", dotted(&d), msg);
            }
            Some(None)
        }
        _ => None,
    })
}

/// `SomeEnum.values()` — the constants are the class's static fields
/// of its own type, already initialized by `<clinit>` into `statics`.
fn enum_values(vm: &mut Vm, class: &str) -> V {
    let vals = enum_value_list(vm, class);
    arr(vm, class, vals)
}

/// The enum constants of `class` in declaration order.
pub(crate) fn enum_value_list(vm: &mut Vm, class: &str) -> Vec<V> {
    let mut vals = Vec::new();
    for (di, d) in vm.all_dexes().iter().enumerate() {
        for cd in d.class_defs() {
            if d.type_descriptor(cd.class_idx).ok().as_deref() != Some(class) {
                continue;
            }
            let Ok(data) = d.class_data_rc(cd) else {
                continue;
            };
            for ef in &data.static_fields {
                let is_const = d
                    .field(ef.field_idx)
                    .ok()
                    .and_then(|f| d.type_descriptor(f.type_idx as u32).ok())
                    .as_deref()
                    == Some(class);
                if is_const {
                    if let Some(v) =
                        vm.static_of(crate::gx(di, cd.class_idx), crate::gx(di, ef.field_idx))
                    {
                        vals.push(v);
                    }
                }
            }
        }
    }
    if vals.is_empty() {
        // Platform enums (android.graphics.* etc.) have no dex class
        // def — mint their constants so Kotlin $WhenMappings clinits
        // and `values().length` don't die on an empty array.
        if let Some(names) = platform_enum_consts(class) {
            for (i, n) in names.iter().enumerate() {
                let h = host(vm, class, HostState::Empty);
                if let V::Ref(hh) = h {
                    let ns = jstr(vm, n);
                    pseudo_set(vm, hh, P_DATA, ns);
                    pseudo_set(vm, hh, P_FLAGS, V::I32(i as i32));
                }
                vals.push(h);
            }
        }
    }
    vals
}

/// `name`'s declaration ordinal in a mirrored platform enum.
pub(crate) fn platform_enum_ordinal(class: &str, name: &str) -> Option<i32> {
    platform_enum_consts(class).and_then(|l| l.iter().position(|&n| n == name).map(|i| i as i32))
}

/// Declaration-order constants of Android platform enums we mirror.
pub(crate) fn platform_enum_consts(class: &str) -> Option<&'static [&'static str]> {
    Some(match class {
        "Landroid/graphics/Paint$Cap;" => &["BUTT", "ROUND", "SQUARE"],
        "Landroid/graphics/Paint$Style;" => &["FILL", "STROKE", "FILL_AND_STROKE"],
        "Landroid/graphics/Paint$Join;" => &["MITER", "ROUND", "BEVEL"],
        "Landroid/graphics/Paint$Align;" => &["LEFT", "CENTER", "RIGHT"],
        "Landroid/graphics/PorterDuff$Mode;" => &[
            "CLEAR", "SRC", "DST", "SRC_OVER", "DST_OVER", "SRC_IN", "DST_IN", "SRC_OUT",
            "DST_OUT", "SRC_ATOP", "DST_ATOP", "XOR", "DARKEN", "LIGHTEN", "MULTIPLY", "SCREEN",
            "ADD", "OVERLAY",
        ],
        "Landroid/graphics/BlendMode;" => &[
            "CLEAR",
            "SRC",
            "DST",
            "SRC_OVER",
            "DST_OVER",
            "SRC_IN",
            "DST_IN",
            "SRC_OUT",
            "DST_OUT",
            "SRC_ATOP",
            "DST_ATOP",
            "XOR",
            "PLUS",
            "MODULATE",
            "SCREEN",
            "OVERLAY",
            "DARKEN",
            "LIGHTEN",
            "COLOR_DODGE",
            "COLOR_BURN",
            "HARD_LIGHT",
            "SOFT_LIGHT",
            "DIFFERENCE",
            "EXCLUSION",
            "MULTIPLY",
            "HUE",
            "SATURATION",
            "COLOR",
            "LUMINOSITY",
        ],
        "Landroid/graphics/Path$Direction;" => &["CW", "CCW"],
        "Landroid/graphics/Region$Op;" => &[
            "DIFFERENCE",
            "INTERSECT",
            "UNION",
            "XOR",
            "REVERSE_DIFFERENCE",
            "REPLACE",
        ],
        _ => return None,
    })
}

/// An enum constant's `ordinal()` as stored by `Enum.<init>`.
pub(crate) fn enum_ordinal(vm: &Vm, v: V) -> i32 {
    match v {
        V::Ref(h) => i32v(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0))),
        _ => 0,
    }
}

fn enum_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "<init>" => {
            // Enum(name, ordinal) — the base ctor is a stub, so keep
            // both on pseudo fields for name()/ordinal()/compareTo.
            let this = args.first().copied().unwrap_or(V::Ref(0));
            if let V::Ref(h) = this {
                if let Some(&n) = args.get(1) {
                    pseudo_set(vm, h, P_DATA, n);
                }
                if let Some(&o) = args.get(2) {
                    pseudo_set(vm, h, P_FLAGS, o);
                }
            }
            Some(None)
        }
        "name" | "toString" => {
            let h = match args[0] {
                V::Ref(h) => h,
                _ => 0,
            };
            let n = pseudo_get(vm, h, P_DATA)
                .and_then(|v| sv(vm, v))
                .or_else(|| s(vm, h))
                .unwrap_or_else(|| "ENUM".into());
            Some(Some(jstr(vm, &n)))
        }
        "ordinal" => Some(Some(match args[0] {
            V::Ref(h) => pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0)),
            _ => V::I32(0),
        })),
        "compareTo" => {
            // ordinal difference — Lifecycle gates like
            // `state.compareTo(CREATED) >= 0` hang on this.
            let ord = |v: V| match v {
                V::Ref(h) => i64v(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0))),
                _ => 0,
            };
            let d = ord(args[0]) - ord(args[1]);
            if crate::dbg_flag("OVELO_ENUMDBG") {
                let nm = |v: V| match v {
                    V::Ref(h) => pseudo_get(vm, h, P_DATA)
                        .and_then(|s| sv(vm, s))
                        .unwrap_or_else(|| "?".into()),
                    _ => "nonref".into(),
                };
                eprintln!(
                    "[enumdbg] compareTo {}({}) vs {}({}) -> {d}",
                    nm(args[0]),
                    ord(args[0]),
                    nm(args[1]),
                    ord(args[1])
                );
            }
            Some(Some(V::I32(d.signum() as i32)))
        }
        "getDeclaringClass" => Some(Some(class_obj(vm, "Ljava/lang/Enum;".into()))),
        "valueOf" => {
            // Both `Enum.valueOf(Class,String)` (2 args) and the
            // compiler-generated `EnumType.valueOf(String)` (1 arg)
            // land here — the name is always the LAST argument.
            let n = args.last().and_then(|v| sv(vm, *v)).unwrap_or_default();
            // Resolve through values() so platform enums return a
            // constant with its real ordinal (Kotlin `when` tables
            // index by it).
            let hit = enum_value_list(vm, class).into_iter().find(|v| {
                matches!(v, V::Ref(h)
                    if pseudo_get(vm, *h, P_DATA)
                        .and_then(|s| sv(vm, s))
                        .as_deref() == Some(n.as_str()))
            });
            Some(Some(hit.unwrap_or_else(|| {
                host(vm, "Ljava/lang/Enum;", HostState::StrBuf(n))
            })))
        }
        "values" => {
            let a = enum_values(vm, class);
            if crate::dbg_flag("OVELO_TRACE") {
                let n = match vm.heap.get(match a {
                    V::Ref(h) => h,
                    _ => 0,
                }) {
                    Some(Obj::Array { data, .. }) => data.len(),
                    _ => usize::MAX,
                };
                eprintln!("[trace] Enum.values() on {class} -> {n} constants");
            }
            Some(Some(a))
        }
        "describeConstable" => Some(Some(V::Ref(0))),
        _ => None,
    })
}

fn stacktrace_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let h = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    match name {
        "<init>" => {
            // StackTraceElement(declaringClass, methodName, fileName,
            // lineNumber) — same slots getStackTrace fills.
            if let Some(n) = args.get(1).and_then(|v| sv(vm, *v)) {
                if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(h) {
                    *state = HostState::StrBuf(n);
                }
            }
            if let Some(&mn) = args.get(2) {
                pseudo_set(vm, h, P_COMPONENT, mn);
            }
            if let Some(&f) = args.get(3) {
                pseudo_set(vm, h, P_DATA, f);
            }
            if let Some(&l) = args.get(4) {
                pseudo_set(vm, h, P_FLAGS, l);
            }
            Some(None)
        }
        "getClassName" => Some(Some(s(vm, h).map(|n| jstr(vm, &n)).unwrap_or(V::Ref(0)))),
        "getMethodName" => Some(Some(
            pseudo_get(vm, h, P_COMPONENT)
                .and_then(|v| sv(vm, v))
                .map(|n| jstr(vm, &n))
                .unwrap_or(V::Ref(0)),
        )),
        "getFileName" => Some(Some(
            pseudo_get(vm, h, P_DATA)
                .and_then(|v| sv(vm, v))
                .map(|n| jstr(vm, &n))
                .unwrap_or_else(|| jstr(vm, "")),
        )),
        "getLineNumber" => Some(Some(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0)))),
        "getModuleVersion" => Some(Some(V::I32(0))),
        "isNativeMethod" => Some(Some(V::I32(0))),
        _ => Some(Some(V::Ref(0))),
    }
}

// ---------------------------------------------------------------------
// java.lang.reflect — Field/Method/Constructor mirrors backed by the
// declaring class descriptor (P_COMPONENT) and member name (StrBuf).

/// The `GIdx` a `Method` mirror stands for (`P_METH` set when the
/// mirror was minted by `getDeclaredMethods`).
fn meth_idx(vm: &Vm, h: u32) -> Option<crate::GIdx> {
    match pseudo_get(vm, h, P_METH) {
        Some(V::I64(x)) => Some(x as crate::GIdx),
        _ => None,
    }
}

fn reflect_call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    let h = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => 0,
    };
    let member = s(vm, h).unwrap_or_default();
    let owner = pseudo_get(vm, h, P_COMPONENT)
        .and_then(|v| sv(vm, v))
        .unwrap_or_default();
    Ok(match name {
        _ if class == "Ljava/lang/reflect/ParameterizedType;" => match name {
            "getActualTypeArguments" => {
                Some(pseudo_get(vm, h, P_DATA).or(Some(arr(vm, OBJ, vec![]))))
            }
            "getRawType" => Some(pseudo_get(vm, h, P_TARGET).or(Some(V::Ref(0)))),
            "getOwnerType" => Some(Some(V::Ref(0))),
            _ => None,
        },
        _ if class == "Ljava/lang/reflect/Modifier;" => {
            // Static `Modifier.isX(int)` predicates over the
            // java.lang.reflect modifier bit set.
            let m = i64v(args.first().copied().unwrap_or(V::I32(0))) as u32;
            let bit = match name {
                "isPublic" => 0x0001,
                "isPrivate" => 0x0002,
                "isProtected" => 0x0004,
                "isStatic" => 0x0008,
                "isFinal" => 0x0010,
                "isSynchronized" => 0x0020,
                "isVolatile" => 0x0040,
                "isTransient" => 0x0080,
                "isNative" => 0x0100,
                "isInterface" => 0x0200,
                "isAbstract" => 0x0400,
                "isStrict" => 0x0800,
                _ => return Ok(None),
            };
            Some(Some(V::I32(if m & bit != 0 { 1 } else { 0 })))
        }
        "getName" => Some(Some(jstr(vm, &member))),
        "getDeclaringClass" => Some(Some(class_obj(vm, owner))),
        "getModifiers" => {
            // Real dex access_flags for P_METH mirrors, masked to the
            // java.lang.reflect.Modifier bit set; bare mirrors default
            // to public (the stub behavior they had before).
            let f = match pseudo_get(vm, h, P_FLAGS) {
                Some(V::I32(x))
                    if pseudo_get(vm, h, P_METH).is_some()
                        || pseudo_get(vm, h, P_DATA).is_some() =>
                {
                    x & 0xffff
                }
                _ => 1,
            };
            Some(Some(V::I32(f)))
        }
        "setAccessible"
        | "checkCanSetAccessible"
        | "checkCanSetAccessible0"
        | "trySetAccessible"
        | "notifyAccessFlagsForrant" => Some(if name == "trySetAccessible" {
            Some(V::I32(1))
        } else {
            None
        }),
        "isAccessible" | "canAccess" | "isDefault" | "isEnumConstant" => Some(Some(V::I32(1))),
        "isSynthetic" | "isBridge" | "isVarArgs" => {
            let f = pseudo_get(vm, h, P_FLAGS)
                .map(|v| match v {
                    V::I32(x) => x,
                    _ => 0,
                })
                .unwrap_or(0);
            let bit = match name {
                "isBridge" => 0x40,
                "isVarArgs" => 0x80,
                _ => 0x1000,
            };
            Some(Some(V::I32(if f & bit != 0 { 1 } else { 0 })))
        }
        "getType" | "getReturnType" | "getGenericType" | "getGenericReturnType" => {
            // Method mirror → declared return type; Field mirror (P_DATA
            // carries the field index) → declared field type.
            let d = meth_idx(vm, h)
                .and_then(|m| vm.method_return_desc(m))
                .or_else(|| match pseudo_get(vm, h, P_DATA) {
                    Some(V::I64(f)) => vm.field_type_desc(f as crate::GIdx),
                    _ => None,
                });
            Some(Some(class_obj(vm, d.unwrap_or_else(|| OBJ.into()))))
        }
        "getParameterTypes" | "getGenericParameterTypes" => {
            let items = meth_idx(vm, h)
                .map(|m| vm.method_param_descs(m))
                .unwrap_or_default()
                .into_iter()
                .map(|d| class_obj(vm, d))
                .collect();
            Some(Some(arr(vm, "Ljava/lang/Class;", items)))
        }
        "getExceptionTypes"
        | "getGenericExceptionTypes"
        | "getParameterAnnotations"
        | "getTypeParameters" => Some(Some(arr(vm, OBJ, vec![]))),
        "getAnnotations" | "getDeclaredAnnotations" => {
            let items = meth_idx(vm, h)
                .map(|m| vm.method_annotations(m))
                .unwrap_or_default();
            Some(Some(arr(vm, "Ljava/lang/annotation/Annotation;", items)))
        }
        "getAnnotation" | "getDeclaredAnnotation" => {
            let v = match (meth_idx(vm, h), args.get(1)) {
                (Some(m), Some(V::Ref(c))) => class_desc_of(vm, *c)
                    .map(|d| vm.method_annotation(m, &d))
                    .unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            };
            Some(Some(v))
        }
        "isAnnotationPresent" => {
            let v = match (meth_idx(vm, h), args.get(1)) {
                (Some(m), Some(V::Ref(c))) => class_desc_of(vm, *c)
                    .map(|d| vm.method_annotation(m, &d))
                    .unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            };
            Some(Some(V::I32(if matches!(v, V::Ref(0)) { 0 } else { 1 })))
        }
        "getDefaultValue" => {
            let v = meth_idx(vm, h)
                .and_then(|m| vm.method_default_value(m))
                .unwrap_or(V::Ref(0));
            Some(Some(v))
        }
        "getGenericSignature" | "getSignature" => Some(Some(V::Ref(0))),
        "getParameterCount" => Some(Some(V::I32(
            meth_idx(vm, h)
                .map(|m| vm.method_param_descs(m).len() as i32)
                .unwrap_or(0),
        ))),
        "toString" | "toGenericString" => {
            Some(Some(jstr(vm, &format!("{}.{}", dotted(&owner), member))))
        }
        "invoke" if class.contains("Method") => {
            // args: [method_mirror, obj, args_array]
            let target = match args.get(1) {
                Some(V::Ref(t)) => *t,
                _ => 0,
            };
            let argv: Vec<V> = match args.get(2) {
                Some(V::Ref(a)) => match vm.heap.get(*a) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => vec![],
                },
                _ => vec![],
            };
            let res = match meth_idx(vm, h) {
                // The mirror knows its exact method — dispatch it
                // instead of re-resolving by name+arity (wrong overload).
                Some(m) => vm.invoke_method_mirror(m, target, argv),
                None => {
                    if target != 0 {
                        vm.call_virtual(target, &member, &argv)
                    } else {
                        // Static method — resolve by declaring class.
                        vm.call_static_named(&owner, &member, argv.clone())
                    }
                }
            };
            match res {
                Ok(Some(v)) => {
                    if crate::dbg_flag("OVELO_REFDBG") {
                        eprintln!("[refdbg] invoke {owner}.{member} ok");
                    }
                    Some(Some(v))
                }
                Ok(None) => {
                    if crate::dbg_flag("OVELO_REFDBG") {
                        eprintln!("[refdbg] invoke {owner}.{member} ok(void)");
                    }
                    Some(Some(V::Ref(0)))
                }
                Err(e) => {
                    if crate::dbg_flag("OVELO_REFDBG") {
                        eprintln!("[refdbg] invoke {owner}.{member} ERR {e}");
                    }
                    let t = vm.throw_builtin(
                        "Ljava/lang/reflect/InvocationTargetException;",
                        &format!("{member}: {e}"),
                    );
                    return Err(t);
                }
            }
        }
        "newInstance" if class.contains("Constructor") => {
            // args: [ctor_mirror, argv_array] — run the exact `<init>`
            // `getConstructor` resolved (P_CTOR) when present; otherwise
            // fall back to the slot-count guess.
            let argv: Vec<V> = match args.get(1) {
                Some(V::Ref(a)) => match vm.heap.get(*a) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => vec![],
                },
                _ => vec![],
            };
            let res = match args[0] {
                V::Ref(h) => match pseudo_get(vm, h, P_CTOR) {
                    Some(V::I64(m)) => {
                        // Pad/truncate to the ctor's declared arity.
                        let n = vm.method_params(m as u64).len();
                        let mut av = argv;
                        av.resize(n, V::I32(0));
                        av.truncate(n);
                        vm.instantiate_ctor(&owner, m as u64, &av)
                    }
                    _ => vm.instantiate_with(&owner, &argv),
                },
                _ => vm.instantiate_with(&owner, &argv),
            };
            match res {
                Ok(nh) => Some(Some(V::Ref(nh))),
                Err(e) => {
                    if crate::dbg_flag("OVELO_VIEWDBG") {
                        eprintln!("[viewdbg] newInstance {owner} failed: {e}");
                    }
                    let t = vm.throw_builtin(
                        "Ljava/lang/InstantiationException;",
                        &format!("{owner}: {e}"),
                    );
                    return Err(t);
                }
            }
        }
        _ if class.contains("Field") => {
            let fid = match pseudo_get(vm, h, P_DATA) {
                Some(V::I64(f)) => vm.canon_field(f as crate::GIdx),
                _ => vm
                    .field_index_of(&owner, &member)
                    .map(|g| vm.canon_field(g))
                    .unwrap_or(P_DATA as u64),
            };
            match name {
                "get" | "getInt" | "getLong" | "getBoolean" | "getByte" | "getShort"
                | "getChar" | "getFloat" | "getDouble" => {
                    let obj = args.get(1).copied().unwrap_or(V::Ref(0));
                    let v = match obj {
                        V::Ref(oh) if oh != 0 => match vm.heap.get(oh) {
                            Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                                fields.get(&fid).copied().unwrap_or(V::Ref(0))
                            }
                            _ => V::Ref(0),
                        },
                        // `Field.get(null)` reads a static field — pull the
                        // real stored value first (kotlinx.serialization's
                        // `INSTANCE`/`Companion` probes depend on it),
                        // then fall back to an opaque placeholder for
                        // guest-owned fields we can't decode.
                        _ if name == "get" => {
                            let v = vm
                                .probe_static_named(&owner, &member)
                                .or_else(|| crate::jdk::sfield(vm, &owner, &member, OBJ));
                            if crate::dbg_flag("OVELO_REFDBG") {
                                let hit = v.map(|v| !matches!(v, V::Ref(0))).unwrap_or(false);
                                eprintln!("[refdbg] Field.get(null) {owner}.{member} hit={hit}");
                            }
                            v.unwrap_or_else(|| host(vm, OBJ, HostState::Empty))
                        }
                        _ if name == "getLong" => V::I64(0),
                        _ if name == "getFloat" => V::F32(0.0),
                        _ if name == "getDouble" => V::F64(0.0),
                        _ => V::I32(0),
                    };
                    Some(Some(v))
                }
                "set" | "setInt" | "setLong" | "setBoolean" | "setByte" | "setShort"
                | "setChar" | "setFloat" | "setDouble" => {
                    let obj = args.get(1).copied().unwrap_or(V::Ref(0));
                    let v = args.get(2).copied().unwrap_or(V::Ref(0));
                    if let V::Ref(oh) = obj {
                        if let Some(f) = fields_mut(vm, oh) {
                            f.insert(fid, v);
                        }
                    }
                    Some(None)
                }
                _ => None,
            }
        }
        _ if class.contains("Array") => match name {
            "newInstance" => {
                // `Array.newInstance(componentType, dims…)`: the
                // component may itself be an array (`int[].class` →
                // `int[][]`) or a primitive (`int.class` → `int[]`).
                let comp = match args.first() {
                    Some(V::Ref(h)) => class_desc_of(vm, *h).unwrap_or_else(|| OBJ.to_string()),
                    _ => OBJ.to_string(),
                };
                let dims: Vec<i32> = match args.get(1) {
                    Some(V::Ref(dh)) => match vm.heap.get(*dh) {
                        Some(Obj::Array { data, .. }) => data.iter().map(|v| i32v(*v)).collect(),
                        _ => vec![0],
                    },
                    Some(v) => vec![i32v(*v)],
                    None => vec![0],
                };
                Some(Some(new_array_dims(vm, &comp, &dims)))
            }
            "getLength" => {
                let n = match args.get(1) {
                    Some(V::Ref(a)) => match vm.heap.get(*a) {
                        Some(Obj::Array { data, .. }) => data.len() as i32,
                        _ => 0,
                    },
                    _ => 0,
                };
                Some(Some(V::I32(n)))
            }
            "get" | "getInt" | "getLong" | "getBoolean" | "getByte" | "getShort" | "getChar"
            | "getFloat" | "getDouble" => {
                let v = match (args.get(1), args.get(2)) {
                    (Some(V::Ref(a)), Some(i)) => match vm.heap.get(*a) {
                        Some(Obj::Array { data, .. }) => {
                            data.get(i32v(*i) as usize).copied().unwrap_or(V::Ref(0))
                        }
                        _ => V::Ref(0),
                    },
                    _ => V::Ref(0),
                };
                Some(Some(v))
            }
            "set" | "setInt" | "setLong" | "setBoolean" | "setByte" | "setShort" | "setChar"
            | "setFloat" | "setDouble" => {
                if let (Some(V::Ref(a)), Some(i), Some(v)) = (args.get(1), args.get(2), args.get(3))
                {
                    let (a, i, v) = (*a, i32v(*i) as usize, *v);
                    if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(a) {
                        if i < data.len() {
                            data[i] = v;
                        }
                    }
                }
                Some(None)
            }
            _ => None,
        },
        _ => {
            let _ = (sig, class);
            None
        }
    })
}

/// sun.misc.Unsafe. Offsets below `UNSAFE_TOKEN_BASE` are real dex field
/// ids, so CAS/volatile get-put share storage with normal `iget`/`iput`;
/// unresolvable fields get opaque tokens over `Vm::unsafe_offsets`.
const UNSAFE_TOKEN_BASE: i64 = 1 << 62;

fn unsafe_key(args: &[V]) -> Option<(u32, i64)> {
    match (args.get(1), args.get(2)) {
        (Some(V::Ref(h)), Some(V::I64(off))) => Some((*h, *off)),
        (Some(V::Ref(h)), Some(V::I32(off))) => Some((*h, *off as i64)),
        _ => None,
    }
}

/// Marker descriptor for the object `staticFieldBase` hands out — ops on
/// it read/write the owning class's static table, not instance fields.
const UNSAFE_STATIC_BASE: &str = "Lsun/misc/Unsafe$StaticBase;";

fn unsafe_read(vm: &mut Vm, obj: u32, off: i64) -> V {
    // Array-element ops (arrayBaseOffset=0, scale=1 ⇒ off == index).
    if let Some(Obj::Array { data, .. }) = vm.heap.get(obj) {
        return data.get(off as usize).copied().unwrap_or(V::Ref(0));
    }
    if off < UNSAFE_TOKEN_BASE {
        // Static base: the offset indexes the owning class's statics.
        if let Some(Obj::Host { desc, .. }) = vm.heap.get(obj) {
            if desc == UNSAFE_STATIC_BASE {
                let owner = pseudo_get(vm, obj, P_DATA)
                    .map(|v| match v {
                        V::I64(x) => x as u64,
                        V::I32(x) => x as u64,
                        _ => crate::GX_NONE,
                    })
                    .unwrap_or(crate::GX_NONE);
                return vm.static_read(owner, off as u64).unwrap_or(V::I32(0));
            }
        }
        return match vm.heap.get(obj) {
            Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                fields.get(&(off as u64)).copied().unwrap_or(V::I32(0))
            }
            _ => V::I32(0),
        };
    }
    vm.unsafe_offsets
        .get(&(obj, off))
        .copied()
        .unwrap_or(V::I32(0))
}

fn unsafe_write(vm: &mut Vm, obj: u32, off: i64, v: V) {
    // Array-element ops (arrayBaseOffset=0, scale=1 ⇒ off == index).
    if let Some(Obj::Array { .. }) = vm.heap.get(obj) {
        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(obj) {
            if (off as usize) < data.len() {
                data[off as usize] = v;
            }
        }
        return;
    }
    if off < UNSAFE_TOKEN_BASE {
        if let Some(Obj::Host { desc, .. }) = vm.heap.get(obj) {
            if desc == UNSAFE_STATIC_BASE {
                let owner = pseudo_get(vm, obj, P_DATA)
                    .map(|v| match v {
                        V::I64(x) => x as u64,
                        V::I32(x) => x as u64,
                        _ => crate::GX_NONE,
                    })
                    .unwrap_or(crate::GX_NONE);
                vm.static_write(owner, off as u64, v);
                return;
            }
        }
        if let Some(f) = fields_mut(vm, obj) {
            f.insert(off as u64, v);
        }
        return;
    }
    vm.unsafe_offsets.insert((obj, off), v);
}

/// CAS equality: guest `null` reaches us as `I32(0)` while an unset slot
/// holds `Ref(0)` — both must count as null.
fn cas_eq(_vm: &mut Vm, a: V, b: V) -> bool {
    // CAS semantics are `==` on the slot, not `equals` — two distinct
    // boxed objects with equal payloads must NOT swap successfully
    // (Unsafe.compareAndSwapObject / AtomicReference.compareAndSet).
    let nullish = |v: V| matches!(v, V::I32(0) | V::Ref(0));
    (nullish(a) && nullish(b)) || a == b
}

fn unsafe_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let v = match name {
        "getUnsafe" => Some(host(vm, "Lsun/misc/Unsafe;", HostState::Empty)),
        "objectFieldOffset" | "staticFieldOffset" => {
            // Same field, same offset — resolve the mirror's owner+name to
            // the real field id so reads share storage with `iget`/`iput`.
            // args[0] is the Unsafe receiver; args[1] is the Field mirror.
            let h = match args.get(1) {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            let member = s(vm, h).unwrap_or_default();
            let owner = pseudo_get(vm, h, P_COMPONENT)
                .and_then(|v| sv(vm, v))
                .unwrap_or_default();
            let off = vm
                .field_index_of(&owner, &member)
                .map(|f| vm.canon_field(f) as i64)
                .unwrap_or_else(|| {
                    vm.unsafe_next_offset += 1;
                    UNSAFE_TOKEN_BASE + vm.unsafe_next_offset
                });
            if crate::dbg_flag("OVELO_TRACE_FIELD") {
                eprintln!("[ftrace] {name} {owner}.{member} -> {off:#x}");
            }
            Some(V::I64(off))
        }
        "staticFieldBase" => {
            // Marker object carrying the owning class idx — ops on it
            // read/write that class's static table.
            let h = match args.get(1) {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            let owner = pseudo_get(vm, h, P_COMPONENT)
                .and_then(|v| sv(vm, v))
                .unwrap_or_default();
            let owner_idx = vm.type_index_of(&owner).unwrap_or(crate::GX_NONE);
            let base = V::Ref(vm.alloc_host(UNSAFE_STATIC_BASE, HostState::Empty));
            if let V::Ref(bh) = base {
                pseudo_set(vm, bh, P_DATA, V::I64(owner_idx as i64));
            }
            Some(base)
        }
        "arrayBaseOffset" => Some(V::I32(0)),
        "arrayIndexScale" => Some(V::I32(1)),
        "allocateInstance" => {
            // No constructor runs for Unsafe.allocateInstance.
            let d = class_desc_of(
                vm,
                match args.get(1) {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            match vm.alloc_instance_raw(&d) {
                Ok(h) => Some(V::Ref(h)),
                Err(_) => Some(host(vm, OBJ, HostState::Empty)),
            }
        }
        "getObject" | "getObjectVolatile" | "getAndSetObject" => {
            let (o, k) = unsafe_key(args).unwrap_or((0, 0));
            let old = unsafe_read(vm, o, k);
            if crate::dbg_flag("OVELO_TRACE_FIELD") {
                eprintln!("[ftrace] {name} obj={o} off={k:#x} -> {old:?}");
            }
            let old = if matches!(old, V::I32(0) | V::I64(0)) {
                V::Ref(0)
            } else {
                old
            };
            if name == "getAndSetObject" {
                unsafe_write(vm, o, k, args.get(3).copied().unwrap_or(V::Ref(0)));
            }
            Some(old)
        }
        "putObject" | "putObjectVolatile" => {
            if let Some((o, k)) = unsafe_key(args) {
                unsafe_write(vm, o, k, args.get(3).copied().unwrap_or(V::Ref(0)));
            }
            None
        }
        "compareAndSwapObject" => {
            let (o, k) = unsafe_key(args).unwrap_or((0, 0));
            let (expect, new) = (
                args.get(3).copied().unwrap_or(V::Ref(0)),
                args.get(4).copied().unwrap_or(V::Ref(0)),
            );
            let cur = unsafe_read(vm, o, k);
            Some(V::I32(if cas_eq(vm, cur, expect) {
                unsafe_write(vm, o, k, new);
                1
            } else {
                0
            }))
        }
        "getInt" | "getIntVolatile" | "getAndSetInt" | "getAndAddInt" | "getLong"
        | "getLongVolatile" | "getAndSetLong" | "getAndAddLong" => {
            let wide = name.contains("Long");
            let (o, k) = unsafe_key(args).unwrap_or((0, 0));
            let old = unsafe_read(vm, o, k);
            let old = match (old, wide) {
                (V::I32(0), true) | (V::I64(0), false) => {
                    if wide {
                        V::I64(0)
                    } else {
                        V::I32(0)
                    }
                }
                (other, _) => other,
            };
            match name {
                n if n.starts_with("getAndSet") => {
                    unsafe_write(vm, o, k, args.get(3).copied().unwrap_or(V::I32(0)))
                }
                n if n.starts_with("getAndAdd") => {
                    let add = args.get(3).copied().unwrap_or(V::I32(0));
                    let new = match (old, add) {
                        (V::I64(a), V::I64(b)) => V::I64(a.wrapping_add(b)),
                        (V::I32(a), V::I32(b)) => V::I32(a.wrapping_add(b)),
                        (V::I32(a), V::I64(b)) => V::I64((a as i64).wrapping_add(b)),
                        (V::I64(a), V::I32(b)) => V::I64(a.wrapping_add(b as i64)),
                        _ => old,
                    };
                    unsafe_write(vm, o, k, new);
                }
                _ => {}
            }
            Some(old)
        }
        "putInt" | "putIntVolatile" | "putLong" | "putLongVolatile" => {
            if let Some((o, k)) = unsafe_key(args) {
                unsafe_write(vm, o, k, args.get(3).copied().unwrap_or(V::I32(0)));
            }
            None
        }
        "compareAndSwapInt" | "compareAndSwapLong" => {
            let (o, k) = unsafe_key(args).unwrap_or((0, 0));
            let (expect, new) = (
                args.get(3).copied().unwrap_or(V::I32(0)),
                args.get(4).copied().unwrap_or(V::I32(0)),
            );
            let cur = unsafe_read(vm, o, k);
            // An unset field reads I32(0) while a long CAS expects
            // I64 — coerce the raw read to the operand width or the
            // CAS never succeeds and the retry loop spins.
            let cur = match (cur, expect) {
                (V::I32(i), V::I64(_)) => V::I64(i as i64),
                (V::I64(i), V::I32(_)) => V::I32(i as i32),
                _ => cur,
            };
            Some(V::I32(if cas_eq(vm, cur, expect) {
                unsafe_write(vm, o, k, new);
                1
            } else {
                0
            }))
        }
        "shouldBeInitialized" => {
            let d = class_desc_of(
                vm,
                match args.get(1) {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let idx = vm.type_index_of(&d);
            Some(V::I32(match idx {
                Some(i) => !vm.class_initialized(i) as i32,
                None => 0,
            }))
        }
        "ensureClassInitialized" => {
            let d = class_desc_of(
                vm,
                match args.get(1) {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            if let Some(i) = vm.type_index_of(&d) {
                vm.ensure_class_init(i)?;
            }
            None
        }
        "throwException" => {
            // Null throwable -> NullPointerException (per Unsafe spec).
            return Err(match args.get(1) {
                Some(V::Ref(h)) if *h != 0 => crate::E::Throw(*h),
                _ => vm.throw_builtin("Ljava/lang/NullPointerException;", "throwException(null)"),
            });
        }
        "park" => {
            // `park(isAbsolute, time)`: isAbsolute → absolute ms epoch,
            // else relative nanos. A bounded wait must NOT unwind —
            // re-running the runnable from the top restarts the guest's
            // wait (e.g. `AbstractFuture.get(t)` computes its deadline
            // inside `get`), so its clock never advances and the
            // TimeoutException never fires: drain queued work if any,
            // then sleep a slice of the wait and return inside the same
            // call so the guest's own deadline check eventually trips.
            let time = i64v(args.get(2).copied().unwrap_or(V::I64(0)));
            if time > 0 {
                let absolute = i64v(args.get(1).copied().unwrap_or(V::I32(0))) != 0;
                let remain_ms = if absolute {
                    time.saturating_sub(now_ms())
                } else {
                    time / 1_000_000
                };
                // Delayed tasks that came due are runnable now — a
                // completing task must reach the waiter before its
                // deadline, or `get(t)` always times out.
                let now = now_ms() as u64;
                let (due, later): (Vec<_>, Vec<_>) = vm
                    .delayed_runnables
                    .drain(..)
                    .partition(|(_, _, t)| *t <= now);
                vm.delayed_runnables = later;
                for (r, on_main, _) in due {
                    vm.pending_runnables.push_back((r, on_main));
                }
                if !vm.pending_runnables.is_empty() {
                    vm.park_drain()?;
                } else {
                    // Sleep at most until the next delayed task is due
                    // so a future completion isn't slept through.
                    let until_due = vm
                        .delayed_runnables
                        .iter()
                        .map(|(_, _, t)| t.saturating_sub(now))
                        .min()
                        .unwrap_or(u64::MAX);
                    let ms = (remain_ms.max(0) as u64).min(until_due).clamp(1, 64);
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                return Ok(Some(None));
            }
            // Parked threads can't resume their Java stack, so run
            // queued work inline (see `park_drain`); past the nesting
            // cap it falls back to the preempt-and-requeue unwind.
            vm.park_drain()?;
            return Ok(Some(None));
        }
        // `unpark` may target a worker that parked out of the live
        // queue — re-arm dropped parkers so they rescan their queues.
        "unpark" => {
            vm.wake_parked();
            None
        }
        "loadFence" | "storeFence" | "fullFence" | "monitorEnter" | "monitorExit" => None,
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// `Array.newInstance` array builder: `comp` is the component
/// descriptor, `dims` the sizes outermost-first. The innermost array's
/// element type is `comp` itself — which may already be `[I` — while
/// outer levels nest one `[` per remaining dimension.
fn new_array_dims(vm: &mut Vm, comp: &str, dims: &[i32]) -> V {
    let n = dims.first().copied().unwrap_or(0).max(0) as usize;
    let elem = if dims.len() <= 1 {
        comp.to_string()
    } else {
        format!("{}{}", "[".repeat(dims.len() - 1), comp)
    };
    let data = if dims.len() <= 1 {
        vec![V::default_for(&elem); n]
    } else {
        (0..n)
            .map(|_| new_array_dims(vm, comp, &dims[1..]))
            .collect()
    };
    arr(vm, &elem, data)
}
