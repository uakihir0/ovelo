//! The `JNIEnv` vtable model and opaque JNI handle types.
//!
//! A real `JNIEnv*` is a pointer to a `JNINativeInterface` function table.
//! Our bridge models the table densely: `EnvTable` maps spec slot indices to
//! host entry points, and the subset we implement lives behind `Slot`.
//! Handles are indices into `Handles`, never raw pointers.

/// Opaque JNI handles (all indices into the handle table).
pub type JObject = u32;
/// A `jclass` handle.
pub type JClass = u32;
/// A `jstring` handle.
pub type JString = u32;
/// A `jmethodID`.
pub type JMethodId = u32;
/// A `jfieldID`.
pub type JFieldId = u32;
/// A `jthrowable` handle.
pub type JThrowable = u32;

/// `JNINativeInterface` slot index in the spec ordering.
///
/// Indices match the platform's function table (slots 0..4 are the
/// reserved/null entries, so `GetVersion` is index 4). Only the subset the
/// bridge implements is named; unknown indices stay raw `usize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// `GetVersion` — index 4.
    GetVersion = 4,
    /// `DefineClass` — index 5.
    DefineClass = 5,
    /// `FindClass` — index 6.
    FindClass = 6,
    /// `ToReflectedMethod` — index 9.
    ToReflectedMethod = 9,
    /// `GetSuperclass` — index 10.
    GetSuperclass = 10,
    /// `IsAssignableFrom` — index 11.
    IsAssignableFrom = 11,
    /// `Throw` — index 13.
    Throw = 13,
    /// `ThrowNew` — index 14.
    ThrowNew = 14,
    /// `ExceptionOccurred` — index 15.
    ExceptionOccurred = 15,
    /// `ExceptionClear` — index 17.
    ExceptionClear = 17,
    /// `PushLocalFrame` — index 19.
    PushLocalFrame = 19,
    /// `PopLocalFrame` — index 20.
    PopLocalFrame = 20,
    /// `NewGlobalRef` — index 21.
    NewGlobalRef = 21,
    /// `DeleteGlobalRef` — index 22.
    DeleteGlobalRef = 22,
    /// `DeleteLocalRef` — index 23.
    DeleteLocalRef = 23,
    /// `IsSameObject` — index 24.
    IsSameObject = 24,
    /// `NewLocalRef` — index 25.
    NewLocalRef = 25,
    /// `NewObject` — index 28.
    NewObject = 28,
    /// `GetObjectClass` — index 31.
    GetObjectClass = 31,
    /// `IsInstanceOf` — index 32.
    IsInstanceOf = 32,
    /// `GetMethodID` — index 33.
    GetMethodID = 33,
    /// `CallObjectMethod` — index 34.
    CallObjectMethod = 34,
    /// `CallBooleanMethod` — index 37.
    CallBooleanMethod = 37,
    /// `CallIntMethod` — index 49.
    CallIntMethod = 49,
    /// `CallLongMethod` — index 52.
    CallLongMethod = 52,
    /// `CallVoidMethod` — index 61.
    CallVoidMethod = 61,
    /// `CallStaticObjectMethod` — index 114.
    CallStaticObjectMethod = 114,
    /// `CallStaticIntMethod` — index 129.
    CallStaticIntMethod = 129,
    /// `CallStaticVoidMethod` — index 141.
    CallStaticVoidMethod = 141,
    /// `NewStringUTF` — index 167.
    NewStringUTF = 167,
    /// `GetStringUTFChars` — index 169.
    GetStringUTFChars = 169,
    /// `ReleaseStringUTFChars` — index 170.
    ReleaseStringUTFChars = 170,
    /// `GetArrayLength` — index 171.
    GetArrayLength = 171,
    /// `RegisterNatives` — index 215.
    RegisterNatives = 215,
    /// `UnregisterNatives` — index 216.
    UnregisterNatives = 216,
    /// `GetJavaVM` — index 219.
    GetJavaVM = 219,
}

impl Slot {
    /// The raw table index.
    pub fn index(self) -> usize {
        self as usize
    }
}

/// Host entry points live behind this table: slot index → native function
/// address in the host shim image.
pub struct EnvTable {
    /// `slots[i]` = host address for interface slot `i`, or 0.
    pub slots: Vec<u64>,
}

impl EnvTable {
    /// A table of the given size (at least `Slot::GetJavaVM + 1`).
    pub fn new() -> Self {
        Self {
            slots: vec![0; 256],
        }
    }

    /// Bind `slot` to a host entry point.
    pub fn set(&mut self, slot: Slot, addr: u64) {
        self.slots[slot.index()] = addr;
    }

    /// Lookup by index.
    pub fn get(&self, slot: Slot) -> u64 {
        self.slots[slot.index()]
    }
}

impl Default for EnvTable {
    fn default() -> Self {
        Self::new()
    }
}
