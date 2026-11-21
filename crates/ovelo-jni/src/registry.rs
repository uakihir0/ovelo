//! `RegisterNatives` bookkeeping and JNI signature parsing.
//!
//! `NativeRegistry` binds `(class, name, sig)` to a native entry point; the
//! interpreter calls `resolve` before marshalling a `native` method call.

use std::collections::HashMap;

use crate::err;
use ovelo_error::OveloError;

/// One parsed JNI signature: `(args)ret`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JniSig {
    /// Argument types in order (see `parse_sig` for the alphabet).
    pub args: Vec<String>,
    /// Return type (`V` for void).
    pub ret: String,
}

impl JniSig {
    /// Register-widths the shim must marshal (Z/B/C/S/I/J each 1 slot,
    /// J/D 2 slots wide in the JVM register model — but JNI native args are
    /// always one 64-bit slot each; we report arg count +1 for `this` when
    /// non-static).
    pub fn arg_count(&self) -> usize {
        self.args.len()
    }
}

/// Parse a JNI signature `(types)ret`.
/// Types: `Z B C S I J F D`, `Lpkg/Class;`, `[T` (arrays nest).
pub fn parse_sig(sig: &str) -> Result<JniSig, OveloError> {
    let b = sig.as_bytes();
    if b.is_empty() || b[0] != b'(' {
        return Err(err("jni.sig.malformed").with_detail(sig));
    }
    let mut i = 1;
    let mut args = Vec::new();
    while i < b.len() && b[i] != b')' {
        let (t, next) =
            parse_type(b, i, false).ok_or_else(|| err("jni.sig.malformed").with_detail(sig))?;
        args.push(t);
        i = next;
    }
    if i >= b.len() || b[i] != b')' {
        return Err(err("jni.sig.malformed").with_detail(sig));
    }
    let (ret, end) =
        parse_type(b, i + 1, true).ok_or_else(|| err("jni.sig.malformed").with_detail(sig))?;
    if end != b.len() {
        return Err(err("jni.sig.malformed").with_detail(sig));
    }
    Ok(JniSig { args, ret })
}

/// `allow_v`: `V` (void) is legal only as a return type.
fn parse_type(b: &[u8], i: usize, allow_v: bool) -> Option<(String, usize)> {
    match *b.get(i)? {
        b'V' if allow_v => Some(("V".to_string(), i + 1)),
        c @ (b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D') => {
            Some(((c as char).to_string(), i + 1))
        }
        b'L' => {
            let end = i + 1 + b[i + 1..].iter().position(|&c| c == b';')?;
            if end == i + 1 {
                return None; // `L;` — empty class name
            }
            Some((String::from_utf8_lossy(&b[i..=end]).into_owned(), end + 1))
        }
        b'[' => {
            let (inner, next) = parse_type(b, i + 1, false)?;
            Some((format!("[{inner}"), next))
        }
        _ => None,
    }
}

/// A bound native method.
#[derive(Debug, Clone, Copy)]
pub struct NativeMethod {
    /// Address in the mapped library image.
    pub fn_ptr: u64,
    /// Whether the method is static (no `this` arg).
    pub is_static: bool,
}

/// The `RegisterNatives` table.
#[derive(Default)]
pub struct NativeRegistry {
    /// (class_descriptor, name, sig) → entry point.
    map: HashMap<(String, String, String), NativeMethod>,
}

impl NativeRegistry {
    /// `RegisterNatives(clazz, methods, count)` — bind each entry.
    pub fn register(
        &mut self,
        class: &str,
        name: &str,
        sig: &str,
        fn_ptr: u64,
        is_static: bool,
    ) -> Result<(), OveloError> {
        parse_sig(sig)?; // validate eagerly — bad sigs fail RegisterNatives
        self.map.insert(
            (class.to_string(), name.to_string(), sig.to_string()),
            NativeMethod { fn_ptr, is_static },
        );
        Ok(())
    }

    /// `UnregisterNatives` — drop every binding for `class`.
    pub fn unregister_class(&mut self, class: &str) {
        self.map.retain(|(c, _, _), _| c != class);
    }

    /// Resolve a `native` method call. Err = unbound (the interpreter
    /// surfaces this as a VM error, per the no-silent-fallback rule).
    pub fn resolve(&self, class: &str, name: &str, sig: &str) -> Result<NativeMethod, OveloError> {
        self.map
            .get(&(class.to_string(), name.to_string(), sig.to_string()))
            .copied()
            .ok_or_else(|| err("jni.native.unbound").with_detail(format!("{class}.{name}{sig}")))
    }

    /// Binding count (diagnostics).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
