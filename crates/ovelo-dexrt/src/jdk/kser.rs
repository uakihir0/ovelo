//! Host fast paths for hot kotlinx-serialization JSON helper classes.
//! Their algorithms and externally visible state remain the dex object
//! model; only the byte/char scanning loops execute natively.

use super::{i32v, jdynstr, pseudo_get, pseudo_set, sv};
use crate::value::{Obj, V};
use crate::Vm;
use ovelo_error::Result;
use std::cell::RefCell;
use std::rc::Rc;

const SEQ: &str = "Lkotlinx/serialization/json/internal/ArrayAsSequence;";
const WRITER: &str = "Lkotlinx/serialization/json/internal/JsonToStringWriter;";
const LEXER: &str = "Lkotlinx/serialization/json/internal/ReaderJsonLexer;";
const SLEXER: &str = "Lkotlinx/serialization/json/internal/StringJsonLexer;";
const ALEXER: &str = "Lkotlinx/serialization/json/internal/AbstractJsonLexer;";

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
            // ReaderJsonLexer.indexOf intentionally searches only the
            // currently buffered sequence. consumeKeyString treats -1
            // as the chunk-boundary/escape slow path and drives refills
            // itself; prefetching here would skip that state machine.
            return -1;
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

fn lexer_pos(vm: &Vm, h: u32) -> i32 {
    field(vm, h, ALEXER, "currentPosition")
        .map(i32v)
        .unwrap_or(0)
}

fn set_lexer_pos(vm: &mut Vm, h: u32, pos: i32) {
    vm.set_object_field(h, ALEXER, "currentPosition", V::I32(pos));
}

/// Fast simple-key case for consumeKeyString: an opening quote followed
/// by a quote-terminated, escape-free key inside the current chunk.
/// Anything else restores the entry position and re-enters the dex
/// implementation (which owns chunked input and escape handling).
fn call_int(vm: &mut Vm, h: u32, name: &str, args: &[V]) -> Option<i32> {
    match vm.call_virtual(h, name, args) {
        Ok(Some(v)) => Some(i32v(v)),
        _ => None,
    }
}

fn invoke_lexer_exact(vm: &mut Vm, name: &str, params: &[&str], args: Vec<V>) -> bool {
    let Some(midx) = vm.find_method_by_sig(LEXER, name, params).ok().flatten() else {
        return false;
    };
    vm.invoke_no_host(midx, args, crate::InvokeKind::Virtual)
        .is_ok()
}

/// Full consumeKeyString state machine, with the character scanning loop
/// in Rust and escape/refill primitives delegated to the dex lexer. The
/// dex fast path only scans one chunk; long localized descriptions in
/// F-Droid routinely cross chunks, so falling back for them re-scans the
/// whole string one interpreted character at a time.
fn consume_key_string_fast(vm: &mut Vm, args: &[V]) -> V {
    let h = this(args);
    if !invoke_lexer_exact(vm, "consumeNextToken", &["C"], vec![V::Ref(h), V::I32(34)]) {
        return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
    }
    let start = lexer_pos(vm, h);
    let Some(source) = lexer_source(vm, h) else {
        return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
    };
    let len = seq_len(vm, source);
    let mut closing = -1;
    let mut escaped = false;
    let mut pos = start;
    while pos >= 0 && pos < len {
        match seq_char(vm, source, pos).map(i32v).unwrap_or(0) {
            34 => {
                closing = pos;
                break;
            }
            92 => escaped = true,
            _ => {}
        }
        pos += 1;
    }

    let (string_start, mut current) = if closing >= 0 && !escaped {
        let out = seq_substring(vm, source, start, closing);
        set_lexer_pos(vm, h, closing + 1);
        return out;
    } else if closing >= 0 {
        // An escaped quote may be the first quote in the chunk. The
        // general scanner decides whether it really terminates.
        (start, start)
    } else {
        // No closing quote in this chunk. prefetchOrEof compacts and
        // resets positions; the general scanner continues from the new
        // buffer position just like the dex implementation.
        match call_int(vm, h, "prefetchOrEof", &[V::I32(start)]) {
            Some(next) if next >= 0 => (lexer_pos(vm, h), next),
            _ => return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0)),
        }
    };

    // consumeString(source, string_start, current): scan until the real
    // closing quote, copying chunk prefixes/escaped sections into the
    // lexer's escapedString only when needed.
    let mut last = string_start;
    let mut used_append = closing >= 0 && escaped;
    loop {
        let Some(source) = lexer_source(vm, h) else {
            return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
        };
        let len = seq_len(vm, source);
        if current < 0 || current >= len {
            return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
        }
        let ch = seq_char(vm, source, current).map(i32v).unwrap_or(0);
        if ch == 34 {
            let out = if used_append {
                match vm.call_virtual(h, "decodedString", &[V::I32(last), V::I32(current)]) {
                    Ok(Some(v)) => v,
                    _ => return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0)),
                }
            } else {
                seq_substring(vm, source, last, current)
            };
            set_lexer_pos(vm, h, current + 1);
            return out;
        }
        if ch == 92 {
            used_append = true;
            let Some(after_escape) =
                call_int(vm, h, "appendEscape", &[V::I32(last), V::I32(current)])
            else {
                return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
            };
            let Some(next) = call_int(vm, h, "prefetchOrEof", &[V::I32(after_escape)]) else {
                return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
            };
            if next < 0 {
                return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
            }
            current = next;
            last = current;
        } else {
            current += 1;
            if current >= len {
                used_append = true;
                if vm
                    .call_virtual(h, "appendRange", &[V::I32(last), V::I32(current)])
                    .is_err()
                {
                    return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
                }
                let Some(next) = call_int(vm, h, "prefetchOrEof", &[V::I32(current)]) else {
                    return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
                };
                if next < 0 {
                    return consume_key_string_dex(vm, args).unwrap_or(V::Ref(0));
                }
                current = next;
                last = current;
            }
        }
    }
}

fn consume_key_string_dex(vm: &mut Vm, args: &[V]) -> Option<V> {
    let midx = vm
        .find_method_by_sig(LEXER, "consumeKeyString", &[])
        .ok()
        .flatten()?;
    vm.invoke_no_host(midx, args.to_vec(), crate::InvokeKind::Virtual)
        .ok()
        .flatten()
}

fn register_lexer(vm: &mut Vm) {
    let _ = vm.register_host(
        LEXER,
        "consumeKeyString",
        "()Ljava/lang/String;",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            Ok(Some(consume_key_string_fast(vm, a)))
        })) as crate::HostCall,
    );
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

const P_SLEXER_JAVA_POS: u64 = 0xFFFD_FC71;
const P_SLEXER_BYTE_POS: u64 = 0xFFFD_FC72;

fn is_exact_string_lexer(vm: &Vm, h: u32) -> bool {
    vm.heap
        .class_of(h)
        .and_then(|ci| vm.type_desc_of(ci).ok())
        .is_some_and(|d| d == SLEXER)
}

fn with_string_source<R>(vm: &Vm, h: u32, f: impl FnOnce(&str) -> R) -> Option<R> {
    let V::Ref(sh) = field(vm, h, SLEXER, "source")? else {
        return None;
    };
    match vm.heap.get(sh) {
        Some(Obj::Str(text)) => Some(f(text)),
        _ => None,
    }
}

fn byte_for_java(text: &str, start_byte: usize, start_java: i32, target: i32) -> Option<usize> {
    if target < start_java {
        return None;
    }
    let mut java = start_java;
    for (off, _ch) in text.get(start_byte..)?.char_indices() {
        if java == target {
            return Some(start_byte + off);
        }
        java = java.checked_add(1)?;
        if java > target {
            return None;
        }
    }
    (java == target).then_some(text.len())
}

fn slexer_byte_pos(vm: &Vm, h: u32, text: &str, java: i32) -> Option<usize> {
    let (mj, mb) = match (
        pseudo_get(vm, h, P_SLEXER_JAVA_POS).map(i32v),
        pseudo_get(vm, h, P_SLEXER_BYTE_POS).map(i32v),
    ) {
        (Some(j), Some(b)) if j <= java && b >= 0 => (j, b as usize),
        _ => (0, 0),
    };
    byte_for_java(text, mb, mj, java)
}

fn mark_slexer_pos(vm: &mut Vm, h: u32, java: i32, byte: usize) {
    set_lexer_pos(vm, h, java);
    pseudo_set(vm, h, P_SLEXER_JAVA_POS, V::I32(java));
    pseudo_set(vm, h, P_SLEXER_BYTE_POS, V::I32(byte as i32));
}

fn skip_ws_units(text: &str, mut byte: usize, mut java: i32) -> (usize, i32) {
    while byte < text.len() && matches!(text.as_bytes()[byte], b' ' | b'\n' | b'\r' | b'\t') {
        byte += 1;
        java += 1;
    }
    (byte, java)
}

struct ScannedString {
    end_java: i32,
    end_byte: usize,
    slice_end: usize,
    units: Vec<u16>,
    escaped: bool,
}

fn scan_json_string(text: &str, start_byte: usize, start_java: i32) -> Option<ScannedString> {
    let mut chars = text.get(start_byte..)?.char_indices();
    let mut java = start_java;
    let mut units = Vec::new();
    let mut escaped = false;
    let mut segment_start = 0usize;
    while let Some((off, ch)) = chars.next() {
        match ch {
            '"' => {
                if escaped {
                    units.extend(text[start_byte + segment_start..start_byte + off].encode_utf16());
                }
                return Some(ScannedString {
                    end_java: java + 1,
                    end_byte: start_byte + off + 1,
                    slice_end: start_byte + off,
                    units,
                    escaped,
                });
            }
            '\\' => {
                escaped = true;
                units.extend(text[start_byte + segment_start..start_byte + off].encode_utf16());
                let (eoff, esc) = chars.next()?;
                let mut used = 1 + esc.len_utf16() as i32;
                let mut next_byte = start_byte + eoff + esc.len_utf8();
                match esc {
                    '"' => units.push(34),
                    '\\' => units.push(92),
                    '/' => units.push(47),
                    'b' => units.push(8),
                    'f' => units.push(12),
                    'n' => units.push(10),
                    'r' => units.push(13),
                    't' => units.push(9),
                    'u' => {
                        let mut value = 0u16;
                        for _ in 0..4 {
                            let (doff, digit) = chars.next()?;
                            used += digit.len_utf16() as i32;
                            next_byte = start_byte + doff + digit.len_utf8();
                            value = (value << 4) | digit.to_digit(16)? as u16;
                        }
                        units.push(value);
                    }
                    _ => return None,
                }
                java += used;
                segment_start = next_byte - start_byte;
            }
            // Runtime strings index Unicode scalar values (see String.length/charAt),
            // not UTF-16 units.
            _ => java += 1,
        }
    }
    None
}

fn invoke_string_lexer_dex_rt(
    vm: &mut Vm,
    h: u32,
    name: &str,
    params: &[&str],
    extra: &[V],
) -> crate::Rt<Option<V>> {
    let Some(ci) = vm.heap.class_of(h) else {
        return Ok(None);
    };
    let Some(midx) = vm.find_method_on_chain_sig(ci, name, params).ok().flatten() else {
        return Ok(None);
    };
    let mut args = Vec::with_capacity(extra.len() + 1);
    args.push(V::Ref(h));
    args.extend_from_slice(extra);
    vm.invoke_no_host(midx, args, crate::InvokeKind::Virtual)
}

fn string_lexer_dex_or_throw(
    vm: &mut Vm,
    h: u32,
    name: &str,
    params: &[&str],
    extra: &[V],
) -> Result<Option<V>> {
    match invoke_string_lexer_dex_rt(vm, h, name, params, extra) {
        Ok(out) => Ok(out),
        Err(crate::E::Throw(t)) => {
            vm.host_throw = Some(t);
            // The invoke dispatcher turns `host_throw` back into E::Throw;
            // this placeholder is never delivered to guest code.
            Ok(Some(V::Ref(0)))
        }
        Err(crate::E::Vm(e)) => Err(e),
    }
}

fn consume_key_string_string_fast(vm: &mut Vm, args: &[V]) -> Option<V> {
    let h = this(args);
    if !is_exact_string_lexer(vm, h) {
        return None;
    }
    let entry = lexer_pos(vm, h);
    let scan = with_string_source(vm, h, |text| {
        let quote_byte = slexer_byte_pos(vm, h, text, entry)?;
        let (quote_byte, quote_java) = skip_ws_units(text, quote_byte, entry);
        if text.as_bytes().get(quote_byte) != Some(&b'"') {
            return None;
        }
        let start_java = quote_java + 1;
        let start_byte = quote_byte + 1;
        scan_json_string(text, start_byte, start_java).map(|s| (start_byte, s))
    })??;
    let (start_byte, scan) = scan;
    let out = if scan.escaped {
        let text = String::from_utf16_lossy(&scan.units);
        jdynstr(vm, &text)
    } else {
        with_string_source(vm, h, |text| text[start_byte..scan.slice_end].to_string())
            .map(|s| jdynstr(vm, &s))
            .unwrap_or(V::Ref(0))
    };
    mark_slexer_pos(vm, h, scan.end_java, scan.end_byte);
    Some(out)
}

fn skip_whitespaces_string_fast(vm: &mut Vm, args: &[V]) -> Option<V> {
    let h = this(args);
    if !is_exact_string_lexer(vm, h) {
        return None;
    }
    let pos = lexer_pos(vm, h);
    if pos < 0 {
        return Some(V::I32(pos));
    }
    let scanned = with_string_source(vm, h, |text| {
        let byte = slexer_byte_pos(vm, h, text, pos)?;
        Some(skip_ws_units(text, byte, pos))
    })??;
    mark_slexer_pos(vm, h, scanned.1, scanned.0);
    Some(V::I32(scanned.1))
}

fn register_string_lexer(vm: &mut Vm) {
    let _ = vm.register_host(
        SLEXER,
        "consumeKeyString",
        "()Ljava/lang/String;",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            if let Some(out) = consume_key_string_string_fast(vm, a) {
                return Ok(Some(out));
            }
            string_lexer_dex_or_throw(vm, this(a), "consumeKeyString", &[], &[])
        })) as crate::HostCall,
    );
    let _ = vm.register_host(
        SLEXER,
        "skipWhitespaces",
        "()I",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            if let Some(out) = skip_whitespaces_string_fast(vm, a) {
                return Ok(Some(out));
            }
            string_lexer_dex_or_throw(vm, this(a), "skipWhitespaces", &[], &[])
        })) as crate::HostCall,
    );
    // StringJsonLexer.getSource is a one-field getter, but the dex body is
    // invoked once for every scanned character in consumeKeyString. Keep the
    // exact field semantics while avoiding two nested virtual invocations.
    for sig in ["()Ljava/lang/String;", "()Ljava/lang/CharSequence;"] {
        let _ = vm.register_host(
            SLEXER,
            "getSource",
            sig,
            Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
                Ok(Some(
                    field(vm, this(a), SLEXER, "source").unwrap_or(V::Ref(0)),
                ))
            })) as crate::HostCall,
        );
    }
}

pub(crate) fn register(vm: &mut Vm) {
    register_seq(vm);
    register_writer(vm);
    register_lexer(vm);
    register_string_lexer(vm);
}
