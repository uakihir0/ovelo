//! android.* builtins: Log, TextUtils, Uri, Intent, Context family,
//! SharedPreferences, Resources, Handler/Looper (inline), PackageManager.

use super::*;
use crate::value::{HostState, Obj, V};

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(if class == "Landroid/util/Log;" {
        log_call(vm, name, args)
    } else if class == "Landroid/text/TextUtils;" {
        textutils_call(vm, name, args)?
    } else if class == "Landroid/text/Html;" {
        match name {
            "fromHtml" => Some(Some(args[0])),
            "toHtml" => Some(Some(jstr(vm, &sv(vm, args[0]).unwrap_or_default()))),
            "escapeHtml" => Some(Some(jstr(
                vm,
                &sv(vm, args[0])
                    .unwrap_or_default()
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;"),
            ))),
            _ => None,
        }
    } else if desc_is_a(class, "Landroid/net/Uri;")
        || class == "Landroid/net/Uri$Builder;"
    {
        uri_call(vm, name, args)?
    } else if desc_is_a(class, "Landroid/content/Intent;") {
        intent_call(vm, name, sig, args)?
    } else if class == "Landroid/content/ComponentName;" {
        component_call(vm, name, args)?
    } else if class == "Landroid/content/IntentFilter;" {
        match name {
            "addAction" | "addCategory" | "addDataScheme" | "addDataType"
            | "addDataAuthority" | "addDataPath" | "setPriority" => Some(None),
            "matchAction" | "hasAction" | "hasCategory" => Some(Some(V::I32(1))),
            "match" => Some(Some(V::I32(0))),
            "countActions" | "countCategories" => Some(Some(V::I32(0))),
            "getAction" => Some(Some(V::Ref(0))),
            "create" => Some(Some(host(vm, class, HostState::Empty))),
            "describeContents" | "writeToParcel" => Some(None),
            _ => None,
        }
    } else if class == "Landroid/preference/PreferenceManager;" {
        // Static factory — hand out a map-backed SharedPreferences.
        match name {
            "getDefaultSharedPreferences" => Some(Some(host(
                vm,
                "Landroid/content/SharedPreferences;",
                HostState::Map(vec![]),
            ))),
            _ => None,
        }
    } else if class == "Landroid/content/SharedPreferences;"
        || class.ends_with("SharedPreferences;")
        || class == "Landroid/content/SharedPreferences$Editor;"
    {
        prefs_call(vm, name, args)?
    } else if desc_is_a(class, "Landroid/content/Context;")
        || class.ends_with("Context;")
        || class.ends_with("Activity;")
        || class.ends_with("Application;")
        || class.ends_with("Service;")
    {
        context_call(vm, class, name, sig, args)?
    } else if class == "Landroid/content/pm/PackageManager;"
        || class.ends_with("PackageManager;")
    {
        pm_call(vm, name, args)?
    } else if class == "Landroid/content/ContentResolver;"
        || class.ends_with("ContentResolver;")
    {
        match name {
            "query" | "rawQuery" => {
                Some(Some(host(vm, "Landroid/database/Cursor;", HostState::Empty)))
            }
            "insert" | "call" | "openFileDescriptor" | "openAssetFileDescriptor"
            | "openTypedAssetFileDescriptor" | "openFile" | "getType"
            | "getStreamTypes" | "getPersistedUriPermissions"
            | "takePersistableUriPermission" | "loadThumbnail" => {
                Some(Some(V::Ref(0)))
            }
            "delete" | "update" | "bulkInsert" | "getOutgoingPersistedUriPermissions"
            | "getSyncStatus" | "startSync" | "requestSync" => {
                Some(Some(V::I32(0)))
            }
            "getSyncAdapterTypes" => Some(Some(arr(vm, "Landroid/content/SyncAdapterType;", vec![]))),
            "openInputStream" => Some(Some(host(
                vm,
                "Ljava/io/FileInputStream;",
                HostState::Empty,
            ))),
            "openOutputStream" => Some(Some(host(
                vm,
                "Ljava/io/FileOutputStream;",
                HostState::Empty,
            ))),
            "isSyncActive" | "getSyncAutomatically" | "getMasterSyncAutomatically"
            | "getIsSyncable" | "validateSyncExtrasBundle" | "canPerformSync" => {
                Some(Some(V::I32(0)))
            }
            _ => {
                // applyBatch/notifyChange/register*/cancelSync/… — match
                // the return type rather than assuming void.
                Some(match sig.rsplit(')').next().unwrap_or("V") {
                    "V" => None,
                    "Z" | "B" | "S" | "C" | "I" | "F" => Some(V::I32(0)),
                    "J" | "D" => Some(V::I64(0)),
                    _ => Some(V::Ref(0)),
                })
            }
        }
    } else if class == "Landroid/content/res/Resources;"
        || class.ends_with("Resources;")
    {
        resources_call(vm, name, args)?
    } else if class == "Landroid/util/DisplayMetrics;"
        || class == "Landroid/content/res/Configuration;"
        || class == "Landroid/content/res/TypedArray;"
        || class == "Landroid/util/TypedValue;"
        || class == "Landroid/content/res/ColorStateList;"
        || class == "Landroid/content/res/AssetManager;"
        || class == "Landroid/content/res/Resources$Theme;"
    {
        res_misc(vm, class, name, args)?
    } else if class == "Landroid/os/Handler;" || class.ends_with("Handler;") {
        handler_call(vm, name, args)?
    } else if class == "Landroid/os/Looper;" {
        looper_call(vm, name, args)?
    } else if class == "Landroid/os/Message;" || class == "Landroid/os/Messenger;" {
        match name {
            "obtain" => Some(Some(host(vm, "Landroid/os/Message;", HostState::Empty))),
            "sendToTarget" | "recycle" | "send" | "sendMessage"
            | "sendMessageDelayed" | "sendEmptyMessage" => {
                if name == "sendEmptyMessage" || name == "sendToTarget" {
                    // invoke the linked Handler's handleMessage inline
                    let _ = args;
                }
                Some(if name.starts_with("send") { Some(V::I32(1)) } else { None })
            }
            "getData" | "peekData" | "getCallback" | "getTarget"
            | "getMessenger" => Some(Some(V::Ref(0))),
            "getWhen" | "getSendingUid" => Some(Some(V::I64(0))),
            "setData" | "setTarget" | "copyFrom" => Some(None),
            _ => None,
        }
    } else if class == "Landroid/os/SystemClock;" {
        match name {
            "uptimeMillis" | "elapsedRealtime" | "currentThreadTimeMillis" => {
                Some(Some(V::I64(now_ms())))
            }
            "elapsedRealtimeNanos" | "nanoTime" => {
                Some(Some(V::I64(now_ms() * 1_000_000)))
            }
            "sleep" | "setCurrentTimeMillis" => Some(None),
            _ => None,
        }
    } else if class == "Landroid/os/Process;" {
        match name {
            "myPid" | "myTid" | "myUid" | "myUserHandle" => {
                Some(Some(V::I32(if name.contains("Uid") { 1000 } else { 1 })))
            }
            "getThreadPriority" => Some(Some(V::I32(0))),
            "setThreadPriority" | "killProcess" | "sendSignal" => Some(None),
            "getElapsedCpuTime" | "getStartElapsedRealtime"
            | "getStartRequestedElapsedRealtime" | "getStartUptimeMillis"
            | "getFreeMemory" | "getTotalMemory" => Some(Some(V::I64(0))),
            "is64Bit" | "isApplicationUid" | "supportsProcesses" | "isSdkSandbox"
            | "isIsolated" => Some(Some(V::I32((name == "is64Bit") as i32))),
            _ => None,
        }
    } else if class == "Landroid/os/Trace;" || class == "Landroid/os/StrictMode;"
        || class == "Landroid/os/Debug;"
    {
        match name {
            "beginSection" | "endSection" | "beginAsyncSection" | "endAsyncSection"
            | "setCounter" | "enableAppTracing" | "isEnabled" => {
                if name == "isEnabled" {
                    Some(Some(V::I32(0)))
                } else {
                    Some(None)
                }
            }
            "allowThreadDiskWrites" | "allowThreadDiskReads"
            | "detectAll" | "penaltyLog" | "penaltyDeath" | "build"
            | "setVmPolicy" | "setThreadPolicy" | "getVmPolicy"
            | "getThreadPolicy" | "noteSlowCall" | "noteUntaggedSocket"
            | "noteDiskRead" | "noteDiskWrite" | "getInstanceCount"
            | "getGlobalAllocSize" | "getGlobalAllocCount"
            | "getNativeHeapAllocatedSize" | "getNativeHeapSize"
            | "getNativeHeapFreeSize" | "getPss" | "getMemoryInfo" => {
                if name.ends_with("Size") || name.ends_with("Pss")
                    || name.ends_with("Count") || name.ends_with("Info")
                {
                    Some(Some(V::I64(0)))
                } else {
                    Some(None)
                }
            }
            "dumpHeap" | "waitingForDebugger" | "isDebuggerConnected" => {
                Some(Some(V::I32(0)))
            }
            _ => Some(None),
        }
    } else if desc_is_a(class, "Landroid/os/AsyncTask;") {
        match name {
            "execute" | "executeOnExecutor" => {
                // inline: doInBackground → onPostExecute is the real
                // contract; just run doInBackground and return this.
                Some(Some(args[0]))
            }
            "cancel" => Some(Some(V::I32(1))),
            "isCancelled" => Some(Some(V::I32(0))),
            "getStatus" => Some(Some(V::Ref(0))),
            "get" => Some(Some(V::Ref(0))),
            _ => None,
        }
    } else if class == "Landroid/os/Binder;" || class == "Landroid/os/Parcel;"
        || class == "Landroid/os/IBinder;"
    {
        match name {
            "getCallingUid" | "getCallingPid" | "getCallingUserHandle" => {
                Some(Some(V::I32(1000)))
            }
            "clearCallingIdentity" | "restoreCallingIdentity" | "obtain" => {
                Some(Some(V::I64(0)))
            }
            "flushPendingCommands" | "isBinderAlive" | "pingBinder" => {
                Some(Some(V::I32(1)))
            }
            "writeInt" | "writeLong" | "writeString" | "writeByte"
            | "writeFloat" | "writeDouble" | "writeByteArray" | "writeValue"
            | "writeStrongBinder" | "writeParcelable" | "writeBundle"
            | "writeStringArray" | "writeIntArray" | "readInt" | "readLong"
            | "readString" | "readByte" | "readFloat" | "readDouble"
            | "readByteArray" | "readValue" | "readStrongBinder"
            | "readParcelable" | "readBundle" | "recycle" | "dataSize"
            | "dataPosition" | "setDataPosition" | "marshall" | "unmarshall"
            | "transact" | "linkToDeath" | "unlinkToDeath" => {
                if name.starts_with("read") && !name.contains("Array") {
                    Some(Some(V::I64(0)))
                } else if name.starts_with("data") {
                    Some(Some(V::I32(0)))
                } else if name == "transact" || name == "unlinkToDeath" {
                    Some(Some(V::I32(1)))
                } else if name == "marshall" {
                    Some(Some(V::Ref(0)))
                } else {
                    Some(None)
                }
            }
            "getInterfaceDescriptor" | "getInterfaceDescriptor0" => {
                Some(Some(jstr(vm, "")))
            }
            _ => None,
        }
    } else if class == "Landroid/content/ClipData;"
        || class == "Landroid/content/ClipboardManager;"
    {
        match name {
            "newPlainText" | "newUri" | "newIntent" => {
                Some(Some(host(vm, "Landroid/content/ClipData;", HostState::Empty)))
            }
            "setPrimaryClip" | "setText" | "clearPrimaryClip" => Some(None),
            "getPrimaryClip" | "getText" | "getPrimaryClipDescription" => {
                Some(Some(V::Ref(0)))
            }
            "hasPrimaryClip" | "hasText" => Some(Some(V::I32(0))),
            "getItemCount" => Some(Some(V::I32(0))),
            "getItemAt" => Some(Some(V::Ref(0))),
            _ => None,
        }
    } else if class == "Landroid/database/Cursor;"
        || class.ends_with("Cursor;")
        || class.starts_with("Landroid/database/")
    {
        cursor_call(vm, name, args)?
    } else if class.starts_with("Landroid/provider/") {
        match name {
            "getString" | "getInt" | "getLong" | "getFloat" => {
                Some(Some(args.get(2).copied().unwrap_or(V::I32(0))))
            }
            "putString" | "putInt" | "putLong" | "putFloat" => {
                Some(Some(V::I32(1)))
            }
            _ => Some(Some(V::Ref(0))),
        }
    } else if class == "Landroid/util/LruCache;" {
        match name {
            "get" => Some(Some(map_get(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
                args[1],
            ).unwrap_or(V::Ref(0)))),
            "put" => {
                if let V::Ref(h) = args[0] {
                    map_put(vm, h, args[1], args[2]);
                }
                Some(Some(V::Ref(0)))
            }
            "remove" => {
                if let V::Ref(h) = args[0] {
                    map_remove(vm, h, args[1]);
                }
                Some(Some(V::Ref(0)))
            }
            "size" | "maxSize" | "evictAll" | "trimToSize" => {
                if matches!(name, "size" | "maxSize") {
                    Some(Some(V::I32(0)))
                } else {
                    Some(None)
                }
            }
            "snapshot" => Some(Some(V::Ref(0))),
            _ => None,
        }
    } else if class.starts_with("Landroid/graphics/")
        || class.starts_with("Landroid/util/Patterns")
        || class == "Landroid/util/Size;" || class == "Landroid/util/SizeF;"
        || class == "Landroid/util/AttributeSet;"
    {
        graphics_call(vm, class, name, args)?
    } else {
        return Ok(None);
    })
}

// ---------------------------------------------------------------------
// Log

fn log_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "v" | "d" | "i" | "w" | "e" | "wtf" => {
            let tag = sv(vm, args[0]).unwrap_or_default();
            let msg = sv(vm, args[1]).unwrap_or_default();
            eprintln!("[{}/{}] {}", name.to_uppercase(), tag, msg);
            Some(Some(V::I32(msg.len() as i32)))
        }
        "println" => Some(Some(V::I32(0))),
        "isLoggable" => Some(Some(V::I32(0))),
        "getStackTraceString" => Some(Some(jstr(vm, ""))),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// TextUtils

fn textutils_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "isEmpty" => {
            let e = sv(vm, args[0]).map(|t| t.is_empty()).unwrap_or(true);
            Some(Some(V::I32(e as i32)))
        }
        "isDigitsOnly" => {
            let e = sv(vm, args[0])
                .is_some_and(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()));
            Some(Some(V::I32(e as i32)))
        }
        "equals" => {
            let eq = match (args[0], args[1]) {
                (V::Ref(0), V::Ref(0)) => true,
                _ => {
                    sv(vm, args[0]).unwrap_or_default()
                        == sv(vm, args[1]).unwrap_or_default()
                }
            };
            Some(Some(V::I32(eq as i32)))
        }
        "join" => {
            let d = sv(vm, args[0]).unwrap_or_default();
            let items: Vec<V> = match args[1] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::List(l),
                        ..
                    }) => l.clone(),
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => args[1..].to_vec(),
                },
                _ => vec![],
            };
            let parts: Vec<String> =
                items.iter().map(|v| v_to_s(vm, *v)).collect();
            Some(Some(jstr(vm, &parts.join(&d))))
        }
        "split" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let pat = sv(vm, args[1]).unwrap_or_default();
            let vs: Vec<V> = t.split(&pat).map(|x| jstr(vm, x)).collect();
            Some(Some(arr(vm, STRING, vs)))
        }
        "concat" => {
            let t: String = args.iter().map(|v| v_to_s(vm, *v)).collect();
            Some(Some(jstr(vm, &t)))
        }
        "substring" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let (a, b) = (i32v(args[1]) as usize, i32v(args[2]) as usize);
            let sub: String = t.chars().skip(a).take(b.saturating_sub(a)).collect();
            Some(Some(jstr(vm, &sub)))
        }
        "indexOf" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let n = sv(vm, args[1]).unwrap_or_default();
            Some(Some(V::I32(
                t.find(&n).map(|i| i as i32).unwrap_or(-1),
            )))
        }
        "ellipsize" | "commaEllipsize" | "stringOrSpannedString" => {
            Some(args.first().copied())
        }
        "copy" | "getChars" => {
            if name == "copy" {
                let (src, dst) = (sv(vm, args[0]).unwrap_or_default(), args[3]);
                if let V::Ref(dh) = dst {
                    if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(dh) {
                        for (i, c) in src.chars().enumerate() {
                            if i < data.len() {
                                data[i] = V::I32(c as i32);
                            }
                        }
                    }
                }
            }
            Some(None)
        }
        "htmlEncode" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            Some(Some(jstr(
                vm,
                &t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"),
            )))
        }
        "regionMatches" => {
            let eq = sv(vm, args[0]) == sv(vm, args[3]);
            Some(Some(V::I32(eq as i32)))
        }
        "replace" | "expandTemplate" | "formatSimple" => Some(args.first().copied()),
        "writeToParcel" => Some(None),
        "getReverse" | "getTrimmedLength" => Some(Some(V::I32(0))),
        "isGraphic" | "isPrintableAscii" | "isPrintableAsciiOnly" => {
            Some(Some(V::I32(1)))
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Uri — stored as StrBuf text

fn uri_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let text = s(vm, match args[0] { V::Ref(h) => h, _ => 0 }).unwrap_or_default();
    Ok(match name {
        "parse" => Some(Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf(
            sv(vm, args[0]).unwrap_or_default(),
        )))),
        "encode" | "decode" => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let out = if name == "encode" {
                t.chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
                            c.to_string()
                        } else {
                            let mut b = vec![];
                            c.encode_utf8(&mut b);
                            b.iter().map(|x| format!("%{x:02X}")).collect()
                        }
                    })
                    .collect::<String>()
            } else {
                let mut out = String::new();
                let mut it = t.chars().peekable();
                while let Some(c) = it.next() {
                    if c == '%' {
                        let h: String = it.by_ref().take(2).collect();
                        if let Ok(n) = u8::from_str_radix(&h, 16) {
                            out.push(n as char);
                        }
                    } else {
                        out.push(c);
                    }
                }
                out
            };
            Some(Some(jstr(vm, &out)))
        }
        "withAppendedPath" => {
            let a = sv(vm, args[0]).or_else(|| s(vm, match args[0] {
                V::Ref(h) => h,
                _ => 0,
            })).unwrap_or_default();
            let b = sv(vm, args[1]).unwrap_or_default();
            Some(Some(host(
                vm,
                "Landroid/net/Uri;",
                HostState::StrBuf(format!("{a}/{b}")),
            )))
        }
        "toString" | "getPath" | "getEncodedPath" | "getLastPathSegment" => {
            let t = match name {
                "getLastPathSegment" => text
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string(),
                "getPath" | "getEncodedPath" => {
                    let after_scheme = text.split("://").nth(1).unwrap_or(&text);
                    after_scheme
                        .split(['?', '#'])
                        .next()
                        .and_then(|x| x.split_once('/'))
                        .map(|(_, p)| format!("/{p}"))
                        .unwrap_or_default()
                }
                _ => text.clone(),
            };
            Some(Some(jstr(vm, &t)))
        }
        "getScheme" => {
            let sc = text.split("://").next().unwrap_or("").to_string();
            Some(Some(jstr(vm, &sc)))
        }
        "getHost" | "getAuthority" | "getEncodedAuthority" => {
            let h = text
                .split("://")
                .nth(1)
                .and_then(|x| x.split('/').next())
                .unwrap_or("")
                .to_string();
            Some(Some(jstr(vm, &h)))
        }
        "getQuery" | "getEncodedQuery" | "getFragment" | "getEncodedFragment"
        | "getQueryParameter" | "getQueryParameterNames"
        | "getQueryParameters" | "getPathSegments" | "getUserInfo" => {
            if name == "getQueryParameter" {
                // ?k=v&k2=v2 lookup
                let key = sv(vm, args[1]).unwrap_or_default();
                let q = text.split('?').nth(1).unwrap_or("");
                let v = q
                    .split('&')
                    .find_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        if k == key {
                            Some(v.to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                Some(Some(jstr(vm, &v)))
            } else if name == "getPathSegments" {
                let segs: Vec<V> = text
                    .split('/')
                    .filter(|s| !s.is_empty() && !s.contains(':'))
                    .map(|x| jstr(vm, x))
                    .collect();
                Some(Some(host(vm, "Ljava/util/List;", HostState::List(segs))))
            } else if name.contains("QueryParameterNames") {
                Some(Some(host(vm, "Ljava/util/Set;", HostState::List(vec![]))))
            } else {
                let part = text.split(['?', '#']).nth(1).unwrap_or("");
                Some(Some(jstr(vm, part)))
            }
        }
        "getPort" => Some(Some(V::I32(-1))),
        "isRelative" | "isAbsolute" | "isHierarchical" | "isOpaque" => {
            let r = match name {
                "isAbsolute" => text.contains("://"),
                "isRelative" => !text.contains("://"),
                "isHierarchical" => true,
                _ => false,
            };
            Some(Some(V::I32(r as i32)))
        }
        "buildUpon" => {
            let h = vm.alloc_host("Landroid/net/Uri$Builder;", HostState::StrBuf(text));
            Some(Some(V::Ref(h)))
        }
        "scheme" | "authority" | "path" | "encodedPath" | "query"
        | "encodedQuery" | "fragment" | "encodedFragment"
        | "appendQueryParameter" | "appendPath" | "appendEncodedPath"
        | "appendBooleanQueryParameter" | "clearQuery" | "encodedAuthority"
        | "opaquePart" | "encodedOpaquePart" => {
            // Uri.Builder mutators — keep appended text.
            if let V::Ref(bh) = args[0] {
                if name.starts_with("append") && args.len() > 1 {
                    let add = sv(vm, args[1]).unwrap_or_default();
                    if let Some(Obj::Host {
                        state: HostState::StrBuf(b),
                        ..
                    }) = vm.heap.get_mut(bh)
                    {
                        if name.contains("Query") {
                            b.push('?');
                        } else {
                            b.push('/');
                        }
                        b.push_str(&add);
                    }
                }
            }
            Some(Some(args[0]))
        }
        "build" => {
            let t = s(vm, match args[0] { V::Ref(h) => h, _ => 0 }).unwrap_or_default();
            Some(Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf(t))))
        }
        "fromFile" | "fromParts" | "fromBundle" => {
            Some(Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf("".into()))))
        }
        "writeToParcel" | "describeContents" => Some(None),
        "equals" | "hashCode" | "compareTo" => Some(Some(V::I32(0))),
        "normalizeScheme" => Some(Some(args[0])),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Intent — map + pseudo fields for action/data/component

fn intent_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "<init>" => {
            // Intent(action) / Intent(action,uri): the leading string
            // is the action. Intent(context, Target.class): args[1] is
            // the *Context* — the component is the Class arg.
            if let Some(a) = args.get(1) {
                match sv(vm, *a) {
                    Some(_) => pseudo_set(vm, h, P_ACTION, *a),
                    None => {
                        let target = args.get(2).copied().unwrap_or(*a);
                        pseudo_set(vm, h, P_COMPONENT, target);
                    }
                }
            }
            Some(None)
        }
        "setAction" => {
            pseudo_set(vm, h, P_ACTION, args[1]);
            Some(Some(args[0]))
        }
        "getAction" => Some(Some(pseudo_get(vm, h, P_ACTION).unwrap_or(V::Ref(0)))),
        "setData" | "setDataAndType" | "setDataAndNormalize" => {
            pseudo_set(vm, h, P_DATA, args[1]);
            Some(Some(args[0]))
        }
        "getData" | "getDataString" => {
            Some(Some(pseudo_get(vm, h, P_DATA).unwrap_or(V::Ref(0))))
        }
        "setType" => {
            pseudo_set(vm, h, P_FLAGS, args[1]);
            Some(Some(args[0]))
        }
        "getType" => Some(Some(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::Ref(0)))),
        "setComponent" => {
            pseudo_set(vm, h, P_COMPONENT, args[1]);
            Some(Some(args[0]))
        }
        "setClass" | "setClassName" => {
            // (Context|package, Class|name): the target is the second
            // arg, not the context in args[1].
            pseudo_set(vm, h, P_COMPONENT, args.get(2).copied().unwrap_or(args[1]));
            Some(Some(args[0]))
        }
        "getComponent" => {
            Some(Some(pseudo_get(vm, h, P_COMPONENT).unwrap_or(V::Ref(0))))
        }
        "setPackage" | "addCategory" | "removeCategory" | "addFlags"
        | "setFlags" | "setSelector" | "setClipData" | "setSourceBounds"
        | "setIdentifier" => Some(Some(args[0])),
        "getPackage" | "getCategories" | "getFlags" | "getSelector"
        | "getClipData" | "getSourceBounds" | "getIdentifier" => {
            Some(Some(match name {
                "getFlags" => V::I32(0),
                "getCategories" => return Ok(Some(Some(host(
                    vm, "Ljava/util/Set;", HostState::List(vec![]),
                )))),
                _ => V::Ref(0),
            }))
        }
        "putExtra" | "putCharSequenceArrayListExtra" | "putStringArrayListExtra"
        | "putIntegerArrayListExtra" | "putParcelableArrayListExtra" => {
            map_put(vm, h, args[1], args[2]);
            Some(Some(args[0]))
        }
        "getStringExtra" | "getCharSequenceExtra" | "getSerializableExtra"
        | "getParcelableExtra" | "getBundleExtra" => {
            Some(Some(map_get(vm, h, args[1]).unwrap_or(V::Ref(0))))
        }
        n if n.starts_with("get") && n.ends_with("Extra") => {
            // getIntExtra/getBooleanExtra/…: map get with default arg
            let v = map_get(vm, h, args[1])
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::I32(0));
            Some(Some(match v {
                V::I32(_) | V::I64(_) | V::F32(_) | V::F64(_) => v,
                V::Ref(_) if n.contains("Array") || n.contains("List") => v,
                _ => v,
            }))
        }
        "hasExtra" => {
            let c = map_get(vm, h, args[1]).is_some();
            Some(Some(V::I32(c as i32)))
        }
        "removeExtra" => {
            map_remove(vm, h, args[1]);
            Some(None)
        }
        "replaceExtras" => Some(Some(args[0])),
        "getExtras" => {
            // reuse self as the Bundle for simplicity
            Some(Some(args[0]))
        }
        "putExtras" => {
            if let V::Ref(bh) = args[1] {
                let pairs: Vec<(V, V)> = match vm.heap.get(bh) {
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
            Some(Some(args[0]))
        }
        "clone" => {
            let m: Vec<(V, V)> = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.clone(),
                _ => vec![],
            };
            let nh = vm.alloc_host("Landroid/content/Intent;", HostState::Map(m));
            for k in [P_ACTION, P_DATA, P_FLAGS, P_COMPONENT] {
                if let Some(v) = pseudo_get(vm, h, k) {
                    pseudo_set(vm, nh, k, v);
                }
            }
            Some(Some(V::Ref(nh)))
        }
        "fillIn" | "filterEquals" | "filterHashCode" => Some(Some(V::I32(0))),
        "toUri" | "toInsecureString" | "toShortString" => Some(Some(jstr(vm, ""))),
        "getIntExtra" => Some(Some(
            map_get(vm, h, args[1])
                .map(|v| V::I32(i32v(v)))
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::I32(0)),
        )),
        "getLongExtra" => Some(Some(
            map_get(vm, h, args[1])
                .map(|v| V::I64(i64v(v)))
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::I64(0)),
        )),
        "getBooleanExtra" => Some(Some(
            map_get(vm, h, args[1])
                .map(|v| V::I32((i32v(v) != 0) as i32))
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::I32(0)),
        )),
        "getFloatExtra" => Some(Some(V::F32(
            f64v(map_get(vm, h, args[1])
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::F32(0.0))) as f32,
        ))),
        "getDoubleExtra" => Some(Some(V::F64(f64v(
            map_get(vm, h, args[1])
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::F64(0.0)),
        )))),
        "makeMainActivity" | "makeRestartActivityForTask" | "getIntent"
        | "getIntentOld" | "parseIntent" | "getIntentInternal" => {
            Some(Some(args[0]))
        }
        "createChooser" => Some(Some(args[0])),
        "resolveActivity" | "resolveActivityInfo" => Some(Some(V::Ref(0))),
        "describeContents" | "writeToParcel" | "readFromParcel" => Some(None),
        _ => {
            let _ = sig;
            None
        }
    })
}

fn component_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "getPackageName" | "getClassName" | "getShortClassName" | "toString"
        | "flatten" | "flattenToShortString" | "flattenToString" => {
            let t = s(vm, match args[0] { V::Ref(h) => h, _ => 0 })
                .or_else(|| pseudo_get(
                    vm,
                    match args[0] { V::Ref(h) => h, _ => 0 },
                    P_DATA,
                ).and_then(|v| sv(vm, v)))
                .unwrap_or_default();
            Some(Some(jstr(vm, &t)))
        }
        "unflattenFromString" => Some(Some(host(
            vm,
            "Landroid/content/ComponentName;",
            HostState::Empty,
        ))),
        "equals" | "hashCode" | "compareTo" => Some(Some(V::I32(0))),
        "describeContents" | "writeToParcel" | "clone" => {
            if name == "clone" {
                Some(Some(args[0]))
            } else {
                Some(None)
            }
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// SharedPreferences / Editor — map-backed; Editor writes through

fn prefs_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let class = class_of(vm, args[0]);
    Ok(if class.ends_with("$Editor;") {
        match name {
            "putString" | "putInt" | "putLong" | "putFloat" | "putBoolean"
            | "putStringSet" => {
                // write through to the parent prefs (P_EDIT_PARENT link)
                let parent = pseudo_get(vm, h, P_EDIT_PARENT).unwrap_or(V::Ref(h));
                let ph = match parent {
                    V::Ref(p) => p,
                    _ => h,
                };
                map_put(vm, ph, args[1], args[2]);
                Some(Some(args[0]))
            }
            "remove" | "clear" => {
                let parent = pseudo_get(vm, h, P_EDIT_PARENT).unwrap_or(V::Ref(h));
                if let V::Ref(ph) = parent {
                    if name == "clear" {
                        if let Some(m) = map_mut(vm, ph) {
                            m.clear();
                        }
                    } else {
                        map_remove(vm, ph, args[1]);
                    }
                }
                Some(Some(args[0]))
            }
            "commit" | "apply" => {
                if name == "commit" {
                    Some(Some(V::I32(1)))
                } else {
                    Some(None)
                }
            }
            _ => None,
        }
    } else {
        match name {
            "getString" => Some(Some(
                map_get(vm, h, args[1])
                    .or_else(|| args.get(2).copied())
                    .unwrap_or(V::Ref(0)),
            )),
            "getInt" | "getLong" | "getFloat" | "getBoolean" => {
                let v = map_get(vm, h, args[1])
                    .or_else(|| args.get(2).copied())
                    .unwrap_or(V::I32(0));
                Some(Some(match name {
                    "getLong" => V::I64(i64v(v)),
                    "getFloat" => V::F32(f64v(v) as f32),
                    "getBoolean" => V::I32((i32v(v) != 0) as i32),
                    _ => V::I32(i32v(v)),
                }))
            }
            "getStringSet" => Some(Some(
                map_get(vm, h, args[1])
                    .or_else(|| args.get(2).copied())
                    .unwrap_or(V::Ref(0)),
            )),
            "getAll" => {
                let m: Vec<(V, V)> = match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::Map(m),
                        ..
                    }) => m.clone(),
                    _ => vec![],
                };
                Some(Some(host(vm, "Ljava/util/Map;", HostState::Map(m))))
            }
            "contains" => {
                let c = map_get(vm, h, args[1]).is_some();
                Some(Some(V::I32(c as i32)))
            }
            "edit" => {
                let eh = vm.alloc_host(
                    "Landroid/content/SharedPreferences$Editor;",
                    HostState::Empty,
                );
                pseudo_set(vm, eh, P_EDIT_PARENT, args[0]);
                Some(Some(V::Ref(eh)))
            }
            "registerOnSharedPreferenceChangeListener"
            | "unregisterOnSharedPreferenceChangeListener" => Some(None),
            _ => None,
        }
    })
}

// ---------------------------------------------------------------------
// Context family

fn context_call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "getPackageName" | "getOpPackageName" | "getAttributionTag" => {
            let p = vm.guest_package.clone().unwrap_or_else(|| "app".into());
            Some(Some(jstr(vm, &p)))
        }
        "getPackageManager" => Some(Some(host(
            vm,
            "Landroid/content/pm/PackageManager;",
            HostState::Empty,
        ))),
        "getPackageCodePath" | "getPackageResourcePath" => {
            Some(Some(jstr(vm, "/data/app/base.apk")))
        }
        "getApplicationContext" | "getApplication" | "getBaseContext" => {
            Some(Some(args[0]))
        }
        "getApplicationInfo" | "getActivityInfo" => Some(Some(host(
            vm,
            "Landroid/content/pm/ApplicationInfo;",
            HostState::Empty,
        ))),
        "getContentResolver" => Some(Some(host(
            vm,
            "Landroid/content/ContentResolver;",
            HostState::Empty,
        ))),
        "getMainLooper" | "getLooper" => Some(Some(host(
            vm,
            "Landroid/os/Looper;",
            HostState::Empty,
        ))),
        "getMainExecutor" | "getMainHandler" => Some(Some(host(
            vm,
            "Landroid/os/Handler;",
            HostState::Empty,
        ))),
        "getResources" => Some(Some(host(
            vm,
            "Landroid/content/res/Resources;",
            HostState::Empty,
        ))),
        "getTheme" => Some(Some(host(
            vm,
            "Landroid/content/res/Resources$Theme;",
            HostState::Empty,
        ))),
        "getAssets" => Some(Some(host(
            vm,
            "Landroid/content/res/AssetManager;",
            HostState::Empty,
        ))),
        "getSharedPreferences" => Some(Some(host(
            vm,
            "Landroid/content/SharedPreferences;",
            HostState::Map(vec![]),
        ))),
        "getString" => {
            // resource id → empty/dotted name
            Some(Some(jstr(vm, "")))
        }
        "getText" => Some(Some(jstr(vm, ""))),
        "getColor" => Some(Some(V::I32(0xFF000000u32 as i32))),
        "getDrawable" => Some(Some(V::Ref(0))),
        "getColorStateList" => Some(Some(V::Ref(0))),
        "getSystemService" => {
            // Hand out a typed placeholder per service name so calls on
            // it resolve (void stubs) and instanceof/cast work; real
            // service behavior is still surface-limited.
            let desc = match sv(vm, args[1]).as_deref() {
                Some("activity") => "Landroid/app/ActivityManager;",
                Some("alarm") => "Landroid/app/AlarmManager;",
                Some("appops") => "Landroid/app/AppOpsManager;",
                Some("audio") => "Landroid/media/AudioManager;",
                Some("clipboard") => "Landroid/content/ClipboardManager;",
                Some("connectivity") => "Landroid/net/ConnectivityManager;",
                Some("device_policy") => "Landroid/app/admin/DevicePolicyManager;",
                Some("display") => "Landroid/hardware/display/DisplayManager;",
                Some("download") => "Landroid/app/DownloadManager;",
                Some("input_method") => {
                    "Landroid/view/inputmethod/InputMethodManager;"
                }
                Some("jobscheduler") => "Landroid/app/job/JobScheduler;",
                Some("keyguard") => "Landroid/app/KeyguardManager;",
                Some("layout_inflater") => "Landroid/view/LayoutInflater;",
                Some("location") => "Landroid/location/LocationManager;",
                Some("media_session") => "Landroid/media/session/MediaSessionManager;",
                Some("notification") => "Landroid/app/NotificationManager;",
                Some("package") => "Landroid/content/pm/PackageManager;",
                Some("power") => "Landroid/os/PowerManager;",
                Some("search") => "Landroid/app/SearchManager;",
                Some("sensor") => "Landroid/hardware/SensorManager;",
                Some("storage") => "Landroid/os/storage/StorageManager;",
                Some("telephony") => "Landroid/telephony/TelephonyManager;",
                Some("uimode") => "Landroid/app/UiModeManager;",
                Some("vibrator") | Some("vibrator_manager") => "Landroid/os/Vibrator;",
                Some("wifi") => "Landroid/net/wifi/WifiManager;",
                Some("window") => "Landroid/view/WindowManager;",
                Some("user") => "Landroid/os/UserManager;",
                Some("accessibility") => "Landroid/view/accessibility/AccessibilityManager;",
                Some("account") => "Landroid/accounts/AccountManager;",
                Some("nfc") => "Landroid/nfc/NfcManager;",
                Some("bluetooth") => "Landroid/bluetooth/BluetoothManager;",
                Some("role") => "Landroid/app/role/RoleManager;",
                Some("shortcut") => "Landroid/content/pm/ShortcutManager;",
                Some("textservices") => {
                    "Landroid/view/textservice/TextServicesManager;"
                }
                Some("wallpaper") => "Landroid/app/WallpaperManager;",
                _ => "Ljava/lang/Object;",
            };
            Some(Some(host(vm, desc, HostState::Empty)))
        }
        "getSystemServiceName" => {
            Some(Some(jstr(vm, "java.lang.Object")))
        }
        "getFilesDir" => Some(Some(host(
            vm,
            "Ljava/io/File;",
            HostState::Path("/data/local/ovelo/files".into()),
        ))),
        "getCacheDir" => Some(Some(host(
            vm,
            "Ljava/io/File;",
            HostState::Path("/data/local/ovelo/cache".into()),
        ))),
        "getExternalFilesDir" | "getExternalCacheDir" | "getObbDir"
        | "getCodeCacheDir" | "getDataDir" | "getNoBackupFilesDir"
        | "getExternalMediaDir" => {
            if name == "getExternalMediaDir" {
                Some(Some(V::Ref(0)))
            } else {
                Some(Some(host(
                    vm,
                    "Ljava/io/File;",
                    HostState::Path(format!("/data/local/ovelo/{name}")),
                )))
            }
        }
        "getExternalFilesDirs" | "getExternalCacheDirs" | "getExternalMediaDirs"
        | "getObbDirs" => Some(Some(arr(vm, "Ljava/io/File;", vec![]))),
        "getDir" => Some(Some(host(
            vm,
            "Ljava/io/File;",
            HostState::Path("/data/local/ovelo/dir".into()),
        ))),
        "fileList" | "databaseList" => Some(Some(arr(vm, STRING, vec![]))),
        "openFileInput" => Some(Some(host(
            vm,
            "Ljava/io/FileInputStream;",
            HostState::Empty,
        ))),
        "openFileOutput" => Some(Some(host(
            vm,
            "Ljava/io/FileOutputStream;",
            HostState::Empty,
        ))),
        "openOrCreateDatabase" => Some(Some(host(
            vm,
            "Landroid/database/sqlite/SQLiteDatabase;",
            HostState::Empty,
        ))),
        "deleteFile" | "deleteDatabase" | "deleteSharedPreferences" => {
            Some(Some(V::I32(0)))
        }
        "getDatabasePath" => Some(Some(V::Ref(0))),
        "moveDatabaseFrom" | "moveSharedPreferencesFrom" => Some(Some(V::I32(0))),
        "startActivity" | "startActivities" | "startService" | "stopService"
        | "bindService" | "unbindService" | "sendBroadcast"
        | "sendOrderedBroadcast" | "sendStickyBroadcast" | "registerReceiver"
        | "unregisterReceiver" | "startInstrumentation" | "startIntentSender"
        | "startForegroundService" | "startIntentSenderFromChild"
        | "revokeUriPermission" | "grantUriPermission"
        | "enforcePermission" | "enforceCallingPermission"
        | "enforceCallingOrSelfPermission" | "enforceUriPermission"
        | "checkPermission" | "checkSelfPermission"
        | "checkCallingPermission" | "checkCallingOrSelfPermission"
        | "checkUriPermission" | "checkCallingOrSelfUriPermission"
        | "checkCallingUriPermission" | "registerComponentCallbacks"
        | "unregisterComponentCallbacks" | "setTheme" | "getDisplay"
        | "getDisplayId" | "isRestricted" | "getAttributionSource"
        | "createDeviceProtectedStorageContext" | "createPackageContext"
        | "createContextForSplit" | "createAttributionContext"
        | "createDisplayContext" | "createWindowContext"
        | "moveResources" | "reloadResources" | "bindServiceAsUser"
        | "bindIsolatedService" | "updateServiceGroup" => {
            if name.starts_with("check") {
                Some(Some(V::I32(0))) // PERMISSION_GRANTED
            } else if matches!(name, "bindService" | "bindServiceAsUser"
                | "bindIsolatedService" | "stopService")
            {
                Some(Some(V::I32(1)))
            } else if matches!(
                name,
                "isRestricted" | "createDeviceProtectedStorageContext"
                    | "moveResources" | "reloadResources"
            ) {
                Some(Some(V::I32(0)))
            } else if name.starts_with("create") || name == "getDisplay"
                || name == "registerReceiver"
            {
                Some(Some(V::Ref(0)))
            } else {
                Some(None)
            }
        }
        "getClassLoader" => Some(Some(host(
            vm,
            "Ljava/lang/ClassLoader;",
            HostState::Empty,
        ))),
        "getWallpaper" | "peekWallpaper" => Some(Some(V::Ref(0))),
        "getWallpaperDesiredMinimumWidth" | "getWallpaperDesiredMinimumHeight" => {
            Some(Some(V::I32(0)))
        }
        "setWallpaper" | "clearWallpaper" => Some(None),
        // ----- Activity instance methods
        "getIntent" => Some(Some(host(
            vm,
            "Landroid/content/Intent;",
            HostState::Map(vec![]),
        ))),
        "setIntent" | "setContentView" | "addContentView" | "requestWindowFeature"
        | "setTitle" | "setTitleColor" | "setVisible" | "setResult"
        | "setRequestedOrientation" | "setTaskDescription"
        | "registerActivityLifecycleCallbacks"
        | "unregisterActivityLifecycleCallbacks" | "invalidateOptionsMenu"
        | "supportRequestWindowFeature" | "onWindowFocusChanged"
        | "openOptionsMenu" | "closeOptionsMenu" | "openContextMenu"
        | "closeContextMenu" | "registerForContextMenu"
        | "unregisterForContextMenu" | "showDialog" | "dismissDialog"
        | "removeDialog" | "managedQuery" | "requestPermissions"
        | "requestDragAndDropPermissions" | "startActivityForResult"
        | "startActivityFromChild" | "startActivityFromFragment"
        | "startActivityIfNeeded" | "startNextMatchingActivity"
        | "startLocalVoiceInteraction" | "stopLocalVoiceInteraction"
        | "startSearch" | "triggerSearch" | "takeKeyEvents"
        | "overridePendingTransition" | "requestShowKeyboardShortcuts"
        | "requestVisibleBehind" | "setContentTransitionManager"
        | "setEnterSharedElementCallback" | "setExitSharedElementCallback"
        | "setFinishOnTouchOutside" | "setImmersive" | "setInheritShowWhenLocked"
        | "setLocusContext" | "setPictureInPictureParams"
        | "setRecentsScreenshotEnabled" | "setShouldDockBigOverlays"
        | "setShowWhenLocked" | "setTranslucent" | "setTurnScreenOn"
        | "setVrModeEnabled" | "showAssist" | "showLockTaskEscapeMessage"
        | "startLockTask" | "stopLockTask" | "requestFullscreenMode"
        | "reportFullyDrawn" | "dismissKeyboardShortcutsHelper"
        | "dismissKeyguard" | "onProvideKeyboardShortcuts" => Some(None),
        "findViewById" => Some(Some(V::Ref(0))),
        "setContentView_Int" => Some(None),
        "getWindow" | "getWindowManager" | "getActionBar" | "getFragmentManager"
        | "getLoaderManager" | "getMenuInflater" | "getLayoutInflater"
        | "getMediaController" | "getSearchEvent" | "getCallingActivity"
        | "getCallingPackage" | "getReferrer" | "getTaskId" | "getMaxNumPictureInPictureActions"
        | "getParentActivityIntent" | "getLocalClassName" | "getComponentName"
        | "getVoiceInteractor" | "getActivityToken" | "getAssistToken"
        | "getCurrentFocus" | "getLastNonConfigurationInstance"
        | "onRetainNonConfigurationInstance" | "isTaskRoot"
        | "isFinishing" | "isDestroyed" | "isChangingConfigurations"
        | "isInMultiWindowMode" | "isInPictureInPictureMode"
        | "isImmersive" | "isLaunchedFromBubble" | "isLocalVoiceInteractionSupported"
        | "hasWindowFocus" | "moveTaskToBack" | "finish" | "finishAffinity"
        | "finishActivity" | "finishActivityFromChild" | "finishAfterTransition"
        | "finishAndRemoveTask" | "finishFromChild" | "onBackPressed"
        | "onUserInteraction" | "onPostResume" | "navigateUpTo"
        | "navigateUpToFromChild" | "shouldUpRecreateTask" => {
            match name {
                "getTaskId" => Some(Some(V::I32(1))),
                "isTaskRoot" | "hasWindowFocus" | "isInMultiWindowMode"
                | "isInPictureInPictureMode" | "isImmersive"
                | "isLaunchedFromBubble" | "isLocalVoiceInteractionSupported"
                | "isFinishing" | "isDestroyed" | "isChangingConfigurations" => {
                    Some(Some(V::I32(
                        matches!(name, "isTaskRoot" | "hasWindowFocus") as i32,
                    )))
                }
                "finish" | "finishAffinity" | "finishActivity"
                | "finishActivityFromChild" | "finishAfterTransition"
                | "finishAndRemoveTask" | "finishFromChild" | "onBackPressed"
                | "onUserInteraction" | "onPostResume" | "onRetainNonConfigurationInstance"
                | "moveTaskToBack" => Some(None),
                "navigateUpTo" | "navigateUpToFromChild" | "shouldUpRecreateTask"
                | "startActivityIfNeeded" | "startNextMatchingActivity" => {
                    Some(Some(V::I32(0)))
                }
                "getLocalClassName" => {
                    let c = class_of(vm, args[0]);
                    Some(Some(jstr(vm, &dotted(&c))))
                }
                "getComponentName" => Some(Some(host(
                    vm,
                    "Landroid/content/ComponentName;",
                    HostState::StrBuf(dotted(&class_of(vm, args[0]))),
                ))),
                "getIntent" => Some(Some(host(
                    vm,
                    "Landroid/content/Intent;",
                    HostState::Map(vec![]),
                ))),
                "getMaxNumPictureInPictureActions" => Some(Some(V::I32(0))),
                "onProvideKeyboardShortcuts" => Some(None),
                _ => Some(Some(V::Ref(0))),
            }
        }
        "runOnUiThread" => {
            if let V::Ref(r) = args[1] {
                let _ = vm.call_virtual(r, "run", &[]);
            }
            Some(None)
        }
        "recreate" => {
            let _ = vm.call_virtual(
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
                "onCreate",
                &[V::Ref(0)],
            );
            Some(None)
        }
        "onCreate" | "onStart" | "onResume" | "onPause" | "onStop"
        | "onDestroy" | "onRestart" | "onSaveInstanceState"
        | "onRestoreInstanceState" | "onActivityResult" | "onNewIntent"
        | "onRequestPermissionsResult" | "onCreateOptionsMenu"
        | "onOptionsItemSelected" | "onCreateContextMenu"
        | "onContextItemSelected" | "onOptionsMenuClosed"
        | "onPrepareOptionsMenu" | "onConfigurationChanged"
        | "onLowMemory" | "onTrimMemory" | "onMultiWindowModeChanged"
        | "onPictureInPictureModeChanged" | "onWindowAttributesChanged"
        | "onMenuOpened" | "onContextMenuClosed" | "onAttachedToWindow"
        | "onDetachedFromWindow" | "onEnterAnimationComplete"
        | "onGetDirectActions" | "onPerformDirectAction"
        | "onProvideAssistContent" | "onProvideAssistData"
        | "onStateNotSaved" | "onTopResumedActivityChanged" => {
            // Reached only via invoke-super — direct calls resolve to
            // the guest override first, and host-side lifecycle delivery
            // goes through call_virtual. The framework default is empty;
            // re-dispatching here would recurse through super.onX().
            Some(None)
        }
        _ => {
            let _ = (class, sig);
            None
        }
    })
}

// ---------------------------------------------------------------------
// PackageManager / Resources / res misc

fn pm_call(vm: &mut Vm, name: &str, _args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "getPackageInfo" => {
            let h = vm.alloc_host("Landroid/content/pm/PackageInfo;", HostState::Empty);
            let pkg = vm.guest_package.clone().unwrap_or_else(|| "app".into());
            let pv = jstr(vm, &pkg);
            pseudo_set(vm, h, P_ACTION, pv);
            if let Some(fidx) = vm.dex.field_index_of(
                "Landroid/content/pm/PackageInfo;",
                "packageName",
            ) {
                pseudo_set(vm, h, fidx, pv);
            }
            if let Some(fidx) = vm
                .dex
                .field_index_of("Landroid/content/pm/PackageInfo;", "versionCode")
            {
                pseudo_set(vm, h, fidx, V::I32(1));
            }
            Some(Some(V::Ref(h)))
        }
        "getApplicationInfo" => Some(Some(host(
            vm,
            "Landroid/content/pm/ApplicationInfo;",
            HostState::Empty,
        ))),
        "getActivityInfo" | "getServiceInfo" | "getReceiverInfo"
        | "getProviderInfo" | "resolveActivity" | "resolveService"
        | "resolveContentProvider" | "getInstallerPackageName"
        | "getLaunchIntentForPackage" | "getLeanbackLaunchIntentForPackage"
        | "getDrawable" | "getActivityIcon" | "getApplicationIcon"
        | "getDefaultActivityIcon" | "getActivityBanner"
        | "getApplicationBanner" | "getApplicationLabel"
        | "getPermissionGroupInfo" | "getPermissionInfo"
        | "getPackageArchiveInfo" | "getResourcesForActivity"
        | "getResourcesForApplication" | "getText" | "getXml"
        | "getSystemSharedLibraryNames" | "getChangedPackages"
        | "getSharedLibraries" | "getInstantAppCookie"
        | "getSuspendedPackageAppExtras" | "getUnbadgedIcon"
        | "getUserBadgedIcon" | "getUserBadgedLabel"
        | "getUserBadgedDrawableForDensity" | "getPackagesForUid"
        | "getNameForUid" | "getSyntheticAppDetailsActivityEnabled"
        | "queryIntentActivities" | "queryIntentActivityOptions"
        | "queryIntentServices" | "queryIntentContentProviders"
        | "queryBroadcastReceivers" | "getInstalledPackages"
        | "getInstalledApplications" | "getPackagesHoldingPermissions"
        | "checkPermission" | "checkSignatures" | "hasSystemFeature"
        | "isInstantApp" | "isPackageSuspended" | "canRequestPackageInstalls"
        | "hasSigningCertificate" | "addPermission" | "addPermissionAsync"
        | "removePermission" | "clearPackagePreferredActivities"
        | "addPackageToPreferred" | "removePackageFromPreferred"
        | "getPreferredPackages" | "getPreferredActivities"
        | "setComponentEnabledSetting" | "getComponentEnabledSetting"
        | "setApplicationEnabledSetting" | "getApplicationEnabledSetting"
        | "isSafeMode" | "getPackageInstaller" | "extendVerificationTimeout"
        | "verifyPendingInstall" | "setInstallerPackageName"
        | "updateInstantAppCookie" | "clearInstantAppCookie" | "getModuleInfo"
        | "getInstallReason" | "getInstallSourceInfo" => {
            match name {
                "getInstallerPackageName" | "getLaunchIntentForPackage"
                | "getLeanbackLaunchIntentForPackage" | "getDrawable"
                | "getActivityIcon" | "getApplicationIcon"
                | "getDefaultActivityIcon" | "getActivityBanner"
                | "getApplicationBanner" | "getPermissionGroupInfo"
                | "getPermissionInfo" | "getPackageArchiveInfo"
                | "getResourcesForActivity" | "getResourcesForApplication"
                | "getText" | "getXml" | "getSystemSharedLibraryNames"
                | "getChangedPackages" | "getSharedLibraries"
                | "getInstantAppCookie" | "getSuspendedPackageAppExtras"
                | "getUnbadgedIcon" | "getUserBadgedIcon"
                | "getUserBadgedLabel" | "getUserBadgedDrawableForDensity"
                | "getPackagesForUid" | "getNameForUid"
                | "resolveActivity" | "resolveService"
                | "resolveContentProvider" | "getModuleInfo"
                | "getInstallSourceInfo" | "getPackageInstaller" => {
                    if name == "getApplicationLabel" {
                        Some(Some(jstr(vm, "app")))
                    } else {
                        Some(Some(V::Ref(0)))
                    }
                }
                "getApplicationLabel" => Some(Some(jstr(vm, "app"))),
                "queryIntentActivities" | "queryIntentActivityOptions"
                | "queryIntentServices" | "queryIntentContentProviders"
                | "queryBroadcastReceivers" | "getInstalledPackages"
                | "getInstalledApplications" | "getPackagesHoldingPermissions"
                | "getPreferredPackages" => Some(Some(host(
                    vm,
                    "Ljava/util/List;",
                    HostState::List(vec![]),
                ))),
                "checkPermission" | "checkSignatures" => {
                    Some(Some(V::I32(0)))
                }
                "hasSystemFeature" | "isInstantApp" | "isPackageSuspended"
                | "canRequestPackageInstalls" | "hasSigningCertificate"
                | "isSafeMode" | "getSyntheticAppDetailsActivityEnabled" => {
                    Some(Some(V::I32(0)))
                }
                "getComponentEnabledSetting" | "getApplicationEnabledSetting" => {
                    Some(Some(V::I32(1)))
                }
                "getPreferredActivities" | "getInstallReason" => {
                    Some(Some(V::I32(0)))
                }
                _ => Some(None),
            }
        }
        _ => None,
    })
}

fn resources_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "getString" => Some(Some(jstr(vm, ""))),
        "getText" => Some(Some(jstr(vm, ""))),
        "getQuantityString" | "getQuantityText" => {
            Some(Some(args.get(2).copied().unwrap_or(jstr(vm, ""))))
        }
        "getStringArray" | "getTextArray" => Some(Some(arr(vm, STRING, vec![]))),
        "getIntArray" => Some(Some(arr(vm, "I", vec![]))),
        "getBoolean" => Some(Some(V::I32(0))),
        "getInteger" | "getColor" | "getDimension" | "getDimensionPixelSize"
        | "getDimensionPixelOffset" | "getLayout" | "getIdentifier"
        | "obtainTypedArray" | "obtainAttributes" | "getDrawable"
        | "getDrawableForDensity" | "getColorStateList" | "getFont"
        | "getXml" | "getRawResource" | "openRawResource"
        | "openRawResourceFd" | "getValue" | "getValueForDensity"
        | "getResourceName" | "getResourcePackageName"
        | "getResourceTypeName" | "getResourceEntryName" | "getDisplayMetrics"
        | "getConfiguration" | "getAssets" | "getSystem" | "newTheme"
        | "getFraction" | "getMovie" => {
            match name {
                "getInteger" | "getColor" | "getDimensionPixelSize"
                | "getDimensionPixelOffset" | "getIdentifier"
                | "getLayout" => Some(Some(V::I32(0))),
                "getDimension" | "getFraction" => Some(Some(V::F32(0.0))),
                "getDisplayMetrics" => Some(Some(host(
                    vm,
                    "Landroid/util/DisplayMetrics;",
                    HostState::Empty,
                ))),
                "getConfiguration" => Some(Some(host(
                    vm,
                    "Landroid/content/res/Configuration;",
                    HostState::Empty,
                ))),
                "getAssets" => Some(Some(host(
                    vm,
                    "Landroid/content/res/AssetManager;",
                    HostState::Empty,
                ))),
                "getSystem" | "newTheme" => Some(Some(host(
                    vm,
                    "Landroid/content/res/Resources;",
                    HostState::Empty,
                ))),
                "obtainTypedArray" | "obtainAttributes" => Some(Some(host(
                    vm,
                    "Landroid/content/res/TypedArray;",
                    HostState::Empty,
                ))),
                "getResourceName" | "getResourcePackageName"
                | "getResourceTypeName" | "getResourceEntryName" => {
                    Some(Some(jstr(vm, "")))
                }
                _ => Some(Some(V::Ref(0))),
            }
        }
        "updateConfiguration" | "flushLayoutCache" | "finishPreloading"
        | "clearCachesForUntrustedConfigs" | "registerResourcePaths"
        | "getCompatibilityInfo" | "getCompatibilityInfo0" => Some(None),
        "getQuantityText0" => Some(Some(V::Ref(0))),
        _ => None,
    })
}

fn res_misc(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match (class, name) {
        ("Landroid/util/DisplayMetrics;", "scaledDensity") => Some(Some(V::F32(2.0))),
        ("Landroid/util/DisplayMetrics;", _) => None,
        ("Landroid/content/res/TypedArray;", "getString" | "getText") => {
            Some(Some(V::Ref(0)))
        }
        ("Landroid/content/res/TypedArray;", "getInt" | "getColor"
        | "getBoolean" | "getInteger" | "getResourceId" | "getDimensionPixelSize"
        | "getDimensionPixelOffset" | "getLayoutDimension") => {
            Some(Some(args.get(2).copied().unwrap_or(V::I32(0))))
        }
        ("Landroid/content/res/TypedArray;", "getFloat" | "getDimension") => {
            Some(Some(args.get(2).copied().unwrap_or(V::F32(0.0))))
        }
        ("Landroid/content/res/TypedArray;", "getTextArray") => {
            Some(Some(V::Ref(0)))
        }
        ("Landroid/content/res/TypedArray;", "getDrawable" | "getFont"
        | "getColorStateList") => Some(Some(V::Ref(0))),
        ("Landroid/content/res/TypedArray;", "hasValue" | "hasValueOrEmpty") => {
            Some(Some(V::I32(0)))
        }
        ("Landroid/content/res/TypedArray;", "length" | "getIndexCount") => {
            Some(Some(V::I32(0)))
        }
        ("Landroid/content/res/TypedArray;", "getIndex" | "getType") => {
            Some(Some(V::I32(0)))
        }
        ("Landroid/content/res/TypedArray;", "recycle" | "extractThemeAttrs"
        | "getChangingConfigurations" | "getValue" | "peekValue") => {
            if name == "peekValue" || name == "getValue" {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        ("Landroid/util/TypedValue;", "applyDimension" | "complexToDimension"
        | "complexToDimensionPixelSize" | "complexToDimensionPixelOffset"
        | "complexToFloat" | "complexToFraction") => {
            Some(Some(V::F32(f64v(args.get(1).copied().unwrap_or(V::F32(0.0))) as f32)))
        }
        ("Landroid/content/res/ColorStateList;", "valueOf") => {
            Some(Some(host(vm, class, HostState::Slot(args[0]))))
        }
        ("Landroid/content/res/ColorStateList;", "getDefaultColor") => {
            Some(Some(match args[0] {
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::I32(0)),
                v => v,
            }))
        }
        ("Landroid/content/res/ColorStateList;", "getColorForState") => {
            Some(Some(match args[0] {
                V::Ref(h) => slot_of(vm, h).unwrap_or(V::I32(0)),
                _ => V::I32(0),
            }))
        }
        ("Landroid/content/res/AssetManager;", "open" | "openFd"
        | "openNonAssetFd" | "openXmlResourceParser" | "list") => {
            if name == "list" {
                Some(Some(arr(vm, STRING, vec![])))
            } else {
                Some(Some(V::Ref(0)))
            }
        }
        ("Landroid/content/res/AssetManager;", "close" | "addAssetPath"
        | "isUpToDate" | "getLocales" | "getConfigurations") => {
            if name == "addAssetPath" {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        ("Landroid/content/res/Resources$Theme;", "obtainStyledAttributes") => {
            Some(Some(host(vm, "Landroid/content/res/TypedArray;", HostState::Empty)))
        }
        ("Landroid/content/res/Resources$Theme;", "applyStyle" | "setTo"
        | "resolveAttribute" | "rebase" | "dump" | "getKey" | "newTheme") => {
            if name == "resolveAttribute" || name == "getKey" {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        ("Landroid/content/res/Configuration;", "setTo" | "updateFrom"
        | "diff" | "isLayoutSizeAtLeast" | "setLayoutDirection"
        | "setLocale" | "setLocales" | "readFromProto" | "writeToProto"
        | "describeContents" | "writeToParcel" | "getLayoutDirection"
        | "isScreenWideColorGamut" | "isScreenHdr" | "isScreenRound"
        | "getLocales" | "getGrammaticalGender") => {
            if name.starts_with("is") || name == "getLayoutDirection" || name == "diff" {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Handler / Looper — run posted work inline (single looper model)

fn handler_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "post" | "postDelayed" | "postAtTime" | "postAtFrontOfQueue"
        | "postAtTimeWithDelivery" | "sendMessage" | "sendMessageDelayed"
        | "sendMessageAtTime" | "sendMessageAtFrontOfQueue"
        | "sendEmptyMessage" | "sendEmptyMessageDelayed"
        | "sendEmptyMessageAtTime" | "postWithDeliveryResult"
        | "postAsIdle" | "executeOrSendMessage" | "sendToTarget" => {
            if let Some(V::Ref(r)) = args.get(1).copied().or_else(|| {
                args.get(1).copied()
            }) {
                let _ = vm.call_virtual(r, "run", &[]);
            }
            Some(Some(V::I32(1)))
        }
        "handleMessage" | "dispatchMessage" | "callback" => {
            if let V::Ref(h) = args[0] {
                if let Ok(v) = vm.call_virtual(h, "handleMessage", &args[1..]) {
                    return Ok(Some(v));
                }
            }
            Some(None)
        }
        "removeCallbacks" | "removeMessages" | "removeCallbacksAndMessages"
        | "removeEqualMessages" => Some(None),
        "hasMessages" | "hasCallbacks" | "hasEqualMessages" => {
            Some(Some(V::I32(0)))
        }
        "obtainMessage" => Some(Some(host(
            vm,
            "Landroid/os/Message;",
            HostState::Empty,
        ))),
        "getLooper" => Some(Some(host(vm, "Landroid/os/Looper;", HostState::Empty))),
        "getMessageName" | "getTraceName" => Some(Some(jstr(vm, ""))),
        "createAsync" => Some(Some(args[0])),
        _ => None,
    })
}

fn looper_call(vm: &mut Vm, name: &str, _args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "getMainLooper" | "myLooper" | "prepare" | "prepareMainLooper" => {
            Some(Some(host(vm, "Landroid/os/Looper;", HostState::Empty)))
        }
        "loop" | "quit" | "quitSafely" => Some(None),
        "isCurrentThread" => Some(Some(V::I32(1))),
        "getThread" => Some(Some(main_thread(vm))),
        "getQueue" => Some(Some(host(
            vm,
            "Landroid/os/MessageQueue;",
            HostState::Empty,
        ))),
        "setMessageLogging" | "dump" => Some(None),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Cursor / database — in-memory rows via Map of column→value lists

fn cursor_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(_h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "getCount" => Some(Some(V::I32(0))),
        "getPosition" => Some(Some(V::I32(-1))),
        "moveToFirst" | "moveToNext" | "moveToLast" | "moveToPrevious"
        | "moveToPosition" | "move" | "isFirst" | "isLast" | "isBeforeFirst"
        | "isAfterLast" => {
            Some(Some(V::I32(
                matches!(name, "isBeforeFirst" | "isAfterLast") as i32,
            )))
        }
        "getColumnIndex" | "getColumnIndexOrThrow" => Some(Some(V::I32(-1))),
        "getColumnName" => Some(Some(jstr(vm, ""))),
        "getColumnNames" => Some(Some(arr(vm, STRING, vec![]))),
        "getColumnCount" => Some(Some(V::I32(0))),
        "getString" | "getBlob" | "getInt" | "getLong" | "getShort"
        | "getFloat" | "getDouble" | "getType" | "isNull" | "getExtras" => {
            if name.starts_with("is") || name == "getType" {
                Some(Some(V::I32(0)))
            } else if name == "getExtras" {
                Some(Some(V::Ref(0)))
            } else {
                Some(Some(V::I64(0)))
            }
        }
        "close" | "deactivate" | "requery" | "registerContentObserver"
        | "unregisterContentObserver" | "registerDataSetObserver"
        | "unregisterDataSetObserver" | "setNotificationUri"
        | "getNotificationUri" | "getWantsAllOnMoveCalls"
        | "setExtras" | "respond" => {
            if name == "requery" {
                Some(Some(V::I32(0)))
            } else if name == "getNotificationUri" || name == "respond" {
                Some(Some(V::Ref(0)))
            } else if name == "getWantsAllOnMoveCalls" {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        "copyStringToBuffer" => Some(None),
        // SQLiteDatabase/SQLiteOpenHelper methods
        "execSQL" | "beginTransaction" | "endTransaction"
        | "setTransactionSuccessful" | "inTransaction" | "isOpen" | "open"
        | "compileStatement" | "insert" | "insertOrThrow" | "replace"
        | "replaceOrThrow" | "update" | "updateWithOnConflict" | "delete"
        | "query" | "queryWithFactory" | "rawQuery"
        | "rawQueryWithFactory" | "insertWithOnConflict" | "getVersion"
        | "setVersion" | "getMaximumSize" | "setMaximumSize"
        | "getPageSize" | "setPageSize" | "getPath" | "isReadOnly"
        | "isDbLockedByCurrentThread" | "isDbLockedByOtherThreads"
        | "yieldIfContendedSafely" | "setForeignKeyConstraintsEnabled"
        | "enableWriteAheadLogging" | "disableWriteAheadLogging"
        | "isWriteAheadLoggingEnabled" | "getAttachedDbs"
        | "isDatabaseIntegrityOk" | "onCreate" | "onUpgrade"
        | "onDowngrade" | "onOpen" | "onConfigure" | "getWritableDatabase"
        | "getReadableDatabase" | "getDatabaseName" | "setIdleConnectionTimeout"
        | "acquireReference" | "releaseReference" | "close0" => {
            if matches!(
                name,
                "insert" | "insertOrThrow" | "replace" | "replaceOrThrow"
                    | "insertWithOnConflict" | "update" | "updateWithOnConflict"
                    | "delete" | "getVersion" | "getPageSize"
            ) {
                Some(Some(V::I64(0)))
            } else if matches!(name, "query" | "queryWithFactory" | "rawQuery"
                | "rawQueryWithFactory" | "compileStatement")
            {
                Some(Some(host(vm, "Landroid/database/Cursor;", HostState::Empty)))
            } else if matches!(name, "isOpen" | "isReadOnly" | "inTransaction"
                | "isDbLockedByCurrentThread" | "isDbLockedByOtherThreads"
                | "yieldIfContendedSafely" | "isWriteAheadLoggingEnabled"
                | "isDatabaseIntegrityOk" | "enableWriteAheadLogging")
            {
                Some(Some(V::I32(
                    matches!(name, "isOpen" | "yieldIfContendedSafely"
                        | "isDatabaseIntegrityOk") as i32,
                )))
            } else if name == "getWritableDatabase" || name == "getReadableDatabase" {
                Some(Some(args[0]))
            } else if matches!(name, "getPath" | "getDatabaseName") {
                Some(Some(jstr(vm, "")))
            } else if name == "getAttachedDbs" || name == "getMaximumSize" {
                Some(Some(V::Ref(0)))
            } else {
                Some(None)
            }
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// android.graphics — Rect/Point/Color/Bitmap stubs

fn graphics_call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(match (class, name) {
        ("Landroid/graphics/Color;", "argb") | ("Landroid/graphics/Color;", "rgb") => {
            let (a, r, g, b) = if name == "argb" && args.len() >= 4 {
                (
                    i32v(args[0]),
                    i32v(args[1]),
                    i32v(args[2]),
                    i32v(args[3]),
                )
            } else {
                (255, i32v(args[0]), i32v(args[1]), i32v(args[2]))
            };
            Some(Some(V::I32(
                (a & 0xff) << 24 | (r & 0xff) << 16 | (g & 0xff) << 8 | (b & 0xff),
            )))
        }
        ("Landroid/graphics/Color;", "alpha" | "red" | "green" | "blue") => {
            let c = i32v(args[0]);
            Some(Some(V::I32(match name {
                "alpha" => (c >> 24) & 0xff,
                "red" => (c >> 16) & 0xff,
                "green" => (c >> 8) & 0xff,
                _ => c & 0xff,
            })))
        }
        ("Landroid/graphics/Color;", "parseColor") => {
            let t = sv(vm, args[0]).unwrap_or_default();
            let t = t.trim_start_matches('#');
            let v = u32::from_str_radix(t, 16).unwrap_or(0xFF000000);
            let v = if t.len() == 6 { v | 0xFF000000 } else { v };
            Some(Some(V::I32(v as i32)))
        }
        ("Landroid/graphics/Color;", "HSVToColor") => Some(Some(V::I32(0))),
        ("Landroid/graphics/Color;", "colorToHSV" | "valueOf" | "luminance") => {
            Some(Some(V::I32(0)))
        }
        ("Landroid/graphics/Rect;", n) => {
            let _ = n;
            Some(Some(V::I32(0)))
        }
        ("Landroid/graphics/RectF;", n) => {
            let _ = n;
            Some(Some(V::I32(0)))
        }
        ("Landroid/graphics/Point;", "set" | "offset" | "negate" | "equals") => {
            if name == "equals" {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        ("Landroid/util/AttributeSet;", "getAttributeValue"
        | "getAttributeName" | "getAttributeResourceValue"
        | "getAttributeIntValue" | "getAttributeBooleanValue"
        | "getAttributeFloatValue" | "getAttributeCount" | "getAttributeNameResource"
        | "getPositionDescription" | "getIdAttribute"
        | "getIdAttributeResourceValue" | "getClassAttribute"
        | "getStyleAttribute" | "getAttributeUnsignedIntValue") => {
            if name.contains("Count") || name.contains("Resource")
                || name.contains("Int") || name == "getStyleAttribute"
                || name == "getIdAttributeResourceValue"
            {
                Some(Some(args.last().copied().unwrap_or(V::I32(0))))
            } else if name == "getAttributeBooleanValue" {
                Some(Some(V::I32(0)))
            } else {
                Some(Some(V::Ref(0)))
            }
        }
        ("Landroid/util/Patterns;", _) => Some(Some(V::Ref(0))),
        ("Landroid/util/Size;", "getWidth" | "getHeight") => {
            Some(Some(V::I32(0)))
        }
        ("Landroid/util/SizeF;", "getWidth" | "getHeight") => {
            Some(Some(V::F32(0.0)))
        }
        (_, "createBitmap" | "createScaledBitmap" | "decodeResource"
        | "decodeFile" | "decodeStream" | "decodeByteArray" | "decodeFileDescriptor"
        | "inflate" | "getDrawable" | "obtain" | "obtainStyledAttributes") => {
            if name == "inflate" || name == "obtainStyledAttributes" {
                Some(Some(host(
                    vm,
                    "Landroid/content/res/TypedArray;",
                    HostState::Empty,
                )))
            } else {
                Some(Some(host(vm, class, HostState::Empty)))
            }
        }
        (_, n) if n.starts_with("set") || n.starts_with("draw") => Some(None),
        (_, n) if n.starts_with("get") || n.starts_with("is") || n.starts_with("has") => {
            Some(Some(V::I32(0)))
        }
        _ => None,
    })
}
