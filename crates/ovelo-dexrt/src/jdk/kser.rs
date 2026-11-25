//! Host fast paths for hot kotlinx-serialization JSON helper classes.
//! Their algorithms and externally visible state remain the dex object
//! model; only the byte/char scanning loops execute natively.

use super::{i32v, jdynstr, sv};
use crate::value::{Obj, V};
use crate::Vm;
use std::cell::RefCell;
use std::rc::Rc;

const SEQ: &str = "Lkotlinx/serialization/json/internal/ArrayAsSequence;";
const WRITER: &str = "Lkotlinx/serialization/json/internal/JsonToStringWriter;";
const LEXER: &str = "Lkotlinx/serialization/json/internal/ReaderJsonLexer;";

fn this(args: &[V]) -> u32 {
    match args.first() {
        Some(V::Ref(h)) => *h,
        _ => 0,
    }
}

fn field(vm: &Vm, h: u32, owner: &str, name: &str) -> Option<V> {
    vm.get_object_field(h, owner, name)
}

fn seq_buffer(vm: &Vm, h: u32) -> Option<u32> {
    match field(vm, h, SEQ, "buffer") {
        Some(V::Ref(b)) if b != 0 => Some(b),
        _ => None,
    }
}

fn seq_len(vm: &Vm, h: u32) -> i32 {
    field(vm, h, SEQ, "length").map(i32v).unwrap_or(0)
}

fn arr_units(vm: &Vm, h: u32, start: usize, end: usize) -> Vec<u16> {
    match vm.heap.get(h) {
        Some(Obj::Array { data, .. }) => data
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|v| i32v(*v) as u16)
            .collect(),
        _ => Vec::new(),
    }
}

fn set_arr_unit(vm: &mut Vm, h: u32, at: usize, unit: u16) {
    if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(h) {
        if let Some(slot) = data.get_mut(at) {
            *slot = V::I32(unit as i32);
        }
    }
}

fn seq_char(vm: &Vm, h: u32, idx: i32) -> Option<V> {
    let b = seq_buffer(vm, h)?;
    match vm.heap.get(b) {
        Some(Obj::Array { data, .. }) => data.get(idx as usize).copied(),
        _ => None,
    }
}

fn seq_substring(vm: &mut Vm, h: u32, start: i32, end: i32) -> V {
    let len = seq_len(vm, h).max(0);
    let end = end.min(len).max(0) as usize;
    let start = start.max(0) as usize;
    let text = seq_buffer(vm, h)
        .map(|b| String::from_utf16_lossy(&arr_units(vm, b, start, end)))
        .unwrap_or_default();
    jdynstr(vm, &text)
}

fn register_seq(vm: &mut Vm) {
    for (name, sig) in [("charAt", "(I)C"), ("get", "(I)C")] {
        let _ = vm.register_host(
            SEQ,
            name,
            sig,
            Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
                let idx = a.get(1).copied().map(i32v).unwrap_or(0);
                Ok(Some(seq_char(vm, this(a), idx).unwrap_or(V::I32(0))))
            })) as crate::HostCall,
        );
    }
    let _ = vm.register_host(
        SEQ,
        "getBuffer$kotlinx_serialization_json",
        "()[C",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            Ok(Some(field(vm, this(a), SEQ, "buffer").unwrap_or(V::Ref(0))))
        })) as crate::HostCall,
    );
    for (name, sig) in [("getLength", "()I"), ("length", "()I")] {
        let _ = vm.register_host(
            SEQ,
            name,
            sig,
            Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
                Ok(Some(V::I32(seq_len(vm, this(a)))))
            })) as crate::HostCall,
        );
    }
    let _ = vm.register_host(
        SEQ,
        "setLength",
        "(I)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let v = a.get(1).copied().unwrap_or(V::I32(0));
            vm.set_object_field(this(a), SEQ, "length", v);
            Ok(None)
        })) as crate::HostCall,
    );
    for (name, sig) in [
        ("substring", "(II)Ljava/lang/String;"),
        ("subSequence", "(II)Ljava/lang/CharSequence;"),
    ] {
        let _ = vm.register_host(
            SEQ,
            name,
            sig,
            Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
                let start = a.get(1).copied().map(i32v).unwrap_or(0);
                let end = a.get(2).copied().map(i32v).unwrap_or(0);
                Ok(Some(seq_substring(vm, this(a), start, end)))
            })) as crate::HostCall,
        );
    }
    let _ = vm.register_host(
        SEQ,
        "toString",
        "()Ljava/lang/String;",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let h = this(a);
            let end = seq_len(vm, h);
            Ok(Some(seq_substring(vm, h, 0, end)))
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        SEQ,
        "trim",
        "(I)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let h = this(a);
            let want = a.get(1).copied().map(i32v).unwrap_or(0);
            let cap = seq_buffer(vm, h)
                .and_then(|b| match vm.heap.get(b) {
                    Some(Obj::Array { data, .. }) => Some(data.len() as i32),
                    _ => None,
                })
                .unwrap_or(0);
            vm.set_object_field(h, SEQ, "length", V::I32(want.min(cap)));
            Ok(None)
        })) as crate::HostCall,
    );
}

fn writer_size(vm: &Vm, h: u32) -> i32 {
    field(vm, h, WRITER, "size").map(i32v).unwrap_or(0)
}

fn writer_array(vm: &Vm, h: u32) -> Option<u32> {
    match field(vm, h, WRITER, "array") {
        Some(V::Ref(b)) if b != 0 => Some(b),
        _ => None,
    }
}

fn writer_ensure(vm: &mut Vm, h: u32, current: i32, additional: i32) -> i32 {
    let need = current.saturating_add(additional).max(0) as usize;
    let Some(arr) = writer_array(vm, h) else {
        return current;
    };
    let len = match vm.heap.get(arr) {
        Some(Obj::Array { data, .. }) => data.len(),
        _ => 0,
    };
    if len < need {
        let new_len = need.max(current.max(0) as usize * 2).max(16);
        let mut data = vec![V::I32(0); new_len];
        if let Some(Obj::Array { data: old, .. }) = vm.heap.get(arr) {
            data[..old.len()].copy_from_slice(old);
        }
        let nh = vm.heap.alloc(Obj::Array {
            elem: "C".into(),
            data,
        });
        vm.set_object_field(h, WRITER, "array", V::Ref(nh));
    }
    current
}

fn writer_write_units(vm: &mut Vm, h: u32, units: &[u16]) {
    let size = writer_size(vm, h).max(0);
    writer_ensure(vm, h, size, units.len() as i32);
    if let Some(arr) = writer_array(vm, h) {
        for (i, unit) in units.iter().enumerate() {
            set_arr_unit(vm, arr, size as usize + i, *unit);
        }
    }
    vm.set_object_field(
        h,
        WRITER,
        "size",
        V::I32(size.saturating_add(units.len() as i32)),
    );
}

fn writer_write_str(vm: &mut Vm, h: u32, text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    writer_write_units(vm, h, &units);
}

fn escape_json_units(text: &str) -> Vec<u16> {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out.encode_utf16().collect()
}

fn register_writer(vm: &mut Vm) {
    let _ = vm.register_host(
        WRITER,
        "ensureTotalCapacity",
        "(II)I",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let h = this(a);
            let cur = a.get(1).copied().map(i32v).unwrap_or(0);
            let add = a.get(2).copied().map(i32v).unwrap_or(0);
            Ok(Some(V::I32(writer_ensure(vm, h, cur, add))))
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        WRITER,
        "ensureAdditionalCapacity",
        "(I)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let h = this(a);
            let add = a.get(1).copied().map(i32v).unwrap_or(0);
            writer_ensure(vm, h, writer_size(vm, h), add);
            Ok(None)
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        WRITER,
        "write",
        "(Ljava/lang/String;)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            if let Some(text) = a.get(1).copied().and_then(|v| sv(vm, v)) {
                writer_write_str(vm, this(a), &text);
            }
            Ok(None)
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        WRITER,
        "writeChar",
        "(C)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let unit = a.get(1).copied().map(i32v).unwrap_or(0) as u16;
            writer_write_units(vm, this(a), &[unit]);
            Ok(None)
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        WRITER,
        "writeLong",
        "(J)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let n = match a.get(1).copied() {
                Some(V::I64(n)) => n,
                Some(V::I32(n)) => n as i64,
                _ => 0,
            };
            writer_write_str(vm, this(a), &n.to_string());
            Ok(None)
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        WRITER,
        "writeQuoted",
        "(Ljava/lang/String;)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            if let Some(text) = a.get(1).copied().and_then(|v| sv(vm, v)) {
                let units = escape_json_units(&text);
                writer_write_units(vm, this(a), &units);
            }
            Ok(None)
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        WRITER,
        "toString",
        "()Ljava/lang/String;",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let h = this(a);
            let size = writer_size(vm, h).max(0) as usize;
            let text = writer_array(vm, h)
                .map(|arr| String::from_utf16_lossy(&arr_units(vm, arr, 0, size)))
                .unwrap_or_default();
            Ok(Some(jdynstr(vm, &text)))
        })) as crate::HostCall,
    );
}

fn lexer_source(vm: &Vm, h: u32) -> Option<u32> {
    // ReaderJsonLexer.source is the concrete ArrayAsSequence field;
    // AbstractJsonLexer's accessors all bottom out here.
    match field(vm, h, LEXER, "source") {
        Some(V::Ref(s)) if s != 0 => Some(s),
        _ => None,
    }
}

fn lexer_scan_index(vm: &mut Vm, h: u32, start: i32, needle: Option<i32>, whitespace: bool) -> i32 {
    let mut pos = start;
    loop {
        if pos < 0 {
            return pos;
        }
        let Some(source) = lexer_source(vm, h) else {
            return -1;
        };
        let len = seq_len(vm, source);
        if pos >= len {
            // Preserve chunked-input semantics through the real dex
            // prefetch path; it refills/compacts and may answer EOF.
            match vm.call_virtual(h, "prefetchOrEof", &[V::I32(pos)]) {
                Ok(Some(V::I32(next))) if next >= 0 => pos = next,
                _ => return -1,
            }
            continue;
        }
        let ch = seq_char(vm, source, pos).map(i32v).unwrap_or(0);
        let hit = if let Some(n) = needle {
            ch == n
        } else if whitespace {
            !matches!(ch, 32 | 10 | 13 | 9)
        } else {
            false
        };
        if hit {
            // indexOf is a pure query in the dex implementation. The
            // lexer consumes the returned index itself; moving the
            // cursor here corrupts strings that need escape decoding or
            // span a refill boundary.
            return pos;
        }
        pos += 1;
    }
}

fn register_lexer(vm: &mut Vm) {
    let _ = vm.register_host(
        LEXER,
        "indexOf",
        "(CI)I",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let h = this(a);
            let needle = a.get(1).copied().map(i32v).unwrap_or(0);
            let start = a.get(2).copied().map(i32v).unwrap_or(0);
            Ok(Some(V::I32(lexer_scan_index(
                vm,
                h,
                start,
                Some(needle),
                false,
            ))))
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        LEXER,
        "substring",
        "(II)Ljava/lang/String;",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let h = this(a);
            let start = a.get(1).copied().map(i32v).unwrap_or(0);
            let end = a.get(2).copied().map(i32v).unwrap_or(0);
            let out = lexer_source(vm, h)
                .map(|s| seq_substring(vm, s, start, end))
                .unwrap_or(V::Ref(0));
            Ok(Some(out))
        })) as crate::HostCall,
    );
}

pub(crate) fn register(vm: &mut Vm) {
    register_seq(vm);
    register_writer(vm);
    register_lexer(vm);
}
