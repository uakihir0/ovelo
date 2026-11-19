//! `JNI_OnLoad` invocation shape.
//!
//! Loading a `.so` looks up `JNI_OnLoad` in the library's export table and
//! calls it with `(JavaVM*, null)`. The call itself runs on the emulator/
//! trampoline; this module resolves the entry point and models the version
//! handshake — `JNI_VERSION_1_6` is the minimum the runtime accepts.

use std::collections::HashMap;

use crate::err;
use ovelo_error::OveloError;

/// `JNI_VERSION_1_6` — minimum accepted version.
pub const JNI_VERSION_1_6: i32 = 0x0001_0006;

/// Find `JNI_OnLoad` in a library's export map (from ovelo-elf).
/// Returns its address; a missing symbol is not an error (`None`) —
/// `JNI_OnLoad` is optional per the spec.
pub fn find_on_load(exports: &HashMap<String, u64>) -> Option<u64> {
    exports.get("JNI_OnLoad").copied()
}

/// Validate the value `JNI_OnLoad` returned: the requested JNI version.
/// `>= JNI_VERSION_1_6` is required; anything lower is a version error.
pub fn check_version(ret: i64) -> Result<i32, OveloError> {
    let v = ret as i32;
    if v < JNI_VERSION_1_6 {
        return Err(err("jni.onload.version")
            .with_detail(format!("returned 0x{v:08x}, need >= 0x{JNI_VERSION_1_6:08x}")));
    }
    Ok(v)
}
