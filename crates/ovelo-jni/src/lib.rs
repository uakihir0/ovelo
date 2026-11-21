//! ovelo-jni — the JNI bridge between the DEX runtime and native libraries.
//!
//! Native code is a mapped image (ovelo-elf), not `dlopen`ed: this crate is
//! the glue the interpreter and the emulator both call — the `JNIEnv`
//! function-table model (`env`), the local/global reference table
//! (`handles`), `RegisterNatives` bookkeeping plus JNI signature parsing
//! (`registry`), and the `JNI_OnLoad` handshake (`onload`).

pub mod env;
pub mod handles;
pub mod onload;
pub mod registry;

pub use env::{EnvTable, JClass, JFieldId, JMethodId, JObject, JString, JThrowable, Slot};
pub use handles::{Handles, NULL};
pub use onload::{check_version, find_on_load, JNI_VERSION_1_6};
pub use registry::{parse_sig, JniSig, NativeMethod, NativeRegistry};

use ovelo_error::{ErrorCategory, OveloError, Phase};

/// JNI-layer error constructor.
pub(crate) fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::AbiSymbol, Phase::Native, cause)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn sig_parsing() {
        let s = parse_sig("(ILjava/lang/String;[B)J").unwrap();
        assert_eq!(s.args, vec!["I", "Ljava/lang/String;", "[B"]);
        assert_eq!(s.ret, "J");
        assert_eq!(s.arg_count(), 3);

        let s = parse_sig("()V").unwrap();
        assert!(s.args.is_empty());
        assert_eq!(s.ret, "V");

        let s = parse_sig("([[I)Ljava/util/List;").unwrap();
        assert_eq!(s.args, vec!["[[I"]);
        assert_eq!(s.ret, "Ljava/util/List;");

        assert!(parse_sig("I)V").is_err());
        assert!(parse_sig("(Q)V").is_err());
        assert!(parse_sig("(I)Vx").is_err());
    }

    #[test]
    fn registry_bind_and_resolve() {
        let mut r = NativeRegistry::default();
        r.register("LFoo;", "n", "(I)I", 0x4000, false).unwrap();
        assert!(r.register("LFoo;", "bad", "(X)I", 0x1, false).is_err());
        let m = r.resolve("LFoo;", "n", "(I)I").unwrap();
        assert_eq!(m.fn_ptr, 0x4000);
        assert!(!m.is_static);
        assert_eq!(
            r.resolve("LFoo;", "n", "()V").unwrap_err().cause(),
            "jni.native.unbound"
        );
        r.unregister_class("LFoo;");
        assert!(r.is_empty());
    }

    #[test]
    fn handle_frames_and_globals() {
        let mut h = Handles::new();
        let a = h.new_local(10);
        h.push_frame();
        let b = h.new_local(20);
        let g = h.new_global(b);
        let kept = h.pop_frame(b); // b survives as new local in parent frame
        assert_ne!(kept, NULL);
        assert_eq!(h.get(kept), 20);
        assert_eq!(h.get(g), 20);
        h.delete_global(g);
        let dup = h.new_local(10);
        assert!(h.same(a, dup));
        h.delete_local(kept);
    }

    #[test]
    fn onload_lookup_and_version() {
        let mut exports = HashMap::new();
        exports.insert("JNI_OnLoad".to_string(), 0x8000u64);
        assert_eq!(find_on_load(&exports), Some(0x8000));
        assert_eq!(find_on_load(&HashMap::new()), None);
        assert_eq!(check_version(0x0001_0006).unwrap(), JNI_VERSION_1_6);
        assert_eq!(
            check_version(0x0001_0004).unwrap_err().cause(),
            "jni.onload.version"
        );
    }

    #[test]
    fn env_table_slots() {
        let mut t = EnvTable::new();
        t.set(Slot::FindClass, 0x1234);
        t.set(Slot::RegisterNatives, 0x5678);
        assert_eq!(t.get(Slot::FindClass), 0x1234);
        assert_eq!(t.get(Slot::GetVersion), 0);
        assert_eq!(Slot::GetVersion.index(), 4);
        assert_eq!(Slot::RegisterNatives.index(), 215);
    }
}
